//! `/storage512` 挂载点校验、实时容量统计、空间预检与三级磁盘报警模块。
//!
//! 严格遵循以下核心规则：
//! - 先校验目标路径确为活跃的预期挂载点（解析 `/proc/self/mountinfo` 并排除 `tmpfs`、`overlay` 等伪文件系统），
//!   绝不将根分区 `/` 的容量误认为 `/storage512`。
//! - 报警阈值使用未四舍五入的 `df` 同口径占用率严格大于比较：**>90% 一级、>95% 二级、>98% 三级**；
//!   展示百分比采用与 GNU `df` `Use%` 一致的向上取整口径。
//! - 仅在 Telegram 报警消息实际发送成功后才记录该等级“已报警”并原子持久化状态；发送失败保留待发送状态以便下次重试。

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::errors::AppError;
use crate::storage::filesystem::write_private_file_atomic;
use crate::utils::format::escape_html;
use crate::utils::redact::redact_secrets;

/// 一级报警触发阈值（严格大于 90.0%）。
pub const ALERT_THRESHOLD_L1: f64 = 90.0;
/// 二级报警触发阈值（严格大于 95.0%）。
pub const ALERT_THRESHOLD_L2: f64 = 95.0;
/// 三级报警触发阈值（严格大于 98.0%）。
pub const ALERT_THRESHOLD_L3: f64 = 98.0;

/// 拒绝当作目标媒体盘的虚拟/叠加文件系统类型列表。
#[cfg_attr(not(unix), allow(dead_code))]
const REJECTED_FS_TYPES: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "overlay",
    "ramfs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "squashfs",
    "nsfs",
    "tracefs",
    "debugfs",
];

/// 磁盘报警等级枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DiskAlertLevel {
    /// 一级预警：占用率 > 90%
    Level1 = 1,
    /// 二级告警：占用率 > 95%
    Level2 = 2,
    /// 三级紧急告警：占用率 > 98%
    Level3 = 3,
}

impl DiskAlertLevel {
    /// 返回数组下标（0..=2）。
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Level1 => 0,
            Self::Level2 => 1,
            Self::Level3 => 2,
        }
    }

    /// 返回等级中文标题。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Level1 => "一级预警 (>90%)",
            Self::Level2 => "二级告警 (>95%)",
            Self::Level3 => "三级紧急告警 (>98%)",
        }
    }

    /// 根据未取整占用率判定当前最高已达到的报警等级（严格大于阈值才触发）。
    #[must_use]
    pub fn from_raw_percent(raw_percent: f64) -> Option<Self> {
        if raw_percent > ALERT_THRESHOLD_L3 {
            Some(Self::Level3)
        } else if raw_percent > ALERT_THRESHOLD_L2 {
            Some(Self::Level2)
        } else if raw_percent > ALERT_THRESHOLD_L1 {
            Some(Self::Level1)
        } else {
            None
        }
    }
}

/// 实时文件系统容量统计快照。
#[derive(Debug, Clone, PartialEq)]
pub struct DiskUsageStats {
    /// 挂载点路径
    pub mount_point: PathBuf,
    /// 文件系统类型（来自 `mountinfo`，如 `ext4`、`xfs`）
    pub fs_type: String,
    /// 总字节数
    pub total_bytes: u64,
    /// 已用字节数（`(f_blocks - f_bfree) * frsize`）
    pub used_bytes: u64,
    /// 非特权用户可用字节数（`f_bavail * frsize`）
    pub avail_bytes: u64,
    /// 未四舍五入的 `df` 口径占用率百分比（`used / (used + avail) * 100.0`，用于精确阈值判断）
    pub raw_use_percent: f64,
    /// 与 GNU `df` `Use%` 一致的展示整数百分比（`ceil(used * 100 / (used + avail))`）
    pub display_use_percent: u64,
}

impl DiskUsageStats {
    /// 从原始块计数与块大小构造容量统计（纯函数，便于跨平台精确单元测试）。
    #[must_use]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn from_raw_blocks(
        mount_point: PathBuf,
        fs_type: String,
        frsize: u64,
        total_blocks: u64,
        free_blocks: u64,
        avail_blocks: u64,
    ) -> Self {
        let unit = u128::from(frsize.max(1));
        let used_blocks = total_blocks.saturating_sub(free_blocks);
        let nonroot_total_blocks = used_blocks.saturating_add(avail_blocks);

        let total_bytes = u64::try_from(u128::from(total_blocks) * unit).unwrap_or(u64::MAX);
        let used_bytes = u64::try_from(u128::from(used_blocks) * unit).unwrap_or(u64::MAX);
        let avail_bytes = u64::try_from(u128::from(avail_blocks) * unit).unwrap_or(u64::MAX);

        let (raw_use_percent, display_use_percent) = if nonroot_total_blocks > 0 {
            let raw = (used_blocks as f64 * 100.0) / (nonroot_total_blocks as f64);
            let ceil_pct =
                (u128::from(used_blocks) * 100).div_ceil(u128::from(nonroot_total_blocks));
            (raw, u64::try_from(ceil_pct).unwrap_or(100))
        } else {
            (0.0, 0)
        };

        Self {
            mount_point,
            fs_type,
            total_bytes,
            used_bytes,
            avail_bytes,
            raw_use_percent,
            display_use_percent,
        }
    }

    /// 格式化为主菜单展示的单行 HTML 文本。
    ///
    /// 形如：`💽 <code>/storage512</code>：已用 <code>431G</code> / 总量 <code>503G</code>（<code>91%</code>），可用 <code>48G</code>`
    #[must_use]
    pub fn format_menu_line(&self) -> String {
        let mount_disp = escape_html(&self.mount_point.display().to_string());
        let used_h = format_df_human_bytes(self.used_bytes);
        let total_h = format_df_human_bytes(self.total_bytes);
        let avail_h = format_df_human_bytes(self.avail_bytes);
        format!(
            "💽 <code>{mount_disp}</code>：已用 <code>{used_h}</code> / 总量 <code>{total_h}</code>（<code>{}%</code>），可用 <code>{avail_h}</code>",
            self.display_use_percent
        )
    }
}

