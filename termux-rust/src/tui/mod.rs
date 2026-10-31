//! 终端界面。
//!
//! 把原 App 的几个页面搬成 TUI：概览（账号 + 额度 + 服务）、模型、调用记录、成长
//! 中心、设置。原版的内置 Agent 页不在这里——终端里用别的客户端就行，这也是这次
//! 移植一开始就定下来的取舍。
//!
//! 设计上有一条硬规则：**渲染循环绝不等待网络**。所有上游请求都在后台任务里跑，
//! 结果通过 channel 回到事件循环；这样按一下键立刻有反应，哪怕上游正在超时。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::bridge::{self, Bridge};
use crate::calllog::{CallLog, CallRecord, Summary};
use crate::growth;
use crate::settings::Settings;
use crate::store::{SavedAccount, Store};
use crate::upstream::{self, Balance};
use crate::wire::ProtoModel;
use crate::wire::Region;

mod ui;

/// 上游数据的刷新间隔。
const REFRESH: Duration = Duration::from_secs(60);

/// 事件循环的节拍：用来定时刷新和驱动界面重绘。
const TICK: Duration = Duration::from_millis(250);

/// 端口占用探测的间隔。
const PROBE_EVERY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Models,
    Calls,
    Growth,
    Settings,
}

impl Page {
    fn all() -> [Page; 5] {
        [
            Page::Overview,
            Page::Models,
            Page::Calls,
            Page::Growth,
            Page::Settings,
        ]
    }

    fn title(self) -> &'static str {
        match self {
            Page::Overview => "1 概览",
            Page::Models => "2 模型",
            Page::Calls => "3 调用",
            Page::Growth => "4 成长",
            Page::Settings => "5 设置",
        }
    }

    fn index(self) -> usize {
        Page::all().iter().position(|p| *p == self).unwrap_or(0)
    }

    fn from_index(index: usize) -> Page {
        *Page::all().get(index).unwrap_or(&Page::Overview)
    }
}

/// 后台任务回来的消息。
enum Msg {
    Balance(Option<Balance>),
    Models(Vec<ProtoModel>),
    Calls(Vec<CallRecord>, Summary),
    Growth(GrowthView),
    /// 一次性动作的结果（签到、旅行、换密钥……）。
    Notice(String, bool),
    /// 服务状态变化。
    Server(Option<u16>),
    Error(String),
}

/// 成长页要显示的东西。国际版账号没有成长中心，所以整体是可选的。
#[derive(Default, Clone)]
pub struct GrowthView {
    pub available: bool,
    pub days: i64,
    pub makeup_cards: i64,
    pub next_tier: String,
    pub next_tier_remaining: i64,
    pub redeemable: Vec<String>,
    pub lottery_chances: i64,
    pub travel: Option<growth::TravelState>,
    pub night: Option<growth::GrowthTask>,
    pub night_window: bool,
    pub tasks_total: usize,
    pub tasks_claimed: usize,
}

pub struct App {
    store: Arc<Store>,
    settings: Settings,
    accounts: Vec<SavedAccount>,
    active: Option<SavedAccount>,
    bridge: Option<Arc<Bridge>>,
    server: Option<(tokio::task::JoinHandle<()>, u16)>,
    page: Page,
    models: Vec<ProtoModel>,
    calls: Vec<CallRecord>,
    summary: Summary,
    balance: Option<Balance>,
    growth: GrowthView,
    /// 正在进行的后台请求数，用来在标题栏转圈。
    inflight: usize,
    /// 端口上现在是谁在服务。每几秒探一次，所以别的终端里起的 `serve` 也能看到。
    occupant: crate::health::Occupant,
    /// 端口占用探测的上次时间。
    last_probe: Option<Instant>,
    /// 正在编辑的数值行：行号 + 已输入的字符。
    editing: Option<(usize, String)>,
    /// 最近几条一次性动作的结果，最新的在最后。
    log: VecDeque<(String, bool)>,
    settings_row: usize,
    last_refresh: Option<Instant>,
    should_quit: bool,
}

impl App {
    fn new(store: Arc<Store>, settings: Settings) -> App {
        let accounts = store.all_accounts();
        let active = store.active();
        App {
            store,
            settings,
            accounts,
            active,
            bridge: None,
            server: None,
            page: Page::Overview,
            models: Vec::new(),
            calls: Vec::new(),
            summary: Summary::default(),
            balance: None,
            growth: GrowthView::default(),
            inflight: 0,
            occupant: crate::health::Occupant::Nothing,
            last_probe: None,
            editing: None,
            log: VecDeque::new(),
            settings_row: 0,
            last_refresh: None,
            should_quit: false,
        }
    }

