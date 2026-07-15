use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bot_token: String,
    pub admin_id: i64,
    pub base_dir: PathBuf,
    pub rtmp_url: String,
    pub items_per_page: usize,
    pub video_extensions: Vec<String>,
    pub ffprobe_timeout_seconds: u64,
    pub youtube_max_concurrent_uploads: usize,
    pub youtube_upload_chunk_mb: usize,
    pub youtube_upload_queue_file: PathBuf,
    pub token_file: PathBuf,
    pub merge_min_duration_ratio: f64,
    pub merge_min_size_ratio: f64,
}

impl AppConfig {
    pub fn load_from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let bot_token = env::var("BOT_TOKEN")
            .map_err(|_| "环境变量中缺少 BOT_TOKEN")?.trim().to_string();
        
        let admin_id: i64 = env::var("ADMIN_ID")
            .map_err(|_| "环境变量中缺少 ADMIN_ID")?.trim().parse()?;

        let base_dir_str = env::var("BASE_DIR")
            .unwrap_or_else(|_| "/storage512/bilivego/download".to_string());
        let base_dir = PathBuf::from(base_dir_str.trim());

        let rtmp_url = env::var("RTMP_URL").unwrap_or_default().trim().to_string();
        
        let items_per_page: usize = env::var("ITEMS_PER_PAGE")
            .unwrap_or_else(|_| "8".to_string()).trim().parse().unwrap_or(8);

        let ext_raw = env::var("VIDEO_EXTENSIONS")
            .unwrap_or_else(|_| ".mp4,.mkv,.flv,.ts".to_string());
        let video_extensions: Vec<String> = ext_raw
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .map(|s| if s.starts_with('.') { s } else { format!(".{}", s) })
            .collect();

        let ffprobe_timeout_seconds: u64 = env::var("FFPROBE_TIMEOUT_SECONDS")
            .unwrap_or_else(|_| "30".to_string()).trim().parse().unwrap_or(30);

        let youtube_max_concurrent_uploads: usize = env::var("YOUTUBE_MAX_CONCURRENT_UPLOADS")
            .unwrap_or_else(|_| "2".to_string()).trim().parse().unwrap_or(2);

        let youtube_upload_chunk_mb: usize = env::var("YOUTUBE_UPLOAD_CHUNK_MB")
            .unwrap_or_else(|_| "10".to_string()).trim().parse().unwrap_or(10);

        let app_dir = env::var("APP_DIR").unwrap_or_else(|_| ".".to_string());
        let youtube_upload_queue_file = PathBuf::from(
            env::var("YOUTUBE_UPLOAD_QUEUE_FILE")
                .unwrap_or_else(|_| format!("{}/youtube_upload_queue.json", app_dir))
        );
        let token_file = PathBuf::from(
            env::var("TOKEN_FILE")
                .unwrap_or_else(|_| format!("{}/token.json", app_dir))
        );

        let merge_min_duration_ratio: f64 = env::var("MERGE_MIN_DURATION_RATIO")
            .unwrap_or_else(|_| "0.95".to_string()).trim().parse().unwrap_or(0.95);
        let merge_min_size_ratio: f64 = env::var("MERGE_MIN_SIZE_RATIO")
            .unwrap_or_else(|_| "0.30".to_string()).trim().parse().unwrap_or(0.30);

        Ok(Self {
            bot_token,
            admin_id,
            base_dir,
            rtmp_url,
            items_per_page,
            video_extensions,
            ffprobe_timeout_seconds,
            youtube_max_concurrent_uploads,
            youtube_upload_chunk_mb,
            youtube_upload_queue_file,
            token_file,
            merge_min_duration_ratio,
            merge_min_size_ratio,
        })
    }

    pub fn is_admin(&self, user_id: u64) -> bool {
        self.admin_id == user_id as i64
    }
}