//! Port of the WorkBuddy wire protocol.
//!
//! Every rule here is a measured quirk of the upstream gateway rather than a
//! design choice: the request body must be reshaped, the identity headers must
//! be present, and the model roster is an intersection rather than a flat list.
//! Ported from the reference Kotlin implementation so the bridge behaves
//! identically.

use serde_json::{json, Map, Value};

pub const CN_CHAT_BASE: &str = "https://copilot.tencent.com";
pub const CN_BILLING_BASE: &str = "https://www.codebuddy.cn";
pub const GLOBAL_BASE: &str = "https://www.workbuddy.ai";

/// The CLI-shaped user agent the CN endpoints expect.
pub const CLIENT_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";

pub const ERROR_BODY_LIMIT: usize = 4096;

const BADGE_PREFIX: &str = "badge:";
const EFFORT_VALUES: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Insufficient-credit markers the gateway reports in prose, not status codes.
const HARD_CREDIT_MARKERS: [&str; 17] = [
    "insufficient credit",
    "no credit",
    "credit exhausted",
    "credits exhausted",
    "out of credit",
    "quota exceeded",
    "quota exhaust",
    "payment required",
    "credit not enough",
    "not enough credit",
    "积分不足",
    "额度不足",
    "余额不足",
    "积分用完",
    "额度用尽",
    "没有积分",
    "",
];

/// A dead session is sometimes reported as 403, so the body is checked too.
const SESSION_DEAD_MARKERS: [&str; 2] = ["Offline user session not found", "12153"];

/// Which build a credential belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Region {
    Cn,
    Global,
}

impl Region {
    pub fn name(self) -> &'static str {
        match self {
            Region::Cn => "cn",
            Region::Global => "global",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Region::Cn => "国内版",
            Region::Global => "国际版",
        }
    }

    pub fn parse(value: &str) -> Option<Region> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cn" | "china" | "国内" | "国内版" => Some(Region::Cn),
            "global" | "intl" | "international" | "国际" | "国际版" => Some(Region::Global),
            _ => None,
        }
    }

    pub fn chat_base(self) -> &'static str {
        match self {
            Region::Global => GLOBAL_BASE,
            Region::Cn => CN_CHAT_BASE,
        }
    }

    pub fn billing_base(self) -> &'static str {
        match self {
            Region::Global => GLOBAL_BASE,
            Region::Cn => CN_BILLING_BASE,
        }
    }

    pub fn origin(self) -> &'static str {
        self.billing_base()
    }

    pub fn all() -> [Region; 2] {
        [Region::Cn, Region::Global]
    }
}

/// Which build a credential belongs to; an empty domain means CN.
pub fn region_of(domain: &str) -> Region {
    let lowered = domain.trim().to_ascii_lowercase();
    if lowered == "workbuddy.ai" || lowered.ends_with(".workbuddy.ai") {
        Region::Global
    } else {
        Region::Cn
    }
}

/// Failure classes, mapped onto distinct HTTP answers by the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    HardCredit,
    SoftRate,
    SessionDead,
    NotFound,
    Server,
    Client,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::HardCredit => "hard_credit",
            ErrorKind::SoftRate => "soft_rate",
            ErrorKind::SessionDead => "session_dead",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Server => "server",
            ErrorKind::Client => "client",
        }
    }

    /// HTTP status this failure class surfaces as.
    pub fn status(self) -> u16 {
        match self {
            ErrorKind::HardCredit => 402,
            ErrorKind::SoftRate => 429,
            ErrorKind::SessionDead => 401,
            ErrorKind::NotFound => 502,
            ErrorKind::Server => 502,
            ErrorKind::Client => 400,
        }
    }
}

/// Classifies a failure from its status and body excerpt.
pub fn classify(status: u16, body: &str) -> ErrorKind {
    if status == 402 {
        return ErrorKind::HardCredit;
    }
    // A 401 is always an auth failure on this upstream, whatever the body says.
    if status == 401 {
        return ErrorKind::SessionDead;
    }
    let lowered = body.to_lowercase();
    if HARD_CREDIT_MARKERS
        .iter()
        .any(|m| !m.is_empty() && (lowered.contains(&m.to_lowercase()) || body.contains(m)))
    {
        return ErrorKind::HardCredit;
    }
    if SESSION_DEAD_MARKERS.iter().any(|m| body.contains(m)) {
        return ErrorKind::SessionDead;
    }
    match status {
        429 => ErrorKind::SoftRate,
        404 => ErrorKind::NotFound,
        s if s >= 500 => ErrorKind::Server,
        _ => ErrorKind::Client,
    }
}