    fn active(&self) -> Option<&SavedAccount> {
        self.active.as_ref()
    }

    /// 重新读一遍账号列表与当前选择。
    fn reload(&mut self) {
        self.accounts = self.store.all_accounts();
        self.active = self.store.active();
    }

    /// 这个实例是不是本进程启动的。
    fn owns_server(&self) -> bool {
        self.server.is_some()
    }

    fn note(&mut self, message: impl Into<String>, ok: bool) {
        self.log.push_back((message.into(), ok));
        while self.log.len() > 4 {
            self.log.pop_front();
        }
    }

    /// 页面之间切换。
    fn next_page(&mut self) {
        let index = (self.page.index() + 1) % Page::all().len();
        self.page = Page::from_index(index);
    }

    fn previous_page(&mut self) {
        let count = Page::all().len();
        let index = (self.page.index() + count - 1) % count;
        self.page = Page::from_index(index);
    }
}

/// 进入 TUI。返回时终端已经恢复。
pub async fn run(store: Store) -> Result<()> {
    let store = Arc::new(store);
    let mut settings = Settings::load(store.dir())?;
    // 服务端在 TUI 里不能往 stderr 打字，否则会把整个界面搅乱；请求计数改从
    // BridgeStatus 读。
    settings.verbose = false;
    settings.save(store.dir())?;

    let mut terminal = ui::enter()?;
    let outcome = event_loop(&mut terminal, store, settings).await;
    ui::leave(&mut terminal)?;
    outcome
}

async fn event_loop(
    terminal: &mut Terminal<ui::Backend>,
    store: Arc<Store>,
    settings: Settings,
) -> Result<()> {
    let mut app = App::new(Arc::clone(&store), settings);
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // 首屏先把数据拉起来，别让用户对着空白页等。
    spawn_refresh(&mut app, tx.clone());

    loop {
        {
            let app = &app;
            terminal.draw(|frame| ui::draw(frame, app))?;
        }

        // 先排空所有已经到手的消息，再等下一帧。
        while let Ok(message) = rx.try_recv() {
            apply(&mut app, message, &tx);
        }
        if app.should_quit {
            break;
        }

        // 每 3 秒探一次端口：别的终端里起的 serve 也要能在界面上看见。
        if app
            .last_probe
            .map(|at| at.elapsed() >= PROBE_EVERY)
            .unwrap_or(true)
        {
            app.last_probe = Some(Instant::now());
            app.occupant = crate::health::probe(app.settings.port).await;
        }

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    handle_key(&mut app, key, &tx).await;
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        // 定时刷新：到点了就重新拉一遍。
        if app
            .last_refresh
            .map(|at| at.elapsed() >= REFRESH)
            .unwrap_or(true)
        {
            spawn_refresh(&mut app, tx.clone());
        }
    }

    if let Some((task, _)) = app.server.take() {
        task.abort();
    }
    Ok(())
}

fn apply(app: &mut App, message: Msg, tx: &mpsc::UnboundedSender<Msg>) {
    let _ = tx;
    match message {
        Msg::Balance(balance) => app.balance = balance,
        Msg::Models(models) => app.models = models,
        Msg::Calls(calls, summary) => {
            app.calls = calls;
            app.summary = summary;
        }
        Msg::Growth(view) => app.growth = view,
        Msg::Notice(message, ok) => app.note(message, ok),
        Msg::Server(port) => {
            if port.is_none() {
                app.bridge = None;
            }
        }
        Msg::Error(message) => app.note(message, false),
    }
    app.inflight = app.inflight.saturating_sub(1);
}

/// 调用记录在本地，单独抽出来是因为「没登录也要能看」。
fn spawn_calls(store: &Arc<Store>, limit: usize, tx: mpsc::UnboundedSender<Msg>) {
    let dir = store.dir().to_path_buf();
    tokio::spawn(async move {
        let log = CallLog::open(&dir, limit);
        let calls = log.recent(200).unwrap_or_default();
        let summary = log.summary().unwrap_or_default();
        let _ = tx.send(Msg::Calls(calls, summary));
    });
}

