use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const ENV_PATH: &str = "/etc/xboard-audit/.env";
const LOG_DIR: &str = "/var/log/xboard-audit/nodes";
const STATE_PATH: &str = "/var/lib/xboard-audit/state.json";
const LOCK_PATH: &str = "/var/lib/xboard-audit/maintenance.lock";
const MAX_MESSAGE_CHARS: usize = 3900;
const MAX_EVENTS: usize = 50_000;
const TX_TTL_SECONDS: i64 = 300;

unsafe extern "C" { fn flock(fd: i32, operation: i32) -> i32; }

fn exclusive_lock(path: &Path) -> Result<File, String> {
    let lock = OpenOptions::new().read(true).write(true).open(path).map_err(|_| "无法打开维护锁文件")?;
    // Linux LOCK_EX = 2. Dropping the file releases the lock even on failure.
    if unsafe { flock(lock.as_raw_fd(), 2) } != 0 { return Err("无法等待维护锁".into()); }
    Ok(lock)
}

fn wait_for_telegram_window() -> Result<File, String> { exclusive_lock(Path::new(LOCK_PATH)) }

#[derive(Clone)]
struct Config {
    token: String,
    chat_ids: Vec<String>,
    interval: Duration,
    retention_hours: i64,
}

fn load_config(path: &Path) -> Result<Config, String> {
    let content = fs::read_to_string(path).map_err(|_| "无法读取主机 .env".to_string())?;
    let mut values = HashMap::new();
    for (line_no, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let (key, value) = line.split_once('=').ok_or_else(|| format!(".env 第 {} 行格式错误", line_no + 1))?;
        let value = value.trim();
        let value = if value.len() >= 2 && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\''))) {
            &value[1..value.len() - 1]
        } else { value };
        values.insert(key.trim().to_string(), value.to_string());
    }
    let token = values.remove("TG_BOT_TOKEN").unwrap_or_default();
    if token.is_empty() || token == "replace-me" || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-') {
        return Err("TG_BOT_TOKEN 未配置或格式错误".into());
    }
    let chat_ids: Vec<String> = values.remove("TG_CHAT_IDS").unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    if chat_ids.is_empty() || chat_ids.iter().any(|s| s.parse::<i64>().is_err()) {
        return Err("TG_CHAT_IDS 必须是逗号分隔的数字 ID".into());
    }
    let interval = values.get("PUSH_INTERVAL_SECONDS").map(String::as_str).unwrap_or("60").parse::<u64>().map_err(|_| "PUSH_INTERVAL_SECONDS 必须是数字")?;
    if !(10..=3600).contains(&interval) { return Err("PUSH_INTERVAL_SECONDS 必须在 10–3600 秒之间".into()); }
    let retention_hours = values.get("LOG_RETENTION_HOURS").map(String::as_str).unwrap_or("48").parse::<i64>().map_err(|_| "LOG_RETENTION_HOURS 必须是数字")?;
    if !(1..=8760).contains(&retention_hours) { return Err("LOG_RETENTION_HOURS 必须在 1–8760 小时之间".into()); }
    Ok(Config { token, chat_ids, interval: Duration::from_secs(interval), retention_hours })
}

#[derive(Clone, Serialize, Deserialize)]
struct Event {
    node: String,
    epoch: i64,
    uuid: String,
    src: String,
    protocol: String,
    target: String,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Cursor { dev: u64, ino: u64, offset: u64 }

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    chat_id: String,
    text: String,
    newest_epoch: i64,
    #[serde(default)]
    retry_at_epoch: i64,
}

#[derive(Serialize, Deserialize)]
struct TxEntry { src: String, seen: i64 }

#[derive(Default, Serialize, Deserialize)]
struct State {
    cursors: HashMap<String, Cursor>,
    events: Vec<Event>,
    pending: Vec<Pending>,
    #[serde(default)]
    dead_letters: Vec<Pending>,
    transactions: HashMap<String, TxEntry>,
}

fn read_state(path: &Path) -> State {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            eprintln!("状态文件损坏；已停止以避免重复发送，请检查状态文件");
            std::process::exit(1)
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(_) => { eprintln!("无法读取状态文件"); std::process::exit(1) }
    }
}

