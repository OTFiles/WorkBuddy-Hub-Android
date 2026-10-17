//! Termux 的唤醒锁。
//!
//! 原 Android 版靠悬浮窗 + 前台服务 + WakeLock + 精确闹钟来避免进程被系统冻结。
//! Termux 里没有这些，对应的是 `termux-wake-lock`：它拿一个 `PARTIAL_WAKE_LOCK`，
//! 让 CPU 在息屏后继续跑，这样转发端点不会在 Deep Sleep 里静默停摆。
//!
//! 锁由系统按包名持有，所以进程被杀了锁也还在——因此退出时一定要显式释放，否则
//! 会白耗电。

use std::process::{Command, Stdio};

/// 是否装了 `termux-wake-lock`。
pub fn available() -> bool {
    which("termux-wake-lock").is_some()
}

fn which(binary: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn run(binary: &str, arg: Option<&str>) -> bool {
    let Some(path) = which(binary) else {
        return false;
    };
    let mut command = Command::new(path);
    if let Some(arg) = arg {
        command.arg(arg);
    }
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 拿锁。返回是否真的拿到了，好让调用方只在成功时安排释放。
pub fn acquire() -> bool {
    run("termux-wake-lock", None)
}

/// 放锁。
pub fn release() -> bool {
    run("termux-wake-unlock", None)
}

/// 一个拿到就放的作用域守卫，保证 panic 或 early-return 时也会释放。
pub struct WakeLock {
    held: bool,
}

impl WakeLock {
    /// 尝试拿锁；拿不到就是 `held = false`，其余部分照常工作。
    pub fn acquire() -> WakeLock {
        WakeLock { held: acquire() }
    }

    pub fn held(&self) -> bool {
        self.held
    }
}

impl Drop for WakeLock {
    fn drop(&mut self) {
        if self.held {
            release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_a_binary_that_must_exist() {
        // `sh` 一定在 PATH 里；这里只验查找逻辑本身。
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn acquire_without_termux_is_a_no_op_rather_than_a_panic() {
        // 在没有 Termux:API 的机器上跑测试时，这里必须安静地返回 false。
        let lock = WakeLock::acquire();
        // 拿不到锁时应当立刻报告没拿到，而不是假装拿到了。
        if !available() {
            assert!(!lock.held());
        }
        drop(lock);
    }
}