/// 按 `df -h` 风格格式化容量字节数（二进制 GiB/MiB/TiB，例如 `431G`、`48G`）。
#[must_use]
pub fn format_df_human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= TIB {
        format!("{:.1}T", b / TIB)
    } else if b >= 10.0 * GIB {
        format!("{:.0}G", b / GIB)
    } else if b >= GIB {
        format!("{:.1}G", b / GIB)
    } else if b >= 10.0 * MIB {
        format!("{:.0}M", b / MIB)
    } else if b >= MIB {
        format!("{:.1}M", b / MIB)
    } else if b >= KIB {
        format!("{:.0}K", b / KIB)
    } else {
        format!("{bytes}B")
    }
}

/// 磁盘探测失败原因分类（安全诊断信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum DiskProbeError {
    /// 预期挂载点在 `/proc/self/mountinfo` 中不存在（未挂载或挂载已脱落，拒绝回退读取根盘）
    NotMounted {
        /// 预期挂载路径
        mount_point: PathBuf,
    },
    /// 挂载到了非预期的虚拟/叠加文件系统（如 `tmpfs`、`overlay`）
    UnexpectedFsType {
        /// 预期挂载路径
        mount_point: PathBuf,
        /// 实际探测到的文件系统类型
        fs_type: String,
    },
    /// 挂载点目录不存在、为符号链接或 `statvfs` 读取失败
    StatFailed {
        /// 预期挂载路径
        mount_point: PathBuf,
        /// 安全原因摘要
        reason: String,
    },
}

impl std::fmt::Display for DiskProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotMounted { mount_point } => {
                write!(
                    f,
                    "目标路径 {} 当前未作为独立挂载点挂载（拒绝读取根分区目录）",
                    mount_point.display()
                )
            }
            Self::UnexpectedFsType {
                mount_point,
                fs_type,
            } => {
                write!(
                    f,
                    "目标路径 {} 的文件系统类型异常（当前为 {fs_type}）",
                    mount_point.display()
                )
            }
            Self::StatFailed {
                mount_point,
                reason,
            } => {
                write!(f, "无法读取挂载点 {} 的统计信息：{reason}", mount_point.display())
            }
        }
    }
}

/// 主菜单使用的磁盘展示结果（成功显示实时统计，失败降级显示“磁盘信息暂不可用”且不阻断 `/start`）。
#[derive(Debug, Clone)]
pub enum DiskMenuSummary {
    /// 正常获取到的实时容量快照
    Available(DiskUsageStats),
    /// 挂载异常或统计暂不可用
    Unavailable {
        /// 挂载点路径
        mount_point: PathBuf,
    },
}

impl DiskMenuSummary {
    /// 渲染主菜单中的磁盘信息行。
    #[must_use]
    pub fn format_menu_line(&self) -> String {
        match self {
            Self::Available(stats) => stats.format_menu_line(),
            Self::Unavailable { mount_point } => {
                let mount_disp = escape_html(&mount_point.display().to_string());
                format!("💽 <code>{mount_disp}</code>：磁盘信息暂不可用")
            }
        }
    }
}

// ── `/proc/self/mountinfo` 解析 ────────────────────────────────────────────────

/// `/proc/self/mountinfo` 中的单条挂载信息。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct MountInfoEntry {
    /// 挂载点路径（已解码八进制转义）
    pub mount_point: PathBuf,
    /// 文件系统类型（如 `ext4`、`xfs`、`tmpfs`）
    pub fs_type: String,
}

