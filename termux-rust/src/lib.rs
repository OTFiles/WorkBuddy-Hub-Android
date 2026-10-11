//! WorkBuddy Hub for Termux — the crate root.
//!
//! The binary is a thin shell over these modules so the protocol logic stays
//! unit-testable without a network.

pub mod bridge;
pub mod calllog;
pub mod cli;
pub mod http;
pub mod login;
pub mod settings;
pub mod store;
pub mod upstream;
pub mod wire;
