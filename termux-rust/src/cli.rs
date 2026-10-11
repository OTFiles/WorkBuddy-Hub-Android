//! Command line surface.
//!
//! The Android build exposed these actions as screens in a Compose app. Termux
//! has no UI, so each screen becomes a subcommand and the whole surface is
//! scriptable — which is the point of running it under Termux at all.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::bridge::{self, Bridge};
use crate::calllog::CallRecord;
use crate::login;
use crate::settings::Settings;
use crate::store::Store;
use crate::upstream;
use crate::wire::{self, Region};

#[derive(Parser, Debug)]
#[command(
    name = "wbhub",
    version,
    about = "WorkBuddy Hub for Termux — an OpenAI-compatible bridge to WorkBuddy/CodeBuddy quota",
    long_about = "Runs a local OpenAI-compatible endpoint backed by your own WorkBuddy \
                  (国内版) or WorkBuddy Global account. Point any OpenAI client at \
                  http://127.0.0.1:8765/v1 and it will be served by your account."
)]
pub struct Cli {
    /// Data directory (credentials, settings, call log).
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in to a build and save the credential.
    Login(LoginArgs),
    /// List saved accounts.
    Accounts,
    /// Switch the account or build that the endpoint serves.
    Use(UseArgs),
    /// Remove a saved account.
    Logout(LogoutArgs),
    /// Run the OpenAI-compatible endpoint in the foreground.
    Serve(ServeArgs),
    /// Show the endpoint URL and key for a running bridge.
    Status,
    /// Show remaining credit.
    Balance(BalanceArgs),
    /// Claim the daily check-in bonus.
    Checkin(CheckinArgs),
    /// List the models the account can serve.
    Models(ModelsArgs),
    /// Show the recorded calls.
    Calls(CallsArgs),
    /// Remove the recorded calls.
    ClearCalls,
    /// Show or change settings.
    Config(ConfigArgs),
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Which build to sign in to.
    #[arg(long, value_parser = parse_region, default_value = "cn")]
    pub region: Region,
    /// Do not try to open the URL in a browser (print it only).
    #[arg(long)]
    pub no_open: bool,
    /// Seconds to wait for the browser half of the flow.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
}

#[derive(Args, Debug)]
pub struct UseArgs {
    /// Account id, nickname, or a unique prefix of either.
    pub account: String,
    /// Which build the account belongs to.
    #[arg(long, value_parser = parse_region)]
    pub region: Option<Region>,
}

#[derive(Args, Debug)]
pub struct LogoutArgs {
    /// Account id, nickname, or a unique prefix. Omit to remove every account.
    pub account: Option<String>,
    /// Which build to remove from.
    #[arg(long, value_parser = parse_region)]
    pub region: Option<Region>,
    /// Remove without asking.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Port to listen on. Defaults to the port in settings.
    #[arg(long)]
    pub port: Option<u16>,
    /// Also serve the local network, authenticating peers with the LAN key.
    #[arg(long)]
    pub lan: bool,
    /// Address to bind. Defaults to loopback, or every interface with --lan.
    #[arg(long)]
    pub bind: Option<String>,
    /// Do not fetch the model catalogue at startup.
    #[arg(long)]
    pub no_refresh: bool,
    /// Print nothing per request.
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Args, Debug)]
pub struct BalanceArgs {
    /// Print raw JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CheckinArgs {
    /// Print raw JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ModelsArgs {
    /// Print the full catalogue as JSON.
    #[arg(long)]
    pub json: bool,
    /// Print only the model ids, one per line.
    #[arg(long)]
    pub ids: bool,
}

#[derive(Args, Debug)]
pub struct CallsArgs {
    /// How many records to show, newest first.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Print raw JSON.
    #[arg(long)]
    pub json: bool,
    /// Summarise instead of listing.
    #[arg(long)]
    pub summary: bool,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// Print the settings as JSON.
    #[arg(long)]
    pub json: bool,
    /// Set a value: `--set port=9000`, `--set lan_enabled=true`.
    #[arg(long, value_name = "KEY=VALUE")]
    pub set: Vec<String>,
    /// Generate a new local API key.
    #[arg(long)]
    pub rotate_key: bool,
    /// Generate a new LAN API key.
    #[arg(long)]
    pub rotate_lan_key: bool,
}