/// 解码 Linux `mountinfo` 中的八进制转义序列（如 `\040` 空格、`\011` Tab、`\012` 换行、`\134` 反斜杠）。
#[must_use]
#[cfg_attr(not(unix), allow(dead_code))]
fn unescape_mountinfo_field(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && (b'0'..=b'7').contains(&bytes[i + 1])
            && (b'0'..=b'7').contains(&bytes[i + 2])
            && (b'0'..=b'7').contains(&bytes[i + 3])
        {
            let val = ((bytes[i + 1] - b'0') << 6)
                | ((bytes[i + 2] - b'0') << 3)
                | (bytes[i + 3] - b'0');
            out.push(val);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解析完整的 `/proc/self/mountinfo` 文本。
#[must_use]
#[cfg_attr(not(unix), allow(dead_code))]
pub fn parse_mountinfo(content: &str) -> Vec<MountInfoEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // mountinfo 格式：id parent major:minor root mount_point opts [optional...] - fstype source super_opts
        let Some((left, right)) = trimmed.split_once(" - ") else {
            continue;
        };
        let left_fields: Vec<&str> = left.split_whitespace().collect();
        let right_fields: Vec<&str> = right.split_whitespace().collect();
        if left_fields.len() < 5 || right_fields.is_empty() {
            continue;
        }

        let mount_point = PathBuf::from(unescape_mountinfo_field(left_fields[4]));
        let fs_type = unescape_mountinfo_field(right_fields[0]);
        entries.push(MountInfoEntry {
            mount_point,
            fs_type,
        });
    }
    entries
}

/// 规范化挂载路径末尾斜杠（保留根路径 `/`）。
#[must_use]
fn normalize_mount_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() {
        PathBuf::from("/")
    } else {
        PathBuf::from(trimmed)
    }
}

/// 在解析好的 `mountinfo` 列表中校验目标挂载点（取最后一次覆盖挂载记录）。
#[cfg_attr(not(unix), allow(dead_code))]
pub fn verify_mount_in_entries(
    entries: &[MountInfoEntry],
    expected_mount: &Path,
) -> std::result::Result<String, DiskProbeError> {
    let normalized = normalize_mount_path(expected_mount);

    // Linux 同路径多次挂载时，最后一条记录是当前生效的顶层挂载
    let matched = entries
        .iter()
        .rev()
        .find(|entry| normalize_mount_path(&entry.mount_point) == normalized);

    let Some(entry) = matched else {
        return Err(DiskProbeError::NotMounted {
            mount_point: normalized,
        });
    };

    let lower_fs = entry.fs_type.to_ascii_lowercase();
    if REJECTED_FS_TYPES.contains(&lower_fs.as_str()) {
        return Err(DiskProbeError::UnexpectedFsType {
            mount_point: normalized,
            fs_type: entry.fs_type.clone(),
        });
    }

    Ok(entry.fs_type.clone())
}

/// 读取 Linux 文件系统挂载表与 `statvfs` 原始统计，返回实时 [`DiskUsageStats`]。
#[cfg(unix)]
pub fn read_live_disk_stats(mount_point: &Path) -> std::result::Result<DiskUsageStats, DiskProbeError> {
    let normalized = normalize_mount_path(mount_point);

    // 1. 校验挂载点目录本身存在且不是符号链接
    let sym_meta = std::fs::symlink_metadata(&normalized).map_err(|e| DiskProbeError::StatFailed {
        mount_point: normalized.clone(),
        reason: e.to_string(),
    })?;
    if sym_meta.file_type().is_symlink() || !sym_meta.file_type().is_dir() {
        return Err(DiskProbeError::StatFailed {
            mount_point: normalized,
            reason: "挂载路径不是真实目录".to_string(),
        });
    }

    // 2. 通过 /proc/self/mountinfo 验证其确为预期独立挂载点，而非根分区普通子目录
    let mountinfo_text =
        std::fs::read_to_string("/proc/self/mountinfo").map_err(|e| DiskProbeError::StatFailed {
            mount_point: normalized.clone(),
            reason: format!("无法读取 /proc/self/mountinfo: {e}"),
        })?;
    let entries = parse_mountinfo(&mountinfo_text);
    let fs_type = verify_mount_in_entries(&entries, &normalized)?;

    // 3. 调用 POSIX statvfs 获取原始块统计
    let vfs = rustix::fs::statvfs(&normalized).map_err(|e| DiskProbeError::StatFailed {
        mount_point: normalized.clone(),
        reason: e.to_string(),
    })?;

    let frsize = if vfs.f_frsize > 0 {
        vfs.f_frsize
    } else {
        vfs.f_bsize
    };

    if vfs.f_blocks == 0 || frsize == 0 {
        return Err(DiskProbeError::StatFailed {
            mount_point: normalized,
            reason: "statvfs 返回总块数为 0".to_string(),
        });
    }

    Ok(DiskUsageStats::from_raw_blocks(
        normalized,
        fs_type,
        frsize,
        vfs.f_blocks,
        vfs.f_bfree,
        vfs.f_bavail,
    ))
}

/// 非 Unix 平台的编译兼容桩实现（生产目标环境为 Linux VPS）。
#[cfg(not(unix))]
pub fn read_live_disk_stats(mount_point: &Path) -> std::result::Result<DiskUsageStats, DiskProbeError> {
    Err(DiskProbeError::StatFailed {
        mount_point: normalize_mount_path(mount_point),
        reason: "当前非 Linux/Unix 平台不支持挂载表与 statvfs 检查".to_string(),
    })
}

// ── 三级磁盘报警与去重复位状态机 ──────────────────────────────────────────────