fn save_state(path: &Path, state: &State) -> Result<(), String> {
    let parent = path.parent().ok_or("状态路径无效")?;
    fs::create_dir_all(parent).map_err(|_| "无法创建状态目录")?;
    let tmp = parent.join("state.json.tmp");
    let bytes = serde_json::to_vec(state).map_err(|_| "无法编码状态")?;
    {
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp).map_err(|_| "无法写入状态")?;
        file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|_| "无法同步状态")?;
    }
    fs::rename(&tmp, path).map_err(|_| "无法替换状态")?;
    File::open(parent).and_then(|f| f.sync_all()).map_err(|_| "无法同步状态目录")?;
    Ok(())
}

fn persist_if_changed(path: &Path, state: &State, previous: &mut Vec<u8>) -> Result<(), String> {
    let current = serde_json::to_vec(state).map_err(|_| "无法编码状态")?;
    if current != *previous {
        save_state(path, state)?;
        *previous = current;
    }
    Ok(())
}

struct Parser { from: Regex, to: Regex }
struct From { tx: String, src: String }
struct To { tx: String, event: Event }

impl Parser {
    fn new() -> Self {
        let head = r"(?P<tz>[+-]\d{4})\s+(?P<time>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})\s+INFO\s+\[(?P<tx>\d+)\s+\d+ms\]\s+inbound/(?P<protocol>[A-Za-z0-9_-]+)\[[^\]]*\]:\s+";
        Self {
            from: Regex::new(&(head.to_owned() + r"inbound connection from\s+(?P<src>\[[^\]]+\]|[^:\s]+):\d+")).expect("from regex"),
            to: Regex::new(&(head.to_owned() + r"\[(?P<uuid>[A-Fa-f0-9-]+)\]\s+inbound connection to\s+(?P<target>\S+)" )).expect("to regex"),
        }
    }
    fn parse_from(&self, line: &str) -> Option<From> {
        let c = self.from.captures(line)?;
        Some(From { tx: c.name("tx")?.as_str().to_owned(), src: c.name("src")?.as_str().to_owned() })
    }
    fn parse_to(&self, node: &str, line: &str) -> Option<To> {
        let c = self.to.captures(line)?;
        let stamp = format!("{} {}", c.name("time")?.as_str(), c.name("tz")?.as_str());
        let epoch = DateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S %z").ok()?.timestamp();
        let target: String = c.name("target")?.as_str().chars().filter(|ch| !ch.is_control()).take(1024).collect();
        Some(To { tx: c.name("tx")?.as_str().to_owned(), event: Event {
            node: node.to_owned(), epoch, uuid: c.name("uuid")?.as_str().to_owned(),
            src: String::new(), protocol: c.name("protocol")?.as_str().to_owned(),
            target,
        } })
    }
}

struct Reader { file: BufReader<File>, dev: u64, ino: u64 }

