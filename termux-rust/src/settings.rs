//! Runtime settings.
//!
//! The Android build read these from `SharedPreferences` (`wb-overlay`) and
//! `BridgeSettings`. Here they live in one JSON file next to the credentials,
//! so a single `wbhub.yaml`-free install stays self-describing and a user can
//! diff or hand-edit it.

use std::fs;
use std::net::IpAddr;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::store::write_private;

pub const DEFAULT_PORT: u16 = 8765;

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_true() -> bool {
    true
}

fn default_call_limit() -> usize {
    500
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Loopback port the OpenAI-compatible endpoint listens on.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Key presented by clients on this device.
    pub api_key: String,
    /// Key presented by peers on the local network.
    pub lan_key: String,
    /// Whether to serve the local network as well as this device.
    ///
    /// Loopback needs no trust decision; anything wider does, because the
    /// endpoint holds a credential and a key that a peer can observe travels
    /// in clear text over plain HTTP.
    pub lan_enabled: bool,
    /// How many call records to keep in the log.
    pub call_log_limit: usize,
    /// Refresh the account list and token on this interval while serving.
    pub refresh_minutes: u64,
    /// Print a line per request.
    #[serde(default = "default_true")]
    pub verbose: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            port: DEFAULT_PORT,
            api_key: String::new(),
            lan_key: String::new(),
            lan_enabled: false,
            call_log_limit: default_call_limit(),
            refresh_minutes: 30,
            verbose: true,
        }
    }
}

impl Settings {
    pub fn path(dir: &Path) -> std::path::PathBuf {
        dir.join("settings.json")
    }

    /// Loads settings, creating a file with fresh random keys on first run.
    ///
    /// The keys are generated rather than defaulted: a hard-coded default would
    /// be shared by every install, which on a LAN-enabled bridge is the same as
    /// having no key at all.
    pub fn load(dir: &Path) -> Result<Settings> {
        let path = Settings::path(dir);
        let mut settings = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Settings::default(),
        };
        let mut dirty = false;
        if settings.api_key.trim().is_empty() {
            settings.api_key = random_key();
            dirty = true;
        }
        if settings.lan_key.trim().is_empty() {
            settings.lan_key = random_key();
            dirty = true;
        }
        if settings.port == 0 {
            settings.port = DEFAULT_PORT;
            dirty = true;
        }
        if dirty || !path.exists() {
            settings.save(dir)?;
        }
        Ok(settings)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        write_private(&Settings::path(dir), &serde_json::to_string_pretty(self)?)
    }

    /// Replaces the local key, returning the new value so it can be shown once.
    pub fn rotate_api_key(&mut self, dir: &Path) -> Result<String> {
        self.api_key = random_key();
        self.save(dir)?;
        Ok(self.api_key.clone())
    }

    pub fn rotate_lan_key(&mut self, dir: &Path) -> Result<String> {
        self.lan_key = random_key();
        self.save(dir)?;
        Ok(self.lan_key.clone())
    }
}

/// A 32-character key with the `wb-local-` prefix the original used, so clients
/// already configured against the Android build keep working with minimal fuss.
fn random_key() -> String {
    format!("wb-local-{}", uuid::Uuid::new_v4().simple())
}

/// Whether an address is inside a private range.
///
/// The endpoint is only meant for the local network, so the check is the private
/// blocks rather than "any address": a public name resolving here would mean the
/// request came from somewhere it should not have.
pub fn is_private_address(host: &str) -> bool {
    let lowered = host.trim().to_ascii_lowercase();
    if lowered == "localhost" {
        return true;
    }
    match lowered.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        Ok(IpAddr::V6(v6)) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return mapped.is_private() || mapped.is_loopback() || mapped.is_link_local();
            }
            v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
        // A hostname rather than a literal: accept it only if it resolves into a
        // private range, which covers the "phone.local" case.
        Err(_) => resolve_all(&lowered)
            .map(|addresses| addresses.iter().any(|a| is_private_address(&a.to_string())))
            .unwrap_or(false),
    }
}

fn resolve_all(host: &str) -> Option<Vec<IpAddr>> {
    use std::net::ToSocketAddrs;
    let addrs = (host, 0u16).to_socket_addrs().ok()?;
    Some(addrs.map(|a| a.ip()).collect())
}

/// Whether a host string names a loopback address.
pub fn is_loopback_host(host: &str) -> bool {
    let lowered = host.trim().to_ascii_lowercase();
    if lowered == "localhost" || lowered == "[::1]" || lowered == "::1" {
        return true;
    }
    matches!(lowered.parse::<IpAddr>(), Ok(addr) if addr.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wbhub-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_run_generates_distinct_keys() {
        let dir = scratch("keys");
        let settings = Settings::load(&dir).unwrap();
        assert!(settings.api_key.starts_with("wb-local-"));
        assert!(settings.lan_key.starts_with("wb-local-"));
        assert_ne!(settings.api_key, settings.lan_key);
        let reloaded = Settings::load(&dir).unwrap();
        assert_eq!(reloaded.api_key, settings.api_key, "keys must persist");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blank_key_in_an_existing_file_is_replaced() {
        let dir = scratch("blank");
        fs::write(
            Settings::path(&dir),
            r#"{"port":9999,"api_key":"","lan_key":""}"#,
        )
        .unwrap();
        let settings = Settings::load(&dir).unwrap();
        assert_eq!(settings.port, 9999, "explicit values survive");
        assert!(!settings.api_key.is_empty());
        assert!(!settings.lan_key.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_changes_the_key_and_persists() {
        let dir = scratch("rotate");
        let mut settings = Settings::load(&dir).unwrap();
        let before = settings.api_key.clone();
        let after = settings.rotate_api_key(&dir).unwrap();
        assert_ne!(before, after);
        assert_eq!(Settings::load(&dir).unwrap().api_key, after);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_ranges_are_recognised() {
        assert!(is_private_address("127.0.0.1"));
        assert!(is_private_address("192.168.1.5"));
        assert!(is_private_address("10.0.0.3"));
        assert!(is_private_address("172.16.4.4"));
        assert!(is_private_address("172.31.255.1"));
        assert!(is_private_address("169.254.1.1"));
        assert!(is_private_address("::1"));
        assert!(is_private_address("localhost"));
        assert!(!is_private_address("8.8.8.8"));
        assert!(!is_private_address("172.32.0.1"));
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("192.168.1.5"));
    }
}
