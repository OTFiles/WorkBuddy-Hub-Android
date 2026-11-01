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
    /// 上游标了 `Status = 3`（已过期）。过期包不计入总额，但保留下来好解释明细。
    pub expired: bool,
}

/// 一组积分。官方把它分成「套餐基础 / 套餐赠送 / 购买 / 平台奖励」四类。
#[derive(Debug, Clone, Default)]
pub struct CreditGroup {
    pub kind: String,
    pub name: String,
    pub remain: f64,
    pub size: f64,
    pub count: i64,
}

/// Aggregated balance.
#[derive(Debug, Clone, Default)]
pub struct Balance {
    pub total: f64,
    pub accounts: Vec<CreditAccount>,
    pub cycle_end: String,
    pub unlimited: bool,
    /// 账号当前的套餐名，例如「体验版」。免费体验版每月自带 500 积分，那部分**不在**
    /// `get-user-resource` 的返回里，只在汇总接口里。
    pub plan_name: String,
    pub is_paid: bool,
    /// 按官方口径分好的组，用来解释总额是怎么构成的。
    pub groups: Vec<CreditGroup>,
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

/// 查询剩余积分。
///
/// 个人账号走两段：
///
/// 1. `get-user-resource-summary` —— **总额以它为准**。它把积分分成「套餐基础 /
///    套餐赠送 / 购买 / 平台奖励」四组，App 上显示的那个数就是它的 `TotalCredits`。
/// 2. `get-user-resource` —— 逐包明细，用来算最近到期时间、以及 `--detail` 的列表。
///
/// 只查第 2 个是错的：它只返回「平台奖励积分」（签到攒出来的赠送包），不含套餐自带
/// 的基础积分。免费体验版每月那 500 就在基础积分里，所以只查明细会正好少 500——
/// 这正是一开始和 App 对不上的原因。
///
/// 注意这两个路径**不带 `/v2` 前缀**（带了会 404），与 daily-checkin 不同。
pub async fn fetch_credits(account: &SavedAccount) -> Result<Balance> {
    if account.region() == Region::Cn && account.enterprise_id().is_some() {
        return fetch_enterprise_credits(account).await;
    }
    let summary = fetch_personal_summary(account).await;
    let detail = fetch_personal_credits(account).await;
    match (summary, detail) {
        (Ok(summary), Ok(detail)) => Ok(merge(summary, detail)),
        // 汇总接口在老账号/其它站点上可能不存在，这时退回只按明细算，至少还能用。
        (Err(_), Ok(detail)) => Ok(detail),
        (Ok(summary), Err(_)) => Ok(summary),
        (Err(summary_error), Err(_)) => Err(summary_error),
    }
}

fn merge(mut summary: Balance, detail: Balance) -> Balance {
    summary.accounts = detail.accounts;
    summary.cycle_end = detail.cycle_end;
    summary.unlimited = false;
    summary
}

/// 汇总接口：总额与分组。
async fn fetch_personal_summary(account: &SavedAccount) -> Result<Balance> {
    let region = account.region();
    let url = format!(
        "{}/billing/meter/get-user-resource-summary",
        region.billing_base()
    );
    let headers = wire::billing_headers(
        region,
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
    );
    let body = resource_query_body();
    let (status, text) = http::request_with_status(&url, "POST", &headers, Some(&body)).await?;
    if status == 401 || status == 403 {
        bail!("账号会话已失效（HTTP {status}），请重新运行 `wbhub login`");
    }
    if !(200..300).contains(&status) {
        bail!("汇总接口返回 HTTP {status}");
    }
    let envelope = Envelope::parse(&text)?;
    if envelope.code != 0 {
        bail!("额度汇总被拒绝：{}", envelope.msg);
    }
    Ok(parse_summary(&envelope.data))
}

/// 把汇总接口的 `data` 解析成 `Balance`。抽成纯函数是为了能离线测。
fn parse_summary(data: &Value) -> Balance {
    let mut groups = Vec::new();
    if let Some(rows) = data.get("CreditGroups").and_then(Value::as_array) {
        for row in rows {
            groups.push(CreditGroup {
                kind: row
                    .get("Kind")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: row
                    .get("Name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                remain: number(row, "CycleRemainCapacity").max(0.0),
                size: number(row, "CycleTotalCapacity"),
                count: number(row, "TotalCount") as i64,
            });
        }
    }
    // `TotalCredits` 是字符串形式的定点小数（"1876.04"），`number()` 两种都吃。
    let mut total = number(data, "TotalCredits");
    if total == 0.0 && !groups.is_empty() {
        // 万一上游漏了 TotalCredits，分组求和是等价的兜底。
        total = groups.iter().map(|g| g.remain).sum();
    }
    Balance {
        total,
        accounts: Vec::new(),
        cycle_end: String::new(),
        unlimited: false,
        plan_name: data
            .get("SubscriptionPackageName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        is_paid: data
            .get("IsPaidUser")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        groups,
    }
}

/// 查询套餐明细用的请求体。
fn resource_query_body() -> String {
    let now = chrono::Local::now();
    json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRangeBegin": now.format("%Y-%m-%d %H:%M:%S").to_string(),
        // 上游自己也用「往后 101 年」这个粗过滤窗口，照抄即可。
        "PackageEndTimeRangeEnd": (now + chrono::Duration::days(365 * 101))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    })
    .to_string()
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
        let remain = package_remain(size, cycle_remain, cycle_used, capacity_remain);
        // `Status = 3` 只是「已过期」的标记，**不能**据此从总额里扣掉：过期包在
        // 被清零之前还带着余量，而参考实现和上游都是逐行无条件累加的。之前按这个
        // 标记过滤，结果正好比 App 少了 500（那个 500 的包当时已标过期但余量还在）。
        let expired = row.get("Status").and_then(Value::as_i64) == Some(3);
        total += remain;
        let name = row
            .get("PackageName")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .unwrap_or("(无名)")
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
            expired,
        });
    }
    // 「周期结束」取**最近**一个还没结束的周期，而不是第一行的。行的顺序由上游
    // 决定，之前正好取到过一个已过期包（2027 年那个），看起来就像额度快到期了。
    let cycle_end = result
        .iter()
        .filter(|a| !a.expired && a.remain > 0.0 && !a.cycle_end.is_empty())
        .map(|a| a.cycle_end.clone())
        .min()
        .unwrap_or_default();
    Ok(Balance {
        total,
        accounts: result,
        cycle_end,
        // 明细接口本身不含套餐/分组信息，那些由汇总接口补上（见 `merge`）。
        ..Default::default()
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
                expired: false,
            }],
            cycle_end: String::new(),
            unlimited: true,
            ..Default::default()
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
            expired: false,
        }],
        cycle_end: reset,
        unlimited: false,
        ..Default::default()
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

