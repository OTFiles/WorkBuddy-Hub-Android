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
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
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

/// How long a connection may take to deliver its request head.
///
/// A `Content-Length` check alone leaves a chunked body unbounded, and a client
/// that connects and then stalls would hold a task for the life of the process.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the upstream may go without emitting a byte before the relay gives
/// up. A generation can legitimately pause, but a minute of silence means the
/// connection is dead and the client is waiting for nothing.
const UPSTREAM_STALL_TIMEOUT: Duration = Duration::from_secs(90);

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
            // A client that opens a socket and then says nothing must not hold
            // a task forever: the original bounded this with a 30-second socket
            // timeout, which in hyper is the header-read deadline.
            let connection = http1::Builder::new()
                .keep_alive(false)
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
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

    if let Err(response) = authorize(&request, &settings, from_loopback) {
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
        _ => error_response(404, "没有这个接口", "not_found"),
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
    request: &Request<Incoming>,
    settings: &Settings,
    from_loopback: bool,
) -> Result<(), Box<Response<Body>>> {
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
            "API Key 不正确",
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
            "不允许的 Host",
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
                "不允许的 Origin",
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
    let mut ids = bridge.model_ids().await;
    // An empty roster usually means the startup refresh failed (or `--no-refresh`
    // was passed); a client that cannot see any model cannot use the bridge, so
    // one lazy retry is worth an upstream call.
    if ids.is_empty() && bridge.store.has_any() && bridge.refresh_catalogue().await.is_ok() {
        ids = bridge.model_ids().await;
    }
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
            "Content-Type 必须是 application/json",
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
        return error_response(413, "请求体过大", "too_large");
    }

    // A `Content-Length` check is not enough on its own: a chunked body declares
    // no length at all. `Limited` stops the read at the ceiling whether or not
    // the client announced a size, so one client cannot exhaust memory.
    let limited = Limited::new(request.into_body(), REQUEST_BODY_LIMIT);
    let body = match limited.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return error_response(413, "请求体过大", "too_large"),
    };
    let Ok(request_body) = String::from_utf8(body.to_vec()) else {
        return error_response(400, "请求体不是合法 JSON", "bad_request");
    };
    // Parsed once and reused: the body is needed both for validity and for the
    // model name, and parsing a multi-megabyte prompt twice is wasteful.
    let Ok(parsed) = serde_json::from_str::<Value>(&request_body) else {
        return error_response(400, "请求体不是合法 JSON", "bad_request");
    };

    let Some(account) = bridge.credential().await else {
        return error_response(401, "还没有登录账号", "not_signed_in");
    };

    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The original always relayed SSE, so a client that asked for
    // `stream: false` received frames it could not parse. Honouring the flag is
    // the difference between "OpenAI-compatible" and "OpenAI-shaped".
    let wants_stream = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let started = now_millis();
    let source_ip = if from_loopback {
        String::new()
    } else {
        unmapped(peer.ip())
    };

    match upstream::chat_stream(&account, &request_body).await {
        ChatOpen::Failed {
            status,
            kind,
            body: error_body,
        } => {
            let detail = wire::human_error(status, &error_body);
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
            let _ = status;
            // The message is the detail alone: `type` already carries the class
            // for a client to branch on, and prefixing it again just makes the
            // sentence longer without adding information.
            error_response(kind.status(), &detail, kind.as_str())
        }
        ChatOpen::Ok(response) => {
            let multiplier = bridge.multiplier_of(&model).await;
            if !wants_stream {
                return aggregate(
                    bridge, response, model, started, source_ip, account, multiplier,
                )
                .await;
            }
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
    /// Held here so `Drop` can still write a record after the client disappears.
    log: Option<CallLog>,
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
    let log = bridge.calls.clone();
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
        log: Some(log),
    };

    let relayed = futures_util::stream::unfold((state, bridge), |(mut state, bridge)| async move {
        if state.finished {
            return None;
        }
        // A stalled upstream must not pin the client forever: if no byte arrives
        // for the stall window the relay ends the stream rather than hanging.
        let next = tokio::time::timeout(UPSTREAM_STALL_TIMEOUT, state.stream.next()).await;
        let next = match next {
            Ok(next) => next,
            Err(_) => {
                if bridge.settings.read().await.verbose {
                    eprintln!(
                        "上游超过 {} 秒没有响应，已中止本次转发",
                        UPSTREAM_STALL_TIMEOUT.as_secs()
                    );
                }
                state.record.outcome = "failed".into();
                state.record.detail = Some("上游超时".into());
                finish_relay(&bridge, &mut state).await;
                return None;
            }
        };
        match next {
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
                    eprintln!("转发中断：{error}");
                }
                state.record.outcome = "failed".into();
                state.record.detail = Some(format!("{error}").chars().take(120).collect());
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

/// 把上游的 SSE 攒成一个完整的 `chat.completion` 返回。
///
/// 上游永远流式，而 OpenAI 客户端里 `stream: false` 的一方期待一个完整的 JSON。
/// 原 Android 版一律回 SSE，那种客户端直接解析失败；这里替它攒起来。
///
/// 攒的过程同时拿到 usage，所以记账和流式那条路上是一致的。
async fn aggregate(
    bridge: Arc<Bridge>,
    upstream_response: reqwest::Response,
    model: String,
    started: i64,
    source_ip: String,
    account: SavedAccount,
    multiplier: f64,
) -> Response<Body> {
    let mut stream = upstream_response.bytes_stream();
    let mut buffer = String::new();
    let mut assembled = Assembled::default();
    let mut usage = SseUsage::default();
    let mut failure: Option<String> = None;

    loop {
        let next = tokio::time::timeout(UPSTREAM_STALL_TIMEOUT, stream.next()).await;
        let chunk = match next {
            Err(_) => {
                failure = Some("上游超过 90 秒没有响应".to_string());
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(error))) => {
                failure = Some(format!("上游流中断：{error}"));
                break;
            }
            Ok(Some(Ok(bytes))) => bytes,
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        for payload in take_frames(&mut buffer) {
            assembled.absorb(&payload);
            if let Some(parsed) = parse_usage(&payload) {
                usage = parsed;
            }
        }
    }
    // 收尾：上游可能没发空行分隔就结束了。
    for payload in take_frames(&mut buffer) {
        assembled.absorb(&payload);
        if let Some(parsed) = parse_usage(&payload) {
            usage = parsed;
        }
    }
    if usage.prompt == 0 && usage.completion == 0 {
        if let Some(parsed) = parse_usage(&buffer) {
            usage = parsed;
        }
    }

    let mut record = CallRecord {
        timestamp: started,
        model: model.clone(),
        outcome: if failure.is_some() {
            "failed".into()
        } else {
            "ok".into()
        },
        detail: failure.clone(),
        prompt_tokens: usage.prompt,
        completion_tokens: usage.completion,
        credits: usage.credits,
        source_ip,
        account_id: account.id.clone(),
        account_label: account.label(),
        multiplier,
    };
    if record.detail.is_none() {
        record.detail = None;
    }
    bridge.calls.append(&record);
    bridge.status.lock().await.last_call_at = Some(now_millis());

    if let Some(detail) = failure {
        if assembled.is_empty() {
            // 一个字都没攒到，那就老老实实报错，给客户端一个能看懂的东西。
            return error_response(502, &detail, "server");
        }
    }

    json_response(StatusCode::OK, assembled.finish(&model, usage))
}

/// 从缓冲区里取出所有已经完整的 SSE 帧。
///
/// SSE 用空行分帧，但上游偶尔用 `\r\n\r\n`，所以两种分隔符都认。没写完的尾巴
/// 留在缓冲区里等下一次读取。
fn take_frames(buffer: &mut String) -> Vec<String> {
    let mut frames = Vec::new();
    loop {
        let boundary = buffer
            .find("\n\n")
            .map(|index| (index, 2))
            .into_iter()
            .chain(buffer.find("\r\n\r\n").map(|index| (index, 4)))
            .min_by_key(|(index, _)| *index);
        let Some((index, width)) = boundary else {
            return frames;
        };
        let block: String = buffer[..index].to_string();
        buffer.drain(..index + width);
        for line in block.lines() {
            let line = line.trim_end_matches('\r');
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            frames.push(data.to_string());
        }
    }
}

/// 把一串流式增量拼成一条完整回答。
#[derive(Default)]
struct Assembled {
    id: String,
    created: i64,
    model: String,
    content: String,
    reasoning: String,
    finish_reason: Option<String>,
    /// 按 `index` 归并的 tool_calls：流式下参数是一段段吐出来的。
    tool_calls: std::collections::BTreeMap<u64, ToolCall>,
}

#[derive(Default, Clone)]
struct ToolCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

impl Assembled {
    fn absorb(&mut self, payload: &str) {
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        if self.id.is_empty() {
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                self.id = id.to_string();
            }
        }
        if self.created == 0 {
            self.created = chunk.get("created").and_then(Value::as_i64).unwrap_or(0);
        }
        if self.model.is_empty() {
            if let Some(model) = chunk.get("model").and_then(Value::as_str) {
                self.model = model.to_string();
            }
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    self.content.push_str(text);
                }
                if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                    self.reasoning.push_str(text);
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let slot = self.tool_calls.entry(index).or_default();
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            if !id.is_empty() {
                                slot.id = id.to_string();
                            }
                        }
                        if let Some(kind) = call.get("type").and_then(Value::as_str) {
                            if !kind.is_empty() {
                                slot.kind = kind.to_string();
                            }
                        }
                        if let Some(function) = call.get("function") {
                            if let Some(name) = function.get("name").and_then(Value::as_str) {
                                if !name.is_empty() {
                                    slot.name = name.to_string();
                                }
                            }
                            if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                                slot.arguments.push_str(args);
                            }
                        }
                    }
                }
            }
            // 有些上游把完整消息放在 `message` 而不是 `delta` 里。
            if let Some(message) = choice.get("message") {
                if self.content.is_empty() {
                    if let Some(text) = message.get("content").and_then(Value::as_str) {
                        self.content.push_str(text);
                    }
                }
                if self.reasoning.is_empty() {
                    if let Some(text) = message.get("reasoning_content").and_then(Value::as_str) {
                        self.reasoning.push_str(text);
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if !reason.is_empty() {
                    self.finish_reason = Some(reason.to_string());
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.content.is_empty() && self.reasoning.is_empty() && self.tool_calls.is_empty()
    }

    fn finish(&self, requested_model: &str, usage: SseUsage) -> Value {
        let mut message = serde_json::Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), json!(self.content));
        if !self.reasoning.is_empty() {
            message.insert("reasoning_content".into(), json!(self.reasoning));
        }
        if !self.tool_calls.is_empty() {
            let calls: Vec<Value> = self
                .tool_calls
                .values()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": if call.kind.is_empty() { "function" } else { call.kind.as_str() },
                        "function": {"name": call.name, "arguments": call.arguments},
                    })
                })
                .collect();
            message.insert("tool_calls".into(), Value::Array(calls));
        }
        let model = if self.model.is_empty() {
            requested_model.to_string()
        } else {
            self.model.clone()
        };
        let finish_reason = self.finish_reason.clone().unwrap_or_else(|| {
            if self.tool_calls.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            }
        });
        json!({
            "id": if self.id.is_empty() { format!("chatcmpl-wbhub-{}", now_millis()) } else { self.id.clone() },
            "object": "chat.completion",
            "created": if self.created == 0 { now_millis() / 1000 } else { self.created },
            "model": model,
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": finish_reason,
            }],
            "usage": {
                "prompt_tokens": usage.prompt,
                "completion_tokens": usage.completion,
                "total_tokens": usage.prompt + usage.completion,
            },
        })
    }
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

