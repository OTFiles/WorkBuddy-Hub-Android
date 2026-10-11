//! On-disk credential storage.
//!
//! The Android build kept accounts in `SharedPreferences` plus a JSON file per
//! build. Termux has no `SharedPreferences`, so every piece of state lives in
//! one JSON file under `$PREFIX/var/lib/wbhub` (override with `--data-dir`).
//!
//! The file layout mirrors the Kotlin store field for field so an existing
//! `/data/data/com.wbhub.app/files/wb-auth-*.json` can be copied across
//! unchanged.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::wire::{self, Region};

/// Renew this long before the token actually expires.
const REFRESH_MARGIN_SECONDS: i64 = 5 * 60;

/// One saved account. A build can hold several of these, so identity is carried
/// explicitly rather than inferred from the slot it sits in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedAccount {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub domain: String,
    #[serde(rename = "accessToken", default)]
    pub access_token: String,
    #[serde(rename = "refreshToken", default)]
    pub refresh_token: String,
    /// Epoch **seconds**, the unit the upstream reports and a JWT `exp` uses.
    #[serde(rename = "expiresAt", default)]
    pub expires_at: i64,
    #[serde(rename = "enterpriseId", default)]
    pub enterprise_id: String,
    #[serde(default = "default_source")]
    pub source: String,
    /// Which file the account came from; not persisted by the Android build.
    #[serde(skip)]
    pub region: Option<Region>,
}

fn default_source() -> String {
    "oauth".to_string()
}

impl SavedAccount {
    pub fn region(&self) -> Region {
        self.region.unwrap_or_else(|| wire::region_of(&self.domain))
    }

    /// Display label: the nickname, falling back to a short uid.
    pub fn label(&self) -> String {
        if !self.nickname.trim().is_empty() {
            self.nickname.clone()
        } else {
            self.uid.chars().take(8).collect()
        }
    }

