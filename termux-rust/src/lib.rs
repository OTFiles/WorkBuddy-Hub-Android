//! WorkBuddy Hub for Termux。
//!
//! `WorkBuddy-Hub-Android` 的 Rust 重写版：把 Android App 里那三段与平台无关的
//! 逻辑（协议、本地转发端点、上游客户端）搬到 Termux 里，做成一个单文件命令行
//! 程序。

#![forbid(unsafe_code)]

pub mod bridge;
pub mod calllog;
pub mod cli;
pub mod growth;
pub mod http;
pub mod login;
pub mod settings;
pub mod store;
pub mod upstream;
pub mod wake;
pub mod wire;

/// 输出到标准输出，写入失败就忽略。
///
/// 直接用 `println!` 有两个麻烦：Rust 默认忽略 SIGPIPE，于是 `wbhub models |
/// head` 会在 write 时拿到 EPIPE 并 panic；就算恢复 SIGPIPE，进程又会被信号
/// 直接杀掉，信号处理器里的清理（比如释放唤醒锁）根本跑不到。把写入错误吞掉是
/// 唯一同时满足两边的做法，也是大多数命令行工具的默认行为。
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => {{
        #[allow(unused_imports)]
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// 输出到标准错误，写入失败就忽略。
#[macro_export]
macro_rules! say_err {
    ($($arg:tt)*) => {{
        #[allow(unused_imports)]
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}
