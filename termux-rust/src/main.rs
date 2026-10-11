//! WorkBuddy Hub for Termux.
//!
//! A Rust port of `WorkBuddy-Hub-Android`'s local forwarding endpoint, built to
//! run under Termux instead of inside an APK.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod calllog;
pub mod cli;
pub mod http;
pub mod login;
pub mod settings;
pub mod store;
pub mod upstream;
pub mod wire;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();
    if let Err(error) = cli::run(cli).await {
        eprintln!("错误：{error:#}");
        std::process::exit(1);
    }
}
