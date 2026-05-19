# 🤖 VPS 多媒体主控与媒体上传机器人 (vpsupload_bot)

本模块集成了 `FFmpeg` 工具链与 `Google YouTube API v3`，将您的 VPS 转化为一个移动端可控的**多媒体自动化处理工作站**。最新版本（v2.0.0）已重构为全动态路径导航架构。

## 📌 核心功能
* **无限深度路径导航**：支持在设定的根目录（`BASE_DIR`）下进行无限层级的目录穿梭。自动对子文件夹（📁）和视频文件（🎥）进行分类置顶排序。
* **RTMP 无损直播推流**：支持一键将 VPS 本地的视频流（.mp4/.mkv 等）通过 FFmpeg 以 `-c copy` 参数无损推送到指定的直播平台（如 YouTube Live）。
* **YouTube 自动化分块上传**：
  * 支持大视频断点续传，默认分块大小为 10MB。
  * 上传后的视频默认自动设为**私享 (Private)**，并开启 **18+ (Made for Kids: False / Restricted)** 安全限制。
* **智能视频无损拼接 (Concat)**：允许用户勾选同目录下的多个视频，自动按文件名提取日期等标识生成 `concat_list.txt`，并通过 FFmpeg 进行无损拼接合并。
* **批量 MP4 封装重构**：支持将格式各异的视频容器批量高速重封装为标准的 MP4 容器，并注入 `+faststart` 标记以优化网络流式播放。
* **异步控制与进度条**：推流、转码、上传过程均配备高精度的异步文本进度条；发送 `/stop` 指令可立即安全终止底层 FFmpeg 进程或 API 请求。

## 🔑 YouTube API 凭据生成指引
1. 在 Google Cloud Console 中创建项目，启用 YouTube Data API v3，并下载 OAuth 2.0 客户端凭据，重命名为 `client_secrets.json` 放在本目录下。
2. 在配有浏览器的本地电脑上运行认证辅助脚本：
   ```bash
   python "get josn.py"