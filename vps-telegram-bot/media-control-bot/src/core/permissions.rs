//! 权限校验模块。
//!
//! 提供管理员身份验证逻辑，对应 Python 版本的 `is_admin` 函数。
//! 使用 Guard 模式封装权限检查，使调用方代码更简洁。

use teloxide::types::Message;
use tracing::warn;

use crate::errors::AppError;

/// 权限守卫，封装管理员权限检查逻辑。
pub struct PermissionGuard {
    admin_id: i64,
}

impl PermissionGuard {
    /// 创建权限守卫实例。
    #[must_use]
    pub fn new(admin_id: i64) -> Self {
        Self { admin_id }
    }

    /// 检查消息发送者是否为管理员。
    ///
    /// # Arguments
    ///
    /// * `msg` - Telegram 消息
    ///
    /// # Returns
    ///
    /// 若为管理员返回 `Ok(())`，否则返回 [`AppError::PermissionDenied`]。
    pub fn check_message(&self, msg: &Message) -> Result<(), AppError> {
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

        if user_id == self.admin_id {
            Ok(())
        } else {
            warn!(user_id = user_id, "Permission denied for message");
            Err(AppError::PermissionDenied { user_id })
        }
    }

    /// 检查 CallbackQuery 发送者是否为管理员。
    ///
    /// # Arguments
    ///
    /// * `user_id` - 用户 ID
    ///
    /// # Returns
    ///
    /// 若为管理员返回 `Ok(())`，否则返回 [`AppError::PermissionDenied`]。
    pub fn check_user_id(&self, user_id: i64) -> Result<(), AppError> {
        if user_id == self.admin_id {
            Ok(())
        } else {
            warn!(user_id = user_id, "Permission denied for callback");
            Err(AppError::PermissionDenied { user_id })
        }
    }

    /// 判断给定用户 ID 是否为管理员。
    #[must_use]
    #[allow(dead_code)]
    pub fn is_admin(&self, user_id: i64) -> bool {
        user_id == self.admin_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_admin() {
        let guard = PermissionGuard::new(12345);
        assert!(guard.is_admin(12345));
        assert!(!guard.is_admin(99999));
        assert!(!guard.is_admin(0));
    }

    #[test]
    fn test_check_user_id_ok() {
        let guard = PermissionGuard::new(42);
        assert!(guard.check_user_id(42).is_ok());
    }

    #[test]
    fn test_check_user_id_denied() {
        let guard = PermissionGuard::new(42);
        let result = guard.check_user_id(99);
        assert!(matches!(
            result,
            Err(AppError::PermissionDenied { user_id: 99 })
        ));
    }
}
