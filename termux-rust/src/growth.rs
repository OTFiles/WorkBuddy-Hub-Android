//! 成长中心：连登、补签、兑换、抽奖、猫猫旅行、夜猫子。
//!
//! 这一层对应原项目里的 `GrowthClient.kt` + `AutoTask.kt`。成长接口挂在
//! `copilot.tencent.com` 上，但鉴权用的是 billing 那一套头，所以复用
//! `wire` 里的规则而不是聊天路径的规则。
//!
//! 这里每个调用在上游都是幂等的：重复动作返回的是业务码而不是错误，调用方
//! 把返回的消息当成正常结果处理，所以整个日常流程可以每天重复跑。

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::http;
use crate::store::SavedAccount;
use crate::wire::{self, Region};

/// 单步结果，用来如实汇报到底发生了什么。
#[derive(Debug, Clone)]
pub struct Step {
    pub ok: bool,
    pub message: String,
}

impl Step {
    fn ok(message: impl Into<String>) -> Step {
        Step {
            ok: true,
            message: message.into(),
        }
    }

    fn no(message: impl Into<String>) -> Step {
        Step {
            ok: false,
            message: message.into(),
        }
    }
}

/// 连登状态：连续天数、补签卡、以及兑换档位。
#[derive(Debug, Default, Clone)]
pub struct StreakState {
    pub days: i64,
    pub next_tier: String,
    pub next_tier_remaining: i64,
    pub makeup_cards: i64,
    pub tiers: Vec<Tier>,
}

#[derive(Debug, Clone)]
pub struct Tier {
    pub id: String,
    pub status: String,
    pub days: i64,
    pub credit: i64,
    pub energy: i64,
    pub cards: i64,
    pub chances: i64,
}

impl StreakState {
    /// 已解锁但还没兑换的档位。
    pub fn redeemable(&self) -> Vec<&Tier> {
        self.tiers
            .iter()
            .filter(|t| t.status != "locked" && t.status != "claimed")
            .collect()
    }
}

/// 一只在路上的猫。
#[derive(Debug, Default, Clone)]
pub struct TravelState {
    pub state: String,
    pub daily_limit_reached: bool,
    pub record_id: i64,
    pub reward_credit: i64,
    pub arrive_at: i64,
}

impl TravelState {
    pub fn traveling(&self) -> bool {
        self.state == "traveling"
    }

    pub fn arrived(&self) -> bool {
        self.state == "arrived"
    }

    /// 到站前还剩几分钟，向下取整。
    pub fn minutes_until(&self, now_seconds: i64) -> i64 {
        if self.arrive_at <= 0 {
            0
        } else {
            ((self.arrive_at - now_seconds) / 60).max(0)
        }
    }
}

/// 成长任务列表里的一行。
#[derive(Debug, Clone)]
pub struct GrowthTask {
    pub code: String,
    pub current: i64,
    pub target: i64,
    pub accept_status: String,
}

impl GrowthTask {
    pub fn claimed(&self) -> bool {
        self.accept_status == "claimed"
    }

    /// 还差多少次才能拿奖励。
    pub fn remaining(&self) -> i64 {
        if self.target <= 0 {
            0
        } else {
            (self.target - self.current).max(0)
        }
    }
}

/// 四个目的地给的奖励一样，所以没有最优选择，挑一个固定的即可。
const LOCATION_ID: i64 = 4;
const NIGHT_TASK_CODE: &str = "black_cat";
const NIGHT_MODEL_ID: &str = "glm-5.2";
const NIGHT_MODEL_NAME: &str = "GLM-5.2";

// ---------------------------------------------------------------------- //
// 读状态
// ---------------------------------------------------------------------- //