/// 跨重启持久化的最小磁盘报警状态（仅包含去重必需布尔位，不含敏感数据）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskAlertState {
    /// 各容量等级当前是否处于“已成功发送报警且尚未回落到复位阈值以下”状态：`[L1, L2, L3]`
    pub latched_levels: [bool; 3],
    /// 挂载丢失/统计异常报警是否已成功发送且尚未恢复（与容量超限独立去重）
    pub mount_anomaly_latched: bool,
}

/// 回落滞回复位参数配置。
#[derive(Debug, Clone, Copy)]
pub struct AlertResetThresholds {
    /// 一级复位阈值（建议 `<= 85.0%`）
    pub reset_l1: f64,
    /// 二级复位阈值（建议 `<= 90.0%`）
    pub reset_l2: f64,
    /// 三级复位阈值（建议 `<= 95.0%`）
    pub reset_l3: f64,
}

/// 单次报警评估后需要发送的待确认通知及发送成功后的目标状态。
#[derive(Debug, Clone, PartialEq)]
pub enum AlertEvaluation {
    /// 无需发送新报警（可能仅发生了回落复位或持续维持原等级）
    NoAlert {
        /// 需要立即保存的更新后状态（如发生回落复位或挂载恢复）
        updated_state: DiskAlertState,
    },
    /// 需要向管理员发送一条容量超限或升级报警
    SendCapacityAlert {
        /// 本次触发的最高等级
        level: DiskAlertLevel,
        /// 是否从正常状态一次性跨越了全部三级阈值（>98%）
        crossed_all_three: bool,
        /// 本次跨越的所有未锁定等级列表（升序）
        newly_crossed: Vec<DiskAlertLevel>,
        /// 若发送失败也应先行保存的回落状态（通常与原状态一致）
        state_if_send_fails: DiskAlertState,
        /// **仅在 Telegram 消息发送成功后**才可提交的新锁定状态
        state_if_send_succeeds: DiskAlertState,
    },
    /// 需要向管理员发送一条挂载异常/统计不可用报警
    SendMountAnomalyAlert {
        /// 发送失败时保持原状态（下次检查重试）
        state_if_send_fails: DiskAlertState,
        /// **仅在 Telegram 消息发送成功后**才提交的锁定状态
        state_if_send_succeeds: DiskAlertState,
    },
}

/// 根据本次实时容量统计结果评估报警动作（纯状态机，无 I/O）。
#[must_use]
pub fn evaluate_capacity_alert(
    prev: &DiskAlertState,
    stats: &DiskUsageStats,
    resets: AlertResetThresholds,
) -> AlertEvaluation {
    let mut after_reset = prev.clone();
    // 挂载点已正常读取，自动解除挂载异常去重锁
    after_reset.mount_anomaly_latched = false;

    // 1. 先执行回落复位判断（小于等于对应复位值时解锁该级）
    if stats.raw_use_percent <= resets.reset_l1 {
        after_reset.latched_levels[0] = false;
    }
    if stats.raw_use_percent <= resets.reset_l2 {
        after_reset.latched_levels[1] = false;
    }
    if stats.raw_use_percent <= resets.reset_l3 {
        after_reset.latched_levels[2] = false;
    }

    // 2. 判断当前最高已触发等级（严格大于 90 / 95 / 98）
    let Some(highest) = DiskAlertLevel::from_raw_percent(stats.raw_use_percent) else {
        return AlertEvaluation::NoAlert {
            updated_state: after_reset,
        };
    };

    let max_idx = highest.index();
    if after_reset.latched_levels[max_idx] {
        // 最高已达到等级处于已报警锁定状态，不重复刷屏
        return AlertEvaluation::NoAlert {
            updated_state: after_reset,
        };
    }

    let all_levels = [
        DiskAlertLevel::Level1,
        DiskAlertLevel::Level2,
        DiskAlertLevel::Level3,
    ];
    let mut newly_crossed = Vec::new();
    let mut committed = after_reset.clone();

    for lvl in &all_levels[..=max_idx] {
        let idx = lvl.index();
        if !committed.latched_levels[idx] {
            newly_crossed.push(*lvl);
            committed.latched_levels[idx] = true;
        }
    }

    let crossed_all_three = highest == DiskAlertLevel::Level3 && newly_crossed.len() == 3;

    AlertEvaluation::SendCapacityAlert {
        level: highest,
        crossed_all_three,
        newly_crossed,
        state_if_send_fails: after_reset,
        state_if_send_succeeds: committed,
    }
}

/// 根据挂载探测失败结果评估挂载异常报警动作（纯状态机，容量状态保持独立不变）。
#[must_use]
pub fn evaluate_mount_error_alert(prev: &DiskAlertState) -> AlertEvaluation {
    if prev.mount_anomaly_latched {
        AlertEvaluation::NoAlert {
            updated_state: prev.clone(),
        }
    } else {
        let mut committed = prev.clone();
        committed.mount_anomaly_latched = true;
        AlertEvaluation::SendMountAnomalyAlert {
            state_if_send_fails: prev.clone(),
            state_if_send_succeeds: committed,
        }
    }
}

