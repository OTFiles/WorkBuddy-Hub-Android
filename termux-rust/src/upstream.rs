//! Upstream client: chat, catalogue, billing, and daily check-in.
//!
//! Each method mirrors one reference implementation call, including the parts
//! that look redundant: the international catalogue uses an App-shaped user
//! agent while CN keeps the CLI one, and enterprise accounts read a different
//! billing endpoint because the personal one serves them an empty list.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::http::{self, ChatOpen};
use crate::store::SavedAccount;
use crate::wire::{self, Region};

/// One package and its remaining credit.
#[derive(Debug, Clone)]
pub struct CreditAccount {
    pub name: String,
    pub remain: f64,
    pub size: f64,
    pub cycle_end: String,
    pub unlimited: bool,
}

/// Aggregated balance.
#[derive(Debug, Clone, Default)]
pub struct Balance {
    pub total: f64,
    pub accounts: Vec<CreditAccount>,
    pub cycle_end: String,
    pub unlimited: bool,
}

/// Result of a check-in attempt.
#[derive(Debug, Clone)]
pub struct CheckinOutcome {
    pub ok: bool,
    pub message: String,
}

/// Opens a chat stream, forcing `stream: true` and reshaping the body.
pub async fn chat_stream(account: &SavedAccount, body: &str) -> ChatOpen {
    let region = account.region();
    let body = match region {
        Region::Global => wire::prepare_international_chat_body(body),
        Region::Cn => wire::prepare_chat_body(body),
    };
    let identity = wire::ChatIdentity::for_region(region);
    let headers = wire::chat_headers(
        region,
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
        identity.user_agent,
        identity.client_version,
    );
    http::open_chat(region, &headers, &body).await
}

/// The model catalogue both variants serve from `/v3/config`.
pub async fn fetch_models(account: &SavedAccount) -> Result<Vec<wire::ProtoModel>> {
    let region = account.region();
    let mut headers: wire::Headers = vec![
        ("Authorization", format!("Bearer {}", account.access_token)),
        ("Accept", "application/json".to_string()),
        ("Origin", region.origin().to_string()),
        ("Referer", format!("{}/", region.origin())),
    ];
    if region == Region::Global {
        headers.push(("X-Requested-With", "XMLHttpRequest".to_string()));
        headers.push(("X-Product", "SaaS".to_string()));
        headers.push((
            "User-Agent",
            wire::ChatIdentity::for_region(region)
                .app_user_agent
                .to_string(),
        ));
    } else {
        headers.push(("User-Agent", wire::CLIENT_UA.to_string()));
    }
    let url = format!("{}/v3/config", region.chat_base());
    let text = send(&url, "GET", &headers, None).await?;
    let envelope = Envelope::parse(&text)?;
    if envelope.code != 0 {
        bail!("模型列表被拒绝：{}", envelope.msg);
    }
    wire::parse_models(&text).map_err(|e| anyhow::anyhow!(e))
}

/// Aggregated remaining credit. Enterprise accounts read a different endpoint.
pub async fn fetch_credits(account: &SavedAccount) -> Result<Balance> {
    if account.region() == Region::Cn && account.enterprise_id().is_some() {
        fetch_enterprise_credits(account).await
    } else {
        fetch_personal_credits(account).await
    }
}