/// 单个套餐的剩余额度。
///
/// 口径照抄上游与 `workbuddy-manager`：有周期额度时只看 Cycle 三字段，并把余量
/// 钳进 `[0, size]`，再用已用量反修正一次；没有周期额度时才退回 `CapacityRemain`。
///
/// 这个钳位不是洁癖：腾讯偶发 `CycleCapacityRemain > CycleCapacitySize` 的脏数据，
/// 只钳负值会**高估**余额，用户就会看到比 App 多的数字。
fn package_remain(size: f64, cycle_remain: f64, cycle_used: f64, capacity_remain: f64) -> f64 {
    if size > 0.0 {
        let mut remain = cycle_remain.max(0.0).min(size);
        let mut used = size - remain;
        if cycle_used > used {
            used = cycle_used;
            if size >= used {
                remain = size - used;
            }
        }
        remain
    } else {
        capacity_remain.max(0.0)
    }
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
    fn the_summary_total_includes_the_plan_base_credits() {
        // 真实响应里 App 显示的就是 TotalCredits；它等于「平台奖励」+「套餐基础」，
        // 而 get-user-resource 只返回前者 —— 这就是少 500 的根源。
        let data = json!({
            "TotalCredits": "1876.04",
            "SubscriptionPackageName": "体验版",
            "IsPaidUser": false,
            "CreditGroups": [
                {"Kind": "plan_base", "Name": "套餐基础积分",
                 "CycleTotalCapacity": "500", "CycleRemainCapacity": "500", "TotalCount": 1},
                {"Kind": "plan_bonus", "Name": "套餐赠送积分",
                 "CycleTotalCapacity": "0", "CycleRemainCapacity": "0", "TotalCount": 0},
                {"Kind": "addon", "Name": "购买积分",
                 "CycleTotalCapacity": "0", "CycleRemainCapacity": "0", "TotalCount": 0},
                {"Kind": "benefit", "Name": "平台奖励积分",
                 "CycleTotalCapacity": "5400", "CycleRemainCapacity": "1376.04", "TotalCount": 12}
            ]
        });
        let balance = parse_summary(&data);
        assert_eq!(balance.total, 1876.04);
        assert_eq!(balance.plan_name, "体验版");
        assert!(!balance.is_paid);
        assert_eq!(balance.groups.len(), 4);
        let base = balance
            .groups
            .iter()
            .find(|g| g.kind == "plan_base")
            .unwrap();
        assert_eq!(base.remain, 500.0);
        let benefit = balance.groups.iter().find(|g| g.kind == "benefit").unwrap();
        assert_eq!(benefit.count, 12);
    }

    #[test]
    fn a_summary_without_a_total_falls_back_to_the_groups() {
        let data = json!({
            "CreditGroups": [
                {"Kind": "plan_base", "CycleRemainCapacity": "500"},
                {"Kind": "benefit", "CycleRemainCapacity": "120.5"}
            ]
        });
        assert_eq!(parse_summary(&data).total, 620.5);
    }

    #[test]
    fn an_expired_package_with_credit_still_counts() {
        // 这条是给「Rust 少 500」那个 bug 钉的桩：过期标记只用于展示，不参与求和。
        assert_eq!(package_remain(500.0, 500.0, 0.0, 500.0), 500.0);
    }

    #[test]
    fn remain_is_clamped_into_the_package_size() {
        // 上游偶发余量大于总量，只钳负值会高估。
        assert_eq!(package_remain(100.0, 250.0, 0.0, 250.0), 100.0);
        assert_eq!(package_remain(100.0, -30.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn the_used_counter_corrects_a_stale_remain() {
        // 余量写的是 100，但已用写到 60：真实余量是 40。
        assert_eq!(package_remain(100.0, 100.0, 60.0, 100.0), 40.0);
        // 已用量超过总量属于脏数据，此时保留按余量算的结果，不要算出负数。
        assert_eq!(package_remain(100.0, 100.0, 150.0, 100.0), 100.0);
    }

    #[test]
    fn without_a_cycle_size_the_lifetime_remainder_is_used() {
        assert_eq!(package_remain(0.0, 0.0, 0.0, 123.0), 123.0);
        assert_eq!(package_remain(0.0, 0.0, 0.0, -5.0), 0.0);
    }

    #[test]
    fn numeric_fields_accept_both_shapes() {
        let row = json!({"a": 12.5, "b": "3.25", "c": null});
        assert_eq!(number(&row, "a"), 12.5);
        assert_eq!(number(&row, "b"), 3.25);
        assert_eq!(number(&row, "c"), 0.0);
    }
}