/// 把四类数据并发拉一遍。
///
/// 每一类都各自起任务：额度走的是 billing 域名，模型走的是 config，本地记录根本
/// 不出网。串行的话最慢的那一个会拖住整屏。
fn spawn_refresh(app: &mut App, tx: mpsc::UnboundedSender<Msg>) {
    app.reload();
    app.last_refresh = Some(Instant::now());
    app.inflight = 0;

    let account = app.active().cloned();
    let Some(account) = account else {
        // 没登录也要把本地记录读出来：调用汇总是本地数据，跟登录无关。
        spawn_calls(&app.store, app.settings.call_log_limit, tx.clone());
        let _ = tx.send(Msg::Balance(None));
        let _ = tx.send(Msg::Models(Vec::new()));
        let _ = tx.send(Msg::Growth(GrowthView::default()));
        app.inflight = 3;
        return;
    };
    app.inflight = 4;

    {
        let tx = tx.clone();
        let account = account.clone();
        tokio::spawn(async move {
            match upstream::fetch_credits(&account).await {
                Ok(balance) => {
                    let _ = tx.send(Msg::Balance(Some(balance)));
                }
                Err(error) => {
                    let _ = tx.send(Msg::Balance(None));
                    let _ = tx.send(Msg::Error(format!("额度：{error}")));
                }
            }
        });
    }
    {
        let tx = tx.clone();
        let account = account.clone();
        tokio::spawn(async move {
            match upstream::fetch_models(&account).await {
                Ok(models) => {
                    let _ = tx.send(Msg::Models(models));
                }
                Err(error) => {
                    let _ = tx.send(Msg::Error(format!("模型列表：{error}")));
                    let _ = tx.send(Msg::Models(Vec::new()));
                }
            }
        });
    }
    spawn_calls(&app.store, app.settings.call_log_limit, tx.clone());
    {
        let tx = tx.clone();
        let account = account.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::Growth(read_growth(&account).await));
        });
    }
}

/// 一次把成长页需要的东西读齐。
async fn read_growth(account: &SavedAccount) -> GrowthView {
    if account.region() != Region::Cn {
        return GrowthView::default();
    }
    let mut view = GrowthView {
        available: true,
        night_window: growth::in_night_window(),
        ..Default::default()
    };
    if let Ok(streak) = growth::streak(account).await {
        view.days = streak.days;
        view.makeup_cards = streak.makeup_cards;
        view.next_tier = streak.next_tier.clone();
        view.next_tier_remaining = streak.next_tier_remaining;
        view.redeemable = streak
            .redeemable()
            .iter()
            .map(|tier| tier.id.clone())
            .collect();
    }
    view.lottery_chances = growth::lottery_chances(account).await;
    view.travel = growth::travel_status(account).await.ok();
    if let Ok(tasks) = growth::tasks(account).await {
        view.tasks_total = tasks.len();
        view.tasks_claimed = tasks.iter().filter(|t| t.claimed()).count();
        view.night = tasks.into_iter().find(|t| t.code == "black_cat");
    }
    view
}

async fn handle_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Msg>) {
    // 编辑态优先吃掉所有按键，否则打字会触发翻页、启停之类的事情。
    if app.editing.is_some() {
        handle_edit_key(app, key);
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.should_quit = true;
            return;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
            return;
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            app.next_page();
            return;
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            app.previous_page();
            return;
        }
        KeyCode::Char('1'..='5') => {
            if let KeyCode::Char(digit) = key.code {
                let index = digit.to_digit(10).unwrap_or(1) as usize;
                app.page = Page::from_index(index.saturating_sub(1));
            }
            return;
        }
        _ => {}
    }

    match app.page {
        Page::Settings => handle_settings_key(app, key).await,
        Page::Growth => handle_growth_key(app, key, tx).await,
        _ => match key.code {
            KeyCode::Char('r') => spawn_refresh(app, tx.clone()),
            KeyCode::Char('s') => toggle_server(app, tx).await,
            _ => {}
        },
    }
}

/// 数值行的编辑：只收数字，Enter 提交，Esc 放弃。
fn handle_edit_key(app: &mut App, key: KeyEvent) {
    let Some((row, ref mut buffer)) = app.editing else {
        return;
    };
    match key.code {
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            // 端口最多 5 位，其余 4 位，够用且不会输入到失控。
            if buffer.len() < 5 {
                buffer.push(ch);
            }
        }
        KeyCode::Backspace => {
            buffer.pop();
        }
        KeyCode::Esc => {
            app.editing = None;
        }
        KeyCode::Enter => {
            let text = buffer.clone();
            app.editing = None;
            commit_number(app, row, &text);
        }
        _ => {}
    }
}