/// 构造容量超限报警的 Telegram HTML 消息文本。
#[must_use]
pub fn format_capacity_alert_message(
    level: DiskAlertLevel,
    crossed_all_three: bool,
    stats: &DiskUsageStats,
) -> String {
    let icon = match level {
        DiskAlertLevel::Level1 => "⚠️",
        DiskAlertLevel::Level2 => "🚨",
        DiskAlertLevel::Level3 => "🆘",
    };
    let mount_disp = escape_html(&stats.mount_point.display().to_string());
    let used_h = format_df_human_bytes(stats.used_bytes);
    let total_h = format_df_human_bytes(stats.total_bytes);
    let avail_h = format_df_human_bytes(stats.avail_bytes);

    let jump_note = if crossed_all_three {
        "\n⚡ <b>说明</b>: 本次检查容量从正常区间直接跃升超过 98%，已一次性越过全部三级阈值（&gt;90% / &gt;95% / &gt;98%），合并发送本条最高级告警。"
    } else {
        ""
    };

    format!(
        "{icon} <b>磁盘容量{}！</b>\n\
         ━━━━━━━━━━━━\n\
         💽 挂载点: <code>{mount_disp}</code>\n\
         📊 当前占用率: <code>{}%</code>（精确值 <code>{:.2}%</code>）\n\
         📦 已用 / 总量: <code>{used_h}</code> / <code>{total_h}</code>\n\
         🟢 剩余可用: <code>{avail_h}</code>{jump_note}\n\n\
         请及时清理历史录播文件，避免录制或合并因磁盘写满中断。",
        level.label(),
        stats.display_use_percent,
        stats.raw_use_percent
    )
}

/// 构造挂载丢失或读取异常的 Telegram HTML 报警消息文本。
#[must_use]
pub fn format_mount_anomaly_message(err: &DiskProbeError) -> String {
    let safe_reason = escape_html(&redact_secrets(&err.to_string()));
    format!(
        "🚨 <b>存储挂载异常告警！</b>\n\
         ━━━━━━━━━━━━\n\
         {safe_reason}\n\n\
         Bot 已拒绝将根分区目录当作媒体盘使用，请管理员立即登录 VPS 检查挂载状态（如 <code>findmnt /storage512</code>）。"
    )
}

// ── 统一磁盘监测服务 ──────────────────────────────────────────────────────────

/// 磁盘监测与报警协调器（供启动检查、后台 5 分钟轮询、每次 `/start` 与空间预检共用）。
#[derive(Debug)]
pub struct DiskMonitor {
    mount_point: PathBuf,
    resets: AlertResetThresholds,
    state_file: PathBuf,
    admin_id: i64,
    alert_state: Mutex<DiskAlertState>,
    notification_lock: Mutex<()>,
}

