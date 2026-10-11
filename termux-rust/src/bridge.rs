//! A small OpenAI-compatible endpoint on loopback.
//!
//! Two things about the original are load-bearing and are preserved here:
//!
//! * the upstream always streams, so a non-streaming client is served by
//!   relaying the same bytes and letting the client's own parser see frames it
//!   may not expect — the alternative (buffering) would stall the caller until
//!   the whole answer was generated;
//! * the usage block arrives in a trailing SSE frame, so the stream is scanned
//!   as it is forwarded: the same bytes go to the client, and the last complete
//!   usage object seen is what the call gets billed for.
//!
//! Only loopback is served by default and every request must present the shared
//! secret, because the endpoint holds a credential.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};

use crate::calllog::{CallLog, CallRecord};
use crate::http::ChatOpen;
use crate::settings::{is_loopback_host, is_private_address, Settings};
use crate::store::{SavedAccount, Store};
use crate::upstream::{self, Catalogue};
use crate::wire;

/// Upper bound on a request body, so one client cannot exhaust memory.
const REQUEST_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// How much of the stream tail is kept for usage scanning.
const USAGE_SCAN_BYTES: usize = 8192;

/// Live state shared by every connection.
pub struct Bridge {
    pub store: Store,
    pub settings: RwLock<Settings>,
    pub catalogue: RwLock<Catalogue>,
    pub calls: CallLog,
    pub status: Mutex<Status>,
    /// Serialises token renewal so ten concurrent requests about to cross the
    /// expiry margin produce one refresh call rather than ten.
    refresh_lock: Mutex<()>,
}

#[derive(Debug, Default, Clone)]
pub struct Status {
    pub started_at: Option<Instant>,
    pub requests: u64,
    pub remote_requests: u64,
    pub last_request: String,
    pub last_error: String,
    pub last_call_at: Option<i64>,
}

impl Bridge {
    pub async fn new(store: Store, settings: Settings) -> Arc<Bridge> {
        let limit = settings.call_log_limit;
        let calls = CallLog::open(store.dir(), limit);
        Arc::new(Bridge {
            catalogue: RwLock::new(Catalogue { models: Vec::new() }),
            settings: RwLock::new(settings),
            calls,
            store,
            status: Mutex::new(Status::default()),
            refresh_lock: Mutex::new(()),
        })
    }

    /// The account to serve a request with, renewing the token first when it is
    /// inside the refresh margin.
    ///
    /// A failed renewal returns the existing credential rather than nothing:
    /// the token may still be valid for minutes, and refusing to serve would
    /// turn a flaky refresh endpoint into a bridge outage.
    pub async fn credential(&self) -> Option<SavedAccount> {
        let account = self.store.active()?;
        if !account.needs_refresh() {
            return Some(account);
        }
        let _guard = self.refresh_lock.lock().await;
        // Re-read under the lock: another request may have just renewed it.
        let account = self.store.active()?;
        if !account.needs_refresh() {
            return Some(account);
        }
        match crate::login::refresh(&account).await {
            Ok(renewed) => {
                let _ = self.store.update_tokens(&account, &renewed);
                Some(renewed)
            }
            Err(error) => {
                if self.settings.read().await.verbose {
                    eprintln!("token 刷新失败，继续使用现有 token：{error}");
                }
                Some(account)
            }
        }
    }

    /// Refreshes the catalogue from the upstream, keeping the previous list when
    /// the call fails so a transient outage does not empty `/v1/models`.
    pub async fn refresh_catalogue(&self) -> anyhow::Result<usize> {
        let Some(account) = self.credential().await else {
            return Ok(0);
        };
        let models = upstream::fetch_models(&account).await?;
        let count = models.len();
        *self.catalogue.write().await = Catalogue { models };
        Ok(count)
    }

    pub async fn model_ids(&self) -> Vec<String> {
        self.catalogue.read().await.ids()
    }

    pub async fn multiplier_of(&self, model: &str) -> f64 {
        self.catalogue.read().await.multiplier_of(model)
    }
}

/// Token and credit figures reported by the upstream for one call.
#[derive(Debug, Default, Clone, Copy)]
struct SseUsage {
    prompt: i64,
    completion: i64,
    credits: f64,
}

/// A response body that does not promise `Sync`.
///
/// The relay body wraps a live `reqwest` stream, which is `Send` but not
/// `Sync`; hyper only needs the former, so the stricter box would be a
/// requirement nothing actually makes use of.
type Body = UnsyncBoxBody<Bytes, std::io::Error>;