/// 把编辑框里的数落进设置。
fn commit_number(app: &mut App, row: usize, text: &str) {
    let Ok(value) = text.parse::<i64>() else {
        app.note("没有输入数字，已取消", false);
        return;
    };
    let (field, clamped, unit) = match row {
        0 => {
            let clamped = value.clamp(1024, 65535);
            app.settings.port = clamped as u16;
            ("端口", clamped, "")
        }
        2 => {
            let clamped = value.clamp(100, 20000);
            app.settings.call_log_limit = clamped as usize;
            ("调用记录上限", clamped, "")
        }
        3 => {
            let clamped = value.clamp(1, 240);
            app.settings.refresh_minutes = clamped as u64;
            ("模型刷新间隔", clamped, " 分钟")
        }
        _ => return,
    };
    persist(app);
    if clamped != value {
        app.note(
            format!("{field}已改为 {clamped}{unit}（超出范围，已夹到边界）"),
            true,
        );
    } else {
        app.note(format!("{field}已改为 {clamped}{unit}"), true);
    }
    // 端口变了，端口占用要重新判断。
    if row == 0 {
        app.last_probe = None;
    }
}

async fn handle_growth_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Msg>) {
    let Some(account) = app.store.active() else {
        app.note("还没有登录账号", false);
        return;
    };
    match key.code {
        KeyCode::Char('c') => {
            let tx = tx.clone();
            tokio::spawn(async move {
                let message = match upstream::checkin(&account).await {
                    Ok(outcome) if outcome.ok => format!("签到成功：{}", outcome.message),
                    Ok(outcome) => format!("签到未完成：{}", outcome.message),
                    Err(error) => format!("签到失败：{error}"),
                };
                let _ = tx.send(Msg::Notice(message, true));
            });
        }
        KeyCode::Char('d') => {
            let tx = tx.clone();
            tokio::spawn(async move {
                let steps = growth::run_daily(&account).await;
                let ok = steps.iter().filter(|s| s.ok).count();
                let message = match steps.last() {
                    Some(last) => {
                        format!("日常奖励：完成 {ok}/{} 项 · {}", steps.len(), last.message)
                    }
                    None => "日常奖励：无可执行项".to_string(),
                };
                let _ = tx.send(Msg::Notice(message, ok > 0));
            });
        }
        KeyCode::Char('t') => {
            let tx = tx.clone();
            tokio::spawn(async move {
                let steps = growth::travel_once(&account).await;
                let message = steps
                    .last()
                    .map(|s| s.message.clone())
                    .unwrap_or_else(|| "旅行：没有返回结果".to_string());
                let ok = steps.last().map(|s| s.ok).unwrap_or(false);
                let _ = tx.send(Msg::Notice(message, ok));
            });
        }
        KeyCode::Char('n') => {
            let tx = tx.clone();
            tokio::spawn(async move {
                let steps = growth::run_night_owl(&account).await;
                let message = steps
                    .last()
                    .map(|s| s.message.clone())
                    .unwrap_or_else(|| "夜猫子：没有返回结果".to_string());
                let ok = steps.last().map(|s| s.ok).unwrap_or(false);
                let _ = tx.send(Msg::Notice(message, ok));
            });
        }
        KeyCode::Char('r') => spawn_refresh(app, tx.clone()),
        KeyCode::Char('s') => toggle_server(app, tx).await,
        _ => {}
    }
}

async fn handle_settings_key(app: &mut App, key: KeyEvent) {
    const ROWS: usize = 7;
    match key.code {
        KeyCode::Up => app.settings_row = app.settings_row.saturating_sub(1),
        KeyCode::Down => app.settings_row = (app.settings_row + 1).min(ROWS - 1),
        KeyCode::Enter => match app.settings_row {
            // 数值行进入编辑态，回车再确认。
            0 | 2 | 3 => {
                let current = match app.settings_row {
                    0 => app.settings.port.to_string(),
                    2 => app.settings.call_log_limit.to_string(),
                    _ => app.settings.refresh_minutes.to_string(),
                };
                app.editing = Some((app.settings_row, current));
            }
            1 => {
                app.settings.lan_enabled = !app.settings.lan_enabled;
                persist(app);
                app.note(
                    if app.settings.lan_enabled {
                        "局域网已开启（重启服务生效）"
                    } else {
                        "局域网已关闭（重启服务生效）"
                    },
                    true,
                );
            }
            6 => {
                app.settings.verbose = !app.settings.verbose;
                persist(app);
            }
            _ => {}
        },
        KeyCode::Char('k') => match app.settings_row {
            4 => match app.settings.rotate_api_key(app.store.dir()) {
                Ok(key) => app.note(format!("新的 API Key：{key}"), true),
                Err(error) => app.note(format!("换密钥失败：{error}"), false),
            },
            5 => match app.settings.rotate_lan_key(app.store.dir()) {
                Ok(key) => app.note(format!("新的 LAN Key：{key}"), true),
                Err(error) => app.note(format!("换密钥失败：{error}"), false),
            },
            _ => app.note("这一行没有可轮换的密钥", false),
        },
        _ => {}
    }
}