/// Pulls the user-facing message out of an error body, preferring the localized
/// `displayMsg` the upstream sends.
pub fn display_error(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let obj: Value = serde_json::from_str(trimmed).ok()?;
    if let Some(display) = obj.get("displayMsg").and_then(Value::as_object) {
        for key in ["zh", "en"] {
            if let Some(text) = display.get(key).and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    let msg = obj.get("msg").and_then(Value::as_str).unwrap_or("").trim();
    if msg.is_empty() {
        None
    } else {
        Some(msg.to_string())
    }
}

/// 把一段错误响应体收敛成一句人能读的话。
///
/// 上游出错时可能返回三种东西：带 `displayMsg` 的 JSON、一页网关 HTML、或者纯
/// 文本。直接透传 HTML 会把客户端的日志和这里的调用记录都撑成一堆标签，所以按
/// 形状挑一句最有信息量的话出来。
pub fn human_error(status: u16, body: &str) -> String {
    if let Some(message) = display_error(body) {
        return message;
    }
    let trimmed = body.trim_start();
    let looks_like_html = trimmed.starts_with('<');
    if looks_like_html {
        // 网关的 401/403 是一页 nginx/APISIX 的错误页，页面本身没信息量；真正
        // 有用的信息是「会话没了」。
        return match status {
            401 | 403 => format!("账号会话已失效（HTTP {status}），请重新登录"),
            404 => format!("上游没有这个接口（HTTP {status}）"),
            s if s >= 500 => format!("上游网关错误（HTTP {s}）"),
            s => format!("上游返回了错误页（HTTP {s}）"),
        };
    }
    // 纯文本：把换行和连续空白压成单个空格，免得一条记录占好几行。
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        format!("上游返回空响应（HTTP {status}）")
    } else {
        collapsed.chars().take(200).collect()
    }
}

// ---------------------------------------------------------------------- //
// Request body shaping
// ---------------------------------------------------------------------- //

/// Forces streaming, rewrites `developer` to `system`, and flattens
/// `tool_choice`.
///
/// The `developer` rewrite is load-bearing: OpenAI clients emit the system
/// prompt that way, and the gateway rejects it with 400 code 11128.
pub fn prepare_chat_body(source: &str) -> String {
    let Ok(mut obj) = serde_json::from_str::<Value>(source) else {
        return source.to_string();
    };
    if let Some(map) = obj.as_object_mut() {
        map.insert("stream".into(), Value::Bool(true));
        normalize_developer_role(map);
        normalize_tool_choice(map);
    }
    serde_json::to_string(&obj).unwrap_or_else(|_| source.to_string())
}

fn normalize_developer_role(map: &mut Map<String, Value>) {
    let Some(messages) = map.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        let Some(fields) = message.as_object_mut() else {
            continue;
        };
        let is_developer = fields
            .get("role")
            .and_then(Value::as_str)
            .map(|r| r == "developer")
            .unwrap_or(false);
        if is_developer {
            fields.insert("role".into(), Value::String("system".into()));
        }
    }
}

fn normalize_tool_choice(map: &mut Map<String, Value>) {
    let Some(choice) = map.get("tool_choice").cloned() else {
        return;
    };
    let suppress = |map: &mut Map<String, Value>| {
        map.remove("tools");
        map.remove("functions");
    };
    match choice {
        Value::String(text) => {
            if text.trim().eq_ignore_ascii_case("none") {
                map.remove("tool_choice");
                suppress(map);
            }
        }
        Value::Object(fields) => {
            let kind = fields
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            match kind.as_str() {
                "none" => {
                    map.remove("tool_choice");
                    suppress(map);
                }
                "auto" | "required" => {
                    map.insert("tool_choice".into(), Value::String(kind));
                }
                "function" => {
                    let mut name = fields
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if name.is_empty() {
                        name = fields
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                    }
                    map.insert(
                        "tool_choice".into(),
                        Value::String(if name.is_empty() { "auto".into() } else { name }),
                    );
                }
                _ => {
                    map.remove("tool_choice");
                }
            }
        }
        _ => {
            map.remove("tool_choice");
        }
    }
}

const INTERNATIONAL_SYSTEM_PROMPT: &str = "You are a helpful assistant.";