fn parse_region(value: &str) -> Result<Region, String> {
    Region::parse(value).ok_or_else(|| format!("unknown build {value:?}; use cn or global"))
}

/// Resolves the data directory, honouring `--data-dir` and then the default.
pub fn open_store(cli: &Cli) -> Result<Store> {
    let dir = cli
        .data_dir
        .clone()
        .or_else(|| std::env::var_os("WBHUB_DIR").map(PathBuf::from))
        .unwrap_or_else(Store::default_dir);
    Store::open(dir)
}

pub async fn run(cli: Cli) -> Result<()> {
    let store = open_store(&cli)?;
    match cli.command {
        Command::Login(args) => login_cmd(&store, args).await,
        Command::Accounts => accounts_cmd(&store),
        Command::Use(args) => use_cmd(&store, args),
        Command::Logout(args) => logout_cmd(&store, args),
        Command::Serve(args) => serve_cmd(store, args).await,
        Command::Status => status_cmd(&store),
        Command::Balance(args) => balance_cmd(&store, args).await,
        Command::Checkin(args) => checkin_cmd(&store, args).await,
        Command::Models(args) => models_cmd(&store, args).await,
        Command::Calls(args) => calls_cmd(&store, args),
        Command::ClearCalls => {
            let log = crate::calllog::CallLog::open(store.dir(), 1);
            log.clear()?;
            println!("调用记录已清空。");
            Ok(())
        }
        Command::Config(args) => config_cmd(&store, args),
    }
}

