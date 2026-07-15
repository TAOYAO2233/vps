use anyhow::{anyhow, Result};
use chrono::Local;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

pub fn assert_path_inside_base(base: &Path, target: &Path) -> Result<PathBuf> {
    let base_canon = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let target_canon = if target.exists() {
        target.canonicalize()?
    } else {
        target.to_path_buf()
    };

    if target_canon == base_canon || target_canon.starts_with(&base_canon) {
        Ok(target_canon)
    } else {
        Err(anyhow!("非法路径，已超出 BASE_DIR: {:?}", target))
    }
}

pub fn safe_join(base: &Path, parent: &Path, child: &str) -> Result<PathBuf> {
    let joined = parent.join(child);
    assert_path_inside_base(base, &joined)
}

pub fn remove_if_exists(path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
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

pub fn get_formatted_file_size(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(meta) => {
            let size_bytes = meta.len() as f64;
            let size_mb = size_bytes / (1024.0 * 1024.0);
            if size_mb >= 1024.0 {
                format!("{:.2}GB", size_bytes / (1024.0 * 1024.0 * 1024.0))
            } else {
                format!("{:.2}MB", size_mb)
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

pub async fn get_video_duration(path: &Path, timeout_sec: u64) -> f64 {
    let mut cmd = Command::new("ffprobe");
    cmd.args(&[
        "-v", "error",
        "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1:nokey=1",
    ])
    .arg(path)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return 0.0,
    };

    match timeout(Duration::from_secs(timeout_sec), child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
            s.parse::<f64>().unwrap_or(0.0)
        }
        _ => 0.0,
    }
}

pub fn build_progress_bar(percent: f64, length: usize) -> String {
    let p = percent.clamp(0.0, 100.0);
    let filled = ((p / 100.0) * (length as f64)).floor() as usize;
    let empty = length.saturating_sub(filled);
    format!("[{}{}] {:5.1}%", "█".repeat(filled), "░".repeat(empty), p)
}

pub fn smart_rename(first_file: &Path) -> String {
    let base_name = first_file.file_stem().and_then(|s| s.to_str()).unwrap_or("Merged");
    let ext = first_file.extension().and_then(|s| s.to_str()).unwrap_or("mp4");

    let date_re = Regex::new(r"\d{4}[-_.]\d{2}[-_.]\d{2}(?:[ _-]\d{2}[-_.:]\d{2}[-_.:]\d{2})?").unwrap();
    let date_str = match date_re.find(base_name) {
        Some(m) => m.as_str().to_string(),
        None => format!("Merged_{}", Local::now().format("%Y%m%d_%H%M%S")),
    };

    let title_clean_re = Regex::new(r"^\[\d[^\]]*\]\s*").unwrap();
    let mut title_part = title_clean_re.replace(base_name, "").to_string();

    if title_part == base_name {
        title_part = title_part.replace(&date_str, "");
        let trim_re = Regex::new(r"^[-_.\s]+").unwrap();
        title_part = trim_re.replace(&title_part, "").to_string();
    }

    let output_name = if !title_part.is_empty() {
        format!("{}_{}.{}", date_str, title_part, ext)
    } else {
        format!("{}_merged.{}", date_str, ext)
    };

    output_name.replace("__", "_")
}

pub fn write_concat_list(list_path: &Path, files: &[PathBuf]) -> Result<()> {
    let mut content = String::new();
    for f in files {
        let s = f.to_str().unwrap_or("").replace('\\', "\\\\").replace('\'', "\\'");
        content.push_str(&format!("file '{}'\n", s));
    }
    fs::write(list_path, content)?;
    Ok(())
}

pub struct MergeCheckResult {
    pub is_success: bool,
    pub input_duration: f64,
    pub output_duration: f64,
    pub duration_ratio: f64,
    pub size_ratio: f64,
    pub output_size: u64,
}

pub async fn validate_merged_file(
    output_path: &Path,
    success: bool,
    input_total_duration: f64,
    input_total_size: u64,
    min_dur_ratio: f64,
    min_size_ratio: f64,
    timeout_sec: u64,
) -> MergeCheckResult {
    let output_size = fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    let output_duration = if output_size > 0 {
        get_video_duration(output_path, timeout_sec).await
    } else {
        0.0
    };

    let duration_ok = if input_total_duration > 0.0 {
        output_duration >= input_total_duration * min_dur_ratio
    } else {
        true
    };

    let size_ok = if input_total_size > 0 {
        (output_size as f64) >= (input_total_size as f64) * min_size_ratio
    } else {
        true
    };

    let is_success = success && output_size > 0 && duration_ok && size_ok;
    let duration_ratio = if input_total_duration > 0.0 { output_duration / input_total_duration } else { 0.0 };
    let size_ratio = if input_total_size > 0 { (output_size as f64) / (input_total_size as f64) } else { 0.0 };

    MergeCheckResult {
        is_success,
        input_duration: input_total_duration,
        output_duration,
        duration_ratio,
        size_ratio,
        output_size,
    }
}

pub fn format_merge_check(res: &MergeCheckResult) -> String {
    format!(
        "⏱️ 输入总时长: `{}`\n⏱️ 输出时长: `{}` ({:.1}%)\n📦 输出大小: `{:.2}MB` ({:.1}%)",
        format_duration(res.input_duration),
        format_duration(res.output_duration),
        res.duration_ratio * 100.0,
        (res.output_size as f64) / (1024.0 * 1024.0),
        res.size_ratio * 100.0
    )
}

pub fn format_elapsed(seconds: f64) -> String {
    let s_int = seconds.max(0.0) as u64;
    let h = s_int / 3600;
    let m = (s_int % 3600) / 60;
    let s = s_int % 60;
    if h > 0 { format!("{}h{:02}m{:02}s", h, m, s) }
    else if m > 0 { format!("{}m{:02}s", m, s) }
    else { format!("{}s", s) }
}