/// The international gateway additionally requires a leading `system` message
/// and rejects the adapter's `off` effort spelling.
pub fn prepare_international_chat_body(source: &str) -> String {
    let prepared = prepare_chat_body(source);
    let Ok(mut obj) = serde_json::from_str::<Value>(&prepared) else {
        return prepared;
    };
    let Some(map) = obj.as_object_mut() else {
        return prepared;
    };
    if map.get("reasoning_effort").and_then(Value::as_str) == Some("off") {
        map.remove("reasoning_effort");
    }
    let Some(messages) = map.get_mut("messages").and_then(Value::as_array_mut) else {
        return serde_json::to_string(&obj).unwrap_or(prepared);
    };
    let leading_is_system = messages
        .first()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .map(|r| r == "system")
        .unwrap_or(false);
    if !leading_is_system {
        messages.insert(
            0,
            json!({"role": "system", "content": INTERNATIONAL_SYSTEM_PROMPT}),
        );
    }
    serde_json::to_string(&obj).unwrap_or(prepared)
}

// ---------------------------------------------------------------------- //
// Headers
// ---------------------------------------------------------------------- //

pub type Headers = Vec<(&'static str, String)>;

fn push(headers: &mut Headers, key: &'static str, value: &str) {
    headers.push((key, value.to_string()));
}

/// Headers shared by every upstream request.
pub fn common_headers(region: Region) -> Headers {
    let origin = region.origin();
    vec![
        ("Accept", "application/json, text/plain, */*".to_string()),
        ("X-Requested-With", "XMLHttpRequest".to_string()),
        ("Origin", origin.to_string()),
        ("Referer", format!("{origin}/")),
        ("User-Agent", CLIENT_UA.to_string()),
    ]
}

/// Chat headers, including the `X-No-*` conventions the official client uses to
/// state that a value is deliberately absent.
pub fn chat_headers(
    region: Region,
    access_token: &str,
    uid: &str,
    domain: &str,
    enterprise_id: Option<&str>,
    user_agent: &str,
    client_version: &str,
) -> Headers {
    let mut headers = common_headers(region);
    push(&mut headers, "User-Agent", user_agent);
    push(&mut headers, "Content-Type", "application/json");
    push(
        &mut headers,
        "Authorization",
        &format!("Bearer {access_token}"),
    );
    if uid.is_empty() {
        push(&mut headers, "X-No-User-Id", "1");
    } else {
        push(&mut headers, "X-User-Id", uid);
    }
    match enterprise_id.filter(|v| !v.is_empty()) {
        Some(value) => push(&mut headers, "X-Enterprise-Id", value),
        None => push(&mut headers, "X-No-Enterprise-Id", "1"),
    }
    if domain.is_empty() {
        push(&mut headers, "X-No-Department-Info", "1");
    } else {
        push(&mut headers, "X-Domain", domain);
    }
    push(&mut headers, "X-IDE-Type", "WorkBuddy");
    push(&mut headers, "X-IDE-Name", "WorkBuddy");
    push(&mut headers, "X-IDE-Version", client_version);
    push(&mut headers, "X-Product", "SaaS");
    headers
}

/// Refresh headers; the refresh token appears here and nowhere else.
pub fn refresh_headers(
    region: Region,
    refresh_token: &str,
    enterprise_id: Option<&str>,
) -> Headers {
    let mut headers = common_headers(region);
    push(&mut headers, "X-Refresh-Token", refresh_token);
    push(&mut headers, "X-Auth-Refresh-Source", "workbuddy");
    if let Some(value) = enterprise_id.filter(|v| !v.is_empty()) {
        push(&mut headers, "X-Enterprise-Id", value);
    }
    headers
}

/// Billing headers; this path authenticates with the bearer token.
pub fn billing_headers(
    region: Region,
    access_token: &str,
    uid: &str,
    domain: &str,
    enterprise_id: Option<&str>,
) -> Headers {
    let _ = region;
    let mut headers: Headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("Accept", "application/json".to_string()),
        ("Content-Type", "application/json".to_string()),
    ];
    if !uid.is_empty() {
        push(&mut headers, "X-User-Id", uid);
    }
    if let Some(value) = enterprise_id.filter(|v| !v.is_empty()) {
        push(&mut headers, "X-Enterprise-Id", value);
        push(&mut headers, "X-Tenant-Id", value);
    }
    if !domain.is_empty() {
        push(&mut headers, "X-Domain", domain);
    }
    headers
}