    pub fn enterprise_id(&self) -> Option<&str> {
        let value = self.enterprise_id.trim();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    pub fn needs_refresh(&self) -> bool {
        if self.expires_at <= 0 {
            return true;
        }
        now_seconds() + REFRESH_MARGIN_SECONDS >= self.expires_at
    }

    fn fill_missing(&mut self) {
        if self.domain.is_empty() {
            self.domain = wire::default_domain(self.region.unwrap_or(Region::Cn));
        }
        if self.expires_at <= 0 {
            if let Some(exp) = jwt_exp(&self.access_token) {
                self.expires_at = exp;
            }
        }
        if self.id.is_empty() {
            self.id = account_id(self.region(), &self.uid, &self.domain, &self.access_token);
        }
    }
}

pub fn now_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Stable id for an account. The uid identifies a person, so it is preferred; a
/// credential without one falls back to its domain plus a token digest so two
/// such accounts still get distinct ids.
pub fn account_id(region: Region, uid: &str, domain: &str, access_token: &str) -> String {
    if !uid.trim().is_empty() {
        return format!(
            "{}:{}",
            region.name(),
            uid.chars().take(8).collect::<String>()
        );
    }
    format!(
        "{}:{}:{:x}",
        region.name(),
        domain,
        fnv1a(access_token.as_bytes())
    )
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Decodes the `sub` / `exp` claims out of a JWT without verifying it. The
/// upstream issued the token, so the signature is not this side's concern; the
/// values are only used for labelling and for scheduling a refresh.
pub fn jwt_claim(token: &str, key: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let bytes = engine.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get(key).cloned()
}

pub fn jwt_exp(token: &str) -> Option<i64> {
    jwt_claim(token, "exp").and_then(|v| v.as_i64())
}

pub fn jwt_sub(token: &str) -> String {
    jwt_claim(token, "sub")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Persisted selection: which build is in use and which account within it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Selection {
    #[serde(default = "default_region")]
    #[serde(with = "region_serde")]
    active_region: Region,
    #[serde(default)]
    active_id: std::collections::BTreeMap<String, String>,
}

mod region_serde {
    use super::Region;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(region: &Region, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(region.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Region, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(Region::parse(&raw).unwrap_or(Region::Cn))
    }
}

fn default_region() -> Region {
    Region::Cn
}

impl Default for Selection {
    fn default() -> Self {
        Selection {
            active_region: Region::Cn,
            active_id: Default::default(),
        }
    }
}

/// Stores the accounts of each build and remembers which one is in use.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating data directory {}", dir.display()))?;
        Ok(Store { dir })
    }

    /// Default location: `$PREFIX/var/lib/wbhub`, or `~/.local/share/wbhub`.
    pub fn default_dir() -> PathBuf {
        if let Ok(prefix) = std::env::var("PREFIX") {
            let candidate = PathBuf::from(&prefix).join("var/lib/wbhub");
            if let Some(parent) = candidate.parent() {
                // `$PREFIX/var/lib` exists on a normal Termux install; when it
                // does not, the fallback keeps the tool usable inside a
                // container where only $HOME is writable.
                if parent.exists() {
                    return candidate;
                }
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".local/share/wbhub");
        }
        PathBuf::from(".wbhub")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The account file for one build. The name matches the Android build's so
    /// credentials can be copied across by hand.
    pub fn accounts_path(&self, region: Region) -> PathBuf {
        self.dir.join(format!("wb-auth-{}.json", region.name()))
    }

    fn selection_path(&self) -> PathBuf {
        self.dir.join("selection.json")
    }

    /// Every account saved for one build.
    pub fn accounts(&self, region: Region) -> Vec<SavedAccount> {
        let path = self.accounts_path(region);
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        // The Android build wrote an array; an even older build wrote a single
        // object at this path. Both are accepted so a copied file still loads.
        let mut accounts: Vec<SavedAccount> = match serde_json::from_str::<Vec<SavedAccount>>(&text)
        {
            Ok(list) => list,
            Err(_) => match serde_json::from_str::<SavedAccount>(&text) {
                Ok(single) => vec![single],
                Err(_) => Vec::new(),
            },
        };
        accounts.retain(|a| !a.access_token.trim().is_empty());
        for account in accounts.iter_mut() {
            account.region = Some(region);
            account.fill_missing();
        }
        accounts
    }

    /// Every account across both builds.
    pub fn all_accounts(&self) -> Vec<SavedAccount> {
        let mut all = Vec::new();
        for region in Region::all() {
            all.extend(self.accounts(region));
        }
        all
    }

    fn write_accounts(&self, region: Region, accounts: &[SavedAccount]) -> Result<()> {
        let path = self.accounts_path(region);
        let text = serde_json::to_string_pretty(accounts)?;
        write_private(&path, &text)
    }

    /// Adds a credential, or replaces the one with the same account when it is
    /// already stored. Replacing rather than duplicating means signing in again
    /// renews a token instead of creating a confusing second entry.
    pub fn save(&self, region: Region, mut account: SavedAccount) -> Result<SavedAccount> {
        account.region = Some(region);
        account.fill_missing();
        let mut current = self.accounts(region);
        match current.iter().position(|a| a.id == account.id) {
            Some(index) => current[index] = account.clone(),
            None => current.push(account.clone()),
        }
        self.write_accounts(region, &current)?;
        self.set_active(region, &account.id)?;
        Ok(account)
    }

    /// Removes one account; the selection falls back to another one.
    pub fn delete(&self, region: Region, account_id: &str) -> Result<bool> {
        let current = self.accounts(region);
        let remaining: Vec<SavedAccount> = current
            .iter()
            .filter(|a| a.id != account_id)
            .cloned()
            .collect();
        let removed = remaining.len() != current.len();
        if !removed {
            return Ok(false);
        }
        self.write_accounts(region, &remaining)?;
        let mut selection = self.selection()?;
        if selection.active_id.get(region.name()) == Some(&account_id.to_string()) {
            match remaining.first() {
                Some(next) => {
                    selection
                        .active_id
                        .insert(region.name().into(), next.id.clone());
                }
                None => {
                    selection.active_id.remove(region.name());
                }
            }
            self.write_selection(&selection)?;
        }
        Ok(true)
    }

    fn selection(&self) -> Result<Selection> {
        let path = self.selection_path();
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Selection::default());
        };
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    fn write_selection(&self, selection: &Selection) -> Result<()> {
        write_private(
            &self.selection_path(),
            &serde_json::to_string_pretty(selection)?,
        )
    }

    pub fn active_region(&self) -> Region {
        self.selection()
            .map(|s| s.active_region)
            .unwrap_or(Region::Cn)
    }

    pub fn set_active_region(&self, region: Region) -> Result<()> {
        let mut selection = self.selection()?;
        selection.active_region = region;
        self.write_selection(&selection)
    }

    pub fn set_active(&self, region: Region, account_id: &str) -> Result<()> {
        let mut selection = self.selection()?;
        selection
            .active_id
            .insert(region.name().into(), account_id.to_string());
        self.write_selection(&selection)
    }

    /// Selects a saved account by id, or by nickname/uid prefix when the id is
    /// not given verbatim.
    pub fn select(&self, region: Region, needle: &str) -> Result<SavedAccount> {
        let accounts = self.accounts(region);
        let found = accounts
            .iter()
            .find(|a| a.id == needle)
            .or_else(|| accounts.iter().find(|a| a.nickname == needle))
            .or_else(|| accounts.iter().find(|a| a.id.starts_with(needle)))
            .or_else(|| accounts.iter().find(|a| a.uid.starts_with(needle)))
            .ok_or_else(|| anyhow!("no account in {} matches {needle:?}", region.label()))?;
        self.set_active(region, &found.id)?;
        Ok(found.clone())
    }

    /// The account in use for a build, or `None` when that build holds none.
    pub fn active_account(&self, region: Region) -> Option<SavedAccount> {
        let list = self.accounts(region);
        if list.is_empty() {
            return None;
        }
        let selected = self.selection().ok()?.active_id.get(region.name()).cloned();
        match selected {
            Some(id) => list
                .iter()
                .find(|a| a.id == id)
                .cloned()
                .or_else(|| list.first().cloned()),
            None => list.first().cloned(),
        }
    }

    /// The account in use for the active build, falling back to whichever build
    /// actually has one so a single-account install needs no switching.
    pub fn active(&self) -> Option<SavedAccount> {
        let region = self.active_region();
        if let Some(account) = self.active_account(region) {
            return Some(account);
        }
        Region::all()
            .into_iter()
            .find_map(|r| self.active_account(r))
    }

    /// Writes a refreshed token back to the account it came from, so a switch
    /// back does not serve a stale token.
    pub fn update_tokens(&self, account: &SavedAccount, renewed: &SavedAccount) -> Result<()> {
        let region = account.region();
        let updated: Vec<SavedAccount> = self
            .accounts(region)
            .into_iter()
            .map(|mut a| {
                if a.id == account.id {
                    a.access_token = renewed.access_token.clone();
                    a.refresh_token = renewed.refresh_token.clone();
                    a.expires_at = renewed.expires_at;
                    if !renewed.domain.is_empty() {
                        a.domain = renewed.domain.clone();
                    }
                }
                a
            })
            .collect();
        self.write_accounts(region, &updated)
    }

    pub fn has_any(&self) -> bool {
        Region::all()
            .into_iter()
            .any(|r| !self.accounts(r).is_empty())
    }
}

/// Writes a file with owner-only permissions, creating parent directories.
///
/// The file holds bearer tokens: a world-readable copy would let any other app
/// on the device drive the account.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, text).with_context(|| format!("writing {}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(uid: &str, region: Region, domain: &str) -> SavedAccount {
        SavedAccount {
            id: String::new(),
            nickname: format!("nick-{uid}"),
            uid: uid.to_string(),
            domain: domain.to_string(),
            access_token: "a.b.c".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: 0,
            enterprise_id: String::new(),
            source: "oauth".to_string(),
            region: Some(region),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wbhub-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saving_twice_replaces_rather_than_duplicates() {
        let dir = scratch("save-twice");
        let store = Store::open(&dir).unwrap();
        store
            .save(Region::Cn, account("u1", Region::Cn, "www.codebuddy.cn"))
            .unwrap();
        let mut again = account("u1", Region::Cn, "www.codebuddy.cn");
        again.access_token = "new-token".into();
        store.save(Region::Cn, again).unwrap();
        let list = store.accounts(Region::Cn);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].access_token, "new-token");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn accounts_are_isolated_per_build() {
        let dir = scratch("isolate");
        let store = Store::open(&dir).unwrap();
        store
            .save(Region::Cn, account("cn", Region::Cn, "www.codebuddy.cn"))
            .unwrap();
        store
            .save(
                Region::Global,
                account("gb", Region::Global, "www.workbuddy.ai"),
            )
            .unwrap();
        assert_eq!(store.accounts(Region::Cn).len(), 1);
        assert_eq!(store.accounts(Region::Global).len(), 1);
        assert_eq!(store.accounts(Region::Cn)[0].uid, "cn");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_falls_back_to_a_build_that_has_an_account() {
        let dir = scratch("fallback");
        let store = Store::open(&dir).unwrap();
        // Selection defaults to CN, which holds nothing.
        store
            .save(
                Region::Global,
                account("gb", Region::Global, "www.workbuddy.ai"),
            )
            .unwrap();
        assert_eq!(store.active_region(), Region::Cn);
        assert_eq!(store.active().unwrap().uid, "gb");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_single_object_files_still_load() {
        let dir = scratch("legacy");
        let store = Store::open(&dir).unwrap();
        let legacy =
            r#"{"accessToken":"tok","refreshToken":"ref","uid":"u9","domain":"www.codebuddy.cn"}"#;
        fs::write(store.accounts_path(Region::Cn), legacy).unwrap();
        let list = store.accounts(Region::Cn);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].uid, "u9");
        assert_eq!(
            list[0].id,
            account_id(Region::Cn, "u9", "www.codebuddy.cn", "tok")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_the_active_account_promotes_another() {
        let dir = scratch("delete");
        let store = Store::open(&dir).unwrap();
        store
            .save(Region::Cn, account("u1", Region::Cn, "www.codebuddy.cn"))
            .unwrap();
        store
            .save(Region::Cn, account("u2", Region::Cn, "www.codebuddy.cn"))
            .unwrap();
        let kept = store.accounts(Region::Cn)[0].id.clone();
        store.delete(Region::Cn, &kept).unwrap();
        let list = store.accounts(Region::Cn);
        assert_eq!(list.len(), 1);
        assert_eq!(store.active_account(Region::Cn).unwrap().id, list[0].id);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tokens_are_written_back_to_the_right_account() {
        let dir = scratch("update");
        let store = Store::open(&dir).unwrap();
        store
            .save(Region::Cn, account("u1", Region::Cn, "www.codebuddy.cn"))
            .unwrap();
        store
            .save(Region::Cn, account("u2", Region::Cn, "www.codebuddy.cn"))
            .unwrap();
        let first = store.accounts(Region::Cn)[0].clone();
        let mut renewed = first.clone();
        renewed.access_token = "fresh".into();
        renewed.expires_at = 12345;
        store.update_tokens(&first, &renewed).unwrap();
        let after = store.accounts(Region::Cn);
        let updated = after.iter().find(|a| a.id == first.id).unwrap();
        let untouched = after.iter().find(|a| a.id != first.id).unwrap();
        assert_eq!(updated.access_token, "fresh");
        assert_eq!(updated.expires_at, 12345);
        assert_eq!(untouched.access_token, "a.b.c");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jwt_claims_are_read_without_verification() {
        use base64::Engine;
        let payload = serde_json::json!({"sub": "user-42", "exp": 4102444800_i64});
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("header.{encoded}.signature");
        assert_eq!(jwt_sub(&token), "user-42");
        assert_eq!(jwt_exp(&token), Some(4102444800));
        assert_eq!(jwt_sub("not-a-jwt"), "");
    }

    #[test]
    fn expired_credentials_ask_to_be_refreshed() {
        let mut a = account("u", Region::Cn, "www.codebuddy.cn");
        a.expires_at = now_seconds() - 1;
        assert!(a.needs_refresh());
        a.expires_at = now_seconds() + 3600;
        assert!(!a.needs_refresh());
        a.expires_at = 0;
        assert!(a.needs_refresh());
    }
}
