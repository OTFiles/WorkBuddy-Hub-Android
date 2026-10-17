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