fn node_files() -> Result<Vec<(String, PathBuf)>, String> {
    let mut found = Vec::new();
    let entries = fs::read_dir(LOG_DIR).map_err(|_| "无法读取节点日志目录")?;
    for entry in entries {
        let entry = entry.map_err(|_| "无法遍历节点日志目录")?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") { continue; }
        let Some(node) = path.file_stem().and_then(|s| s.to_str()) else { continue; };
        if !node.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { continue; }
        found.push((node.to_owned(), path));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

// The logrotate suffix is sortable. Replay every retained rotation from the
// persisted inode before opening the current file, including after a long stop.
fn replay_rotations(node: &str, dir: &Path, parser: &Parser, state: &mut State, retention: i64) -> Result<bool, String> {
    let Some(cursor) = state.cursors.get(node).cloned() else { return Ok(true); };
    let prefix = format!("{node}.log-");
    let mut rotated = Vec::new();
    for entry in fs::read_dir(dir).map_err(|_| "无法读取轮转日志目录")? {
        let entry = entry.map_err(|_| "无法遍历轮转日志目录")?;
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()).map(|s| s.starts_with(&prefix)).unwrap_or(false) {
            rotated.push(path);
        }
    }
    rotated.sort();
    let mut start = None;
    for (index, path) in rotated.iter().enumerate() {
        let meta = fs::metadata(path).map_err(|_| "无法读取轮转日志元数据")?;
        if meta.dev() == cursor.dev && meta.ino() == cursor.ino { start = Some(index); break; }
    }
    let start = match start {
        Some(index) => index,
        None => {
            eprintln!("节点 {node} 的上次日志文件已不在保留目录；将从现存轮转日志补读");
            if rotated.is_empty() { return Ok(true); }
            0
        }
    };
    for (index, path) in rotated.iter().enumerate().skip(start) {
        if state.events.len() >= MAX_EVENTS || state.pending.len() >= MAX_EVENTS { return Ok(false); }
        let meta = fs::metadata(path).map_err(|_| "无法读取轮转日志元数据")?;
        let mut file = File::open(path).map_err(|_| "无法打开轮转日志")?;
        if index == start && meta.dev() == cursor.dev && meta.ino() == cursor.ino {
            file.seek(SeekFrom::Start(cursor.offset.min(meta.len()))).map_err(|_| "无法定位轮转日志")?;
        }
        let mut reader = Reader { file: BufReader::new(file), dev: meta.dev(), ino: meta.ino() };
        read_available(node, &mut reader, parser, state, retention)?;
        let position = reader.file.stream_position().map_err(|_| "无法读取轮转日志位置")?;
        if position < meta.len() {
            if state.events.len() >= MAX_EVENTS || state.pending.len() >= MAX_EVENTS { return Ok(false); }
            // A rotated file will not receive the missing newline. Skip only
            // that final incomplete line so later rotations can be replayed.
            eprintln!("节点 {node} 的轮转文件末尾有不完整日志行，已跳过该行");
            state.cursors.insert(node.to_owned(), Cursor { dev: meta.dev(), ino: meta.ino(), offset: meta.len() });
        }
    }
    Ok(true)
}

fn process_line(node: &str, line: &str, parser: &Parser, state: &mut State, retention: i64) {
    let now = Utc::now().timestamp();
    if let Some(from) = parser.parse_from(line) {
        state.transactions.insert(format!("{node}:{}", from.tx), TxEntry { src: from.src, seen: now });
    } else if let Some(mut to) = parser.parse_to(node, line) {
        if let Some(found) = state.transactions.remove(&format!("{node}:{}", to.tx)) {
            if to.event.epoch >= now - retention * 3600 {
                to.event.src = found.src;
                state.events.push(to.event);
            }
        }
    }
}

fn poll_files(readers: &mut HashMap<String, Reader>, parser: &Parser, state: &mut State, retention: i64) -> Result<(), String> {
    for (node, path) in node_files()? {
        let metadata = fs::metadata(&path).map_err(|_| "无法读取节点日志元数据")?;
        let changed = readers.get(&node).map(|r| r.dev != metadata.dev() || r.ino != metadata.ino()).unwrap_or(true);
        if changed {
            if let Some(old) = readers.get_mut(&node) { read_available(&node, old, parser, state, retention)?; }
            if state.events.len() >= MAX_EVENTS || state.pending.len() >= MAX_EVENTS { continue; }
            if state.cursors.get(&node).map(|c| c.dev != metadata.dev() || c.ino != metadata.ino()).unwrap_or(false)
                && !replay_rotations(&node, Path::new(LOG_DIR), parser, state, retention)? { continue; }
            if state.events.len() >= MAX_EVENTS || state.pending.len() >= MAX_EVENTS { continue; }
            let mut file = File::open(&path).map_err(|_| "无法打开节点日志")?;
            let cursor = state.cursors.get(&node).cloned().unwrap_or_default();
            if cursor.dev == metadata.dev() && cursor.ino == metadata.ino() && cursor.offset <= metadata.len() {
                file.seek(SeekFrom::Start(cursor.offset)).map_err(|_| "无法定位节点日志")?;
            }
            readers.insert(node.clone(), Reader { file: BufReader::new(file), dev: metadata.dev(), ino: metadata.ino() });
        } else if let Some(reader) = readers.get_mut(&node) {
            if metadata.len() < reader.file.stream_position().map_err(|_| "无法读取日志位置")? {
                reader.file.seek(SeekFrom::Start(0)).map_err(|_| "无法复位节点日志")?;
            }
        }
        if let Some(reader) = readers.get_mut(&node) { read_available(&node, reader, parser, state, retention)?; }
    }
    Ok(())
}

fn read_available(node: &str, reader: &mut Reader, parser: &Parser, state: &mut State, retention: i64) -> Result<(), String> {
    let mut line = String::new();
    loop {
        if state.events.len() >= MAX_EVENTS || state.pending.len() >= MAX_EVENTS { break; }
        line.clear();
        let start = reader.file.stream_position().map_err(|_| "无法读取日志位置")?;
        let count = reader.file.read_line(&mut line).map_err(|_| "无法读取节点日志")?;
        if count == 0 { break; }
        if !line.ends_with('\n') {
            reader.file.seek(SeekFrom::Start(start)).map_err(|_| "无法复位未完整日志")?;
            break;
        }
        process_line(node, &line, parser, state, retention);
        let offset = reader.file.stream_position().map_err(|_| "无法读取日志位置")?;
        state.cursors.insert(node.to_owned(), Cursor { dev: reader.dev, ino: reader.ino, offset });
    }
    Ok(())
}

// Only closed rotations older than one hour are eligible. The active file is
// never rewritten, and files at or after the persisted reader cursor stay intact.
fn compact_rotated_logs(state: &State, retention_hours: i64) -> Result<(), String> {
    let cutoff = Utc::now().timestamp() - retention_hours * 3600;
    let stamp = Regex::new(r"(?P<tz>[+-]\d{4})\s+(?P<time>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})")
        .map_err(|_| "无法初始化日志时间解析器")?;
    for (node, current_path) in node_files()? {
        let Some(cursor) = state.cursors.get(&node) else { continue; };
        let current_meta = fs::metadata(&current_path).map_err(|_| "无法读取当前节点日志")?;
        let mut rotated = Vec::new();
        for entry in fs::read_dir(LOG_DIR).map_err(|_| "无法读取轮转日志目录")? {
            let entry = entry.map_err(|_| "无法遍历轮转日志目录")?;
            if !entry.file_type().map_err(|_| "无法读取轮转文件类型")?.is_file() { continue; }
            if entry.file_name().to_string_lossy().starts_with(&format!("{node}.log-")) {
                rotated.push(entry.path());
            }
        }
        rotated.sort();
        let safe_end = if current_meta.dev() == cursor.dev && current_meta.ino() == cursor.ino {
            rotated.len()
        } else {
            let Some(index) = rotated.iter().position(|path| fs::metadata(path).map(|m| m.dev() == cursor.dev && m.ino() == cursor.ino).unwrap_or(false)) else {
                // The cursor file is unknown; preserve every rotation.
                continue;
            };
            index
        };
        for path in rotated.iter().take(safe_end) {
            let meta = fs::metadata(path).map_err(|_| "无法读取轮转文件元数据")?;
            if meta.nlink() != 1 || SystemTime::now().duration_since(meta.modified().map_err(|_| "无法读取轮转文件时间")?)
                .unwrap_or_default() < Duration::from_secs(3600) { continue; }
            if rsyslog_holds_file(&meta) { continue; }
            compact_one_rotation(path, &meta, cutoff, &stamp)?;
        }
    }
    Ok(())
}

fn rsyslog_holds_file(meta: &fs::Metadata) -> bool {
    let Ok(processes) = fs::read_dir("/proc") else { return true; };
    for process in processes.flatten() {
        if !process.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) { continue; }
        if fs::read_to_string(process.path().join("comm")).ok().map(|s| s.trim() == "rsyslogd") != Some(true) { continue; }
        let Ok(fds) = fs::read_dir(process.path().join("fd")) else { return true; };
        for fd in fds.flatten() {
            if let Ok(open) = fs::metadata(fd.path()) {
                if open.dev() == meta.dev() && open.ino() == meta.ino() { return true; }
            }
        }
    }
    false
}

