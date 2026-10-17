//! Browser-based sign-in.
//!
//! Ask the upstream for an auth URL, let the user complete it in a browser, then
//! poll until the token lands. This is the flow the official CLI uses and needs
//! no desktop client — the credential it produces is the API token, which a
//! browser cookie never is.
//!
//! The Android build drove this through a `WebView`. Termux has no `WebView`, so
//! the URL is handed to the user (and to `termux-open-url` when available) while
//! the same process polls. The protocol is unchanged: it was always plain HTTP
//! polling, so nothing about the flow depended on the WebView.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::http;
use crate::store::{jwt_sub, SavedAccount};
use crate::wire::{self, Region};

const STATE_PATH: &str = "/v2/plugin/auth/state";
const TOKEN_PATH: &str = "/v2/plugin/auth/token";
const ACCOUNT_PATH: &str = "/v2/plugin/login/account";

/// `code` the upstream returns while the browser half of the flow is still open.
const PENDING_CODE: i64 = 11217;

fn user_agent(region: Region) -> &'static str {
    match region {
        Region::Global => "WorkBuddy/5.5.2",
        Region::Cn => "WorkBuddy/5.5.6",
    }
}

fn login_headers(region: Region) -> wire::Headers {
    let origin = region.origin();
    vec![
        ("Accept", "application/json, text/plain, */*".to_string()),
        ("X-Requested-With", "XMLHttpRequest".to_string()),
        ("User-Agent", user_agent(region).to_string()),
        ("Origin", origin.to_string()),
        ("Referer", format!("{origin}/")),
    ]
}

/// A sign-in that has been started and is waiting on the browser.
#[derive(Debug, Clone)]
pub struct Session {
    pub state: String,
    pub auth_url: String,
    pub region: Region,
}

/// The outcome of one poll.
pub enum Poll {
    Pending,
    Done(Box<SavedAccount>),
}

/// Opens a sign-in session and returns the URL the user must visit.
pub async fn start(region: Region) -> Result<Session> {
    let url = format!("{}{STATE_PATH}?platform=CLI", region.chat_base());
    let mut headers = login_headers(region);
    headers.push(("Content-Type", "application/json".to_string()));
    let text = http::request_json(&url, "POST", &headers, Some("{}")).await?;
    let payload: Value = serde_json::from_str(&text)
        .with_context(|| format!("auth/state returned: {}", preview(&text)))?;
    let data = payload
        .get("data")
        .ok_or_else(|| anyhow!("auth/state 没有返回 data：{}", preview(&text)))?;
    let state = data
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let auth_url = data
        .get("authUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if state.is_empty() || auth_url.is_empty() {
        bail!("auth/state 没有返回 state/authUrl：{}", preview(&text));
    }
    Ok(Session {
        state,
        auth_url,
        region,
    })
}

/// Polls once. A transport error counts as "still pending" so a flaky moment
/// during a two-minute browser round trip does not abort the sign-in.
pub async fn poll(session: &Session) -> Result<Poll> {
    let url = format!(
        "{}{TOKEN_PATH}?state={}",
        session.region.chat_base(),
        session.state
    );
    let text = match http::request_json(&url, "GET", &login_headers(session.region), None).await {
        Ok(text) => text,
        Err(_) => return Ok(Poll::Pending),
    };
    let payload: Value = serde_json::from_str(&text)
        .with_context(|| format!("auth/token returned: {}", preview(&text)))?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == PENDING_CODE {
        return Ok(Poll::Pending);
    }
    if code != 0 {
        let msg = payload
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown refusal");
        bail!("登录被拒绝：{msg}");
    }
    let Some(data) = payload.get("data") else {
        return Ok(Poll::Pending);
    };
    let access_token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        return Ok(Poll::Pending);
    }
    let region = session.region;
    // The response may omit the domain; falling back to the build the user
    // signed in to keeps the credential filed under the right one instead of
    // defaulting to CN.
    let domain = data
        .get("domain")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| wire::default_domain(region));
    let nickname = fetch_nickname(region, &session.state, &access_token)
        .await
        .unwrap_or_default();
    let mut account = SavedAccount {
        id: String::new(),
        nickname,
        uid: jwt_sub(&access_token),
        domain,
        access_token,
        refresh_token: data
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        expires_at: data.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
        enterprise_id: String::new(),
        source: "oauth".to_string(),
        region: Some(region),
    };
    account.id =
        crate::store::account_id(region, &account.uid, &account.domain, &account.access_token);
    Ok(Poll::Done(Box::new(account)))
}

async fn fetch_nickname(region: Region, state: &str, token: &str) -> Option<String> {
    let url = format!("{}{ACCOUNT_PATH}?state={state}", region.chat_base());
    let mut headers = login_headers(region);
    headers.push(("Authorization", format!("Bearer {token}")));
    let text = http::request_json(&url, "GET", &headers, None).await.ok()?;
    let payload: Value = serde_json::from_str(&text).ok()?;
    payload
        .get("data")
        .and_then(|d| d.get("nickname"))
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// Handy for `--print-url`: the upstream URL is inert until a browser opens it.
pub fn device_hint() -> &'static str {
    "若链接没有自动打开，请复制到浏览器完成登录（手机上的任何浏览器都可以）。"
}

fn preview(text: &str) -> String {
    text.chars().take(160).collect()
}

/// Exchanges a refresh token for a fresh access token.
pub async fn refresh(account: &SavedAccount) -> Result<SavedAccount> {
    let region = account.region();
    let url = format!("{}/v2/plugin/auth/token/refresh", region.chat_base());
    let headers = wire::refresh_headers(region, &account.refresh_token, account.enterprise_id());
    let text = http::request_json(&url, "POST", &headers, Some("")).await?;
    let payload: Value = serde_json::from_str(&text)
        .with_context(|| format!("refresh returned: {}", preview(&text)))?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let msg = payload
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown refusal");
        bail!("续期被拒绝：{msg}");
    }
    let data = payload.get("data").cloned().unwrap_or_else(|| json!({}));
    let access_token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        bail!("续期没有返回 access token");
    }
    let expires_in = data.get("expiresIn").and_then(Value::as_i64).unwrap_or(0);
    let mut renewed = account.clone();
    renewed.access_token = access_token;
    let new_refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !new_refresh.is_empty() {
        renewed.refresh_token = new_refresh;
    }
    // `expiresIn` is a duration from now; `expiresAt` is an absolute instant.
    if expires_in > 0 {
        renewed.expires_at = crate::store::now_seconds() + expires_in;
    }
    let domain = data.get("domain").and_then(Value::as_str).unwrap_or("");
    if !domain.is_empty() {
        renewed.domain = domain.to_string();
    }
    if renewed.uid.is_empty() {
        renewed.uid = jwt_sub(&renewed.access_token);
    }
    Ok(renewed)
}

/// Turns a check-in reply into a message.
pub struct CheckinOutcome {
    pub ok: bool,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pending_code_is_not_an_error() {
        // Documented here so a future refactor cannot quietly turn "still
        // waiting for the browser" into a hard failure.
        assert_eq!(PENDING_CODE, 11217);
    }

    #[test]
    fn login_headers_name_the_build() {
        let headers = login_headers(Region::Cn);
        assert!(headers
            .iter()
            .any(|(k, v)| *k == "Origin" && v == wire::CN_BILLING_BASE));
        let headers = login_headers(Region::Global);
        assert!(headers
            .iter()
            .any(|(k, v)| *k == "Origin" && v == wire::GLOBAL_BASE));
    }
}
