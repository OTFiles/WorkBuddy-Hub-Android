//! 连通性自检。
//!
//! `502 transport error` 这类报错几乎都发生在「请求发出去之前」，可能是 DNS、TCP、
//! TLS、也可能是 token 失效。这几层失败在用户眼里长得一模一样，所以这里逐层跑一
//! 遍，指出到底是哪一层断了，并把底层的原始原因一并打印出来。
//!
//! 每一层都用短超时（几秒），因为它要回答的是「现在能不能通」，而不是「最终会不
//! 会通」。

use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use crate::http;
use crate::settings::Settings;
use crate::store::{SavedAccount, Store};
use crate::wire::{self, Region};

/// 单层探测的超时。
const STEP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Verdict {
    fn mark(self) -> &'static str {
        match self {
            Verdict::Ok => "✓",
            Verdict::Warn => "!",
            Verdict::Fail => "✗",
            Verdict::Skip => "-",
        }
    }
}

/// 一层探测的结果。
pub struct Step {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

impl Step {
    fn ok(name: &'static str, detail: impl Into<String>) -> Step {
        Step {
            name,
            verdict: Verdict::Ok,
            detail: detail.into(),
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Step {
        Step {
            name,
            verdict: Verdict::Warn,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Step {
        Step {
            name,
            verdict: Verdict::Fail,
            detail: detail.into(),
        }
    }

    fn skip(name: &'static str, detail: impl Into<String>) -> Step {
        Step {
            name,
            verdict: Verdict::Skip,
            detail: detail.into(),
        }
    }
}

pub struct Report {
    pub steps: Vec<Step>,
    pub account: Option<SavedAccount>,
    pub region: Region,
}

impl Report {
    pub fn healthy(&self) -> bool {
        !self.steps.iter().any(|s| s.verdict == Verdict::Fail)
    }

    /// 给出下一步该做什么，而不是只告诉用户「失败了」。
    pub fn advice(&self) -> Option<&'static str> {
        let failure = |needle: &str| {
            self.steps
                .iter()
                .any(|s| s.name == needle && s.verdict == Verdict::Fail)
        };
        if failure("DNS 解析") {
            return Some("域名解析不了：换一个网络，或者在系统里把 DNS 改成 223.5.5.5 / 8.8.8.8。");
        }
        if failure("TCP 连接") {
            return Some("连不上上游端口：多半是当前网络被限制，换个网络（比如切到流量）再试。");
        }
        if failure("TLS 握手") {
            return Some("TLS 握手失败：这个网络可能在中间人解密（公司网关、公共 Wi-Fi 登录页、抓包工具），换网络试。");
        }
        if failure("配置接口") || failure("模型列表") {
            return Some("HTTP 层通了但接口不通：如果是 401/403，重新跑一次 wbhub login。");
        }
        if failure("额度接口") {
            return Some("额度接口被拒绝：账号会话可能已失效，重新跑一次 wbhub login。");
        }
        None
    }
}

/// 逐层探测。
pub async fn run(store: &Store, settings: &Settings) -> Result<Report> {
    let account = store.active();
    let region = account.as_ref().map(|a| a.region()).unwrap_or(Region::Cn);
    let mut steps = Vec::new();

    // 1. 账号
    match &account {
        Some(account) => steps.push(Step::ok(
            "账号",
            format!(
                "{}（{}）· {}",
                account.label(),
                account.region().label(),
                account.id
            ),
        )),
        None => {
            steps.push(Step::fail("账号", "还没有登录"));
            steps.push(Step::skip("DNS 解析", "没有账号，跳过"));
            steps.push(Step::skip("TCP 连接", "没有账号，跳过"));
            steps.push(Step::skip("TLS 握手", "没有账号，跳过"));
            steps.push(Step::skip("配置接口", "没有账号，跳过"));
            steps.push(Step::skip("模型列表", "没有账号，跳过"));
            steps.push(Step::skip("额度接口", "没有账号，跳过"));
            steps.push(Step::skip("本地端点", format!("端口 {}", settings.port)));
            return Ok(Report {
                steps,
                account: None,
                region,
            });
        }
    }

    // 2. 数据目录落在哪、能不能写
    steps.push(match store.active_account(region) {
        Some(_) => Step::ok("数据目录", store.dir().display().to_string()),
        None => Step::warn(
            "数据目录",
            format!("{}（此版本下没有账号）", store.dir().display()),
        ),
    });

    let host = host_of(region.chat_base());

    // 3. DNS
    let resolved = resolve(host).await;
    match &resolved {
        Ok(addresses) if !addresses.is_empty() => steps.push(Step::ok(
            "DNS 解析",
            format!("{host} → {}", addresses.join(", ")),
        )),
        Ok(_) => steps.push(Step::fail("DNS 解析", format!("{host} 没有解析出任何地址"))),
        Err(error) => steps.push(Step::fail("DNS 解析", format!("{host}：{error}"))),
    }

    // 4. TCP
    let connectable = match &resolved {
        Ok(addresses) if !addresses.is_empty() => {
            let mut best: Option<(Duration, String)> = None;
            let mut last_error = String::new();
            for address in addresses {
                let target = format!("{address}:443");
                let started = Instant::now();
                match tokio::time::timeout(STEP_TIMEOUT, tokio::net::TcpStream::connect(&target))
                    .await
                {
                    Ok(Ok(_)) => {
                        let elapsed = started.elapsed();
                        if best.as_ref().map(|(b, _)| elapsed < *b).unwrap_or(true) {
                            best = Some((elapsed, address.clone()));
                        }
                    }
                    Ok(Err(error)) => last_error = format!("{target}：{error}"),
                    Err(_) => last_error = format!("{target}：连接超时"),
                }
            }
            match best {
                Some((elapsed, address)) => {
                    steps.push(Step::ok(
                        "TCP 连接",
                        format!("{address}:443 用时 {} ms", elapsed.as_millis()),
                    ));
                    true
                }
                None => {
                    steps.push(Step::fail("TCP 连接", last_error));
                    false
                }
            }
        }
        _ => {
            steps.push(Step::skip("TCP 连接", "DNS 没通，跳过"));
            false
        }
    };

    // 5. TLS + HTTP：用最轻的公开接口探。
    let mut http_ok = false;
    if connectable {
        match http::request_with_status(
            &format!("{}/v3/config", region.chat_base()),
            "GET",
            &Vec::new(),
            None,
        )
        .await
        {
            Ok((status, _)) => {
                http_ok = true;
                if (200..300).contains(&status) {
                    steps.push(Step::ok(
                        "TLS 握手",
                        "证书校验通过，HTTP/1.1 或 /2 已协商".to_string(),
                    ));
                    steps.push(Step::ok("配置接口", format!("HTTP {status}")));
                } else {
                    steps.push(Step::warn(
                        "TLS 握手",
                        "握手完成，但接口返回了非 2xx".to_string(),
                    ));
                    steps.push(Step::warn("配置接口", format!("HTTP {status}")));
                }
            }
            Err(error) => {
                let detail = describe_any(&error);
                // 证书类问题归到 TLS，其余归到 HTTP。
                if detail.contains("证书") || detail.contains("TLS") || detail.contains("握手")
                {
                    steps.push(Step::fail("TLS 握手", detail));
                    steps.push(Step::skip("配置接口", "TLS 没通，跳过"));
                } else {
                    steps.push(Step::ok("TLS 握手", "已完成（见下一行）".to_string()));
                    steps.push(Step::fail("配置接口", detail));
                }
            }
        }
    } else {
        steps.push(Step::skip("TLS 握手", "TCP 没通，跳过"));
        steps.push(Step::skip("配置接口", "TCP 没通，跳过"));
    }

    // 6. 带凭证的模型接口
    if http_ok {
        let account_ref = account.as_ref().expect("账号已在上面检查过");
        match tokio::time::timeout(STEP_TIMEOUT, crate::upstream::fetch_models(account_ref)).await {
            Ok(Ok(models)) => steps.push(Step::ok("模型列表", format!("{} 个模型", models.len()))),
            Ok(Err(error)) => steps.push(Step::fail("模型列表", format!("{error:#}"))),
            Err(_) => steps.push(Step::fail("模型列表", "超时")),
        }
        // 7. 额度接口（billing 域名，和 chat 不是同一台机器）
        match tokio::time::timeout(STEP_TIMEOUT, crate::upstream::fetch_credits(account_ref)).await
        {
            Ok(Ok(balance)) if balance.unlimited => steps.push(Step::ok("额度接口", "不限")),
            Ok(Ok(balance)) => steps.push(Step::ok(
                "额度接口",
                format!(
                    "剩余 {}{}",
                    crate::format::credits(balance.total),
                    if balance.plan_name.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", balance.plan_name)
                    }
                ),
            )),
            Ok(Err(error)) => steps.push(Step::fail("额度接口", format!("{error:#}"))),
            Err(_) => steps.push(Step::fail("额度接口", "超时")),
        }
    } else {
        steps.push(Step::skip("模型列表", "HTTP 没通，跳过"));
        steps.push(Step::skip("额度接口", "HTTP 没通，跳过"));
    }

    // 8. 本地端点是否已经在跑（端口被占也可能是 502 的来源）
    let local = format!("127.0.0.1:{}", settings.port);
    match tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(&local),
    )
    .await
    {
        Ok(Ok(_)) => steps.push(Step::warn(
            "本地端点",
            format!("{local} 已有服务在监听（如果要另起一个，先换端口）"),
        )),
        _ => steps.push(Step::ok("本地端点", format!("{local} 空闲"))),
    }

    Ok(Report {
        steps,
        account,
        region,
    })
}

/// 把任何错误都摊成一句带原因链的话。
fn describe_any(error: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in error.chain() {
        let text = cause.to_string();
        if !parts.contains(&text) {
            parts.push(text);
        }
    }
    parts.join(" ← ")
}

fn host_of(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
}

async fn resolve(host: &str) -> Result<Vec<String>> {
    use std::net::ToSocketAddrs;
    let host = host.to_string();
    // `getaddrinfo` 是阻塞调用，放到阻塞线程池里，别卡住 runtime。
    let addresses = tokio::task::spawn_blocking(move || {
        (host.as_str(), 443u16).to_socket_addrs().map(|addrs| {
            let mut list: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
            list.dedup();
            list
        })
    })
    .await?;
    Ok(addresses?)
}

/// 打印报告。
pub fn print(report: &Report) {
    crate::say!("  版本         wbhub {}", crate::build_version());
    for step in &report.steps {
        crate::say!(
            "  {} {:<10} {}",
            step.verdict.mark(),
            step.name,
            step.detail
        );
    }
    crate::say!();
    if report.healthy() {
        crate::say!("  结论：这一侧没问题。");
    } else if let Some(advice) = report.advice() {
        crate::say!("  结论：{}", advice);
    } else {
        crate::say!("  结论：有检查项没通过，见上面的 ✗ 。");
    }
    if let Some(account) = &report.account {
        if account.needs_refresh() {
            crate::say!();
            crate::say!("  提示：这个账号的 token 已过期或即将过期，下一次调用会自动续期；");
            crate::say!("        如果续期也失败，就重新跑一次 wbhub login。");
        }
    }
    let _ = wire::CLIENT_UA;
}

/// 给 `--json` 用。
pub fn to_json(report: &Report) -> Value {
    serde_json::json!({
        "healthy": report.healthy(),
        "region": report.region.name(),
        "steps": report.steps.iter().map(|s| serde_json::json!({
            "name": s.name,
            "verdict": match s.verdict {
                Verdict::Ok => "ok",
                Verdict::Warn => "warn",
                Verdict::Fail => "fail",
                Verdict::Skip => "skip",
            },
            "detail": s.detail,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_extracted_from_urls() {
        assert_eq!(
            host_of("https://copilot.tencent.com"),
            "copilot.tencent.com"
        );
        assert_eq!(
            host_of("https://www.codebuddy.cn/v2/billing"),
            "www.codebuddy.cn"
        );
        assert_eq!(host_of("http://127.0.0.1:8080/x"), "127.0.0.1:8080");
    }

    #[test]
    fn a_report_with_a_failure_is_unhealthy() {
        let report = Report {
            steps: vec![
                Step::ok("账号", "x"),
                Step::fail("DNS 解析", "no such host"),
            ],
            account: None,
            region: Region::Cn,
        };
        assert!(!report.healthy());
        assert!(report.advice().unwrap().contains("DNS"));
    }

    #[test]
    fn warnings_alone_stay_healthy() {
        let report = Report {
            steps: vec![Step::ok("账号", "x"), Step::warn("本地端点", "占用")],
            account: None,
            region: Region::Cn,
        };
        assert!(report.healthy());
    }

    #[test]
    fn a_login_failure_points_at_logging_in_again() {
        let report = Report {
            steps: vec![Step::fail("额度接口", "HTTP 401")],
            account: None,
            region: Region::Cn,
        };
        assert!(report.advice().unwrap().contains("wbhub login"));
    }
}