fn log_line_expired(line: &[u8], cutoff: i64, fallback_mtime: i64, stamp: &Regex) -> bool {
    let parsed = std::str::from_utf8(line).ok().and_then(|text| stamp.captures(text)).and_then(|c| {
        let value = format!("{} {}", c.name("time")?.as_str(), c.name("tz")?.as_str());
        DateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S %z").ok().map(|d| d.timestamp())
    });
    parsed.unwrap_or(fallback_mtime) < cutoff
}

fn compact_one_rotation(path: &Path, meta: &fs::Metadata, cutoff: i64, stamp: &Regex) -> Result<(), String> {
    let mut reader = BufReader::new(File::open(path).map_err(|_| "无法打开轮转日志")?);
    let mut line = Vec::new();
    let mut expired = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).map_err(|_| "无法扫描轮转日志")? == 0 { break; }
        if log_line_expired(&line, cutoff, meta.mtime(), stamp) { expired = true; break; }
    }
    if !expired { return Ok(()); }

    let name = path.file_name().ok_or("轮转文件名无效")?.to_string_lossy();
    let nonce = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_| "系统时间无效")?.as_nanos();
    let temp = path.with_file_name(format!(".{name}.prune.{}.{nonce}", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut output = OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|_| "无法创建清理临时文件")?;
        reader = BufReader::new(File::open(path).map_err(|_| "无法重读轮转日志")?);
        let mut kept = 0u64;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).map_err(|_| "无法读取轮转日志内容")? == 0 { break; }
            if !log_line_expired(&line, cutoff, meta.mtime(), stamp) {
                output.write_all(&line).map_err(|_| "无法写入保留日志")?;
                kept += 1;
            }
        }
        output.sync_all().map_err(|_| "无法同步清理后的日志")?;
        fs::set_permissions(&temp, meta.permissions()).map_err(|_| "无法设置清理文件权限")?;
        let owner = format!("{}:{}", meta.uid(), meta.gid());
        if !Command::new("chown").arg(owner).arg(&temp).status().map_err(|_| "无法设置清理文件属主")?.success() {
            return Err("无法设置清理文件属主".into());
        }
        if !Command::new("touch").arg("-r").arg(path).arg(&temp).status().map_err(|_| "无法保留轮转文件时间")?.success() {
            return Err("无法保留轮转文件时间".into());
        }
        File::open(&temp).and_then(|f| f.sync_all()).map_err(|_| "无法同步清理文件元数据")?;
        // A late writer makes the snapshot unsafe; leave the original intact.
        let latest = fs::metadata(path).map_err(|_| "无法复核轮转日志")?;
        if latest.dev() != meta.dev() || latest.ino() != meta.ino() || latest.len() != meta.len() || latest.modified().ok() != meta.modified().ok() || rsyslog_holds_file(&latest) {
            return Err("轮转日志在清理期间发生变化，已保留原文件".into());
        }
        if kept == 0 { fs::remove_file(path).map_err(|_| "无法删除过期轮转日志")?; }
        else { fs::rename(&temp, path).map_err(|_| "无法替换轮转日志")?; }
        File::open(path.parent().ok_or("轮转目录无效")?).and_then(|f| f.sync_all()).map_err(|_| "无法同步轮转目录")?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