fn full_body(bytes: Bytes) -> Body {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn json_response(code: StatusCode, payload: Value) -> Response<Body> {
    let bytes = Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());
    Response::builder()
        .status(code)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Content-Length", bytes.len())
        .header("Connection", "close")
        .body(full_body(bytes))
        .expect("building a JSON response")
}

fn error_response(code: u16, message: &str, kind: &str) -> Response<Body> {
    json_response(
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        json!({"error": {"message": message, "type": kind}}),
    )
}

/// Serves until the listener is closed or the process is signalled.
pub async fn serve(bridge: Arc<Bridge>, listener: TcpListener) -> anyhow::Result<()> {
    bridge.status.lock().await.started_at = Some(Instant::now());
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                // A transient accept failure (a file-descriptor shortage, a
                // client that vanished mid-handshake) must not stop the bridge.
                if bridge.settings.read().await.verbose {
                    eprintln!("accept failed: {error}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let bridge = Arc::clone(&bridge);
                async move { handle(bridge, request, peer).await }
            });
            let connection = http1::Builder::new()
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service);
            if let Err(error) = connection.await {
                // A client that hangs up mid-response reports here; it is not a
                // bridge failure worth surfacing at normal verbosity.
                let _ = error;
            }
        });
    }
}

async fn handle(
    bridge: Arc<Bridge>,
    request: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<Body>, std::convert::Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let from_loopback = peer.ip().is_loopback();
    let settings = bridge.settings.read().await.clone();

    {
        let mut status = bridge.status.lock().await;
        status.requests += 1;
        status.last_request = format!("{method} {path}");
        if !from_loopback {
            status.remote_requests += 1;
        }
    }
    if settings.verbose {
        eprintln!("{} {method} {path}", now_hms());
    }

    if method == Method::GET && (path == "/healthz" || path == "/health") {
        // Liveness stays unauthenticated so a supervisor (tmux, a watchdog
        // script) can check it without holding the bridge key.
        return Ok(health(&bridge).await);
    }

    if let Err(response) = authorize(&bridge, &request, &settings, from_loopback) {
        return Ok(*response);
    }
    if let Err(response) = inbound_guard(&request, &settings) {
        return Ok(*response);
    }

    let response = match (method.clone(), path.as_str()) {
        (Method::GET, "/v1/models") => models(&bridge).await,
        (Method::POST, "/v1/chat/completions") => {
            chat(bridge.clone(), request, peer, from_loopback).await
        }
        (Method::GET, "/") => json_response(
            StatusCode::OK,
            json!({
                "name": "wbhub",
                "endpoints": ["/v1/models", "/v1/chat/completions", "/healthz"],
                "base_url": format!("http://127.0.0.1:{}/v1", settings.port),
            }),
        ),
        _ => error_response(404, "not found", "not_found"),
    };
    Ok(response)
}

async fn health(bridge: &Arc<Bridge>) -> Response<Body> {
    let account = bridge.store.active();
    let region = account
        .as_ref()
        .map(|a| a.region().name())
        .unwrap_or("none");
    let (requests, remote, uptime) = {
        let status = bridge.status.lock().await;
        (
            status.requests,
            status.remote_requests,
            status
                .started_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0),
        )
    };
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "region": region,
            "account": account.as_ref().map(|a| a.label()),
            "models": bridge.model_ids().await.len(),
            "requests": requests,
            "remote_requests": remote,
            "uptime_seconds": uptime,
        }),
    )
}

/// Checks the shared secret, distinguishing the local key from the LAN key.
///
/// Which key is expected depends on where the request came from: a peer on the
/// network authenticates with the LAN key, this device with its own. Keeping
/// them apart means a leaked LAN key can be rotated without touching the local
/// clients' configuration.
fn authorize(
    bridge: &Arc<Bridge>,
    request: &Request<Incoming>,
    settings: &Settings,
    from_loopback: bool,
) -> Result<(), Box<Response<Body>>> {
    let _ = bridge;
    let header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    let presented = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .unwrap_or(header)
        .trim()
        .to_string();
    let expected = if from_loopback {
        Some(settings.api_key.as_str())
    } else if settings.lan_enabled {
        Some(settings.lan_key.as_str())
    } else {
        None
    };
    match expected {
        Some(key) if !key.is_empty() && presented == key => Ok(()),
        _ => Err(Box::new(error_response(
            401,
            "invalid api key",
            "unauthorized",
        ))),
    }
}