async fn login_cmd(store: &Store, args: LoginArgs) -> Result<()> {
    let region = args.region;
    println!("正在为{}申请登录…", region.label());
    let session = login::start(region).await.context("无法开始登录")?;
    println!();
    println!("请在浏览器中打开下面的链接完成登录：");
    println!();
    println!("  {}", session.auth_url);
    println!();
    println!("{}", login::device_hint());
    println!();
    if !args.no_open {
        open_url(&session.auth_url);
    }
    println!("等待登录完成（最多 {} 秒）…", args.timeout);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.timeout);
    let mut last_tick = std::time::Instant::now();
    loop {
        if std::time::Instant::now() >= deadline {
            bail!(
                "等待登录超时；可重试 `wbhub login --region {}`",
                region.name()
            );
        }
        match login::poll(&session).await {
            Ok(login::Poll::Done(account)) => {
                let saved = store.save(region, *account)?;
                println!();
                println!("登录成功：{}（{}）", saved.label(), saved.id);
                if saved.expires_at > 0 {
                    let when = chrono::DateTime::from_timestamp(saved.expires_at, 0)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_default();
                    println!("Token 有效期至 {when}");
                }
                return Ok(());
            }
            Ok(login::Poll::Pending) => {}
            Err(error) => {
                // A refusal is terminal; a transient transport problem is not,
                // and `poll` already folds those into `Pending`.
                bail!("登录失败：{error}");
            }
        }
        if last_tick.elapsed() >= std::time::Duration::from_secs(10) {
            last_tick = std::time::Instant::now();
            print!(".");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Best-effort browser hand-off. Termux's `termux-open-url` is the only opener
/// that exists on a phone; on a normal Linux box `xdg-open` is used instead.
fn open_url(url: &str) {
    let candidates: [&str; 2] = ["termux-open-url", "xdg-open"];
    for candidate in candidates {
        let opened = std::process::Command::new(candidate)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if opened.is_ok() {
            return;
        }
    }
}

fn accounts_cmd(store: &Store) -> Result<()> {
    let accounts = store.all_accounts();
    if accounts.is_empty() {
        println!("还没有账号，先运行：wbhub login");
        return Ok(());
    }
    let active_region = store.active_region();
    let active_id = store.active().map(|a| a.id).unwrap_or_default();
    println!(
        "{:<4} {:<8} {:<20} {:<12} {:<20} 标识",
        "标记", "版本", "账号", "UID", "Token 到期"
    );
    for account in &accounts {
        let marker = if account.id == active_id { "*" } else { "" };
        let expiry = if account.expires_at > 0 {
            chrono::DateTime::from_timestamp(account.expires_at, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "-".into())
        } else {
            "-".into()
        };
        println!(
            "{:<4} {:<8} {:<20} {:<12} {:<20} {}",
            marker,
            account.region().label(),
            account.label(),
            short_uid(&account.uid),
            expiry,
            account.id
        );
    }
    println!();
    println!(
        "当前使用：{}（{}）",
        active_region.label(),
        if active_id.is_empty() {
            "无"
        } else {
            "见上"
        }
    );
    Ok(())
}

fn short_uid(uid: &str) -> String {
    uid.chars().take(8).collect()
}

fn use_cmd(store: &Store, args: UseArgs) -> Result<()> {
    let region = match args.region {
        Some(region) => region,
        None => {
            // Search both builds so the user does not have to remember which one
            // an account belongs to.
            let matches: Vec<Region> = Region::all()
                .into_iter()
                .filter(|r| {
                    store.accounts(*r).iter().any(|a| {
                        a.id == args.account
                            || a.nickname == args.account
                            || a.id.starts_with(&args.account)
                            || a.uid.starts_with(&args.account)
                    })
                })
                .collect();
            match matches.as_slice() {
                [] => bail!("没有匹配的账号：{}", args.account),
                [only] => *only,
                many => bail!(
                    "账号 {} 在多个版本中存在，请加 --region：{}",
                    args.account,
                    many.iter()
                        .map(|r| r.name())
                        .collect::<Vec<_>>()
                        .join(" / ")
                ),
            }
        }
    };
    let account = store.select(region, &args.account)?;
    store.set_active_region(region)?;
    println!("已切换到 {}（{}）", account.label(), account.id);
    Ok(())
}

fn logout_cmd(store: &Store, args: LogoutArgs) -> Result<()> {
    let regions: Vec<Region> = match args.region {
        Some(region) => vec![region],
        None => Region::all().to_vec(),
    };
    match &args.account {
        Some(needle) => {
            let mut removed = 0;
            for region in &regions {
                let targets: Vec<String> = store
                    .accounts(*region)
                    .iter()
                    .filter(|a| {
                        a.id == *needle
                            || a.nickname == *needle
                            || a.id.starts_with(needle.as_str())
                    })
                    .map(|a| a.id.clone())
                    .collect();
                for id in targets {
                    if store.delete(*region, &id)? {
                        println!("已删除 {} 的账号 {}", region.label(), id);
                        removed += 1;
                    }
                }
            }
            if removed == 0 {
                bail!("没有匹配的账号：{needle}");
            }
        }
        None => {
            for region in &regions {
                for account in store.accounts(*region) {
                    store.delete(*region, &account.id)?;
                    println!("已删除 {} 的账号 {}", region.label(), account.id);
                }
            }
        }
    }
    Ok(())
}

async fn serve_cmd(store: Store, args: ServeArgs) -> Result<()> {
    let mut settings = Settings::load(store.dir())?;
    if let Some(port) = args.port {
        settings.port = port;
    }
    if args.lan {
        settings.lan_enabled = true;
    }
    if args.quiet {
        settings.verbose = false;
    }
    settings.save(store.dir())?;

    let bind = args.bind.unwrap_or_else(|| {
        if settings.lan_enabled {
            "0.0.0.0".to_string()
        } else {
            "127.0.0.1".to_string()
        }
    });
    let listener = tokio::net::TcpListener::bind((bind.as_str(), settings.port))
        .await
        .with_context(|| format!("cannot bind {bind}:{}", settings.port))?;

    let bridge = Bridge::new(store, settings.clone()).await;

    if !bridge.store.has_any() {
        eprintln!("提示：还没有登录任何账号，先运行 `wbhub login`，否则 /v1/chat/completions 会返回 401。");
    }
    if !args.no_refresh {
        match bridge.refresh_catalogue().await {
            Ok(count) => println!("已载入 {count} 个模型。"),
            Err(error) => eprintln!("模型列表获取失败（服务仍会启动）：{error}"),
        }
    }

    let shown_host = if bind == "0.0.0.0" {
        "127.0.0.1"
    } else {
        bind.as_str()
    };
    println!();
    println!("Base URL : http://{shown_host}:{}/v1", settings.port);
    println!("API Key  : {}", settings.api_key);
    if settings.lan_enabled {
        println!("局域网   : 已开启，外部设备用下面的 Key");
        println!("LAN Key  : {}", settings.lan_key);
        for address in local_ipv4_addresses() {
            println!("           http://{address}:{}/v1", settings.port);
        }
    } else {
        println!("局域网   : 未开启（--lan 可开启）");
    }
    println!();
    println!("按 Ctrl-C 停止。");
    println!();

    let refresh = {
        let bridge = Arc::clone(&bridge);
        let minutes = settings.refresh_minutes.max(1);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(minutes * 60));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match bridge.refresh_catalogue().await {
                    Ok(count) => {
                        if bridge.settings.read().await.verbose {
                            println!("模型列表已刷新（{count} 个）。");
                        }
                    }
                    Err(error) => eprintln!("模型列表刷新失败：{error}"),
                }
            }
        })
    };
    // Kept alive for the lifetime of the server; dropped with the runtime.
    std::mem::drop(refresh);

    let server = bridge::serve(Arc::clone(&bridge), listener);
    tokio::select! {
        result = server => result,
        _ = tokio::signal::ctrl_c() => {
            println!();
            println!("已停止。");
            Ok(())
        }
    }
}