async fn fetch_personal_credits(account: &SavedAccount) -> Result<Balance> {
    let region = account.region();
    let now = chrono::Local::now();
    let range_begin = now.format("%Y-%m-%d %H:%M:%S").to_string();
    // The reference asks for a two-year window; the upstream serves the same
    // rows for any range that covers the active cycle.
    let range_end = (now + chrono::Duration::days(730))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let body = json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRangeBegin": range_begin,
        "PackageEndTimeRangeEnd": range_end,
    })
    .to_string();
    let url = format!(
        "{}/v2/billing/meter/get-user-resource",
        region.billing_base()
    );
    let headers = wire::billing_headers(
        region,
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
    );
    let text = send(&url, "POST", &headers, Some(&body)).await?;
    let envelope = Envelope::parse(&text)?;
    if envelope.code != 0 {
        bail!("额度查询被拒绝：{}", envelope.msg);
    }
    let inner = envelope
        .data
        .get("Response")
        .and_then(|r| r.get("Data"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let accounts = inner
        .get("Accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut result = Vec::new();
    let mut total = 0.0;
    for row in &accounts {
        let size = number(row, "CycleCapacitySize");
        let cycle_remain = number(row, "CycleCapacityRemain");
        let cycle_used = number(row, "CycleCapacityUsed");
        let capacity_remain = number(row, "CapacityRemain");
        // A positive cycle size means the quota is cyclical and the cycle
        // remainder is the authoritative figure; otherwise the lifetime
        // remainder applies.
        let remain = if size > 0.0 || cycle_remain > 0.0 || cycle_used > 0.0 {
            cycle_remain
        } else {
            capacity_remain
        }
        .max(0.0);
        total += remain;
        let name = row
            .get("PackageName")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .unwrap_or("(unnamed)")
            .to_string();
        result.push(CreditAccount {
            name,
            remain,
            size: if size > 0.0 {
                size
            } else {
                number(row, "CapacitySize")
            },
            cycle_end: row
                .get("CycleEndTime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            unlimited: false,
        });
    }
    let cycle_end = result
        .first()
        .map(|a| a.cycle_end.clone())
        .unwrap_or_default();
    Ok(Balance {
        total,
        accounts: result,
        cycle_end,
        unlimited: false,
    })
}

/// Enterprise quota read: one cycle figure rather than a package list.
/// `-1` is the upstream's "no cap" marker, carried as a flag so it can never be
/// mistaken for a balance.
async fn fetch_enterprise_credits(account: &SavedAccount) -> Result<Balance> {
    let region = account.region();
    let url = format!(
        "{}/v2/billing/meter/get-enterprise-user-usage",
        region.billing_base()
    );
    let headers = wire::billing_headers(
        region,
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
    );
    let text = send(&url, "POST", &headers, Some("")).await?;
    let envelope = Envelope::parse(&text)?;
    if envelope.code != 0 {
        bail!("企业额度查询被拒绝：{}", envelope.msg);
    }
    // The official reader accepts the payload at data.data, data, or the
    // envelope itself, so all three are tried.
    let mut candidates: Vec<Value> = Vec::new();
    if let Some(inner) = envelope.data.get("data") {
        candidates.push(inner.clone());
    }
    candidates.push(envelope.data.clone());
    if let Some(inner) = envelope.document.get("data") {
        candidates.push(inner.clone());
    }
    candidates.push(envelope.document.clone());

    let mut limit: Option<f64> = None;
    let mut used: Option<f64> = None;
    let mut reset = String::new();
    for source in &candidates {
        let candidate = if source.get("limitNum").is_some() {
            source.get("limitNum").and_then(Value::as_f64)
        } else if source.get("limit_num").is_some() {
            source.get("limit_num").and_then(Value::as_f64)
        } else {
            continue;
        };
        limit = candidate;
        used = if source.get("credit").is_some() {
            source.get("credit").and_then(Value::as_f64)
        } else {
            source.get("used_num").and_then(Value::as_f64)
        };
        reset = source
            .get("cycleResetTime")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        break;
    }
    // No recognisable quota field is a hard error: reporting 0 would be a
    // confident wrong number rather than a visible failure.
    let resolved_limit = limit.ok_or_else(|| anyhow::anyhow!("企业额度响应里没有额度字段"))?;
    if resolved_limit == -1.0 {
        return Ok(Balance {
            total: 0.0,
            accounts: vec![CreditAccount {
                name: "企业额度".into(),
                remain: 0.0,
                size: 0.0,
                cycle_end: reset,
                unlimited: true,
            }],
            cycle_end: String::new(),
            unlimited: true,
        });
    }
    let resolved_used = used.ok_or_else(|| anyhow::anyhow!("企业额度响应里有额度但没有用量"))?;
    let remain = (resolved_limit - resolved_used).max(0.0);
    Ok(Balance {
        total: remain,
        accounts: vec![CreditAccount {
            name: "企业额度".into(),
            remain,
            size: resolved_limit,
            cycle_end: reset.clone(),
            unlimited: false,
        }],
        cycle_end: reset,
        unlimited: false,
    })
}

/// Daily check-in. The gateway answers a non-zero business code with a readable
/// message ("already checked in today") rather than an error, so the body is
/// returned rather than thrown.
pub async fn checkin(account: &SavedAccount) -> Result<CheckinOutcome> {
    let region = account.region();
    let url = format!("{}/billing/meter/daily-checkin", region.chat_base());
    let headers = wire::billing_headers(
        region,
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
    );
    let text = send(&url, "POST", &headers, Some("{}")).await?;
    let envelope = Envelope::parse(&text)?;
    Ok(CheckinOutcome {
        ok: envelope.code == 0,
        message: if envelope.msg.is_empty() {
            text.chars().take(120).collect()
        } else {
            envelope.msg
        },
    })
}

/// 发一个 JSON 请求，并把「会话失效」从「返回异常」里分出来。
///
/// 网关的 401 是一页 HTML，如果只看 body 就只能报「非 JSON」，那对用户毫无
/// 帮助：真正要做的是重新登录。
async fn send(
    url: &str,
    method: &str,
    headers: &wire::Headers,
    body: Option<&str>,
) -> Result<String> {
    let (status, text) = http::request_with_status(url, method, headers, body).await?;
    if status == 401 || status == 403 {
        bail!("账号会话已失效（HTTP {status}），请重新运行 `wbhub login`");
    }
    Ok(text)
}

fn number(row: &Value, key: &str) -> f64 {
    match row.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// A response whose payload may sit under `data` or at the top level.
#[derive(Debug)]
struct Envelope {
    code: i64,
    msg: String,
    data: Value,
    document: Value,
}

impl Envelope {
    fn parse(text: &str) -> Result<Envelope> {
        let document: Value = serde_json::from_str(text).map_err(|_| {
            anyhow::anyhow!(
                "上游返回了非 JSON：{}",
                text.chars().take(160).collect::<String>()
            )
        })?;
        let code = document.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = document
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let data = document.get("data").cloned().unwrap_or_else(|| json!({}));
        Ok(Envelope {
            code,
            msg,
            data,
            document,
        })
    }
}

/// Reads a `data` payload that may be absent, for callers that only need the
/// optional shape.
#[allow(dead_code)]
fn data_or_object(document: &Value) -> Value {
    document.get("data").cloned().unwrap_or_else(|| json!({}))
}

/// Every catalogue row, resolved against the active account, with the raw
/// document retained so `models --raw` can show what the upstream sent.
pub struct Catalogue {
    pub models: Vec<wire::ProtoModel>,
}

impl Catalogue {
    pub fn ids(&self) -> Vec<String> {
        self.models.iter().map(|m| m.id.clone()).collect()
    }

    pub fn multiplier_of(&self, id: &str) -> f64 {
        self.models
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.multiplier())
            .unwrap_or(-1.0)
    }
}

/// Parses a catalogue document that was already fetched, for tests and for the
/// offline preview command.
pub fn catalogue_from(document: &str) -> Result<Catalogue> {
    let models = wire::parse_models(document).map_err(|e| anyhow::anyhow!(e))?;
    Ok(Catalogue { models })
}

/// Convenience for a caller that wants the model list as plain strings.
pub async fn model_ids(account: &SavedAccount) -> Result<Vec<String>> {
    let models = fetch_models(account)
        .await
        .context("reading the model catalogue")?;
    Ok(models.into_iter().map(|m| m.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_zero_code_surfaces_the_upstream_message() {
        let envelope = Envelope::parse(r#"{"code":1,"msg":"already checked in today"}"#).unwrap();
        assert_eq!(envelope.code, 1);
        assert_eq!(envelope.msg, "already checked in today");
    }

    #[test]
    fn a_missing_code_reads_as_success() {
        // Some endpoints answer with a bare object and no envelope at all.
        let envelope = Envelope::parse(r#"{"data":{"ok":true}}"#).unwrap();
        assert_eq!(envelope.code, 0);
    }

    #[test]
    fn a_non_json_body_is_reported_with_a_preview() {
        let error = Envelope::parse("<html>502</html>").unwrap_err();
        assert!(error.to_string().contains("<html>502</html>"));
    }

    #[test]
    fn numeric_fields_accept_both_shapes() {
        let row = json!({"a": 12.5, "b": "3.25", "c": null});
        assert_eq!(number(&row, "a"), 12.5);
        assert_eq!(number(&row, "b"), 3.25);
        assert_eq!(number(&row, "c"), 0.0);
    }
}
