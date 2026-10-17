//! 可执行入口：解析参数，并把错误打印成人能看懂的样子。

use std::ffi::OsString;

use clap::{CommandFactory, FromArgMatches};

use wbhub::cli;

/// 把 SIGPIPE 的处置方式交回系统默认。
///
/// Rust 启动时把 SIGPIPE 设成忽略，于是写入已经关闭的管道会变成 EPIPE 错误，而
/// `println!` 遇到错误会 panic。`wbhub models | head` 或者 `| grep` 是最常见的
/// 用法，必须像普通命令行工具那样安静退出。
#[cfg(unix)]
fn restore_default_sigpipe() {
    // SAFETY: 只调用 signal(2) 把 SIGPIPE 恢复成 SIG_DFL，不涉及任何内存或
    // 生命周期，进程启动早期执行一次。
    #[allow(unsafe_code)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_default_sigpipe() {}

#[tokio::main]
async fn main() {
    restore_default_sigpipe();
    let mut args: Vec<OsString> = std::env::args_os().collect();
    // `help 子命令` 是习惯用法，但 clap 自动生成的 help 子命令只有英文描述，所以
    // 关掉它，在这里把两种写法都折算成 `子命令 --help`。
    if args.len() >= 2 && args[1] == "help" {
        args.remove(1);
        args.push("--help".into());
    }

    let command = cli::localize(cli::Cli::command());
    let matches = match command.try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => {
            // clap 的错误文本本身没法本地化（它要指出具体哪个参数错了），所以
            // 只在外面加一层中文说明。
            match error.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    print!("{error}");
                }
                _ => eprint!("参数错误：{error}"),
            }
            std::process::exit(if error.use_stderr() { 2 } else { 0 });
        }
    };
    let parsed = match cli::Cli::from_arg_matches(&matches) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprint!("参数错误：{error}");
            std::process::exit(2);
        }
    };

    if let Err(error) = cli::run(parsed).await {
        eprintln!("错误：{error:#}");
        std::process::exit(1);
    }
}