fn local_time(epoch: i64) -> String {
    let tz = FixedOffset::east_opt(8 * 3600).expect("UTC+8");
    tz.timestamp_opt(epoch, 0).single().map(|d| d.format("%H:%M:%S").to_string()).unwrap_or_else(|| "--:--:--".into())
}

fn protocol_name(p: &str) -> String {
    match p.to_ascii_lowercase().as_str() {
        "shadowsocks" => "Shadowsocks".into(),
        "vless" => "VLESS".into(),
        "vmess" => "VMess".into(),
        "trojan" => "Trojan".into(),
        _ => p.to_owned(),
    }
}

fn render(node: &str, events: &[Event]) -> Vec<String> {
    if events.is_empty() { return Vec::new(); }
    let first = events.iter().map(|e| e.epoch).min().unwrap_or(0);
    let last = events.iter().map(|e| e.epoch).max().unwrap_or(0);
    let header = format!("📊 节点访问摘要\n{} · {}–{} · {} 次访问", node, local_time(first), local_time(last), events.len());
    let mut groups: Vec<((String, String, String), Vec<&Event>)> = Vec::new();
    for event in events {
        let key = (event.uuid.clone(), event.src.clone(), event.protocol.clone());
        if let Some((_, items)) = groups.iter_mut().find(|(k, _)| *k == key) { items.push(event); }
        else { groups.push((key, vec![event])); }
    }
    let mut pages = Vec::new();
    let mut page = header.clone();
    for ((uuid, src, protocol), items) in groups {
        let short = uuid.split('-').next().unwrap_or(&uuid);
        let block_header = format!("\n\n👤 {} · {}\n协议：{}\n", short, src, protocol_name(&protocol));
        if page.chars().count() + block_header.chars().count() > MAX_MESSAGE_CHARS {
            pages.push(page); page = header.clone();
        }
        page.push_str(&block_header);
        for event in items {
            let row = format!("\n{}  {}", local_time(event.epoch), event.target);
            if page.chars().count() + row.chars().count() > MAX_MESSAGE_CHARS {
                pages.push(page); page = header.clone(); page.push_str(&block_header);
            }
            page.push_str(&row);
        }
    }
    if page != header { pages.push(page); }
    pages
}