impl DiskMonitor {
    /// 从全局配置创建磁盘监测器，并尝试加载持久化的去重状态（加载失败仅记录日志，不阻断启动）。
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let initial_state = load_alert_state_from_file(&config.disk_alert_state_file);
        Self {
            mount_point: config.storage_mount_point.clone(),
            resets: AlertResetThresholds {
                reset_l1: config.disk_alert_reset_l1,
                reset_l2: config.disk_alert_reset_l2,
                reset_l3: config.disk_alert_reset_l3,
            },
            state_file: config.disk_alert_state_file.clone(),
            admin_id: config.admin_id,
            alert_state: Mutex::new(initial_state),
            notification_lock: Mutex::new(()),
        }
    }

    /// 创建用于测试或默认状态初始化的实例。
    #[must_use]
    #[allow(dead_code)]
    pub fn new_default(mount_point: PathBuf, admin_id: i64) -> Self {
        Self {
            mount_point,
            resets: AlertResetThresholds {
                reset_l1: 85.0,
                reset_l2: 90.0,
                reset_l3: 95.0,
            },
            state_file: PathBuf::from("./.disk_alert_state.json"),
            admin_id,
            alert_state: Mutex::new(DiskAlertState::default()),
            notification_lock: Mutex::new(()),
        }
    }

    /// 执行一次实时磁盘统计读取，并在需要时向管理员发送三级容量报警或挂载异常报警。
    ///
    /// 启动检查、后台定时任务和每次 `/start`（及返回主菜单）均调用本方法，保证始终读取最新内核统计值。
    pub async fn check_and_notify(&self, bot: &Bot) -> DiskMenuSummary {
        // Serialize checks so a slow Telegram request cannot generate duplicate alerts.
        // The alert-state mutex itself is never held over a network request.
        let _notification_guard = self.notification_lock.lock().await;
        let mount = self.mount_point.clone();
        let probe_result = tokio::task::spawn_blocking(move || read_live_disk_stats(&mount)).await;

        let stats_res = match probe_result {
            Ok(res) => res,
            Err(join_err) => Err(DiskProbeError::StatFailed {
                mount_point: self.mount_point.clone(),
                reason: format!("blocking task error: {join_err}"),
            }),
        };

        let prev_state = self.alert_state.lock().await.clone();

        match stats_res {
            Ok(stats) => {
                let eval = evaluate_capacity_alert(&prev_state, &stats, self.resets);
                match eval {
                    AlertEvaluation::NoAlert { updated_state } => {
                        if updated_state != prev_state {
                            *self.alert_state.lock().await = updated_state.clone();
                            persist_alert_state_quietly(&self.state_file, &updated_state);
                        }
                    }
                    AlertEvaluation::SendCapacityAlert {
                        level,
                        crossed_all_three,
                        newly_crossed,
                        state_if_send_fails,
                        state_if_send_succeeds,
                    } => {
                        let alert_html =
                            format_capacity_alert_message(level, crossed_all_three, &stats);
                        match bot
                            .send_message(ChatId(self.admin_id), alert_html)
                            .parse_mode(ParseMode::Html)
                            .await
                        {
                            Ok(_) => {
                                info!(
                                    mount = %stats.mount_point.display(),
                                    fs_type = %stats.fs_type,
                                    level = ?level,
                                    newly_crossed = ?newly_crossed,
                                    display_pct = stats.display_use_percent,
                                    raw_pct = format_args!("{:.2}", stats.raw_use_percent),
                                    "Disk capacity alert sent to admin"
                                );
                                *self.alert_state.lock().await = state_if_send_succeeds.clone();
                                persist_alert_state_quietly(
                                    &self.state_file,
                                    &state_if_send_succeeds,
                                );
                            }
                            Err(send_err) => {
                                let safe_err = redact_secrets(&send_err.to_string());
                                warn!(
                                    level = ?level,
                                    error = %safe_err,
                                    "Failed to send disk capacity alert; keeping pending state for retry"
                                );
                                if state_if_send_fails != prev_state {
                                    *self.alert_state.lock().await = state_if_send_fails.clone();
                                    persist_alert_state_quietly(
                                        &self.state_file,
                                        &state_if_send_fails,
                                    );
                                }
                            }
                        }
                    }
                    AlertEvaluation::SendMountAnomalyAlert { .. } => {}
                }

                DiskMenuSummary::Available(stats)
            }
            Err(probe_err) => {
                let safe_err = redact_secrets(&probe_err.to_string());
                error!(
                    mount = %self.mount_point.display(),
                    reason = %safe_err,
                    "Storage mount check or statvfs failed"
                );

                if let AlertEvaluation::SendMountAnomalyAlert {
                    state_if_send_succeeds,
                    ..
                } = evaluate_mount_error_alert(&prev_state)
                {
                    let alert_html = format_mount_anomaly_message(&probe_err);
                    match bot
                        .send_message(ChatId(self.admin_id), alert_html)
                        .parse_mode(ParseMode::Html)
                        .await
                    {
                        Ok(_) => {
                            info!(
                                mount = %self.mount_point.display(),
                                "Mount anomaly alert sent to admin"
                            );
                            *self.alert_state.lock().await = state_if_send_succeeds.clone();
                            persist_alert_state_quietly(
                                &self.state_file,
                                &state_if_send_succeeds,
                            );
                        }
                        Err(send_err) => {
                            let safe_send_err = redact_secrets(&send_err.to_string());
                            warn!(
                                error = %safe_send_err,
                                "Failed to send mount anomaly alert; will retry on next check"
                            );
                        }
                    }
                }

                DiskMenuSummary::Unavailable {
                    mount_point: self.mount_point.clone(),
                }
            }
        }
    }

    /// 在大文件合并/转封装开始前预检 `/storage512` 挂载状态与剩余可用空间。
    ///
    /// # Errors
    ///
    /// - 挂载丢失或读取异常时返回 [`AppError::StorageMountUnavailable`]
    /// - 剩余可用字节低于 `required_bytes` 时返回 [`AppError::InsufficientDiskSpace`]
    pub async fn ensure_space_available(&self, required_bytes: u64) -> Result<DiskUsageStats> {
        let mount = self.mount_point.clone();
        let stats_res = tokio::task::spawn_blocking(move || read_live_disk_stats(&mount))
            .await
            .map_err(|e| AppError::StorageMountUnavailable {
                mount_point: self.mount_point.clone(),
                reason: e.to_string(),
            })?;

        let stats = stats_res.map_err(|e| AppError::StorageMountUnavailable {
            mount_point: self.mount_point.clone(),
            reason: e.to_string(),
        })?;

        if stats.avail_bytes < required_bytes {
            return Err(AppError::InsufficientDiskSpace {
                required_bytes,
                available_bytes: stats.avail_bytes,
            }
            .into());
        }

        Ok(stats)
    }
}

/// 合并任务安全余量（512 MiB）。
pub const MERGE_SPACE_SAFETY_MARGIN_BYTES: u64 = 512 * 1024 * 1024;