/// Inbound hardening.
///
/// The bind address alone is not a trust boundary: any local process, or a page
/// that re-resolves its own domain to the device's address, can reach the port.
/// A browser-originated request must therefore name a host this server actually
/// answers on, and a body must be JSON.
fn inbound_guard(
    request: &Request<Incoming>,
    settings: &Settings,
) -> Result<(), Box<Response<Body>>> {
    let host = request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_is_allowed(host, settings.lan_enabled) {
        return Err(Box::new(error_response(
            403,
            "host not allowed",
            "host_not_allowed",
        )));
    }
    if let Some(origin) = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
    {
        if !origin.is_empty() && !origin_is_allowed(origin, settings.lan_enabled) {
            return Err(Box::new(error_response(
                403,
                "origin not allowed",
                "origin_not_allowed",
            )));
        }
    }
    Ok(())
}

/// Strips an optional port, IPv6-bracket aware.
fn hostname_of(host: &str) -> String {
    let name = host.trim().to_ascii_lowercase();
    if name.starts_with('[') {
        return match name.find(']') {
            Some(end) => name[..=end].to_string(),
            None => name,
        };
    }
    match name.rsplit_once(':') {
        Some((head, tail)) if !head.contains(':') && tail.chars().all(|c| c.is_ascii_digit()) => {
            head.to_string()
        }
        _ => name,
    }
}

/// Whether a Host header names something this server answers on.
///
/// Loopback always passes. With LAN access on, a peer addresses the device by an
/// address it actually holds or by a name resolving to one, so those pass too;
/// an arbitrary name still does not.
fn host_is_allowed(host: &str, lan_enabled: bool) -> bool {
    if host.is_empty() {
        return false;
    }
    let name = hostname_of(host);
    if is_loopback_host(&name) {
        return true;
    }
    if !lan_enabled {
        return false;
    }
    is_private_address(&name) || local_addresses().iter().any(|a| a == &name)
}

/// Whether a browser-sent Origin is allowed; an absent Origin passes.
fn origin_is_allowed(origin: &str, lan_enabled: bool) -> bool {
    let Some(host) = origin_host(origin) else {
        return false;
    };
    let name = hostname_of(&host);
    if is_loopback_host(&name) {
        return true;
    }
    lan_enabled && is_private_address(&name)
}

fn origin_host(origin: &str) -> Option<String> {
    let (_, rest) = origin.split_once("://")?;
    let authority = rest.split('/').next()?;
    // Strip userinfo, which is legal in a URL but never present in an Origin.
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_string())
    }
}

/// Addresses this device holds, so a LAN peer can address it by its own IP.
fn local_addresses() -> Vec<String> {
    let mut addresses = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        // Connecting a UDP socket sends nothing; it only asks the routing table
        // which local address would be used, which is exactly the address a peer
        // on the same network will see.
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                addresses.push(addr.ip().to_string());
            }
        }
    }
    addresses
}

async fn models(bridge: &Arc<Bridge>) -> Response<Body> {
    let ids = bridge.model_ids().await;
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "workbuddy"}))
        .collect();
    json_response(StatusCode::OK, json!({"object": "list", "data": data}))
}

async fn chat(
    bridge: Arc<Bridge>,
    request: Request<Incoming>,
    peer: SocketAddr,
    from_loopback: bool,
) -> Response<Body> {
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    // The upstream rejects non-JSON, and allowing form posts would let a simple
    // cross-site request drive a paid model call.
    if !content_type.starts_with("application/json") {
        return error_response(
            415,
            "Content-Type must be application/json",
            "unsupported_media_type",
        );
    }

    let declared = request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if declared > REQUEST_BODY_LIMIT {
        return error_response(413, "request too large", "too_large");
    }

    let body = match request.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return error_response(400, "invalid json", "bad_request"),
    };
    if body.len() > REQUEST_BODY_LIMIT {
        return error_response(413, "request too large", "too_large");
    }
    let body = match String::from_utf8(body.to_vec()) {
        Ok(text) => text,
        Err(_) => return error_response(400, "invalid json", "bad_request"),
    };
    if serde_json::from_str::<Value>(&body).is_err() {
        return error_response(400, "invalid json", "bad_request");
    }

    let Some(account) = bridge.credential().await else {
        return error_response(401, "no credential", "not_signed_in");
    };

    let model = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    let started = now_millis();
    let source_ip = if from_loopback {
        String::new()
    } else {
        unmapped(peer.ip())
    };

    match upstream::chat_stream(&account, &body).await {
        ChatOpen::Failed {
            status,
            kind,
            body: error_body,
        } => {
            let detail = wire::display_error(&error_body)
                .unwrap_or_else(|| error_body.chars().take(200).collect());
            {
                let mut live = bridge.status.lock().await;
                live.last_error = format!("{}: {detail}", kind.as_str());
            }
            let mut record =
                CallRecord::failed(started, &model, &format!("{}: {detail}", kind.as_str()));
            record.source_ip = source_ip;
            record.account_id = account.id.clone();
            record.account_label = account.label();
            record.multiplier = bridge.multiplier_of(&model).await;
            bridge.calls.append(&record);
            // A 401 keeps its status note so the caller can classify it as auth;
            // every other failure speaks through the message alone, because a
            // bare status word would mask a business error.
            let note = if status == 401 { " (http 401)" } else { "" };
            error_response(
                kind.status(),
                &format!("workbuddy {}{note}: {detail}", kind.as_str()),
                kind.as_str(),
            )
        }
        ChatOpen::Ok(response) => {
            let multiplier = bridge.multiplier_of(&model).await;
            relay(
                bridge,
                response,
                model,
                started,
                source_ip,
                account.id.clone(),
                account.label(),
                multiplier,
            )
            .await
        }
    }
}