/// The device's own LAN addresses, for the "point your laptop at this" hint.
fn local_ipv4_addresses() -> Vec<String> {
    let mut addresses = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                addresses.push(addr.ip().to_string());
            }
        }
    }
    addresses
}

fn status_cmd(store: &Store) -> Result<()> {
    let settings = Settings::load(store.dir())?;
    let account = store.active();
    println!("数据目录 : {}", store.dir().display());
    match &account {
        Some(account) => {
            println!(
                "当前账号 : {}（{}）",
                account.label(),
                account.region().label()
            );
            println!("账号标识 : {}", account.id);
            if account.expires_at > 0 {
                println!("Token 到期 : {}", account.expires_at);
            }
            if account.needs_refresh() {
                println!("Token : 需要刷新（下次调用时会自动刷新）");
            }
        }
        None => println!("当前账号 : 未登录（运行 `wbhub login`）"),
    }
    println!("端口     : {}", settings.port);
    println!("Base URL : http://127.0.0.1:{}/v1", settings.port);
    println!("API Key  : {}", settings.api_key);
    println!(
        "局域网   : {}",
        if settings.lan_enabled {
            "已开启"
        } else {
            "未开启"
        }
    );
    if settings.lan_enabled {
        println!("LAN Key  : {}", settings.lan_key);
    }
    Ok(())
}