/// 连登状态。只有传输或协议失败才会报错；业务层拒绝在读取上不常见。
pub async fn streak(account: &SavedAccount) -> Result<StreakState> {
    let data = growth(account, "GET", "/activity/growth/streak", None).await?;
    let streak = data.get("streak").cloned().unwrap_or_else(|| json!({}));
    let cards = data
        .get("makeup_cards")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let redemption = data
        .get("redemption_status")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let raw_tiers = redemption.get("tiers").and_then(Value::as_array);

    // 每个档位的状态是平铺在三个字段里的，而不是塞在档位行内部，所以只能按 id
    // 反查回来。
    let status_of = |id: &str| -> String {
        let key = match id {
            "7d" => "tier_7d_status",
            "14d" => "tier_14d_status",
            "28d" => "tier_28d_status",
            _ => return String::new(),
        };
        redemption
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    let mut tiers = Vec::new();
    for row in raw_tiers.map(Vec::as_slice).unwrap_or_default() {
        let id = row
            .get("tier")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        tiers.push(Tier {
            status: status_of(&id),
            id,
            days: int(row, "days"),
            credit: int(row, "credit"),
            energy: int(row, "energy"),
            cards: int(row, "cards"),
            chances: int(row, "chances"),
        });
    }

    Ok(StreakState {
        days: int(&streak, "days"),
        next_tier: streak
            .get("next_tier")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        next_tier_remaining: int(&streak, "next_tier_remaining"),
        makeup_cards: int(&cards, "balance"),
        tiers,
    })
}

/// 昨天是不是漏签了。热力图一天一格，漏签的那格分数为 0。
pub async fn missed_yesterday(account: &SavedAccount) -> bool {
    let Ok(data) = growth(account, "GET", "/activity/growth/heatmap", None).await else {
        return false;
    };
    let Some(cells) = data.get("cells").and_then(Value::as_array) else {
        return false;
    };
    let yesterday = day_string(-1);
    for cell in cells {
        let date = cell.get("date").and_then(Value::as_str).unwrap_or("");
        if date.chars().take(10).collect::<String>() == yesterday {
            return int(cell, "score") == 0;
        }
    }
    false
}

/// 还剩几次抽奖。只有兑换才会发这个次数。
pub async fn lottery_chances(account: &SavedAccount) -> i64 {
    match growth(account, "GET", "/activity/growth/lottery/summary", None).await {
        Ok(data) => int(&data, "chances"),
        Err(_) => 0,
    }
}

/// 猫当前在哪。
pub async fn travel_status(account: &SavedAccount) -> Result<TravelState> {
    let data = growth(account, "GET", "/activity/growth/buddy/travel/status", None).await?;
    Ok(TravelState {
        state: data
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        daily_limit_reached: data
            .get("daily_limit_reached")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        record_id: int(&data, "record_id"),
        reward_credit: int(&data, "reward_credit"),
        arrive_at: int(&data, "arrive_at"),
    })
}

/// 读成长任务列表。
///
/// 进度有两种形态：嵌套的 `progress.current/target` 或者平铺的
/// `current`/`target`，两种都得接，因为不同任务用的形态不一样。
///
/// 失败会向上传递：把「请求挂了」报成「没有任务」会让用户以为账号没问题。
pub async fn tasks(account: &SavedAccount) -> Result<Vec<GrowthTask>> {
    let data = growth(account, "GET", "/v2/activity/growth/tasks", None).await?;
    let Some(rows) = data.get("tasks").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(rows
        .iter()
        .filter_map(|row| {
            let code = row.get("task_code").and_then(Value::as_str)?.to_string();
            let progress = row.get("progress");
            Some(GrowthTask {
                code,
                current: progress
                    .map(|p| int(p, "current"))
                    .unwrap_or_else(|| int(row, "current")),
                target: progress
                    .map(|p| int(p, "target"))
                    .unwrap_or_else(|| int(row, "target")),
                accept_status: row
                    .get("accept_status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

/// 找一条任务，找不到就是 `None`。
pub async fn task(account: &SavedAccount, code: &str) -> Result<Option<GrowthTask>> {
    Ok(tasks(account).await?.into_iter().find(|t| t.code == code))
}

// ---------------------------------------------------------------------- //
// 动作
// ---------------------------------------------------------------------- //

/// 兑换一个档位。没到的档位会被上游拒绝，这是预期内的。
pub async fn redeem_tier(account: &SavedAccount, tier: &str) -> Step {
    step(
        account,
        "/activity/growth/redeem",
        Some(&json!({"tier": tier, "client_token": client_token()})),
    )
    .await
}

/// 抽一次奖。奖品结构随活动变化，只做记录。
pub async fn lottery_draw(account: &SavedAccount) -> Step {
    step(
        account,
        "/activity/growth/lottery/draw",
        Some(&json!({"client_token": client_token()})),
    )
    .await
}

/// 花一张补签卡补某个过去的日期，保住连登。
pub async fn use_makeup_card(account: &SavedAccount, date: &str) -> Step {
    step(
        account,
        "/activity/growth/makeup-cards/use",
        Some(&json!({"target_date": date})),
    )
    .await
}

/// 新手礼包。每个账号一次，已领过会以消息形式返回。
pub async fn claim_gift(account: &SavedAccount) -> Step {
    step_on_billing(account, "/billing/meter/claim-gift", Some(&json!({}))).await
}

/// 活动补偿，活动进行时才有。
pub async fn claim_compensation(account: &SavedAccount) -> Step {
    step_on_billing(
        account,
        "/billing/meter/claim-compensation",
        Some(&json!({})),
    )
    .await
}

/// 报名任务。报名之后进度才开始算。
pub async fn accept_tasks(account: &SavedAccount, codes: &[String]) -> Step {
    step(
        account,
        "/v2/activity/growth/tasks/accept",
        Some(&json!({ "task_codes": codes })),
    )
    .await
}

async fn depart(account: &SavedAccount, location_id: i64) -> Step {
    step(
        account,
        "/activity/growth/buddy/travel/depart",
        Some(&json!({ "location_id": location_id })),
    )
    .await
}

/// 领旅行奖励，返回（是否成功、奖励积分、消息）。
async fn claim_travel(account: &SavedAccount, record_id: i64) -> (bool, i64, String) {
    let body = json!({ "record_id": record_id }).to_string();
    let url = format!(
        "{}/activity/growth/buddy/travel/claim",
        account.region().chat_base()
    );
    let headers = growth_headers(account);
    let text = match http::request_with_status(&url, "POST", &headers, Some(&body)).await {
        Ok((status, _)) if status == 401 || status == 403 => {
            return (
                false,
                0,
                format!("账号会话已失效（HTTP {status}），请重新运行 `wbhub login`"),
            )
        }
        Ok((_, text)) => text,
        Err(error) => return (false, 0, format!("{error}")),
    };
    let Ok(envelope) = serde_json::from_str::<Value>(&text) else {
        return (false, 0, text.chars().take(120).collect());
    };
    if int(&envelope, "code") != 0 {
        let message = envelope
            .get("msg")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .unwrap_or("未知错误");
        return (false, 0, message.to_string());
    }
    let data = envelope.get("data").cloned().unwrap_or_else(|| json!({}));
    (true, int(&data, "reward_credit"), String::new())
}

/// 领养第一只 Buddy。今天的活跃上报还没落地时上游会拒绝，所以这里如实汇报而
/// 不重试。
async fn adopt(account: &SavedAccount, steps: &mut Vec<Step>) {
    let agreement = step(
        account,
        "/activity/growth/buddy/agreement",
        Some(&json!({"agree": true})),
    )
    .await;
    if !agreement.ok {
        steps.push(Step::no(format!(
            "Buddy 协议签署失败：{}",
            agreement.message
        )));
        return;
    }
    let first = step(account, "/activity/growth/buddy/first", Some(&json!({}))).await;
    steps.push(if first.ok {
        Step::ok("已领养第一只 Buddy")
    } else {
        Step::no(format!("领养 Buddy 未生效：{}", first.message))
    });
}

/// 猫猫旅行推进一步：没有猫就先领养，然后派出去，到站了再收。
///
/// 上游按北京时间零点重置当天的派发额度，所以同一天第二次调用会报「今日已
/// 派出过」，而不是又放出去一次。
pub async fn travel_once(account: &SavedAccount) -> Vec<Step> {
    let mut steps = Vec::new();
    let status = match travel_status(account).await {
        Ok(status) => status,
        Err(error) => {
            steps.push(Step::no(format!("旅行状态读取失败：{error}")));
            return steps;
        }
    };

    // 状态为空说明还没有猫；领养它同时会发「第一只 Buddy」那 300 积分的奖励，
    // 而那个奖励以今天的活跃上报完成为前提。
    if status.state.is_empty() {
        adopt(account, &mut steps).await;
    }

    if status.arrived() && status.record_id != 0 {
        let (ok, credit, message) = claim_travel(account, status.record_id).await;
        steps.push(if ok {
            if credit > 0 {
                Step::ok(format!("猫猫旅行奖励已领取：+{credit} 积分"))
            } else {
                Step::ok("猫猫旅行奖励已领取")
            }
        } else {
            Step::no(format!("领取旅行奖励失败：{message}"))
        });
        return steps;
    }

    if status.traveling() {
        // 说清楚什么时候回来，而不只是「稍后」：一趟一小时，光说等一下会让人
        // 一直守着屏幕。
        let now = crate::store::now_seconds();
        let minutes = status.minutes_until(now);
        let eta = if status.arrive_at <= 0 {
            String::new()
        } else if minutes <= 0 {
            "（即将到站，再执行一次即可领取）".to_string()
        } else {
            format!("，约 {minutes} 分钟后到站")
        };
        let clock = if status.arrive_at > 0 {
            crate::cli::clock_of(status.arrive_at)
        } else {
            String::new()
        };
        let reward = if status.reward_credit > 0 {
            format!("（预计 +{} 积分）", status.reward_credit)
        } else {
            String::new()
        };
        steps.push(Step::no(format!(
            "猫猫还在路上，{clock} 到站{eta}；到站后再跑一次即可领取{reward}"
        )));
        return steps;
    }

    if status.daily_limit_reached {
        steps.push(Step::no("今日已派出过猫猫"));
        return steps;
    }

    let departed = depart(account, LOCATION_ID).await;
    steps.push(if departed.ok {
        // 再读一次状态，好把回程时间也报出来。
        let after = travel_status(account).await.ok();
        let clock = after
            .as_ref()
            .filter(|s| s.arrive_at > 0)
            .map(|s| crate::cli::clock_of(s.arrive_at))
            .unwrap_or_default();
        let minutes = after
            .as_ref()
            .map(|s| s.minutes_until(crate::store::now_seconds()))
            .unwrap_or(0);
        let eta = if clock.is_empty() {
            String::new()
        } else if minutes <= 0 {
            "，即将到站".to_string()
        } else {
            format!("，约 {minutes} 分钟后（{clock}）到站")
        };
        Step::ok(format!("猫猫已出发去旅行{eta}；到站后再跑一次即可领取奖励"))
    } else {
        Step::no(format!("派出失败：{}", departed.message))
    });
    steps
}

// ---------------------------------------------------------------------- //
// 活跃上报
// ---------------------------------------------------------------------- //

/// 上报一条 `chat_request_send` 活跃事件。
///
/// 发的是完整客户端形状而不是最小三字段：上游会先 200 再静默丢掉短 body，而且
/// 字段列表本身决定了积分算到哪个模型上。`userId` 必填，否则事件不计。
pub async fn report_chat_activity(
    account: &SavedAccount,
    conversation_id: &str,
    request_id: &str,
    model_id: &str,
    model_name: &str,
) -> Step {
    let now = crate::bridge::now_millis();
    let request_id = if request_id.is_empty() {
        conversation_id
    } else {
        request_id
    };
    let event = json!({
        "eventCode": "chat_request_send",
        "timestamp": now,
        "reportDelay": 0,
        "mode": "craft",
        "conversationId": conversation_id,
        "requestId": request_id,
        "inputLength": 12,
        "requestModelId": model_id,
        "requestModelName": if model_name.is_empty() { model_id } else { model_name },
        "isPlan": false,
        "isAutoExecuteTerminal": false,
        "isAutoModify": false,
        "codebaseEnable": false,
        "maxToken": 0,
        "maxSteps": 0,
        "temperature": 0,
        "maxRetries": 0,
        "mentionContexts": [],
        "knowledgeId": [],
        "knowledgeName": [],
        "codebaseId": "",
        "mentionContextCount": 0,
        "command": "",
        "expertId": "",
        "recommendId": "",
        "skillId": "",
        "skillCount": 0,
        "totalCount": 0,
        "fileUri": "",
        "presentAt": now,
        "traceId": "",
        "rootRequestId": request_id,
        "parentConversationId": conversation_id,
        "agentName": "default",
        "agentType": "conversation",
        "userId": account.uid,
    });
    // 上游要的是裸数组而不是对象包装：发 {"events":[...]} 会先返回 200，再用
    // "eventCode is empty or not string" 拒掉，所以这里跳过包装层。
    let body = Value::Array(vec![event]).to_string();
    let url = format!("{}/v2/report", account.region().billing_base());
    let headers = wire::billing_headers(
        account.region(),
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
    );
    match http::request_with_status(&url, "POST", &headers, Some(&body)).await {
        Ok((status, _)) if status == 401 || status == 403 => Step::no(format!(
            "账号会话已失效（HTTP {status}），请重新运行 `wbhub login`"
        )),
        Ok((_, text)) => {
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .map(|v| int(&v, "code"))
                .unwrap_or(0);
            if code == 0 {
                Step::ok("活跃已上报")
            } else {
                let message = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("msg").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| text.chars().take(120).collect());
                Step::no(format!("活跃上报失败：{message}"))
            }
        }
        Err(error) => Step::no(format!("活跃上报失败：{error}")),
    }
}

/// 夜猫子计数窗口（本地时间 23:00–08:00）是否开着。
pub fn in_night_window() -> bool {
    use chrono::Timelike;
    // 23:00 到次日 08:00，也就是「不在 8..23 之间」。
    !(8..23).contains(&chrono::Local::now().hour())
}

/// 上报一次夜猫子对话。
///
/// 这个任务统计的是 23:00–08:00 窗口内的 glm-5.2 对话，所以事件带的是那个模型
/// id 而不是默认的。
pub async fn report_night_activity(account: &SavedAccount, index: i64) -> Step {
    let stamp = crate::bridge::now_millis();
    let conversation_id = format!("wbhub-night-{stamp}-{index}");
    report_chat_activity(
        account,
        &conversation_id,
        &conversation_id,
        NIGHT_MODEL_ID,
        NIGHT_MODEL_NAME,
    )
    .await
}

// ---------------------------------------------------------------------- //
// 日常流程
// ---------------------------------------------------------------------- //

/// 每天签到之后跑的那套。
///
/// 顺序是有讲究的：先用补签卡把昨天补上（断一天要多花七天补回来，比下面任何
/// 一项奖励都重要），再收一次性奖励，然后兑换所有已解锁档位——兑换才发抽奖
/// 次数，所以抽奖放最后。
///
/// 每一步在上游都是幂等的，所以每次签到都能重复跑，不会重复领奖。
pub async fn run_daily(account: &SavedAccount) -> Vec<Step> {
    let mut steps = Vec::new();
    // 连登和它的档位只在国内版有；国际版账号没有成长中心可驱动。
    if account.region() != Region::Cn {
        steps.push(Step::no("国际版账号没有成长中心，跳过日常奖励"));
        return steps;
    }

    // 1. 先保住连登。
    make_up_yesterday(account, &mut steps).await;

    // 2. 一次性领取。这两项领过之后会返回「已领取」这种正常业务码。
    let gift = claim_gift(account).await;
    if gift.ok {
        steps.push(Step::ok("新手礼包已领取"));
    }
    let compensation = claim_compensation(account).await;
    if compensation.ok {
        steps.push(Step::ok("补偿已领取"));
    }

    // 3. 兑换所有已解锁档位。
    let streak = match streak(account).await {
        Ok(streak) => streak,
        Err(error) => {
            steps.push(Step::no(format!("连登状态读取失败：{error}")));
            return steps;
        }
    };
    let redeemable: Vec<(String, i64, i64)> = streak
        .redeemable()
        .into_iter()
        .map(|t| (t.id.clone(), t.credit, t.chances))
        .collect();
    for (id, credit, chances) in redeemable {
        let outcome = redeem_tier(account, &id).await;
        if outcome.ok {
            let mut message = format!("兑换 {id} 档：+{credit} 积分");
            if chances > 0 {
                message.push_str(&format!("、抽奖 ×{chances}"));
            }
            steps.push(Step::ok(message));
        } else {
            // 上游拒绝的档位只报一次；同一轮里没有可重试的东西。
            steps.push(Step::no(format!("兑换 {id} 档未生效：{}", outcome.message)));
        }
    }

    // 4. 把抽奖次数用完。次数要在兑换之后读，因为次数正是兑换发的。
    let chances = lottery_chances(account).await;
    if chances > 0 {
        let mut drawn = 0;
        for index in 1..=chances {
            let outcome = lottery_draw(account).await;
            if !outcome.ok {
                steps.push(Step::no(format!(
                    "第 {index} 次抽奖失败：{}",
                    outcome.message
                )));
                break;
            }
            drawn += 1;
        }
        if drawn > 0 {
            steps.push(Step::ok(format!("抽奖完成 {drawn} 次")));
        }
    }

    steps
}

/// 昨天漏签就花一张补签卡补上。
async fn make_up_yesterday(account: &SavedAccount, steps: &mut Vec<Step>) {
    if !missed_yesterday(account).await {
        return;
    }
    let Ok(state) = streak(account).await else {
        return;
    };
    if state.makeup_cards <= 0 {
        return;
    }
    let yesterday = day_string(-1);
    let outcome = use_makeup_card(account, &yesterday).await;
    steps.push(if outcome.ok {
        Step::ok(format!("已用补签卡补签 {yesterday}（保住连登）"))
    } else {
        Step::no(format!("补签 {yesterday} 失败：{}", outcome.message))
    });
}

/// 夜猫子任务：只是把还差的次数报满。
///
/// 窗口是本地 23:00–08:00，窗口外上游不计分，所以直接拒绝而不是白跑一遍。
/// 任务要先报名进度才会动；只报缺的次数，重复跑也不会超。
pub async fn run_night_owl(account: &SavedAccount) -> Vec<Step> {
    if account.region() != Region::Cn {
        return vec![Step::no("国际版账号没有成长中心")];
    }
    if !in_night_window() {
        return vec![Step::no("不在夜猫子时段（23:00–08:00），此时上报不计分")];
    }
    let task = match task(account, NIGHT_TASK_CODE).await {
        Ok(Some(task)) => task,
        Ok(None) => return vec![Step::no("未找到夜猫子任务")],
        Err(error) => return vec![Step::no(format!("任务进度读取失败：{error}"))],
    };
    if task.claimed() || task.remaining() <= 0 {
        return vec![Step::ok("夜猫子进度已达标")];
    }

    let mut steps = Vec::new();
    // 报名才让上报算数；这个动作幂等。
    if task.accept_status == "not_accepted" {
        let accepted = accept_tasks(account, &[NIGHT_TASK_CODE.to_string()]).await;
        if !accepted.ok {
            return vec![Step::no(format!("报名失败：{}", accepted.message))];
        }
        steps.push(Step::ok("已报名夜猫子任务"));
    }

    let total = task.remaining();
    let mut done = 0;
    for index in 1..=total {
        let outcome = report_night_activity(account, index).await;
        if !outcome.ok {
            steps.push(Step::no(format!(
                "第 {index} 次上报失败：{}",
                outcome.message
            )));
            break;
        }
        done += 1;
        // 拉开间隔，免得整批看起来像脚本流量。
        if index < total {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
    }
    if done > 0 {
        steps.push(Step::ok(format!(
            "夜猫子已上报 {done} 次（还差 {} 次）",
            total - done
        )));
    }
    steps
}

// ---------------------------------------------------------------------- //
// 传输
// ---------------------------------------------------------------------- //

fn growth_headers(account: &SavedAccount) -> wire::Headers {
    wire::growth_headers(
        account.region(),
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
    )
}

/// 发一个成长请求并拆开信封。
///
/// 成长接口返回 `{code, msg, data}`；非零 code 用散文写业务原因，调用方把它
/// 显示出来而不是当成硬失败。
async fn growth(
    account: &SavedAccount,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let url = format!("{}{path}", account.region().chat_base());
    let payload = body.map(|b| b.to_string());
    let (status, text) =
        http::request_with_status(&url, method, &growth_headers(account), payload.as_deref())
            .await?;
    if status == 401 || status == 403 {
        bail!("账号会话已失效（HTTP {status}），请重新运行 `wbhub login`");
    }
    let envelope: Value = serde_json::from_str(&text).map_err(|_| {
        anyhow::anyhow!(
            "成长接口返回了非 JSON：{}",
            text.chars().take(160).collect::<String>()
        )
    })?;
    let code = int(&envelope, "code");
    if code != 0 {
        let message = envelope
            .get("msg")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| text.chars().take(160).collect());
        bail!("{message}");
    }
    Ok(envelope.get("data").cloned().unwrap_or_else(|| json!({})))
}

/// 跑一个业务拒绝算正常结果的成长调用。
async fn step(account: &SavedAccount, path: &str, body: Option<&Value>) -> Step {
    let url = format!("{}{path}", account.region().chat_base());
    step_raw(&url, body, growth_headers(account)).await
}

/// 同一件事，但落在 billing 域名上。
async fn step_on_billing(account: &SavedAccount, path: &str, body: Option<&Value>) -> Step {
    let url = format!("{}{path}", account.region().billing_base());
    let headers = wire::billing_headers(
        account.region(),
        &account.access_token,
        &account.uid,
        &account.domain,
        account.enterprise_id(),
    );
    step_raw(&url, body, headers).await
}

async fn step_raw(url: &str, body: Option<&Value>, headers: wire::Headers) -> Step {
    let payload = body.map(|b| b.to_string());
    match http::request_with_status(url, "POST", &headers, payload.as_deref()).await {
        Err(error) => Step::no(format!("{error}")),
        Ok((status, _)) if status == 401 || status == 403 => Step::no(format!(
            "账号会话已失效（HTTP {status}），请重新运行 `wbhub login`"
        )),
        Ok((status, text)) => {
            // 非 JSON 一律算失败。早先这里把网关的 HTML 错误页当成成功返回，于是
            // 一个 401 会显示成「礼包已领取」——比报错更糟，因为它是假的。
            let Some(envelope) = serde_json::from_str::<Value>(&text).ok() else {
                return Step::no(format!(
                    "接口返回异常（HTTP {status}）：{}",
                    text.chars().take(120).collect::<String>()
                ));
            };
            let code = int(&envelope, "code");
            let message = envelope
                .get("msg")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or("OK")
                .to_string();
            Step {
                ok: code == 0,
                message,
            }
        }
    }
}

/// 形状跟网页前端生成的一样即可的幂等 token。
fn client_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `yyyy-MM-dd`，相对今天偏移若干天，本地时区。
fn day_string(offset_days: i64) -> String {
    let when = chrono::Local::now() + chrono::Duration::days(offset_days);
    when.format("%Y-%m-%d").to_string()
}

fn int(value: &Value, key: &str) -> i64 {
    match value.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unredeemed_tiers_are_offered() {
        let state = StreakState {
            tiers: vec![
                Tier {
                    id: "7d".into(),
                    status: "unlocked".into(),
                    days: 7,
                    credit: 100,
                    energy: 0,
                    cards: 0,
                    chances: 1,
                },
                Tier {
                    id: "14d".into(),
                    status: "claimed".into(),
                    days: 14,
                    credit: 200,
                    energy: 0,
                    cards: 0,
                    chances: 0,
                },
                Tier {
                    id: "28d".into(),
                    status: "locked".into(),
                    days: 28,
                    credit: 400,
                    energy: 0,
                    cards: 0,
                    chances: 0,
                },
            ],
            ..Default::default()
        };
        let ids: Vec<&str> = state.redeemable().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["7d"]);
    }

    #[test]
    fn travel_eta_floors_at_zero() {
        let state = TravelState {
            state: "traveling".into(),
            arrive_at: 1_000,
            ..Default::default()
        };
        assert_eq!(state.minutes_until(400), 10);
        assert_eq!(state.minutes_until(1_000), 0);
        assert_eq!(state.minutes_until(9_999), 0);
        assert!(state.traveling());
        assert!(!state.arrived());
    }

    #[test]
    fn a_task_that_has_not_started_owes_its_whole_target() {
        let task = GrowthTask {
            code: "black_cat".into(),
            current: 0,
            target: 3,
            accept_status: "accepted".into(),
        };
        assert_eq!(task.remaining(), 3);
        assert!(!task.claimed());

        let done = GrowthTask {
            current: 5,
            target: 3,
            ..task.clone()
        };
        assert_eq!(done.remaining(), 0, "over-delivery must not go negative");

        let unknown = GrowthTask { target: 0, ..task };
        assert_eq!(
            unknown.remaining(),
            0,
            "a target of 0 means 'no work known'"
        );
    }

    #[test]
    fn day_strings_walk_back_from_today() {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let yesterday = (chrono::Local::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(day_string(0), today);
        assert_eq!(day_string(-1), yesterday);
    }

    #[test]
    fn numbers_arrive_in_both_shapes() {
        let row = json!({"a": 3, "b": "4", "c": 1.9, "d": null});
        assert_eq!(int(&row, "a"), 3);
        assert_eq!(int(&row, "b"), 4);
        assert_eq!(int(&row, "c"), 1);
        assert_eq!(int(&row, "d"), 0);
    }

    #[test]
    fn client_tokens_differ_per_call() {
        assert_ne!(client_token(), client_token());
    }
}