/// Streams the upstream answer through unchanged while scanning it for usage.
#[allow(clippy::too_many_arguments)]
/// Mutable state the relay carries between chunks: the accumulated tail used
/// for usage scanning, and the record that is written once the stream ends.
struct RelayState {
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>,
    tail: String,
    usage: SseUsage,
    record: CallRecord,
    finished: bool,
}

#[allow(clippy::too_many_arguments)]
async fn relay(
    bridge: Arc<Bridge>,
    upstream_response: reqwest::Response,
    model: String,
    started: i64,
    source_ip: String,
    account_id: String,
    account_label: String,
    multiplier: f64,
) -> Response<Body> {
    let state = RelayState {
        stream: Box::pin(upstream_response.bytes_stream()),
        tail: String::new(),
        usage: SseUsage::default(),
        record: CallRecord {
            timestamp: started,
            model,
            outcome: "ok".into(),
            detail: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            credits: 0.0,
            source_ip,
            account_id,
            account_label,
            multiplier,
        },
        finished: false,
    };

    let relayed = futures_util::stream::unfold((state, bridge), |(mut state, bridge)| async move {
        if state.finished {
            return None;
        }
        match state.stream.next().await {
            Some(Ok(bytes)) => {
                state.tail.push_str(&String::from_utf8_lossy(&bytes));
                if state.tail.len() > USAGE_SCAN_BYTES {
                    let cut = state.tail.len() - USAGE_SCAN_BYTES;
                    let cut = state
                        .tail
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|i| *i >= cut)
                        .unwrap_or(state.tail.len());
                    state.tail.drain(..cut);
                }
                if let Some(parsed) = parse_usage(&state.tail) {
                    state.usage = parsed;
                }
                Some((Ok::<Bytes, std::io::Error>(bytes), (state, bridge)))
            }
            Some(Err(error)) => {
                // The upstream stream broke mid-answer. The client already has
                // the earlier frames, so the stream is ended rather than replaced
                // with an error body it cannot parse.
                if bridge.settings.read().await.verbose {
                    eprintln!("chat relay interrupted: {error}");
                }
                finish_relay(&bridge, &mut state).await;
                None
            }
            None => {
                finish_relay(&bridge, &mut state).await;
                None
            }
        }
    });

    let stream_body =
        StreamBody::new(relayed.map(|chunk: Result<Bytes, std::io::Error>| chunk.map(Frame::data)));
    let body: Body = BodyExt::boxed_unsync(stream_body);

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .header("Connection", "close")
        .body(body)
        .expect("building the SSE response")
}

