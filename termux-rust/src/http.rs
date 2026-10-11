//! A thin HTTP client shared by every upstream call.
//!
//! One client is built once and cloned per request, so the connection pool and
//! the TLS session cache are reused across a chat stream and the calls that
//! follow it — which matters on a phone, where a fresh TLS handshake to the
//! upstream is the single largest fixed cost of a request.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Response;

use crate::wire;

static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent(wire::CLIENT_UA)
            .build()
            .expect("building the shared HTTP client")
    })
}

fn apply_headers(
    mut request: reqwest::RequestBuilder,
    headers: &wire::Headers,
) -> reqwest::RequestBuilder {
    for (key, value) in headers {
        request = request.header(*key, value);
    }
    request
}

/// Sends a request and returns the body as text, including non-2xx bodies:
/// the upstream reports most business failures in prose inside a 200 or a 4xx
/// envelope, and both are classified from the body rather than the status alone.
pub async fn request_json(
    url: &str,
    method: &str,
    headers: &wire::Headers,
    body: Option<&str>,
) -> Result<String> {
    let request = match method {
        "GET" => client().get(url),
        "POST" => {
            let mut request = client().post(url);
            if let Some(payload) = body {
                request = request.body(payload.to_string());
            }
            request
        }
        other => anyhow::bail!("unsupported method {other}"),
    };
    let request = apply_headers(request, headers);
    let response = request
        .send()
        .await
        .with_context(|| format!("{method} {url}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .with_context(|| format!("reading the body of {method} {url} (HTTP {status})"))?;
    Ok(text)
}

/// Opens a chat request for streaming. Errors carry the classification the
/// bridge maps to an HTTP status, so the caller never has to guess.
pub enum ChatOpen {
    Ok(Response),
    Failed {
        status: u16,
        kind: wire::ErrorKind,
        body: String,
    },
}

pub async fn open_chat(region: wire::Region, headers: &wire::Headers, body: &str) -> ChatOpen {
    let url = format!("{}/v2/chat/completions", region.chat_base());
    // No overall timeout: a long generation is the expected case, and the
    // client already bounds the connect phase.
    let request = apply_headers(client().post(&url), headers).body(body.to_string());
    match request.send().await {
        Err(error) => ChatOpen::Failed {
            status: 0,
            kind: wire::ErrorKind::Server,
            body: format!("transport error: {error}"),
        },
        Ok(response) => {
            let status = response.status().as_u16();
            if (200..300).contains(&status) {
                return ChatOpen::Ok(response);
            }
            let text = response.text().await.unwrap_or_default();
            ChatOpen::Failed {
                status,
                kind: wire::classify(status, &text),
                body: text,
            }
        }
    }
}

/// Sends a request whose body is JSON and returns the parsed value.
pub async fn request_value(
    url: &str,
    method: &str,
    headers: &wire::Headers,
    body: Option<&str>,
) -> Result<serde_json::Value> {
    let text = request_json(url, method, headers, body).await?;
    serde_json::from_str(&text).with_context(|| {
        format!(
            "upstream returned non-JSON: {}",
            text.chars().take(160).collect::<String>()
        )
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_client_is_shared_across_calls() {
        // The OnceLock is the whole point: a second call must not build a new
        // pool, because that would mean a fresh TLS handshake per request.
        let first = super::client() as *const _;
        let second = super::client() as *const _;
        assert_eq!(first, second);
    }
}