/// Growth-domain headers: the billing set plus request provenance.
///
/// The growth endpoints sit on the chat base but authenticate like billing, and
/// the upstream additionally expects the origin/referer pair the official web
/// client sends, so both halves are combined here.
pub fn growth_headers(
    region: Region,
    access_token: &str,
    uid: &str,
    domain: &str,
    enterprise_id: Option<&str>,
) -> Headers {
    let mut headers = billing_headers(region, access_token, uid, domain, enterprise_id);
    let origin = region.origin();
    push(&mut headers, "Origin", origin);
    push(&mut headers, "Referer", &format!("{origin}/"));
    headers
}

// ---------------------------------------------------------------------- //
// Model catalogue
// ---------------------------------------------------------------------- //

/// trims an upstream credits string to its language-neutral form.
/// `"x0.79 credits"` becomes `"x0.79"`; a bare unit word carries no rate.
pub fn normalize_credits(credits: Option<&str>) -> Option<String> {
    let trimmed = credits.unwrap_or("").trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("credit") || trimmed.eq_ignore_ascii_case("credits") {
        return None;
    }
    let stripped = [" credits", " credit", "\tcredits", "\tcredit"]
        .iter()
        .find_map(|suffix| {
            let lowered = trimmed.to_lowercase();
            lowered
                .ends_with(&suffix.to_lowercase())
                .then(|| trimmed[..trimmed.len() - suffix.len()].trim().to_string())
        })
        .unwrap_or_else(|| trimmed.to_string());
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

/// Reasoning metadata the catalogue declares for one model.
#[derive(Debug, Clone, Default)]
pub struct ProtoReasoning {
    pub supports: bool,
    pub only_reasoning: bool,
    pub supported_efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub can_disable_thinking: bool,
}

/// One catalogue row, already reduced to what a client needs.
#[derive(Debug, Clone, Default)]
pub struct ProtoModel {
    pub id: String,
    pub name: String,
    pub context_window: i64,
    pub default_context_window: Option<i64>,
    pub max_input_tokens: i64,
    pub supported_context_windows: Vec<i64>,
    pub max_tokens: i64,
    pub supports_images: bool,
    pub reasoning: ProtoReasoning,
    pub credits: Option<String>,
    pub badges: Vec<String>,
}

impl ProtoModel {
    /// The display rate, with its unit word removed.
    pub fn rate(&self) -> String {
        normalize_credits(self.credits.as_deref()).unwrap_or_default()
    }

    pub fn multiplier(&self) -> f64 {
        let rate = self.rate();
        let bare = rate.strip_prefix('x').unwrap_or(&rate);
        bare.parse::<f64>().unwrap_or(-1.0)
    }

    pub fn is_free(&self) -> bool {
        self.multiplier() == 0.0
    }
}

fn effort_or_default(raw: Option<&Value>) -> Option<String> {
    raw.filter(|v| !v.is_null())
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| EFFORT_VALUES.contains(v))
        .map(str::to_string)
}

fn parse_badges(tags: Option<&Value>) -> Vec<String> {
    let Some(items) = tags.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut result: Vec<String> = Vec::new();
    for tag in items {
        let Some(tag) = tag.as_str() else { continue };
        if !tag.to_lowercase().starts_with(BADGE_PREFIX) {
            continue;
        }
        let label = tag[BADGE_PREFIX.len()..]
            .split(':')
            .next()
            .unwrap_or("")
            .to_string();
        if !label.is_empty() && !result.contains(&label) {
            result.push(label);
        }
    }
    result
}