/// Reads the newest usage block out of the accumulated stream tail.
///
/// Scanned rather than accumulated per frame because the block is a plain JSON
/// object inside SSE data lines, and only the final one carries the totals.
fn parse_usage(text: &str) -> Option<SseUsage> {
    let index = text.rfind("\"usage\"")?;
    let open = text[index..].find('{')? + index;
    // The usage object is nested (it carries a token-details member), so the end
    // is found by brace depth rather than by the first closing brace.
    let mut depth = 0usize;
    let mut end = None;
    for (offset, ch) in text[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    let usage: Value = serde_json::from_str(&text[open..=end]).ok()?;
    Some(SseUsage {
        prompt: usage
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        completion: usage
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        // `credit` normally rides inside the usage block, but the upstream has
        // also been seen pairing `usage` with a sibling `credit` on the same
        // frame. A direct scan of the bytes before the block covers both, rather
        // than reporting a confident zero for one of them.
        credits: usage
            .get("credit")
            .and_then(Value::as_f64)
            .or_else(|| last_credit(&text[..index]))
            .unwrap_or(0.0),
    })
}

/// The last numeric `"credit"` value before a given position, if any.
fn last_credit(text: &str) -> Option<f64> {
    let index = text.rfind("\"credit\"")?;
    let rest = &text[index + "\"credit\"".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| {
            !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == 'E' || c == '+')
        })
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Records the completed call once the stream has ended, for either outcome.
async fn finish_relay(bridge: &Arc<Bridge>, state: &mut RelayState) {
    state.finished = true;
    state.record.prompt_tokens = state.usage.prompt;
    state.record.completion_tokens = state.usage.completion;
    state.record.credits = state.usage.credits;
    bridge.calls.append(&state.record);
    bridge.status.lock().await.last_call_at = Some(now_millis());
}

fn unmapped(address: IpAddr) -> String {
    match address {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(),
        },
        IpAddr::V4(v4) => v4.to_string(),
    }
}

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_read_from_a_nested_trailing_frame() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":34,",
            "\"total_tokens\":46,\"prompt_tokens_details\":{\"cached\":0},\"credit\":0.25}}\n\n",
            "data: [DONE]\n\n"
        );
        let usage = parse_usage(stream).expect("usage found");
        assert_eq!(usage.prompt, 12);
        assert_eq!(usage.completion, 34);
        assert_eq!(usage.credits, 0.25);
    }

    #[test]
    fn a_credit_sibling_of_usage_is_still_picked_up() {
        // The upstream has been seen in both shapes; reporting 0 for one of them
        // would be a confident wrong number rather than a visible failure.
        let stream = concat!(
            "data: {\"choices\":[],\"credit\":0.75,\"usage\":{\"prompt_tokens\":3,",
            "\"completion_tokens\":4}}\n\n"
        );
        let usage = parse_usage(stream).unwrap();
        assert_eq!(usage.prompt, 3);
        assert_eq!(usage.credits, 0.75);
    }

    #[test]
    fn the_last_usage_block_wins() {
        let stream = concat!(
            "{\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n",
            "{\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":9,\"credit\":1.5}}\n"
        );
        let usage = parse_usage(stream).unwrap();
        assert_eq!(usage.prompt, 9);
        assert_eq!(usage.credits, 1.5);
    }

    #[test]
    fn a_truncated_usage_block_is_ignored() {
        // The tail is bounded, so a partially captured object must not parse
        // into a wrong number.
        assert!(parse_usage("{\"usage\":{\"prompt_tokens\":12,\"compl").is_none());
        assert!(parse_usage("no usage here").is_none());
    }

    #[test]
    fn hostnames_have_their_port_and_brackets_handled() {
        assert_eq!(hostname_of("127.0.0.1:8765"), "127.0.0.1");
        assert_eq!(hostname_of("localhost"), "localhost");
        assert_eq!(hostname_of("[::1]:8765"), "[::1]");
        assert_eq!(hostname_of("192.168.1.5"), "192.168.1.5");
    }

    #[test]
    fn loopback_hosts_are_always_allowed() {
        assert!(host_is_allowed("127.0.0.1:8765", false));
        assert!(host_is_allowed("localhost", false));
        assert!(host_is_allowed("[::1]:8765", false));
    }

    #[test]
    fn an_arbitrary_name_needs_lan_mode() {
        assert!(!host_is_allowed("evil.example.com", false));
        assert!(!host_is_allowed("evil.example.com", true));
        assert!(host_is_allowed("192.168.1.5:8765", true));
    }

    #[test]
    fn origins_are_checked_the_same_way() {
        assert!(origin_is_allowed("http://127.0.0.1:3000", false));
        assert!(origin_is_allowed("http://localhost", false));
        assert!(!origin_is_allowed("http://evil.example.com", false));
        assert!(origin_is_allowed("http://192.168.1.5", true));
        assert!(!origin_is_allowed("not a url", false));
    }

    #[test]
    fn ipv4_in_ipv6_peers_are_unmapped_for_the_log() {
        let mapped: IpAddr = "::ffff:192.168.1.5".parse().unwrap();
        assert_eq!(unmapped(mapped), "192.168.1.5");
        let plain: IpAddr = "192.168.1.5".parse().unwrap();
        assert_eq!(unmapped(plain), "192.168.1.5");
    }
}
