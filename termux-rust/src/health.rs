//! 探测某个端口上跑的到底是不是 wbhub。
//!
//! TUI 和 `wbhub status` 都要回答同一个问题：「这个端口上现在是谁在服务？」
//! 光看端口有没有人监听不够——同一个端口可能被别的程序占着，也可能是另一份
//! wbhub（另一个终端里跑的 `serve`，或者另一个数据目录）。三种情况要做的事完全
//! 不同，所以这里按层判断，并把结论说清楚。

use std::time::Duration;

use serde_json::Value;

/// 端口上的服务是什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Occupant {
    /// 没人监听。
    Nothing,
    /// 是 wbhub，带着它自报的版本。
    Wbhub { version: String },
    /// 有人在监听，但不是 wbhub（或者是旧版本，没有身份标识）。
    Something(String),
}

/// 探测超时。都是本机回环，正常是毫秒级，给 2 秒已经非常宽松。
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// 探测 `127.0.0.1:port`。
pub async fn probe(port: u16) -> Occupant {
    let address = format!("127.0.0.1:{port}");
    // 先看有没有人听：没有的话连 TCP 都不必试 HTTP。
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(&address)).await {
        Ok(Ok(_)) => {}
        _ => return Occupant::Nothing,
    }

    // `/healthz` 故意不要求鉴权：探测方可能没有这个实例的 key（比如另一份配置
    // 里的独立实例），而这里只暴露「我是谁、活了多久」，不涉及账号。
    let url = format!("http://{address}/healthz");
    let response = match tokio::time::timeout(PROBE_TIMEOUT, async {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(PROBE_TIMEOUT)
            .build()
            .ok()?
            .get(&url)
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()
    })
    .await
    {
        Ok(Some(text)) => match serde_json::from_str::<Value>(&text) {
            Ok(value) => value,
            Err(_) => {
                return Occupant::Something("端口有服务，但 /healthz 返回的不是 JSON".to_string())
            }
        },
        _ => return Occupant::Something("端口有服务，但没有响应 /healthz".to_string()),
    };

    if response.get("service").and_then(Value::as_str) == Some("wbhub") {
        return Occupant::Wbhub {
            version: response
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("未知")
                .to_string(),
        };
    }

    // 旧版 wbhub 和安卓版都只回 {"ok":true,"region":"cn"}，认不出是我们自己。
    let hint = match response.get("region").and_then(Value::as_str) {
        Some(_) => "看起来是安卓版或更早的 wbhub（没有身份标识）",
        None => "不是 wbhub",
    };
    Occupant::Something(hint.to_string())
}

/// 给 `wbhub status` 用的一行说明。
pub fn describe(occupant: &Occupant, port: u16) -> String {
    match occupant {
        Occupant::Nothing => format!("127.0.0.1:{port} 没有服务在监听"),
        Occupant::Wbhub { version } => {
            format!("127.0.0.1:{port} 由 wbhub {version} 提供服务")
        }
        Occupant::Something(hint) => format!("127.0.0.1:{port} 被占用：{hint}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_closed_port_reports_nothing() {
        // 端口 9（discard）在本机不会有人监听，连接会被直接拒绝。
        assert_eq!(probe(9).await, Occupant::Nothing);
    }

    #[test]
    fn descriptions_name_the_port_and_the_occupant() {
        assert!(describe(&Occupant::Nothing, 8765).contains("8765"));
        assert!(describe(
            &Occupant::Wbhub {
                version: "0.3.0-01".into()
            },
            8765
        )
        .contains("0.3.0-01"));
        assert!(describe(&Occupant::Something("不是 wbhub".into()), 8765).contains("被占用"));
    }
}