/// 估算无损合并所需的峰值剩余磁盘空间（字节）。
///
/// 包含可能同时存在的：
/// - 全部 `.ts` 中间封装文件（约输入总大小的 1.15 倍，预留 TS 188B 包头开销）
/// - 临时合并输出 `.mp4`（约输入总大小的 1.05 倍）
/// - 512 MiB 固定安全余量
#[must_use]
pub fn estimate_merge_required_bytes(total_input_bytes: u64) -> u64 {
    let ts_and_mp4 = total_input_bytes.saturating_mul(22) / 10;
    ts_and_mp4.saturating_add(MERGE_SPACE_SAFETY_MARGIN_BYTES)
}

/// 估算批量转封装 MP4 所需的剩余磁盘空间（所有新输出文件 + 512 MiB 安全余量）。
#[must_use]
pub fn estimate_convert_required_bytes(total_input_bytes: u64) -> u64 {
    let outputs = total_input_bytes.saturating_mul(105) / 100;
    outputs.saturating_add(MERGE_SPACE_SAFETY_MARGIN_BYTES)
}

fn load_alert_state_from_file(path: &Path) -> DiskAlertState {
    if !path.exists() {
        return DiskAlertState::default();
    }
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<DiskAlertState>(&content) {
            Ok(state) => state,
            Err(e) => {
                warn!(
                    state_file = %path.display(),
                    error = %redact_secrets(&e.to_string()),
                    "Corrupt disk alert state file; resetting to default"
                );
                DiskAlertState::default()
            }
        },
        Err(e) => {
            warn!(
                state_file = %path.display(),
                error = %redact_secrets(&e.to_string()),
                "Failed to read disk alert state file; using default in-memory state"
            );
            DiskAlertState::default()
        }
    }
}