fn persist(app: &mut App) {
    if let Err(error) = app.settings.save(app.store.dir()) {
        app.note(format!("设置保存失败：{error}"), false);
    }
}

/// 起停本地端点。
async fn toggle_server(app: &mut App, tx: &mpsc::UnboundedSender<Msg>) {
    if let Some((task, port)) = app.server.take() {
        task.abort();
        let _ = tx.send(Msg::Server(None));
        app.note(format!("已停止（原端口 {port}）"), true);
        return;
    }
    // 端口上已经有 wbhub 在跑了（别的终端），再起一个只会撞端口；说清楚比让
    // bind 报一句 "Address already in use" 有用。
    if let crate::health::Occupant::Wbhub { version } = &app.occupant {
        app.note(
            format!(
                "端口 {} 上已经有 wbhub {version} 在服务，不用再起一个",
                app.settings.port
            ),
            true,
        );
        return;
    }
    let mut settings = app.settings.clone();
    settings.verbose = false;
    let bind = if settings.lan_enabled {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    let listener = match tokio::net::TcpListener::bind((bind, settings.port)).await {
        Ok(listener) => listener,
        Err(error) => {
            app.note(format!("无法监听 {bind}:{}：{error}", settings.port), false);
            return;
        }
    };
    let port = settings.port;
    // Bridge::new 自己就返回 Arc，不用再套一层。
    let bridge = Bridge::new((*app.store).clone(), settings).await;
    let catalogued = bridge.refresh_catalogue().await.unwrap_or(0);
    let task = {
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            let _ = bridge::serve(bridge, listener).await;
        })
    };
    app.bridge = Some(bridge);
    app.server = Some((task, port));
    let _ = tx.send(Msg::Server(Some(port)));
    if catalogued > 0 {
        app.note(format!("已启动，载入 {catalogued} 个模型"), true);
    } else {
        app.note("已启动（模型列表稍后刷新）", true);
    }
}

// ---------------------------------------------------------------------- //
// 渲染
// ---------------------------------------------------------------------- //

impl App {
    /// 今天（本地时区）的成功/失败计数，给概览页的「今日调用」用。
    fn today(&self) -> (u64, u64) {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let mut ok = 0;
        let mut failed = 0;
        for record in &self.calls {
            let when = chrono::DateTime::from_timestamp_millis(record.timestamp)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d")
                        .to_string()
                })
                .unwrap_or_default();
            if when != today {
                continue;
            }
            if record.outcome == "ok" {
                ok += 1;
            } else {
                failed += 1;
            }
        }
        (ok, failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_round_trip_through_their_index() {
        for page in Page::all() {
            assert_eq!(Page::from_index(page.index()), page);
        }
        assert_eq!(Page::from_index(99), Page::Overview);
    }

    #[test]
    fn notices_are_bounded() {
        let dir = std::env::temp_dir().join(format!("wbhub-tui-{}", std::process::id()));
        let store = Arc::new(Store::open(&dir).unwrap());
        let mut app = App::new(store, Settings::default());
        for index in 0..10 {
            app.note(format!("第 {index} 条"), true);
        }
        assert_eq!(app.log.len(), 4);
        assert!(app.log.back().unwrap().0.contains('9'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_navigation_wraps_both_ways() {
        let dir = std::env::temp_dir().join(format!("wbhub-tui-nav-{}", std::process::id()));
        let store = Arc::new(Store::open(&dir).unwrap());
        let mut app = App::new(store, Settings::default());
        app.previous_page();
        assert_eq!(app.page, Page::Settings, "backwards from the first wraps");
        app.next_page();
        assert_eq!(app.page, Page::Overview);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn money_is_rendered_with_two_decimals() {
        assert_eq!(ui::credits(1135.0), "1135.00");
        assert_eq!(ui::credits(0.5), "0.50");
    }

    #[test]
    fn long_labels_are_clipped_to_the_column() {
        let long = "一个特别特别长的账号昵称用来测试截断";
        let clipped = ui::fit(long, 8);
        assert!(clipped.chars().count() <= 8);
        assert!(clipped.ends_with('…'));
        assert_eq!(ui::fit("短", 8), "短");
    }
}