fn queue_batch(config: &Config, state: &mut State) {
    let cutoff = Utc::now().timestamp() - config.retention_hours * 3600;
    state.events.retain(|e| e.epoch >= cutoff);
    let mut by_node: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for event in state.events.drain(..) { by_node.entry(event.node.clone()).or_default().push(event); }
    for (node, events) in by_node {
        let newest = events.iter().map(|e| e.epoch).max().unwrap_or(0);
        for text in render(&node, &events) {
            for chat_id in &config.chat_ids { state.pending.push(Pending { chat_id: chat_id.clone(), text: text.clone(), newest_epoch: newest, retry_at_epoch: 0 }); }
        }
    }
}

enum SendOutcome { Delivered, Retry(Duration), Permanent }

fn classify_reply(status: u16, ok: bool, retry_after: Option<u64>) -> SendOutcome {
    if (200..300).contains(&status) && ok { return SendOutcome::Delivered; }
    if status == 429 { return SendOutcome::Retry(Duration::from_secs(retry_after.unwrap_or(5).clamp(1, 300))); }
    // An invalid or rotated Bot Token affects every recipient. Keep the
    // messages queued so changing the token can recover them.
    if status == 401 || status == 404 { return SendOutcome::Retry(Duration::from_secs(300)); }
    if (400..500).contains(&status) { return SendOutcome::Permanent; }
    SendOutcome::Retry(Duration::from_secs(30))
}