fn persist_alert_state_quietly(path: &Path, state: &DiskAlertState) {
    match serde_json::to_vec_pretty(state) {
        Ok(bytes) => {
            if let Err(e) = write_private_file_atomic(path, &bytes) {
                warn!(
                    state_file = %path.display(),
                    error = %redact_secrets(&e.to_string()),
                    "Failed to persist disk alert state (non-fatal)"
                );
            }
        }
        Err(e) => {
            warn!(error = %e, "Failed to serialize disk alert state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_resets() -> AlertResetThresholds {
        AlertResetThresholds {
            reset_l1: 85.0,
            reset_l2: 90.0,
            reset_l3: 95.0,
        }
    }

    fn make_stats_pct(used_units: u64, avail_units: u64) -> DiskUsageStats {
        // total_blocks 含 5% root 保留块，验证 df Use% 口径 used / (used + avail)
        let root_reserved = (used_units + avail_units) / 20;
        let total = used_units + avail_units + root_reserved;
        let free = avail_units + root_reserved;
        DiskUsageStats::from_raw_blocks(
            PathBuf::from("/storage512"),
            "ext4".to_string(),
            4096,
            total,
            free,
            avail_units,
        )
    }

    #[test]
    fn test_mountinfo_verification_and_root_isolation() {
        let sample = "\
29 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
35 29 0:31 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw
88 29 8:16 / /storage512 rw,relatime shared:42 - ext4 /dev/sdb rw
";
        let entries = parse_mountinfo(sample);
        assert_eq!(
            verify_mount_in_entries(&entries, Path::new("/storage512")).unwrap(),
            "ext4"
        );
        // 未挂载路径绝不回退到 `/`
        assert!(matches!(
            verify_mount_in_entries(&entries, Path::new("/storage_missing")),
            Err(DiskProbeError::NotMounted { .. })
        ));
        // tmpfs 被拒绝
        assert!(matches!(
            verify_mount_in_entries(&entries, Path::new("/tmp")),
            Err(DiskProbeError::UnexpectedFsType { .. })
        ));
    }

    #[test]
    fn test_df_use_percent_matches_user_91_percent_baseline() {
        // 模拟用户实机基线：总量 503 GiB，已用 431.2 GiB，可用 47.8 GiB（其余为 ext4 root 预留）
        let gib_blocks = (1024 * 1024 * 1024) / 4096;
        let total_blocks = 503 * gib_blocks;
        let used_blocks = (4312 * gib_blocks) / 10;
        let avail_blocks = (478 * gib_blocks) / 10;
        let free_blocks = total_blocks - used_blocks;

        let stats = DiskUsageStats::from_raw_blocks(
            PathBuf::from("/storage512"),
            "ext4".to_string(),
            4096,
            total_blocks,
            free_blocks,
            avail_blocks,
        );

        assert_eq!(stats.display_use_percent, 91);
        assert!(stats.raw_use_percent > 90.0);
        assert_eq!(
            DiskAlertLevel::from_raw_percent(stats.raw_use_percent),
            Some(DiskAlertLevel::Level1)
        );
        let line = stats.format_menu_line();
        assert!(line.contains("/storage512"));
        assert!(line.contains("431G"));
        assert!(line.contains("503G"));
        assert!(line.contains("91%"));
        assert!(line.contains("48G"));
    }

    #[test]
    fn test_three_level_strict_thresholds_90_91_95_96_98_99() {
        let resets = default_resets();
        let mut state = DiskAlertState::default();

        // 恰好 90.0%：不报警
        let s90 = make_stats_pct(9000, 1000);
        assert_eq!(s90.display_use_percent, 90);
        assert!(matches!(
            evaluate_capacity_alert(&state, &s90, resets),
            AlertEvaluation::NoAlert { .. }
        ));

        // 90.1%（展示 91%）：触发一级报警
        let s91 = make_stats_pct(9010, 990);
        assert_eq!(s91.display_use_percent, 91);
        match evaluate_capacity_alert(&state, &s91, resets) {
            AlertEvaluation::SendCapacityAlert {
                level,
                crossed_all_three,
                state_if_send_succeeds,
                ..
            } => {
                assert_eq!(level, DiskAlertLevel::Level1);
                assert!(!crossed_all_three);
                state = state_if_send_succeeds;
            }
            other => panic!("Expected L1 alert, got {other:?}"),
        }

        // 持续超标 & 恰好 95.0%：不重复发一级，也不发二级
        let s95 = make_stats_pct(9500, 500);
        assert_eq!(s95.display_use_percent, 95);
        assert!(matches!(
            evaluate_capacity_alert(&state, &s95, resets),
            AlertEvaluation::NoAlert { .. }
        ));

        // 95.1%（展示 96%）：升级触发二级报警
        let s96 = make_stats_pct(9510, 490);
        assert_eq!(s96.display_use_percent, 96);
        match evaluate_capacity_alert(&state, &s96, resets) {
            AlertEvaluation::SendCapacityAlert {
                level,
                state_if_send_succeeds,
                ..
            } => {
                assert_eq!(level, DiskAlertLevel::Level2);
                state = state_if_send_succeeds;
            }
            other => panic!("Expected L2 alert, got {other:?}"),
        }

        // 恰好 98.0%：不发三级
        let s98 = make_stats_pct(9800, 200);
        assert_eq!(s98.display_use_percent, 98);
        assert!(matches!(
            evaluate_capacity_alert(&state, &s98, resets),
            AlertEvaluation::NoAlert { .. }
        ));

        // 98.1%（展示 99%）：触发三级报警
        let s99 = make_stats_pct(9810, 190);
        assert_eq!(s99.display_use_percent, 99);
        match evaluate_capacity_alert(&state, &s99, resets) {
            AlertEvaluation::SendCapacityAlert {
                level,
                crossed_all_three,
                state_if_send_succeeds,
                ..
            } => {
                assert_eq!(level, DiskAlertLevel::Level3);
                assert!(!crossed_all_three);
                state = state_if_send_succeeds;
            }
            other => panic!("Expected L3 alert, got {other:?}"),
        }
        assert_eq!(state.latched_levels, [true, true, true]);
    }

    #[test]
    fn test_direct_jump_to_99_sends_single_l3_and_hysteresis_reset() {
        let resets = default_resets();
        let initial = DiskAlertState::default();
        let s99 = make_stats_pct(9850, 150);

        // 从正常直接跳至 99%：只发一条三级报警，且标记 crossed_all_three = true
        let state_after_jump = match evaluate_capacity_alert(&initial, &s99, resets) {
            AlertEvaluation::SendCapacityAlert {
                level,
                crossed_all_three,
                state_if_send_fails,
                state_if_send_succeeds,
                ..
            } => {
                assert_eq!(level, DiskAlertLevel::Level3);
                assert!(crossed_all_three);
                let msg = format_capacity_alert_message(level, crossed_all_three, &s99);
                assert!(msg.contains("越过全部三级阈值"));
                // 验证若发送失败，状态不锁定，下次会重试
                assert_eq!(state_if_send_fails.latched_levels, [false, false, false]);
                state_if_send_succeeds
            }
            other => panic!("Expected single L3 jump alert, got {other:?}"),
        };

        // 回落到 96%（未降至 <=95%）：三级不复位，再次升到 99% 不重发
        let s96 = make_stats_pct(9600, 400);
        let state_at_96 = match evaluate_capacity_alert(&state_after_jump, &s96, resets) {
            AlertEvaluation::NoAlert { updated_state } => updated_state,
            other => panic!("Unexpected alert at 96%: {other:?}"),
        };
        assert_eq!(state_at_96.latched_levels, [true, true, true]);
        assert!(matches!(
            evaluate_capacity_alert(&state_at_96, &s99, resets),
            AlertEvaluation::NoAlert { .. }
        ));

        // 回落到 95.0%（<=95%）：仅三级复位；再次升到 99% 时重新发三级报警
        let s95 = make_stats_pct(9500, 500);
        let state_at_95 = match evaluate_capacity_alert(&state_at_96, &s95, resets) {
            AlertEvaluation::NoAlert { updated_state } => updated_state,
            other => panic!("Unexpected alert at 95%: {other:?}"),
        };
        assert_eq!(state_at_95.latched_levels, [true, true, false]);

        match evaluate_capacity_alert(&state_at_95, &s99, resets) {
            AlertEvaluation::SendCapacityAlert {
                level,
                crossed_all_three,
                ..
            } => {
                assert_eq!(level, DiskAlertLevel::Level3);
                assert!(!crossed_all_three);
            }
            other => panic!("Expected L3 re-trigger after reset, got {other:?}"),
        }
    }
}