impl Drop for RelayState {
    /// A client that disconnects mid-answer drops the body, and with it this
    /// state. Recording here keeps the call in the log instead of losing it —
    /// the tokens were spent whether or not anyone read the reply.
    fn drop(&mut self) {
        if !self.finished {
            self.record.outcome = "aborted".into();
            self.record.detail = Some("客户端提前断开".into());
            self.record.prompt_tokens = self.usage.prompt;
            self.record.completion_tokens = self.usage.completion;
            self.record.credits = self.usage.credits;
            if let Some(log) = self.log.take() {
                log.append(&self.record);
            }
        }
    }
}

/// Records the completed call once the stream has ended, for either outcome.
async fn finish_relay(bridge: &Arc<Bridge>, state: &mut RelayState) {
    state.finished = true;
    let log = state.log.take();
    state.record.prompt_tokens = state.usage.prompt;
    state.record.completion_tokens = state.usage.completion;
    state.record.credits = state.usage.credits;
    if let Some(log) = log {
        log.append(&state.record);
    }
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
    fn frames_split_on_both_separators() {
        let mut buffer = String::from("data: a\n\ndata: b\r\n\r\ndata: partial");
        let frames = take_frames(&mut buffer);
        assert_eq!(frames, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            buffer, "data: partial",
            "an unfinished frame must stay in the buffer"
        );
    }

    #[test]
    fn done_markers_and_blank_data_are_skipped() {
        let mut buffer = String::from("data: [DONE]\n\ndata:\n\ndata: {}\n\n");
        let frames = take_frames(&mut buffer);
        assert_eq!(frames, vec!["{}".to_string()]);
    }

    #[test]
    fn aggregation_joins_deltas_into_one_message() {
        let mut assembled = Assembled::default();
        for payload in [
            r#"{"id":"chatcmpl-1","created":1700000000,"model":"hy3","choices":[{"delta":{"role":"assistant"},"index":0}]}"#,
            r#"{"id":"chatcmpl-1","choices":[{"delta":{"reasoning_content":"想一下"},"index":0}]}"#,
            r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"PO"},"index":0}]}"#,
            r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"NG"},"finish_reason":"stop","index":0}]}"#,
        ] {
            assembled.absorb(payload);
        }
        let out = assembled.finish(
            "hy3",
            SseUsage {
                prompt: 5,
                completion: 2,
                credits: 0.0,
            },
        );
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["id"], "chatcmpl-1");
        assert_eq!(out["model"], "hy3");
        assert_eq!(out["choices"][0]["message"]["content"], "PONG");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "想一下");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["total_tokens"], 7);
    }

    #[test]
    fn streamed_tool_call_arguments_are_concatenated() {
        // Tool arguments arrive as fragments keyed by index; forgetting to join
        // them yields invalid JSON for the client's function call.
        let mut assembled = Assembled::default();
        for payload in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"/x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ] {
            assembled.absorb(payload);
        }
        let out = assembled.finish("hy3", SseUsage::default());
        let call = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "read");
        assert_eq!(call["function"]["arguments"], r#"{"path":"/x"}"#);
        assert_eq!(call["id"], "call_1");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn a_message_shaped_chunk_is_also_understood() {
        // Some upstreams answer non-streamed requests with `message` instead of
        // `delta`; the aggregator has to accept both.
        let mut assembled = Assembled::default();
        assembled.absorb(
            r#"{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
        );
        let out = assembled.finish("hy3", SseUsage::default());
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert!(!assembled.is_empty());
    }

    #[test]
    fn an_empty_aggregation_reports_itself_empty() {
        let assembled = Assembled::default();
        assert!(assembled.is_empty());
        let out = assembled.finish("hy3", SseUsage::default());
        assert!(out["id"].as_str().unwrap().starts_with("chatcmpl-wbhub-"));
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