fn resolve_reasoning(row: &Value) -> ProtoReasoning {
    let supports = row
        .get("supportsReasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let only_reasoning = row
        .get("onlyReasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw = row.get("reasoning");
    let efforts: Vec<String> = raw
        .and_then(|r| r.get("supportedEfforts"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|e| EFFORT_VALUES.contains(e))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let default = effort_or_default(raw.and_then(|r| r.get("defaultEffort")))
        .or_else(|| effort_or_default(raw.and_then(|r| r.get("effort"))));
    ProtoReasoning {
        supports,
        only_reasoning,
        supported_efforts: efforts,
        default_effort: default,
        // Only an explicit true allows switching thinking off; older rows omit
        // the field and reject `off` on the wire.
        can_disable_thinking: raw
            .and_then(|r| r.get("canDisableThinking"))
            .and_then(Value::as_bool)
            == Some(true),
    }
}

fn int_field(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// The roster is the `cli` agent's model list intersected with the rows that are
/// actually servable, so a published-but-unusable id never reaches a client.
/// Errors when either half is missing, because an empty catalogue means the
/// document changed shape.
pub fn parse_models(document: &str) -> Result<Vec<ProtoModel>, String> {
    let root: Value =
        serde_json::from_str(document).map_err(|e| format!("catalog is not JSON: {e}"))?;
    let data = match root.get("data") {
        Some(value) if value.is_object() => value.clone(),
        _ => {
            if root.get("models").is_some() || root.get("agents").is_some() {
                root.clone()
            } else {
                json!({})
            }
        }
    };

    let cli_ids: Vec<String> = data
        .get("agents")
        .and_then(Value::as_array)
        .and_then(|agents| {
            agents.iter().find_map(|agent| {
                if agent.get("name").and_then(Value::as_str) == Some("cli") {
                    agent.get("models").and_then(Value::as_array).map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    if cli_ids.is_empty() {
        return Err("catalog lists no cli agent models".into());
    }

    let mut by_id: std::collections::HashMap<String, ProtoModel> = std::collections::HashMap::new();
    if let Some(rows) = data.get("models").and_then(Value::as_array) {
        for row in rows {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if id.is_empty() || row.get("disabled").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let input = int_field(row, "maxInputTokens");
            let output = int_field(row, "maxOutputTokens");
            if input <= 0 || output <= 0 {
                continue;
            }
            let window = row.get("contextWindow");
            let default_length = window.map(|w| int_field(w, "defaultLength")).unwrap_or(0);
            let supported: Vec<i64> = window
                .and_then(|w| w.get("supportedLengths"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_i64)
                        .filter(|v| *v > 0)
                        .collect()
                })
                .unwrap_or_default();
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or(&id)
                .to_string();
            let credits = row
                .get("credits")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            by_id.insert(
                id.clone(),
                ProtoModel {
                    id,
                    name,
                    context_window: if default_length > 0 {
                        default_length
                    } else {
                        input
                    },
                    default_context_window: (default_length > 0).then_some(default_length),
                    max_input_tokens: input,
                    supported_context_windows: supported,
                    max_tokens: output,
                    // Over-claiming image input would admit a picture the
                    // provider later rejects, so unknown resolves to false.
                    supports_images: row
                        .get("supportsImages")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        && row.get("disabledMultimodal").and_then(Value::as_bool) != Some(true),
                    reasoning: resolve_reasoning(row),
                    credits,
                    badges: parse_badges(row.get("tags")),
                },
            );
        }
    }

    let models: Vec<ProtoModel> = cli_ids
        .iter()
        .filter_map(|id| by_id.get(id).cloned())
        .collect();
    if models.is_empty() {
        return Err("catalog resolved to an empty list".into());
    }
    Ok(models)
}

/// The identity a chat request presents.
///
/// The user agent is the shape the official desktop client composes; the app
/// variant is used for the international catalogue, which is split by client
/// identity on the gateway.
#[derive(Debug, Clone)]
pub struct ChatIdentity {
    pub client_version: &'static str,
    pub user_agent: &'static str,
    pub app_user_agent: &'static str,
}

impl ChatIdentity {
    pub fn for_region(region: Region) -> ChatIdentity {
        match region {
            Region::Global => ChatIdentity {
                client_version: "5.5.2",
                user_agent: "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2",
                app_user_agent: "WorkBuddy/5.5.2",
            },
            Region::Cn => ChatIdentity {
                client_version: "5.6.2",
                user_agent: "WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.63.2",
                app_user_agent: "WorkBuddy/5.6.2",
            },
        }
    }
}

/// The domain that identifies a build when a response omits one.
pub fn default_domain(region: Region) -> String {
    match region {
        Region::Global => GLOBAL_BASE.to_string(),
        Region::Cn => CN_CHAT_BASE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_detection_follows_the_domain() {
        assert_eq!(region_of("www.workbuddy.ai"), Region::Global);
        assert_eq!(region_of("workbuddy.ai"), Region::Global);
        assert_eq!(region_of("www.codebuddy.cn"), Region::Cn);
        assert_eq!(region_of(""), Region::Cn);
    }

    #[test]
    fn developer_role_is_rewritten_and_stream_forced() {
        let source =
            r#"{"model":"hy3","stream":false,"messages":[{"role":"developer","content":"hi"}]}"#;
        let out: Value = serde_json::from_str(&prepare_chat_body(source)).unwrap();
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["messages"][0]["role"], json!("system"));
    }

    #[test]
    fn tool_choice_none_drops_tools() {
        let source = r#"{"messages":[],"tool_choice":"none","tools":[{"type":"function"}]}"#;
        let out: Value = serde_json::from_str(&prepare_chat_body(source)).unwrap();
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());
    }

    #[test]
    fn tool_choice_function_flattens_to_a_name() {
        let source =
            r#"{"messages":[],"tool_choice":{"type":"function","function":{"name":"read"}}}"#;
        let out: Value = serde_json::from_str(&prepare_chat_body(source)).unwrap();
        assert_eq!(out["tool_choice"], json!("read"));
    }

    #[test]
    fn international_body_gains_a_leading_system_message() {
        let source = r#"{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"off"}"#;
        let out: Value = serde_json::from_str(&prepare_international_chat_body(source)).unwrap();
        assert_eq!(out["messages"][0]["role"], json!("system"));
        assert_eq!(out["messages"][1]["role"], json!("user"));
        assert!(out.get("reasoning_effort").is_none());
    }

    #[test]
    fn credits_normalisation_strips_the_unit_word() {
        assert_eq!(
            normalize_credits(Some("x0.79 credits")).as_deref(),
            Some("x0.79")
        );
        assert_eq!(normalize_credits(Some("credits")), None);
        assert_eq!(normalize_credits(Some("  ")), None);
        assert_eq!(normalize_credits(Some("x1")), Some("x1".to_string()));
    }

    #[test]
    fn catalogue_intersects_the_cli_agent_list() {
        let document = r#"{
            "code": 0,
            "data": {
                "models": [
                    {"id":"hy3","name":"Hunyuan 3","maxInputTokens":180000,"maxOutputTokens":8192,
                     "contextWindow":{"defaultLength":180000,"supportedLengths":[180000,200000]},
                     "credits":"x0.5 credits","supportsReasoning":true,
                     "reasoning":{"supportedEfforts":["low","high"],"defaultEffort":"high"},
                     "tags":["badge:free","other"]},
                    {"id":"unlisted","maxInputTokens":1000,"maxOutputTokens":10}
                ],
                "agents": [{"name":"cli","models":["hy3","missing"]}]
            }
        }"#;
        let models = parse_models(document).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "hy3");
        assert_eq!(models[0].rate(), "x0.5");
        assert_eq!(models[0].multiplier(), 0.5);
        assert_eq!(models[0].supported_context_windows, vec![180000, 200000]);
        assert_eq!(models[0].badges, vec!["free".to_string()]);
        assert_eq!(models[0].reasoning.default_effort.as_deref(), Some("high"));
    }

    #[test]
    fn catalogue_without_a_cli_agent_is_an_error() {
        let document = r#"{"code":0,"data":{"models":[],"agents":[]}}"#;
        assert!(parse_models(document).is_err());
    }

    #[test]
    fn error_classification_prefers_credit_markers() {
        assert_eq!(classify(402, ""), ErrorKind::HardCredit);
        assert_eq!(classify(401, "anything"), ErrorKind::SessionDead);
        assert_eq!(classify(200, "积分不足"), ErrorKind::HardCredit);
        assert_eq!(
            classify(403, "Offline user session not found"),
            ErrorKind::SessionDead
        );
        assert_eq!(classify(429, ""), ErrorKind::SoftRate);
        assert_eq!(classify(500, ""), ErrorKind::Server);
        assert_eq!(classify(400, "bad request"), ErrorKind::Client);
    }

    #[test]
    fn a_gateway_error_page_becomes_one_readable_line() {
        let body = "<html>\r\n<head><title>401 Authorization Required</title></head>\r\n</html>";
        let text = human_error(401, body);
        assert!(text.contains("会话已失效"), "{text}");
        assert!(!text.contains('<'), "no markup should leak through: {text}");
        assert!(!text.contains('\n'), "no newlines: {text}");
    }

    #[test]
    fn a_plain_text_error_is_collapsed_onto_one_line() {
        let text = human_error(400, "bad   request\n\nretry later");
        assert_eq!(text, "bad request retry later");
    }

    #[test]
    fn an_empty_body_still_says_something() {
        assert_eq!(human_error(502, "   "), "上游返回空响应（HTTP 502）");
    }

    #[test]
    fn display_error_prefers_the_localised_message() {
        let body =
            r#"{"code":1,"msg":"fallback","displayMsg":{"zh":"没有额度了","en":"no credit"}}"#;
        assert_eq!(display_error(body).as_deref(), Some("没有额度了"));
        assert_eq!(display_error("plain text"), None);
    }
}