fn send_one(client: &Client, config: &Config, pending: &Pending) -> SendOutcome {
    let _send_guard = match wait_for_telegram_window() {
        Ok(lock) => lock,
        Err(e) => { eprintln!("{e}"); return SendOutcome::Retry(Duration::from_secs(30)); }
    };
    let url = format!("https://api.telegram.org/bot{}/sendMessage", config.token);
    let delayed = Utc::now().timestamp() - pending.newest_epoch > config.interval.as_secs() as i64 * 2;
    let text = if delayed { format!("⏱ 延迟补发\n{}", pending.text) } else { pending.text.clone() };
    let response = client.post(&url).json(&json!({
        "chat_id": pending.chat_id, "text": text,
        "link_preview_options": {"is_disabled": true}
    })).send();
    let Ok(response) = response else { return SendOutcome::Retry(Duration::from_secs(30)); };
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap_or(serde_json::Value::Null);
    classify_reply(status.as_u16(), body.get("ok").and_then(|v| v.as_bool()) == Some(true),
        body.pointer("/parameters/retry_after").and_then(|v| v.as_u64()))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let env_path = Path::new(ENV_PATH);
    let config = match load_config(env_path) { Ok(v) => v, Err(e) => { eprintln!("{e}"); std::process::exit(1); } };
    if args.iter().any(|a| a == "--check-config") { println!("配置有效"); return; }
    if args.iter().any(|a| a == "--status") {
        let state = read_state(Path::new(STATE_PATH));
        println!("待汇总访问：{}；待发送消息：{}；失败消息：{}", state.events.len(), state.pending.len(), state.dead_letters.len());
        return;
    }
    if args.iter().any(|a| a == "--compact-logs") {
        let state = read_state(Path::new(STATE_PATH));
        if let Err(e) = compact_rotated_logs(&state, config.retention_hours) { eprintln!("清理日志失败：{e}"); std::process::exit(1); }
        return;
    }
    let client = Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_else(|_| { eprintln!("无法初始化 HTTP 客户端"); std::process::exit(1) });
    if args.iter().any(|a| a == "--test-telegram") {
        let mut failed = false;
        for chat_id in &config.chat_ids {
            let test = Pending { chat_id: chat_id.clone(), text: "✅ Xboard 审计测试消息".into(), newest_epoch: Utc::now().timestamp(), retry_at_epoch: 0 };
            if !matches!(send_one(&client, &config, &test), SendOutcome::Delivered) { failed = true; }
        }
        if failed { eprintln!("测试消息发送失败，请检查网络、Bot 和 Chat ID"); std::process::exit(1); }
        println!("测试消息已发送"); return;
    }
    let parser = Parser::new();
    let state_path = Path::new(STATE_PATH);
    let mut state = read_state(state_path);
    let mut last_saved = serde_json::to_vec(&state).unwrap_or_default();
    let mut readers = HashMap::new();
    let mut last_flush = Instant::now();
    let mut next_send = Instant::now();
    let mut last_persist_check = Instant::now() - Duration::from_secs(2);
    let mut saturation_reported = false;
    let mut poll_error_reported = false;
    loop {
        if let Err(e) = poll_files(&mut readers, &parser, &mut state, config.retention_hours) {
            if !poll_error_reported { eprintln!("节点日志读取失败：{e}"); poll_error_reported = true; }
            thread::sleep(Duration::from_secs(2));
        } else { poll_error_reported = false; }
        let now = Utc::now().timestamp();
        state.transactions.retain(|_, entry| now - entry.seen <= TX_TTL_SECONDS);
        if state.transactions.len() > 20_000 {
            let excess = state.transactions.len() - 20_000;
            let mut keys: Vec<_> = state.transactions.iter().map(|(key, entry)| (entry.seen, key.clone())).collect();
            keys.sort_by_key(|(seen, _)| *seen);
            for (_, key) in keys.into_iter().take(excess) { state.transactions.remove(&key); }
            eprintln!("事务缓存超过上限，已清理最旧记录");
        }
        if last_flush.elapsed() >= config.interval {
            queue_batch(&config, &mut state);
            last_flush = Instant::now();
        }
        state.pending.retain(|p| p.newest_epoch >= now - config.retention_hours * 3600);
        state.dead_letters.retain(|p| p.newest_epoch >= now - config.retention_hours * 3600);
        let saturated = state.events.len() >= MAX_EVENTS || state.pending.len() >= MAX_EVENTS;
        if saturated && !saturation_reported { eprintln!("审计积压达到上限，暂停读取新日志；请检查 Telegram 发送状态和磁盘空间"); saturation_reported = true; }
        if !saturated { saturation_reported = false; }
        if last_persist_check.elapsed() >= Duration::from_secs(2) || last_flush.elapsed() < Duration::from_millis(500) {
            if let Err(e) = persist_if_changed(state_path, &state, &mut last_saved) { eprintln!("{e}"); std::process::exit(1); }
            last_persist_check = Instant::now();
        }
        if !state.pending.is_empty() && Instant::now() >= next_send {
            if let Some(index) = state.pending.iter().position(|p| p.retry_at_epoch <= now) {
                // Persist the queue before making an external request. A crash after
                // Telegram accepts a message can still lead to one duplicate.
                if let Err(e) = persist_if_changed(state_path, &state, &mut last_saved) { eprintln!("{e}"); std::process::exit(1); }
                let mut pending = state.pending.remove(index);
                match send_one(&client, &config, &pending) {
                    SendOutcome::Delivered => {}
                    SendOutcome::Permanent => {
                        eprintln!("Telegram 拒绝向 Chat ID {} 发送；消息已放入失败队列", pending.chat_id);
                        state.dead_letters.push(pending);
                    }
                    SendOutcome::Retry(wait) => {
                        pending.retry_at_epoch = Utc::now().timestamp() + wait.as_secs() as i64;
                        state.pending.push(pending);
                        eprintln!("Telegram 暂时无法发送，稍后重试");
                    }
                }
                if let Err(e) = persist_if_changed(state_path, &state, &mut last_saved) { eprintln!("{e}"); std::process::exit(1); }
                next_send = Instant::now() + Duration::from_secs(1);
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn telegram_waits_for_maintenance_lock() {
        let unique = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("xboard-audit-lock-{}-{unique}", std::process::id()));
        fs::write(&path, b"").unwrap();
        let first = exclusive_lock(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker = thread::spawn(move || {
            let _second = exclusive_lock(&worker_path).unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(first);
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
        worker.join().unwrap();
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn retention_uses_inner_year_and_timezone_not_syslog_prefix() {
        let stamp = Regex::new(r"(?P<tz>[+-]\d{4})\s+(?P<time>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})").unwrap();
        let cutoff = DateTime::parse_from_str("2026-11-02 00:00:00 +0800", "%Y-%m-%d %H:%M:%S %z").unwrap().timestamp();
        let old = b"Nov 02 00:05:00 host xboard-node: +0800 2026-11-01 23:59:59 INFO old\n";
        let fresh = b"Nov 01 23:55:00 host xboard-node: +0800 2026-11-02 00:00:01 INFO fresh\n";
        assert!(log_line_expired(old, cutoff, cutoff + 100, &stamp));
        assert!(!log_line_expired(fresh, cutoff, cutoff - 100, &stamp));
    }
    #[test]
    fn permanent_chat_error_does_not_request_retry() {
        assert!(matches!(classify_reply(403, false, None), SendOutcome::Permanent));
        assert!(matches!(classify_reply(400, false, None), SendOutcome::Permanent));
        assert!(matches!(classify_reply(401, false, None), SendOutcome::Retry(_)));
        assert!(matches!(classify_reply(503, false, None), SendOutcome::Retry(_)));
        assert!(matches!(classify_reply(429, false, Some(7)), SendOutcome::Retry(wait) if wait.as_secs() == 7));
    }
    #[test]
    fn sample_log_pairs_and_ignores_outbound() {
        let parser = Parser::new();
        let mut state = State::default();
        let from = "+0800 2026-11-01 23:32:52 INFO [3810345234 0ms] inbound/shadowsocks[shadowsocks-in]: inbound connection from 203.198.248.150:10096";
        let to = "+0800 2026-11-01 23:32:52 INFO [3810345234 0ms] inbound/shadowsocks[shadowsocks-in]: [d161d620-2754-4db6-8ad5-4e95bdd0480c] inbound connection to assets.msn.cn:443";
        let outbound = "+0800 2026-11-01 23:32:52 INFO [3810345234 0ms] outbound/direct[direct]: outbound connection to assets.msn.cn:443";
        // Use a retention wide enough to keep the fixed historical fixture.
        process_line("vps-main-A", from, &parser, &mut state, 100_000);
        process_line("vps-main-A", to, &parser, &mut state, 100_000);
        process_line("vps-main-A", outbound, &parser, &mut state, 100_000);
        assert_eq!(state.events.len(), 1);
        assert_eq!(state.events[0].src, "203.198.248.150");
        assert!(render("vps-main-A", &state.events)[0].contains("assets.msn.cn:443"));
    }

    #[test]
    fn user_sample_has_eight_visits_and_keeps_duplicates() {
        let parser = Parser::new();
        let mut state = State::default();
        let rows = [
            ("23:32:52", "3810345234", "assets.msn.cn:443"),
            ("23:32:54", "1051147560", "assets.msn.com:443"),
            ("23:32:54", "164478833", "74.125.31.120:443"),
            ("23:33:06", "2523583405", "www.google.com:443"),
            ("23:33:08", "2247942391", "rr5---sn-ntqe6n7r.googlevideo.com:443"),
            ("23:33:08", "93833179", "rr5---sn-ntqe6n7r.googlevideo.com:443"),
            ("23:33:09", "2424115694", "rr5---sn-ntqe6n7r.googlevideo.com:443"),
            ("23:33:09", "1640387321", "rr5---sn-ntqe6n7r.googlevideo.com:443"),
        ];
        for (time, id, target) in rows {
            let from = format!("+0800 2026-11-01 {time} INFO [{id} 0ms] inbound/shadowsocks[shadowsocks-in]: inbound connection from 203.198.248.150:10096");
            let to = format!("+0800 2026-11-01 {time} INFO [{id} 0ms] inbound/shadowsocks[shadowsocks-in]: [d161d620-2754-4db6-8ad5-4e95bdd0480c] inbound connection to {target}");
            process_line("vps-main-A", &from, &parser, &mut state, 100_000);
            process_line("vps-main-A", &to, &parser, &mut state, 100_000);
        }
        assert_eq!(state.events.len(), 8);
        let message = render("vps-main-A", &state.events).join("\n");
        assert!(message.contains("23:32:52–23:33:09 · 8 次访问"));
        assert_eq!(message.matches("rr5---sn-ntqe6n7r.googlevideo.com:443").count(), 4);
    }

    #[test]
    fn restart_replays_all_intermediate_rotations() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("xboard-audit-rotation-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let first = dir.join("vps-a.log-20261101230000");
        let second = dir.join("vps-a.log-20261102000000");
        let make_pair = |id: &str, target: &str| format!(
            "+0800 2026-11-01 23:32:52 INFO [{id} 0ms] inbound/shadowsocks[in]: inbound connection from 203.198.248.150:1234\n\
             +0800 2026-11-01 23:32:52 INFO [{id} 0ms] inbound/shadowsocks[in]: [d161d620-2754-4db6-8ad5-4e95bdd0480c] inbound connection to {target}\n"
        );
        fs::write(&first, make_pair("1", "one.example:443")).unwrap();
        fs::write(&second, make_pair("2", "two.example:443")).unwrap();
        let meta = fs::metadata(&first).unwrap();
        let mut state = State::default();
        state.cursors.insert("vps-a".into(), Cursor { dev: meta.dev(), ino: meta.ino(), offset: 0 });
        assert!(replay_rotations("vps-a", &dir, &Parser::new(), &mut state, 100_000).unwrap());
        assert_eq!(state.events.len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