async fn balance_cmd(store: &Store, args: BalanceArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    let balance = upstream::fetch_credits(&account).await?;
    if args.json {
        let payload = json!({
            "total": balance.total,
            "unlimited": balance.unlimited,
            "cycle_end": balance.cycle_end,
            "accounts": balance.accounts.iter().map(|a| json!({
                "name": a.name,
                "remain": a.remain,
                "size": a.size,
                "cycle_end": a.cycle_end,
                "unlimited": a.unlimited,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    println!("账号：{}（{}）", account.label(), account.region().label());
    if balance.unlimited {
        println!("额度：不限");
        return Ok(());
    }
    println!("剩余：{:.2}", balance.total);
    if !balance.cycle_end.is_empty() {
        println!("周期结束：{}", balance.cycle_end);
    }
    if balance.accounts.len() > 1 {
        println!();
        for account in &balance.accounts {
            println!(
                "  {:<28} 余 {:<10.2} 共 {:<10.2} {}",
                account.name, account.remain, account.size, account.cycle_end
            );
        }
    }
    Ok(())
}

async fn checkin_cmd(store: &Store, args: CheckinArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    let outcome = upstream::checkin(&account).await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok": outcome.ok, "message": outcome.message}))?
        );
        return Ok(());
    }
    if outcome.ok {
        println!("签到成功：{}", outcome.message);
    } else {
        println!("签到未完成：{}", outcome.message);
    }
    Ok(())
}

async fn models_cmd(store: &Store, args: ModelsArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    let models = upstream::fetch_models(&account).await?;
    if args.json {
        let payload: Vec<serde_json::Value> = models
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "name": m.name,
                    "rate": m.rate(),
                    "multiplier": m.multiplier(),
                    "free": m.is_free(),
                    "context_window": m.context_window,
                    "max_tokens": m.max_tokens,
                    "supports_images": m.supports_images,
                    "supports_reasoning": m.reasoning.supports,
                    "efforts": m.reasoning.supported_efforts,
                    "default_effort": m.reasoning.default_effort,
                    "badges": m.badges,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if args.ids {
        for model in &models {
            println!("{}", model.id);
        }
        return Ok(());
    }
    println!("{:<28} {:<10} {:<10} 标记", "模型", "倍率", "上下文");
    for model in &models {
        let rate = model.rate();
        let rate = if rate.is_empty() {
            "-".to_string()
        } else {
            rate
        };
        let mut marks: Vec<String> = model.badges.clone();
        if model.reasoning.supports {
            marks.push("推理".into());
        }
        if model.supports_images {
            marks.push("图片".into());
        }
        println!(
            "{:<28} {:<10} {:<10} {}",
            model.id,
            rate,
            model.context_window,
            marks.join(" ")
        );
    }
    Ok(())
}

fn calls_cmd(store: &Store, args: CallsArgs) -> Result<()> {
    let settings = Settings::load(store.dir())?;
    let log = crate::calllog::CallLog::open(store.dir(), settings.call_log_limit);
    if args.summary {
        let summary = log.summary()?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
            return Ok(());
        }
        println!(
            "调用 {} 次（成功 {} / 失败 {}）",
            summary.calls, summary.succeeded, summary.failed
        );
        println!(
            "Tokens：输入 {}，输出 {}，合计 {}",
            summary.prompt_tokens,
            summary.completion_tokens,
            summary.prompt_tokens + summary.completion_tokens
        );
        println!("积分消耗：{:.2}", summary.credits);
        if !summary.by_model.is_empty() {
            println!();
            for (model, count) in &summary.by_model {
                println!("  {model:<28} {count}");
            }
        }
        return Ok(());
    }
    let records = log.recent(args.limit)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("还没有调用记录。");
        return Ok(());
    }
    println!(
        "{:<20} {:<26} {:<6} {:>8} {:>8} {:<8} 来源",
        "时间", "模型", "结果", "输入", "输出", "积分"
    );
    for record in &records {
        let when = chrono::DateTime::from_timestamp_millis(record.timestamp)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        let detail = record
            .detail
            .as_deref()
            .map(|d| d.chars().take(30).collect::<String>())
            .unwrap_or_default();
        let origin = if record.source_ip.is_empty() {
            record.account_label.clone()
        } else {
            format!("{} {}", record.source_ip, record.account_label)
        };
        println!(
            "{:<20} {:<26} {:<6} {:>8} {:>8} {:<8} {}",
            when,
            record.model,
            if record.outcome == "ok" { "OK" } else { "FAIL" },
            record.prompt_tokens,
            record.completion_tokens,
            format!("{:.2}", record.credits),
            format!("{origin} {detail}").trim_end()
        );
    }
    Ok(())
}

fn config_cmd(store: &Store, args: ConfigArgs) -> Result<()> {
    let mut settings = Settings::load(store.dir())?;
    for assignment in &args.set {
        let (key, value) = assignment
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--set 需要 KEY=VALUE 形式，收到 {assignment:?}"))?;
        match key.trim() {
            "port" => settings.port = value.trim().parse().context("port 需要数字")?,
            "lan_enabled" => settings.lan_enabled = parse_bool(value)?,
            "call_log_limit" => settings.call_log_limit = value.trim().parse().context("call_log_limit 需要数字")?,
            "refresh_minutes" => settings.refresh_minutes = value.trim().parse().context("refresh_minutes 需要数字")?,
            "verbose" => settings.verbose = parse_bool(value)?,
            "api_key" => settings.api_key = value.trim().to_string(),
            "lan_key" => settings.lan_key = value.trim().to_string(),
            other => bail!("未知设置项 {other}；可用：port, lan_enabled, lan_key, api_key, call_log_limit, refresh_minutes, verbose"),
        }
    }
    let mut changed = !args.set.is_empty();
    if args.rotate_key {
        let key = settings.rotate_api_key(store.dir())?;
        println!("新的 API Key：{key}");
        changed = false;
    }
    if args.rotate_lan_key {
        let key = settings.rotate_lan_key(store.dir())?;
        println!("新的 LAN Key：{key}");
        changed = false;
    }
    if changed {
        settings.save(store.dir())?;
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&settings)?);
    }
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => bail!("{other:?} 不是布尔值"),
    }
}

