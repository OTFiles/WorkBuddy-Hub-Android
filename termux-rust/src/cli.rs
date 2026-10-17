//! 命令行界面。
//!
//! 原 Android 版把这些动作做成 Compose 里的一个个页面。Termux 没有 UI，所以每个
//! 页面变成一个子命令，整套能力都能写进脚本——这也是在 Termux 里跑它的意义。

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::bridge::{self, Bridge};
use crate::calllog::CallRecord;
use crate::growth;
use crate::login;
use crate::settings::Settings;
use crate::store::Store;
use crate::upstream;
use crate::wake::WakeLock;
use crate::wire::{self, Region};

#[derive(Parser, Debug)]
#[command(
    name = "wbhub",
    version,
    about = "WorkBuddy Hub for Termux —— 把 WorkBuddy / CodeBuddy 账号变成 OpenAI 兼容端点",
    long_about = "在你的手机上跑一个本地 OpenAI 兼容端点，用你自己的 WorkBuddy（国内版）\
                  或 WorkBuddy 国际版账号作为上游。\n\n\
                  把任意 OpenAI 客户端指向 http://127.0.0.1:8765/v1 即可使用。",
    next_help_heading = "选项",
    after_help = "示例：\n  wbhub login              登录国内版账号\n  wbhub serve              启动端点\n  wbhub models             看可用模型\n  wbhub balance            看剩余额度\n  wbhub daily              签到 + 连登兑换 + 抽奖\n\n\
                  数据目录默认 $PREFIX/var/lib/wbhub，可用 --data-dir 或 WBHUB_DIR 改。"
)]
pub struct Cli {
    /// 数据目录（凭证、设置、调用记录）
    #[arg(long, global = true, value_name = "目录", help_heading = "选项")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 登录一个版本并保存凭证
    Login(LoginArgs),
    /// 列出已保存的账号
    Accounts,
    /// 切换端点使用的账号或版本
    Use(UseArgs),
    /// 删除已保存的账号
    Logout(LogoutArgs),
    /// 前台运行 OpenAI 兼容端点
    Serve(ServeArgs),
    /// 查看端点地址和密钥
    Status,
    /// 查看剩余额度
    Balance(BalanceArgs),
    /// 领取每日签到奖励
    Checkin(CheckinArgs),
    /// 跑一遍日常奖励（补签 → 礼包 → 兑换 → 抽奖）
    Daily(DailyArgs),
    /// 推进猫猫旅行（派出 / 领取）
    Travel(TravelArgs),
    /// 上报夜猫子任务（23:00–08:00 有效）
    Night(NightArgs),
    /// 查看成长任务进度
    Tasks(TasksArgs),
    /// 列出账号可用的模型
    Models(ModelsArgs),
    /// 查看调用记录
    Calls(CallsArgs),
    /// 清空调用记录
    ClearCalls,
    /// 导出当前账号凭证（给别的工具用）
    Export(ExportArgs),
    /// 查看或修改设置
    Config(ConfigArgs),
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct LoginArgs {
    /// 登录哪个版本：cn（国内版）或 global（国际版）
    #[arg(long, value_name = "版本", value_parser = parse_region, default_value = "cn")]
    pub region: Region,
    /// 不自动打开浏览器，只打印链接
    #[arg(long)]
    pub no_open: bool,
    /// 等待浏览器登录完成的秒数
    #[arg(long, value_name = "秒", default_value_t = 300)]
    pub timeout: u64,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct UseArgs {
    /// 账号标识、昵称，或它们的前缀
    #[arg(value_name = "账号")]
    pub account: String,
    /// 账号属于哪个版本（不填则自动查找）
    #[arg(long, value_name = "版本", value_parser = parse_region)]
    pub region: Option<Region>,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct LogoutArgs {
    /// 账号标识、昵称或前缀；不填则删除全部账号
    #[arg(value_name = "账号")]
    pub account: Option<String>,
    /// 只从这个版本里删除
    #[arg(long, value_name = "版本", value_parser = parse_region)]
    pub region: Option<Region>,
    /// 不再确认，直接删除
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct ServeArgs {
    /// 监听端口（不填则用设置里的端口）
    #[arg(long, value_name = "端口")]
    pub port: Option<u16>,
    /// 同时服务局域网（用独立的 LAN Key 鉴权）
    #[arg(long)]
    pub lan: bool,
    /// 监听地址（默认 127.0.0.1；开 --lan 时默认 0.0.0.0）
    #[arg(long, value_name = "地址")]
    pub bind: Option<String>,
    /// 启动时不拉取模型列表
    #[arg(long)]
    pub no_refresh: bool,
    /// 不打印每个请求的日志
    #[arg(long)]
    pub quiet: bool,
    /// 不申请 Termux 唤醒锁
    #[arg(long)]
    pub no_wake_lock: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct BalanceArgs {
    /// 输出原始 JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct CheckinArgs {
    /// 输出原始 JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct DailyArgs {
    /// 同时领取每日签到
    #[arg(long)]
    pub with_checkin: bool,
    /// 顺带推进猫猫旅行
    #[arg(long)]
    pub with_travel: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct TravelArgs {
    /// 只看状态，不派出也不领取
    #[arg(long)]
    pub status: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct NightArgs {
    /// 只看任务进度，不上报
    #[arg(long)]
    pub status: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct TasksArgs {
    /// 输出原始 JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct ModelsArgs {
    /// 输出完整模型信息（JSON）
    #[arg(long)]
    pub json: bool,
    /// 只输出模型名，一行一个
    #[arg(long)]
    pub ids: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct CallsArgs {
    /// 显示最近多少条（新的在前）
    #[arg(long, value_name = "条数", default_value_t = 20)]
    pub limit: usize,
    /// 输出原始 JSON
    #[arg(long)]
    pub json: bool,
    /// 只显示汇总
    #[arg(long)]
    pub summary: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct ExportArgs {
    /// 写到文件而不是标准输出
    #[arg(long, value_name = "文件")]
    pub out: Option<PathBuf>,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "选项")]
pub struct ConfigArgs {
    /// 以 JSON 输出设置
    #[arg(long)]
    pub json: bool,
    /// 修改一项设置，例如 --set port=9000
    #[arg(long, value_name = "键=值")]
    pub set: Vec<String>,
    /// 重新生成本机 API Key
    #[arg(long)]
    pub rotate_key: bool,
    /// 重新生成局域网 Key
    #[arg(long)]
    pub rotate_lan_key: bool,
}

/// 把 clap 内置的英文文案换成中文。
///
/// clap 没有本地化接口，`Usage:` / `Commands:` / `Options:` 这些标题和
/// `-h/--help` 的说明都是写死的。这里的做法是：帮助模板自己写中文标题、小节
/// 标题逐个覆盖、内置的 help/version 开关关掉后换成自己的。递归是因为每个子
/// 命令各有一份。
pub fn localize(command: clap::Command) -> clap::Command {
    localize_at(command, true)
}

fn localize_at(mut command: clap::Command, is_root: bool) -> clap::Command {
    command = command
        // `{usage}` 只渲染用法本身，不带标题，所以标题可以由模板写成中文。
        .help_template("{before-help}{about-with-newline}\n用法: {usage}\n\n{all-args}{after-help}")
        .subcommand_help_heading("命令")
        .next_help_heading("选项")
        .disable_help_subcommand(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(
            clap::Arg::new("help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::Help)
                .help("打印帮助信息")
                .long_help("打印帮助信息（-h 只打印简略版）"),
        );
    if is_root {
        command = command.arg(
            clap::Arg::new("version")
                .short('V')
                .long("version")
                .action(clap::ArgAction::Version)
                .help("打印版本号"),
        );
    }
    let names: Vec<String> = command
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        command = command.mut_subcommand(name, |sub| localize_at(sub, false));
    }
    command
}

fn parse_region(value: &str) -> Result<Region, String> {
    Region::parse(value).ok_or_else(|| format!("未知版本 {value:?}；可用：cn 或 global"))
}

/// 解析数据目录：`--data-dir` 优先，其次 `WBHUB_DIR`，最后是默认位置。
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
        Command::Daily(args) => daily_cmd(&store, args).await,
        Command::Travel(args) => travel_cmd(&store, args).await,
        Command::Night(args) => night_cmd(&store, args).await,
        Command::Tasks(args) => tasks_cmd(&store, args).await,
        Command::Models(args) => models_cmd(&store, args).await,
        Command::Calls(args) => calls_cmd(&store, args),
        Command::ClearCalls => {
            let log = crate::calllog::CallLog::open(store.dir(), 1);
            log.clear()?;
            crate::say!("调用记录已清空。");
            Ok(())
        }
        Command::Export(args) => export_cmd(&store, args),
        Command::Config(args) => config_cmd(&store, args),
    }
}

async fn login_cmd(store: &Store, args: LoginArgs) -> Result<()> {
    let region = args.region;
    crate::say!("正在为{}申请登录…", region.label());
    let session = login::start(region).await.context("无法开始登录")?;
    crate::say!();
    crate::say!("请在浏览器中打开下面的链接完成登录：");
    crate::say!();
    crate::say!("  {}", session.auth_url);
    crate::say!();
    crate::say!("{}", login::device_hint());
    crate::say!();
    if !args.no_open {
        open_url(&session.auth_url);
    }
    crate::say!("等待登录完成（最多 {} 秒）…", args.timeout);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.timeout);
    let mut last_tick = std::time::Instant::now();
    loop {
        if std::time::Instant::now() >= deadline {
            bail!(
                "等待登录超时；可以重试 `wbhub login --region {}`",
                region.name()
            );
        }
        match login::poll(&session).await {
            Ok(login::Poll::Done(account)) => {
                let saved = store.save(region, *account)?;
                crate::say!();
                crate::say!("登录成功：{}（{}）", saved.label(), saved.id);
                if saved.expires_at > 0 {
                    crate::say!("Token 有效期至 {}", clock_of(saved.expires_at));
                }
                crate::say!();
                crate::say!("下一步：wbhub serve");
                return Ok(());
            }
            Ok(login::Poll::Pending) => {}
            Err(error) => {
                // 明确的拒绝是终局；短暂的网络抖动不是，那种情况 `poll` 已经
                // 折成 `Pending` 了。
                bail!("登录失败：{error}");
            }
        }
        if last_tick.elapsed() >= std::time::Duration::from_secs(10) {
            last_tick = std::time::Instant::now();
            {
                use std::io::Write as _;
                let _ = write!(std::io::stdout(), ".");
                let _ = std::io::stdout().flush();
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// 尽量把链接交给浏览器。手机上有 `termux-open-url` 才是关键；普通 Linux 上退回
/// `xdg-open`。
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
        crate::say!("还没有账号，先运行：wbhub login");
        return Ok(());
    }
    let active_id = store.active().map(|a| a.id).unwrap_or_default();
    crate::say!(
        "{:<4} {:<8} {:<18} {:<10} {:<18} 标识",
        "标记",
        "版本",
        "账号",
        "UID",
        "Token 到期"
    );
    for account in &accounts {
        let marker = if account.id == active_id { "*" } else { "" };
        let expiry = if account.expires_at > 0 {
            clock_of(account.expires_at)
        } else {
            "-".into()
        };
        crate::say!(
            "{:<4} {:<8} {:<18} {:<10} {:<18} {}",
            marker,
            account.region().label(),
            truncate(&account.label(), 18),
            truncate(&account.uid, 10),
            expiry,
            account.id
        );
    }
    if let Some(active) = store.active() {
        crate::say!();
        crate::say!(
            "当前使用：{} · {}（{}）",
            active.region().label(),
            active.label(),
            active.id
        );
    }
    Ok(())
}

fn use_cmd(store: &Store, args: UseArgs) -> Result<()> {
    let region = match args.region {
        Some(region) => region,
        None => {
            // 两个版本都找一遍，省得用户还要记着账号属于哪个版本。
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
                    "账号 {} 在多个版本中都存在，请加 --region：{}",
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
    crate::say!("已切换到 {}（{}）", account.label(), account.id);
    Ok(())
}

fn logout_cmd(store: &Store, args: LogoutArgs) -> Result<()> {
    let regions: Vec<Region> = match args.region {
        Some(region) => vec![region],
        None => Region::all().to_vec(),
    };
    if args.account.is_none() && !args.yes {
        let total: usize = regions.iter().map(|r| store.accounts(*r).len()).sum();
        bail!("这会删除全部 {total} 个账号；确认请加 --yes");
    }
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
                        crate::say!("已删除 {} 的账号 {}", region.label(), id);
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
                    crate::say!("已删除 {} 的账号 {}", region.label(), account.id);
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
        .with_context(|| format!("无法监听 {bind}:{}", settings.port))?;

    let bridge = Bridge::new(store, settings.clone()).await;

    // 唤醒锁对应原版的悬浮窗保活：息屏后 CPU 还要能继续跑，否则转发端点会在
    // Deep Sleep 里静默停摆。拿不到就只是没锁，其它照旧。
    let wake = if args.no_wake_lock {
        None
    } else if crate::wake::available() {
        let lock = WakeLock::acquire();
        if lock.held() {
            Some(lock)
        } else {
            crate::say_err!("提示：没能拿到 Termux 唤醒锁，息屏后可能被系统暂停。");
            None
        }
    } else {
        crate::say_err!(
            "提示：没装 termux-wake-lock；建议 `pkg install termux-api` 并装 Termux:API 应用。"
        );
        None
    };

    if !bridge.store.has_any() {
        crate::say_err!("提示：还没有登录任何账号，先运行 `wbhub login`，否则 /v1/chat/completions 会返回 401。");
    }
    if !args.no_refresh {
        match bridge.refresh_catalogue().await {
            Ok(count) => crate::say!("已载入 {count} 个模型。"),
            Err(error) => crate::say_err!("模型列表获取失败（服务仍会启动）：{error}"),
        }
    }

    let shown_host = if bind == "0.0.0.0" {
        "127.0.0.1"
    } else {
        bind.as_str()
    };
    crate::say!();
    crate::say!("Base URL : http://{shown_host}:{}/v1", settings.port);
    crate::say!("API Key  : {}", settings.api_key);
    if settings.lan_enabled {
        crate::say!("局域网   : 已开启，外部设备用下面的 Key");
        crate::say!("LAN Key  : {}", settings.lan_key);
        for address in local_ipv4_addresses() {
            crate::say!("           http://{address}:{}/v1", settings.port);
        }
    } else {
        crate::say!("局域网   : 未开启（--lan 可开启）");
    }
    if wake.is_some() {
        crate::say!("唤醒锁   : 已持有（退出时自动释放）");
    }
    crate::say!();
    crate::say!("按 Ctrl-C 停止。");
    crate::say!();

    let refresh_task = {
        let bridge = std::sync::Arc::clone(&bridge);
        let minutes = settings.refresh_minutes.max(1);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(minutes * 60));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match bridge.refresh_catalogue().await {
                    Ok(count) => {
                        if bridge.settings.read().await.verbose {
                            crate::say!("模型列表已刷新（{count} 个）。");
                        }
                    }
                    Err(error) => crate::say_err!("模型列表刷新失败：{error}"),
                }
            }
        })
    };

    let server = bridge::serve(std::sync::Arc::clone(&bridge), listener);
    let outcome = tokio::select! {
        result = server => result,
        _ = shutdown_signal() => {
            crate::say!();
            crate::say!("已停止。");
            Ok(())
        }
    };
    refresh_task.abort();
    drop(wake);
    drop(bridge);
    outcome
}

/// 收到任何一个「该收工了」的信号都走清理路径。
///
/// SIGHUP 一定要接：`tmux kill-session` 和关掉终端发的都是它，如果不接，进程会被
/// 直接杀掉，而 Termux 的唤醒锁是按包名持有的——进程没了锁还在，白耗电。
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate()).ok();
    let mut hangup = signal(SignalKind::hangup()).ok();
    let mut interrupt = signal(SignalKind::interrupt()).ok();

    let fallback = || async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if terminate.is_none() && hangup.is_none() && interrupt.is_none() {
        fallback().await;
        return;
    }
    tokio::select! {
        _ = async {
            match interrupt.as_mut() {
                Some(stream) => {
                    stream.recv().await;
                }
                None => std::future::pending().await,
            }
        } => {}
        _ = async {
            match terminate.as_mut() {
                Some(stream) => {
                    stream.recv().await;
                }
                None => std::future::pending().await,
            }
        } => {}
        _ = async {
            match hangup.as_mut() {
                Some(stream) => {
                    stream.recv().await;
                }
                None => std::future::pending().await,
            }
        } => {}
    }
}

/// 本机在局域网里的地址，用来提示「笔记本该连哪个」。
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
    crate::say!("数据目录 : {}", store.dir().display());
    match store.active() {
        Some(account) => {
            crate::say!(
                "当前账号 : {}（{}）",
                account.label(),
                account.region().label()
            );
            crate::say!("账号标识 : {}", account.id);
            if account.expires_at > 0 {
                crate::say!("Token 到期 : {}", clock_of(account.expires_at));
            }
            if account.needs_refresh() {
                crate::say!("Token : 已过期或即将过期（下次调用时会自动刷新）");
            }
        }
        None => crate::say!("当前账号 : 未登录（运行 `wbhub login`）"),
    }
    crate::say!("端口     : {}", settings.port);
    crate::say!("Base URL : http://127.0.0.1:{}/v1", settings.port);
    crate::say!("API Key  : {}", settings.api_key);
    crate::say!(
        "局域网   : {}",
        if settings.lan_enabled {
            "已开启"
        } else {
            "未开启"
        }
    );
    if settings.lan_enabled {
        crate::say!("LAN Key  : {}", settings.lan_key);
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
        crate::say!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    crate::say!("账号：{}（{}）", account.label(), account.region().label());
    if balance.unlimited {
        crate::say!("额度：不限");
        return Ok(());
    }
    crate::say!("剩余：{:.2}", balance.total);
    if !balance.cycle_end.is_empty() {
        crate::say!("周期结束：{}", balance.cycle_end);
    }
    if balance.accounts.len() > 1 {
        crate::say!();
        for account in &balance.accounts {
            crate::say!(
                "  {:<28} 余 {:<10.2} 共 {:<10.2} {}",
                account.name,
                account.remain,
                account.size,
                account.cycle_end
            );
        }
    }
    Ok(())
}

async fn checkin_cmd(store: &Store, args: CheckinArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    let outcome = upstream::checkin(&account).await?;
    if args.json {
        crate::say!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok": outcome.ok, "message": outcome.message}))?
        );
        return Ok(());
    }
    if outcome.ok {
        crate::say!("签到成功：{}", outcome.message);
    } else {
        crate::say!("签到未完成：{}", outcome.message);
    }
    Ok(())
}

async fn daily_cmd(store: &Store, args: DailyArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    crate::say!("账号：{}（{}）", account.label(), account.region().label());
    crate::say!();

    let mut steps: Vec<growth::Step> = Vec::new();
    if args.with_checkin {
        match upstream::checkin(&account).await {
            Ok(outcome) => steps.push(growth::Step {
                ok: outcome.ok,
                message: if outcome.ok {
                    format!("签到：{}", outcome.message)
                } else {
                    format!("签到未完成：{}", outcome.message)
                },
            }),
            Err(error) => steps.push(growth::Step {
                ok: false,
                message: format!("签到失败：{error}"),
            }),
        }
    }
    steps.extend(growth::run_daily(&account).await);
    if args.with_travel {
        steps.extend(growth::travel_once(&account).await);
    }

    report(&steps);
    Ok(())
}

async fn travel_cmd(store: &Store, args: TravelArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    if args.status {
        let status = growth::travel_status(&account).await?;
        if status.state.is_empty() {
            crate::say!("还没有猫；执行 `wbhub travel` 会先领养一只。");
        } else if status.traveling() {
            crate::say!(
                "猫在路上，{} 到站（约 {} 分钟）",
                clock_of(status.arrive_at),
                status.minutes_until(crate::store::now_seconds())
            );
        } else if status.arrived() {
            crate::say!("猫已到站，执行 `wbhub travel` 领取奖励。");
        } else {
            crate::say!("猫在家（状态：{}）", status.state);
        }
        if status.daily_limit_reached {
            crate::say!("今日已派出过。");
        }
        return Ok(());
    }
    let steps = growth::travel_once(&account).await;
    report(&steps);
    Ok(())
}

async fn night_cmd(store: &Store, args: NightArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    if args.status {
        match growth::task(&account, "black_cat").await? {
            Some(task) => {
                crate::say!(
                    "夜猫子：{}/{}（状态 {}）",
                    task.current,
                    task.target,
                    task.accept_status
                );
                crate::say!(
                    "当前{}夜猫子时段（23:00–08:00）",
                    if growth::in_night_window() {
                        "处于"
                    } else {
                        "不在"
                    }
                );
            }
            None => crate::say!("没找到夜猫子任务。"),
        }
        return Ok(());
    }
    let steps = growth::run_night_owl(&account).await;
    report(&steps);
    Ok(())
}

async fn tasks_cmd(store: &Store, args: TasksArgs) -> Result<()> {
    let account = resolve_active(store).await?;
    let tasks = growth::tasks(&account).await?;
    if args.json {
        let payload: Vec<serde_json::Value> = tasks
            .iter()
            .map(|t| {
                json!({
                    "code": t.code,
                    "current": t.current,
                    "target": t.target,
                    "remaining": t.remaining(),
                    "accept_status": t.accept_status,
                    "claimed": t.claimed(),
                })
            })
            .collect();
        crate::say!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if tasks.is_empty() {
        crate::say!("没有成长任务（国际版账号没有成长中心）。");
        return Ok(());
    }
    crate::say!("{:<28} {:<10} {:<8} 状态", "任务", "进度", "还差");
    for task in &tasks {
        crate::say!(
            "{:<28} {:<10} {:<8} {}",
            task.code,
            format!("{}/{}", task.current, task.target),
            task.remaining(),
            task.accept_status
        );
    }
    Ok(())
}

/// 把一串步骤打成「✓ / ✗ + 说明」，最后给个总计。
fn report(steps: &[growth::Step]) {
    if steps.is_empty() {
        crate::say!("无可执行项。");
        return;
    }
    let mut ok = 0;
    for step in steps {
        if step.ok {
            ok += 1;
            crate::say!("  ✓ {}", step.message);
        } else {
            crate::say!("  ✗ {}", step.message);
        }
    }
    crate::say!();
    crate::say!("完成 {} / {} 项。", ok, steps.len());
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
        crate::say!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if args.ids {
        for model in &models {
            crate::say!("{}", model.id);
        }
        return Ok(());
    }
    crate::say!("{:<26} {:<10} {:<10} 标记", "模型", "倍率", "上下文");
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
        crate::say!(
            "{:<26} {:<10} {:<10} {}",
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
            crate::say!("{}", serde_json::to_string_pretty(&summary)?);
            return Ok(());
        }
        crate::say!(
            "调用 {} 次（成功 {} / 失败 {}）",
            summary.calls,
            summary.succeeded,
            summary.failed
        );
        crate::say!(
            "Tokens：输入 {}，输出 {}，合计 {}",
            summary.prompt_tokens,
            summary.completion_tokens,
            summary.prompt_tokens + summary.completion_tokens
        );
        crate::say!("积分消耗：{:.2}", summary.credits);
        if !summary.by_model.is_empty() {
            crate::say!();
            for (model, count) in &summary.by_model {
                crate::say!("  {model:<28} {count}");
            }
        }
        return Ok(());
    }
    let records = log.recent(args.limit)?;
    if args.json {
        crate::say!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    if records.is_empty() {
        crate::say!("还没有调用记录。");
        return Ok(());
    }
    crate::say!(
        "{:<16} {:<24} {:<7} {:>7} {:>7} {:<6} 来源",
        "时间",
        "模型",
        "结果",
        "输入",
        "输出",
        "积分"
    );
    for record in &records {
        let when = chrono::DateTime::from_timestamp_millis(record.timestamp)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        let result = match record.outcome.as_str() {
            "ok" => "成功",
            "aborted" => "中断",
            _ => "失败",
        };
        let detail = record
            .detail
            .as_deref()
            .map(|d| {
                d.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(28)
                    .collect::<String>()
            })
            .unwrap_or_default();
        let origin = if record.source_ip.is_empty() {
            record.account_label.clone()
        } else {
            format!("{} {}", record.source_ip, record.account_label)
        };
        crate::say!(
            "{:<16} {:<24} {:<7} {:>7} {:>7} {:<6} {}",
            when,
            truncate(&record.model, 24),
            result,
            record.prompt_tokens,
            record.completion_tokens,
            format!("{:.2}", record.credits),
            format!("{origin} {detail}").trim_end()
        );
    }
    Ok(())
}

/// 导出当前账号的凭证，方便接别的工具（原 Android 版把这份写到
/// `/data/local/tmp/wb-hub/wb-auth.json`）。
fn export_cmd(store: &Store, args: ExportArgs) -> Result<()> {
    let account = store
        .active()
        .ok_or_else(|| anyhow::anyhow!("还没有登录，运行 `wbhub login` 后再试"))?;
    let payload = json!({
        "accessToken": account.access_token,
        "refreshToken": account.refresh_token,
        "expiresAt": account.expires_at,
        "domain": account.domain,
        "uid": account.uid,
        "nickname": account.nickname,
        "enterpriseId": account.enterprise_id,
        "region": account.region().name(),
        "id": account.id,
    });
    let text = serde_json::to_string_pretty(&payload)?;
    match args.out {
        Some(path) => {
            crate::store::write_private(&path, &text)?;
            crate::say!("已写出 {}", path.display());
        }
        None => crate::say!("{text}"),
    }
    Ok(())
}

fn config_cmd(store: &Store, args: ConfigArgs) -> Result<()> {
    let mut settings = Settings::load(store.dir())?;
    for assignment in &args.set {
        let (key, value) = assignment
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--set 需要「键=值」形式，收到 {assignment:?}"))?;
        match key.trim() {
            "port" => settings.port = value.trim().parse().context("port 需要是数字")?,
            "lan_enabled" => settings.lan_enabled = parse_bool(value)?,
            "call_log_limit" => {
                settings.call_log_limit = value.trim().parse().context("call_log_limit 需要是数字")?
            }
            "refresh_minutes" => {
                settings.refresh_minutes = value.trim().parse().context("refresh_minutes 需要是数字")?
            }
            "verbose" => settings.verbose = parse_bool(value)?,
            "api_key" => {
                let key = value.trim();
                if key.is_empty() {
                    bail!("api_key 不能为空；想生成新 key 用 --rotate-key");
                }
                settings.api_key = key.to_string();
            }
            "lan_key" => {
                let key = value.trim();
                if key.is_empty() {
                    bail!("lan_key 不能为空；想生成新 key 用 --rotate-lan-key");
                }
                settings.lan_key = key.to_string();
            }
            other => bail!(
                "未知设置项 {other}；可用：port、lan_enabled、lan_key、api_key、call_log_limit、refresh_minutes、verbose"
            ),
        }
    }
    let mut changed = !args.set.is_empty();
    if args.rotate_key {
        let key = settings.rotate_api_key(store.dir())?;
        crate::say!("新的 API Key：{key}");
        changed = false;
    }
    if args.rotate_lan_key {
        let key = settings.rotate_lan_key(store.dir())?;
        crate::say!("新的 LAN Key：{key}");
        changed = false;
    }
    if changed {
        settings.save(store.dir())?;
        crate::say!("设置已保存。");
    }
    if args.json {
        crate::say!("{}", serde_json::to_string_pretty(&settings)?);
    }
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "开" => Ok(true),
        "0" | "false" | "no" | "off" | "关" => Ok(false),
        other => bail!("{other:?} 不是布尔值"),
    }
}

/// 解析出要操作的账号；快过期就先续一次，免得脚本化的 `wbhub balance` 因为
/// 陈旧 token 挂掉。
async fn resolve_active(store: &Store) -> Result<crate::store::SavedAccount> {
    let account = store
        .active()
        .ok_or_else(|| anyhow::anyhow!("还没有登录，运行 `wbhub login` 后再试"))?;
    if !account.needs_refresh() {
        return Ok(account);
    }
    Ok(refresh_and_store(store, &account).await)
}

/// 续期凭证并写回，CLI 和桥都会用到。
pub async fn refresh_and_store(
    store: &Store,
    account: &crate::store::SavedAccount,
) -> crate::store::SavedAccount {
    match login::refresh(account).await {
        Ok(renewed) => {
            let _ = store.update_tokens(account, &renewed);
            renewed
        }
        // 续期失败时只要原 token 还没过期就继续用它，免得一个连不上的续期接口
        // 把本来能用的会话弄停。
        Err(error) => {
            crate::say_err!("token 刷新失败，继续使用现有 token：{error}");
            account.clone()
        }
    }
}

/// 把 epoch 秒格式化成好读的本地时间。
///
/// 非正值在这套代码里一律表示「不知道」而不是 1970 年：上游没给
/// `expiresAt` 时字段就是 0，把它印成 1970 只会让人以为 token 早就过期了。
pub fn clock_of(epoch_seconds: i64) -> String {
    if epoch_seconds <= 0 {
        return "-".to_string();
    }
    chrono::DateTime::from_timestamp(epoch_seconds, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

/// 按显示宽度截断，避免表格被账号昵称挤歪。
fn truncate(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = chars[..keep].iter().collect();
    out.push('…');
    out
}

/// 给 `wbhub config --json` 附带的协议摘要。
pub fn wire_summary(region: Region) -> serde_json::Value {
    json!({
        "region": region.name(),
        "chat_base": region.chat_base(),
        "billing_base": region.billing_base(),
        "client_ua": wire::CLIENT_UA,
    })
}

/// 一条空记录，给测试和未来的干跑用。
pub fn sample_record() -> CallRecord {
    CallRecord::failed(0, "none", "")
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
        for value in ["1", "true", "YES", "on", "开"] {
            assert!(parse_bool(value).unwrap());
        }
        for value in ["0", "false", "No", "off", "关"] {
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

    #[test]
    fn help_text_is_chinese() {
        // The audience reads Chinese; an English-only CLI in a Chinese project is
        // a usability bug, not a localisation gap.
        use clap::CommandFactory;
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        assert!(help.contains("登录"), "help should mention 登录:\n{help}");
        assert!(help.contains("端点"), "help should mention 端点:\n{help}");
    }

    #[test]
    fn truncation_keeps_the_display_width() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("中文昵称很长", 4), "中文昵…");
    }

    #[test]
    fn clocks_render_or_fall_back() {
        // A real timestamp renders as a local date; a negative one is not a
        // timestamp at all and must not be printed as a confident 1969.
        assert!(clock_of(1_700_000_000).starts_with("2023"));
        assert_eq!(clock_of(0), "-", "0 means 'unknown', not 1970");
        assert_eq!(clock_of(-1), "-");
    }
}
