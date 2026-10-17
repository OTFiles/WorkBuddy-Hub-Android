//! 可执行入口：解析参数，并把错误打印成人能看懂的样子。

use std::ffi::OsString;

use clap::{CommandFactory, FromArgMatches};

use wbhub::cli;

// 这里特意不去动 SIGPIPE：保持 Rust 的默认（忽略），再配合 crate 里的 `say!`
// 宏把写入错误吞掉。恢复成 SIG_DFL 会让进程在管道下游关闭时被信号直接杀掉，
// 于是 SIGTERM/SIGHUP 那条清理路径（释放 Termux 唤醒锁）根本走不到。
// 见 lib.rs 里 `say!` 的说明。

#[tokio::main]
async fn main() {
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
                    wbhub::say!("{error}");
                }
                _ => eprint!("参数错误：{error}"),
            }
            std::process::exit(if error.use_stderr() { 2 } else { 0 });
        }
    };
    let parsed = match cli::Cli::from_arg_matches(&matches) {
        Ok(parsed) => parsed,
        Err(error) => {
            wbhub::say_err!("参数错误：{error}");
            std::process::exit(2);
        }
    };

    if let Err(error) = cli::run(parsed).await {
        wbhub::say_err!("错误：{error:#}");
        std::process::exit(1);
    }
}