/// Resolves the account to act on, refreshing the token when it is close to
/// expiry so a scripted `wbhub balance` does not fail on a stale token.
async fn resolve_active(store: &Store) -> Result<crate::store::SavedAccount> {
    let account = store
        .active()
        .ok_or_else(|| anyhow::anyhow!("还没有登录，运行 `wbhub login` 后再试"))?;
    if !account.needs_refresh() {
        return Ok(account);
    }
    Ok(refresh_and_store(store, &account).await)
}

/// Refreshes a credential and writes the result back, used by both the CLI and
/// the bridge when a token is close to expiry.
pub async fn refresh_and_store(
    store: &Store,
    account: &crate::store::SavedAccount,
) -> crate::store::SavedAccount {
    match login::refresh(account).await {
        Ok(renewed) => {
            let _ = store.update_tokens(account, &renewed);
            renewed
        }
        // A failed renewal still returns the existing token when it has not yet
        // expired, so an unreachable refresh endpoint does not take a working
        // session down.
        Err(error) => {
            eprintln!("token 刷新失败，继续使用现有 token：{error}");
            account.clone()
        }
    }
}

/// The call record a dry run would produce, for `--dry-run` style checks.
pub fn sample_record() -> CallRecord {
    CallRecord::failed(0, "none", "")
}

/// Reports the effective wire settings, used by `wbhub config --json`.
pub fn wire_summary(region: Region) -> serde_json::Value {
    json!({
        "region": region.name(),
        "chat_base": region.chat_base(),
        "billing_base": region.billing_base(),
        "client_ua": wire::CLIENT_UA,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_parser_accepts_both_names_and_chinese() {
        assert_eq!(parse_region("cn").unwrap(), Region::Cn);
        assert_eq!(parse_region("国内版").unwrap(), Region::Cn);
        assert_eq!(parse_region("global").unwrap(), Region::Global);
        assert_eq!(parse_region("国际版").unwrap(), Region::Global);
        assert!(parse_region("nope").is_err());
    }

    #[test]
    fn booleans_accept_the_usual_spellings() {
        for value in ["1", "true", "YES", "on"] {
            assert!(parse_bool(value).unwrap());
        }
        for value in ["0", "false", "No", "off"] {
            assert!(!parse_bool(value).unwrap());
        }
        assert!(parse_bool("maybe").is_err());
    }

    #[test]
    fn the_cli_parses_a_serve_invocation() {
        let cli = Cli::try_parse_from(["wbhub", "serve", "--port", "9100", "--lan"]).unwrap();
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.port, Some(9100));
                assert!(args.lan);
            }
            other => panic!("expected serve, got {other:?}"),
        }
    }

    #[test]
    fn the_data_dir_flag_is_global() {
        let cli = Cli::try_parse_from(["wbhub", "--data-dir", "/x", "status"]).unwrap();
        assert_eq!(cli.data_dir, Some(PathBuf::from("/x")));
    }
}
