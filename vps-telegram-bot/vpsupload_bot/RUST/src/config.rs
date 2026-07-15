use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Config {
    pub bot_token: String,
    pub admin_id: i64,
    pub base_dir: PathBuf,
    pub rtmp_url: String,
    pub items_per_page: usize,
    pub video_extensions: Vec<String>,
    pub ffprobe_timeout_seconds: u64,
    pub token_file: PathBuf,
    pub youtube_max_concurrent_uploads: usize,
    pub youtube_upload_chunk_mb: usize,
    pub youtube_upload_queue_file: PathBuf,
    pub merge_min_duration_ratio: f64,
    pub merge_min_size_ratio: f64,
}

impl Config {
    pub fn from_env() -> Arc<Self> {
        let _ = dotenvy::dotenv();

        let bot_token = env::var("BOT_TOKEN").expect("缺少配置: BOT_TOKEN");
        let admin_id = env::var("ADMIN_ID")
            .expect("缺少配置: ADMIN_ID")
            .parse::<i64>()
            .expect("ADMIN_ID 必须是整数");

        let base_dir = PathBuf::from(
            env::var("BASE_DIR").unwrap_or_else(|_| "/storage512/bilivego/download".to_string()),
        );
        let rtmp_url = env::var("RTMP_URL").unwrap_or_default();

        let items_per_page = env::var("ITEMS_PER_PAGE")
            .unwrap_or_else(|_| "8".to_string())
            .parse::<usize>()
            .unwrap_or(8);

        let ext_str = env::var("VIDEO_EXTENSIONS").unwrap_or_else(|_| ".mp4,.mkv,.flv,.ts".to_string());
        let video_extensions = ext_str
            .split(',')
            .map(|s| {
                let s = s.trim().to_lowercase();
                if s.starts_with('.') { s } else { format!(".{}", s) }
            })
            .collect();

        let ffprobe_timeout_seconds = env::var("FFPROBE_TIMEOUT_SECONDS")
            .unwrap_or_else(|_| "30".to_string())
            .parse::<u64>()
            .unwrap_or(30);

        let token_file = PathBuf::from(env::var("TOKEN_FILE").unwrap_or_else(|_| "token.json".to_string()));
        let youtube_max_concurrent_uploads = env::var("YOUTUBE_MAX_CONCURRENT_UPLOADS")
            .unwrap_or_else(|_| "2".to_string())
            .parse::<usize>()
            .unwrap_or(2);
        let youtube_upload_chunk_mb = env::var("YOUTUBE_UPLOAD_CHUNK_MB")
            .unwrap_or_else(|_| "10".to_string())
            .parse::<usize>()
            .unwrap_or(10);
        let youtube_upload_queue_file = PathBuf::from(
            env::var("YOUTUBE_UPLOAD_QUEUE_FILE").unwrap_or_else(|_| "youtube_upload_queue.json".to_string()),
        );

        let merge_min_duration_ratio = env::var("MERGE_MIN_DURATION_RATIO")
            .unwrap_or_else(|_| "0.95".to_string())
            .parse::<f64>()
            .unwrap_or(0.95);
        let merge_min_size_ratio = env::var("MERGE_MIN_SIZE_RATIO")
            .unwrap_or_else(|_| "0.30".to_string())
            .parse::<f64>()
            .unwrap_or(0.30);

        if !base_dir.exists() {
            tracing::warn!("BASE_DIR 目录不存在: {:?}", base_dir);
        }

        Arc::new(Self {
            bot_token,
            admin_id,
            base_dir,
            rtmp_url,
            items_per_page,
            video_extensions,
            ffprobe_timeout_seconds,
            token_file,
            youtube_max_concurrent_uploads,
            youtube_upload_chunk_mb,
            youtube_upload_queue_file,
            merge_min_duration_ratio,
            merge_min_size_ratio,
        })
    }
}

pub fn action_name_map(key: &str) -> &'static str {
    match key {
        "browse" => "浏览与查看详情",
        "stream" => "推流",
        "youtube" => "上传 YT",
        "concat" => "合并",
        "convert" => "转码 MP4",
        "delete" => "删除",
        _ => "未知操作",
    }
}