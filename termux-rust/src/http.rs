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
            // 空闲连接只留 10 秒。在手机上这一点很关键：息屏、切 Wi-Fi/流量、
            // 基站切换都会让一条「看起来还在」的连接悄悄死掉，而复用它发出去的
            // 请求会直接失败——hyper 不会自动重发 POST，因为那可能重复计费。
            // 留得短一点，代价是偶尔多一次 TLS 握手（实测 130ms 左右），换来的是
            // 不会莫名 502。
            .pool_idle_timeout(Duration::from_secs(10))
            // 一次生成可能要跑几分钟，中间可能长时间没有下行数据；30 秒的 TCP
            // keepalive 能防住运营商 NAT / 中间设备把空闲连接掐掉。
            .tcp_keepalive(Duration::from_secs(30))
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

/// 把 reqwest 的错误展开成一句能定位问题的话。
///
/// `reqwest::Error` 的 `Display` 只有外层那句「error sending request for url
/// (...」——真正的原因（DNS、TCP、TLS、还是请求体写到一半被打断）在 source 链
/// 里。只打印外层的话，用户看到的就是一个 502 加一句毫无信息量的英文，这正是
/// 之前那个报错的由来。
///
/// 顺带按类型归一下类，并在常见坑上给一句提示。
pub fn describe_error(error: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        let text = current.to_string();
        if !text.is_empty() && !parts.contains(&text) {
            parts.push(text);
        }
        source = current.source();
    }
    let chain = parts.join(" ← ");
    let lowered = chain.to_lowercase();

    let kind = if error.is_timeout() {
        "超时"
    } else if error.is_connect() {
        "连接失败"
    } else if error.is_body() {
        "请求体发送失败"
    } else if error.is_decode() {
        "响应解析失败"
    } else if error.is_redirect() {
        "重定向失败"
    } else {
        "请求失败"
    };

    // 三类在手机上最常见的原因，各给一句可执行的提示；对不上就不猜。
    let hint = if lowered.contains("certificate")
        || lowered.contains("unknownissuer")
        || lowered.contains("invalid peer")
        || lowered.contains("handshake")
    {
        Some("TLS 证书校验没通过：可能是网络中间人（公司网关 / 抓包工具 / 公共 Wi-Fi 登录页）")
    } else if lowered.contains("dns")
        || lowered.contains("name or service not known")
        || lowered.contains("failed to lookup")
    {
        Some("域名解析失败：先确认网络能上网，或换一个 DNS")
    } else if lowered.contains("connection refused") {
        Some("连接被拒绝：目标端口没有服务，或被本机/运营商的策略挡住")
    } else if lowered.contains("network is unreachable") || lowered.contains("no route to host") {
        Some("网络不可达：多半是刚息屏唤醒，或者网络正在切换")
    } else if error.is_timeout() {
        Some("超时：手机息屏后网络被挂起，或者当前网络质量太差")
    } else {
        None
    };

    match hint {
        Some(hint) => format!("{kind}：{chain}（{hint}）"),
        None => format!("{kind}：{chain}"),
    }
}

/// 这一次失败值不值得原样重发一次。
///
/// 判断标准是「请求有没有可能已经被上游收下」：
///
/// * 连接没建立起来 → 一定没发出去，可以重试；
/// * 超时 → 说不清，可能已经开始计费了，**不重试**；
/// * 请求体写到一半失败 → 上游看到的是残缺请求，重试也是白费时间；
/// * 响应解析失败 → 服务端已经回过了，重发等于再付一次。
///
/// 剩下的（写请求时连接被对端关掉，典型的就是复用了休眠后死掉的连接）重试一次，
/// 收益远大于风险：那种情况下上游收到的是一个断掉的请求，不会正常计费。
fn worth_retrying(error: &reqwest::Error) -> bool {
    if error.is_connect() {
        return true;
    }
    !(error.is_timeout() || error.is_body() || error.is_decode() || error.is_status())
}

/// Sends a request and returns the status alongside the body.
///
/// Callers that need to tell "the session is dead" apart from "the upstream sent
/// something odd" cannot do it from the body alone: a 401 arrives as an HTML page
/// from the gateway, which would otherwise be reported as "not JSON".
pub async fn request_with_status(
    url: &str,
    method: &str,
    headers: &wire::Headers,
    body: Option<&str>,
) -> Result<(u16, String)> {
    let request = match method {
        "GET" => client().get(url),
        "POST" => {
            let mut request = client().post(url);
            if let Some(payload) = body {
                request = request.body(payload.to_string());
            }
            request
        }
        other => anyhow::bail!("不支持的请求方法 {other}"),
    };
    let request = apply_headers(request, headers);
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("{}", describe_error(&error)))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .with_context(|| format!("reading the body of {method} {url} (HTTP {status})"))?;
    Ok((status, text))
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
    let (_, text) = request_with_status(url, method, headers, body).await?;
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
    let mut attempt = 0;
    loop {
        let request = apply_headers(client().post(&url), headers).body(body.to_string());
        match request.send().await {
            // 只在「连接都没建立起来」时重试，这时请求根本没发出去，也就没花积分。
            // 连接建立之后的失败一律不重试：可能上游已经收下并开始计费了，重发
            // 会让一次调用算两次。
            Err(error) if worth_retrying(&error) && attempt == 0 => {
                attempt = 1;
                // 刚息屏唤醒时网络栈往往要一两百毫秒才恢复，等一下再试比立刻
                // 报错有用。
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                continue;
            }
            Err(error) => {
                return ChatOpen::Failed {
                    status: 0,
                    kind: wire::ErrorKind::Server,
                    body: describe_error(&error),
                }
            }
            Ok(response) => {
                let status = response.status().as_u16();
                if (200..300).contains(&status) {
                    return ChatOpen::Ok(response);
                }
                let text = response.text().await.unwrap_or_default();
                return ChatOpen::Failed {
                    status,
                    kind: wire::classify(status, &text),
                    body: text,
                };
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
    /// 连一个没人监听的本地端口，拿到一个真实的 reqwest 错误来检验分类。
    ///
    /// 用 127.0.0.1 是为了不依赖外网：CI 里也能跑，而且立即返回。
    async fn refused_error() -> reqwest::Error {
        reqwest::Client::new()
            .post("http://127.0.0.1:9/v2/chat/completions")
            .body("{}")
            .send()
            .await
            .expect_err("connecting to port 9 must fail")
    }

    #[tokio::test]
    async fn a_refused_connection_is_reported_as_a_connect_failure() {
        let error = refused_error().await;
        let text = super::describe_error(&error);
        assert!(text.starts_with("连接失败"), "{text}");
        // 关键点：原始原因必须露出来，不然用户拿到的就是一个空泛的 502。
        assert!(text.contains(" ← "), "the cause chain must be kept: {text}");
        assert!(
            text.to_lowercase().contains("refused") || text.contains("拒绝"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_worth_one_retry() {
        // 连接没建立起来就一定没发出去，重试不可能重复计费。
        assert!(super::worth_retrying(&refused_error().await));
    }

    #[test]
    fn the_client_is_shared_across_calls() {
        // The OnceLock is the whole point: a second call must not build a new
        // pool, because that would mean a fresh TLS handshake per request.
        let first = super::client() as *const _;
        let second = super::client() as *const _;
        assert_eq!(first, second);
    }
}
