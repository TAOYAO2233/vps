use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use regex::Regex;
use chrono::Local;
use tracing::{warn, error};

pub fn assert_path_inside_base(base: &Path, target: &Path) -> Result<PathBuf, String> {
    let base_real = base.canonicalize().map_err(|e| format!("Base dir invalid: {}", e))?;
    let target_real = if target.exists() {
        target.canonicalize().map_err(|e| format!("Target path invalid: {}", e))?
    } else {
        let parent = target.parent().ok_or("No parent dir")?;
        let parent_real = parent.canonicalize().map_err(|e| format!("Parent invalid: {}", e))?;
        parent_real.join(target.file_name().ok_or("No filename")?)
    };

    if target_real == base_real || target_real.starts_with(&base_real) {
        Ok(target_real)
    } else {
        Err(format!("非法路径，已超出 BASE_DIR: {:?}", target_real))
    }
}

pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    
    let mut index = 1;
    loop {
        let candidate_name = if ext.is_empty() {
            format!("{}_{}", stem, index)
        } else {
            format!("{}_{}.{}", stem, index, ext)
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

pub fn get_formatted_file_size(filepath: &Path) -> String {
    match std::fs::metadata(filepath) {
        Ok(meta) => {
            let size = meta.len() as f64;
            let mb = size / (1024.0 * 1024.0);
            if mb >= 1024.0 {
                format!("{:.2}GB", size / (1024.0 * 1024.0 * 1024.0))
            } else {
                format!("{:.2}MB", mb)
            }
        }
        Err(_) => "0.00MB".to_string(),
    }
}

pub fn format_duration(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "未知".to_string();
    }
    let s_int = seconds as u64;
    let h = s_int / 3600;
    let m = (s_int % 3600) / 60;
    let s = s_int % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

pub fn format_elapsed(seconds: f64) -> String {
    let s_int = seconds.max(0.0) as u64;
    let h = s_int / 3600;
    let m = (s_int % 3600) / 60;
    let s = s_int % 60;
    if h > 0 {
        format!("{}h{:02}m{:02}s", h, m, s)
    } else if m > 0 {
        format!("{}m{:02}s", m, s)
    } else {
        format!("{}s", s)
    }
}

/// 支持在 Telegram 端自定义一键切换的皮肤生成函数
pub fn build_progress_bar(percent: f64, length: usize, theme: usize) -> String {
    let p = percent.clamp(0.0, 100.0);
    
    match theme {
        // 1. 彩色水果流 (Emoji，建议短长度 10 格)
        1 => {
            let local_length = 10;
            let filled = ((p / 100.0) * local_length as f64).floor() as usize;
            let bar = "🟩".repeat(filled) + &"⬜".repeat(local_length - filled);
            format!("<code>{}</code> <b>{:5.1}%</b>", bar, p)
        }
        // 2. 简约细线流
        2 => {
            let filled = ((p / 100.0) * length as f64).floor() as usize;
            let bar = "━".repeat(filled) + &"─".repeat(length.saturating_sub(filled));
            format!("<code>{}</code> <b>{:5.1}%</b>", bar, p)
        }
        // 0. 默认：科幻方块流
        _ => {
            let filled = ((p / 100.0) * length as f64).floor() as usize;
            let bar = "▰".repeat(filled) + &"▱".repeat(length.saturating_sub(filled));
            format!("<code>{}</code> <b>{:5.1}%</b>", bar, p)
        }
    }
}

pub fn smart_rename(first_file_path: &Path) -> String {
    let base_name = first_file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
    let ext = first_file_path.extension().and_then(|s| s.to_str()).unwrap_or(".mp4");
    
    let re_date = Regex::new(r"\d{4}[-_.]\d{2}[-_.]\d{2}(?:[ _-]\d{2}[-_.:]\d{2}[-_.:]\d{2})?").unwrap();
    let date_str = match re_date.find(base_name) {
        Some(m) => m.as_str().to_string(),
        None => format!("Merged_{}", Local::now().format("%Y%m%d_%H%M%S")),
    };

    let re_prefix = Regex::new(r"^\[\d[^\]]*\]\s*").unwrap();
    let mut title_part = re_prefix.replace(base_name, "").to_string();

    if title_part == base_name {
        title_part = title_part.replace(&date_str, "");
        let re_clean = Regex::new(r"^[-_.\s]+").unwrap();
        title_part = re_clean.replace(&title_part, "").to_string();
    }

    let output_name = if !title_part.is_empty() {
        format!("{}_{}.{}", date_str, title_part, ext)
    } else {
        format!("{}_merged.{}", date_str, ext)
    };

    output_name.replace("__", "_")
}

pub async fn get_video_duration(filepath: &Path, timeout_sec: u64) -> f64 {
    let child = Command::new("ffprobe")
        .args(&[
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
            filepath.to_str().unwrap_or(""),
        ])
        .output();

    match timeout(Duration::from_secs(timeout_sec), child).await {
        Ok(Ok(out)) => {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                text.trim().parse::<f64>().unwrap_or(0.0)
            } else {
                warn!("ffprobe 处理失败: {:?}", filepath);
                0.0
            }
        }
        Ok(Err(e)) => {
            error!("执行 ffprobe 进程出错: {}", e);
            0.0
        }
        Err(_) => {
            warn!("ffprobe 读取超时: {:?}", filepath);
            0.0
        }
    }
}