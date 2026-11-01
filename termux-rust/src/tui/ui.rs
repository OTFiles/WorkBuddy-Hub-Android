//! 终端界面的渲染与终端生命周期。
//!
//! 这里只做两件事：把 `App` 画出来，以及保证无论怎么退出（正常退出、报错、
//! panic）都把终端还回去。丢一个在 raw mode 里的终端比崩溃本身更让人难受。

use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::{Frame, Terminal};

// 在 crate 内转出，测试和别的模块都能用同一份实现。
pub(crate) use crate::format::{credits, pad, thousands};

use super::{App, Page};

pub type Backend = CrosstermBackend<std::io::Stdout>;

/// 进入备用屏幕并开 raw mode。
pub fn enter() -> Result<Terminal<Backend>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    Ok(terminal)
}

/// 把终端还原。
pub fn leave(terminal: &mut Terminal<Backend>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    terminal.show_cursor()?;
    Ok(())
}

// ---------------------------------------------------------------------- //
// 小工具
// ---------------------------------------------------------------------- //

/// 按显示宽度截断，超出部分用省略号。
///
/// 中文是双宽字符，所以不能按字节也不能只按 char 个数算，这里用
/// `unicode_width` 的口径（ratatui 自己也是这么算的），否则列会错位。
pub fn fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            // 优先「不动已放下的内容，直接补一个省略号」；只有实在腾不出那一格
            // 时才丢掉最后一个字符去换——丢掉能显示的信息是最不得已的做法。
            if used < width {
                out.push('…');
            } else if width >= 1 {
                out.pop();
                out.push('…');
            }
            return out;
        }
        used += w;
        out.push(ch);
    }
    out
}

fn outcome_style(outcome: &str) -> Style {
    match outcome {
        "ok" => Style::default().fg(Color::Green),
        "aborted" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Red),
    }
}

fn outcome_label(outcome: &str) -> &'static str {
    match outcome {
        "ok" => "成功",
        "aborted" => "中断",
        _ => "失败",
    }
}

fn section(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
}

// ---------------------------------------------------------------------- //
// 主渲染
// ---------------------------------------------------------------------- //

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 标题
            Constraint::Length(1), // 标签页
            Constraint::Min(6),    // 页面主体
            Constraint::Length(1), // 最近一条消息
            Constraint::Length(1), // 快捷键
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_tabs(frame, chunks[1], app);
    match app.page {
        Page::Overview => draw_overview(frame, chunks[2], app),
        Page::Models => draw_models(frame, chunks[2], app),
        Page::Calls => draw_calls(frame, chunks[2], app),
        Page::Growth => draw_growth(frame, chunks[2], app),
        Page::Settings => draw_settings(frame, chunks[2], app),
    }
    draw_notice(frame, chunks[3], app);
    draw_keys(frame, chunks[4], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let account = match app.active() {
        Some(account) => format!(
            "{} · {}",
            account.region().label(),
            fit(&account.label(), 18)
        ),
        None => "未登录".to_string(),
    };
    // 端口上是谁在服务，分四种情况说清楚。原来只判断「本进程有没有启动服务」，
    // 于是别的终端里跑着 serve 时这里仍然显示「未启动」，会让人误以为需要再按 s。
    let port = app.settings.port;
    let (service, style) = match &app.occupant {
        crate::health::Occupant::Wbhub { .. } if app.owns_server() => (
            format!("服务运行中 :{port}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        crate::health::Occupant::Wbhub { version } => (
            format!("外部实例 {version} :{port}"),
            Style::default().fg(Color::Cyan),
        ),
        crate::health::Occupant::Something(_) => (
            format!("端口 {port} 被占用"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        crate::health::Occupant::Nothing => (
            "服务未启动".to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    };
    let right = if app.inflight > 0 {
        format!("{service} · 刷新中")
    } else {
        service
    };

    // 窄屏时右侧让位给账号名，不然 50 列的终端里只能看到「国内」。
    let right_width = if area.width >= 80 { 30 } else { 16 };
    let split = Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_width)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " WorkBuddy Hub ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(account, Style::default().fg(Color::White)),
        ])),
        split[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(right, style)))
            .alignment(ratatui::layout::Alignment::Right),
        split[1],
    );
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = Page::all()
        .iter()
        .map(|page| Line::from(format!(" {} ", page.title())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.page.index())
        .divider("")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn draw_notice(frame: &mut Frame, area: Rect, app: &App) {
    let line = match app.log.back() {
        Some((message, ok)) => Line::from(Span::styled(
            format!(" {}", fit(message, area.width.saturating_sub(2) as usize)),
            Style::default().fg(if *ok { Color::Green } else { Color::Red }),
        )),
        None => Line::from(Span::styled(" 就绪", Style::default().fg(Color::DarkGray))),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let hints = match app.page {
        Page::Overview | Page::Models | Page::Calls => "q 退出  Tab 切换  r 刷新  s 启停服务",
        Page::Growth => "q 退出  Tab 切换  r 刷新  c 签到  d 日常  t 旅行  n 夜猫子",
        Page::Settings => "q 退出  ←→/Tab 切页  ↑↓ 选择  Enter 编辑/切换  k 换密钥",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {hints}"),
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn draw_overview(frame: &mut Frame, area: Rect, app: &App) {
    let account_lines = overview_account_lines(app);
    let api_lines = overview_api_lines(app);
    let tip_lines = overview_tip_lines(app);

    // 高度按内容算，不写死。手机竖屏的终端通常只有 50x20 出头，写死行数会把下面
    // 的「调用次数」「剩余积分」直接顶出屏幕。
    let chrome = 4u16; // 上下边框各一，外加页面上下留白
    let mut constraints = vec![
        Constraint::Length(account_lines.len() as u16 + 2),
        Constraint::Length(api_lines.len() as u16 + 2),
    ];
    let used: u16 = (account_lines.len() + api_lines.len()) as u16 + chrome + 2;
    // 提示块只在真的放得下时才出现，挤掉正文是本末倒置。
    if area.height > used + 6 {
        constraints.push(Constraint::Min(0));
    } else {
        constraints.push(Constraint::Length(0));
    }

    let rows = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints(constraints)
        .split(area);

    frame.render_widget(
        Paragraph::new(account_lines).block(section(&format!(
            "账号与额度 · wbhub {}",
            crate::build_version()
        ))),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(api_lines).block(section("API 平台")),
        rows[1],
    );
    if rows.len() > 2 && rows[2].height > 2 {
        frame.render_widget(
            Paragraph::new(tip_lines)
                .block(section("接入提示"))
                .wrap(Wrap { trim: false }),
            rows[2],
        );
    }
}

fn overview_account_lines(app: &App) -> Vec<Line<'static>> {
    // 顺序按「被裁掉时最不心疼」排：手机竖屏的终端常常只有 17 行可用，最后一行
    // 随时会被顶掉，所以版本号、内部 id 这类信息放最后。
    let mut lines: Vec<Line<'static>> = Vec::new();
    match app.active() {
        Some(account) => {
            lines.push(kv("账号", &account.label()));
        }
        None => {
            lines.push(kv("账号", "未登录"));
            lines.push(Line::from(Span::styled(
                "  在终端里运行 wbhub login 添加账号",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    match &app.balance {
        Some(balance) if balance.unlimited => {
            lines.push(kv_colored("剩余积分", "不限", Color::Green))
        }
        Some(balance) => {
            let with_credit = balance.accounts.iter().filter(|a| a.remain > 0.0).count();
            lines.push(Line::from(vec![
                Span::styled(pad("剩余积分", LABEL), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    credits(balance.total),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    // 总额包含套餐自带的那部分（体验版每月 500），所以套餐名和包数
                    // 都要写出来，否则用户没法核对。
                    format!(
                        "    {} · {} 个包（{with_credit} 个有余量）",
                        if balance.plan_name.is_empty() {
                            "—".to_string()
                        } else {
                            format!("{}（免费版）", balance.plan_name)
                        },
                        balance.accounts.len()
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    if balance.cycle_end.is_empty() {
                        String::new()
                    } else {
                        format!("    最近到期 {}", balance.cycle_end)
                    },
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        }
        None => lines.push(kv_colored("剩余积分", "查询中或不可用", Color::DarkGray)),
    }

    let (today_ok, today_failed) = app.today();
    lines.push(Line::from(vec![
        Span::styled(pad("调用次数", LABEL), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}", app.summary.calls),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 次累计，今日 ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{today_ok} 成功"),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" / "),
        Span::styled(
            format!("{today_failed} 失败"),
            Style::default().fg(if today_failed > 0 {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            pad("Token 用量", LABEL),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format!(
            "输入 {}，输出 {}",
            thousands(app.summary.prompt_tokens),
            thousands(app.summary.completion_tokens)
        )),
        Span::styled("   已消耗 ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            credits(app.summary.credits),
            Style::default().fg(Color::Yellow),
        ),
    ]));

    if let Some(account) = app.active() {
        let expiry = if account.expires_at > 0 {
            crate::cli::clock_of(account.expires_at)
        } else {
            "未知".to_string()
        };
        let token_state = if account.needs_refresh() {
            Span::styled(
                "（已过期或即将过期，调用时自动续期）",
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled("（有效）", Style::default().fg(Color::Green))
        };
        lines.push(Line::from(vec![
            Span::styled(
                pad("Token 到期", LABEL),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(expiry),
            Span::raw(" "),
            token_state,
        ]));
        lines.push(kv(
            "版本",
            &format!("{}（{}）", account.region().label(), account.domain),
        ));
        lines.push(kv("账号标识", &account.id));
    }
    lines
}

fn overview_api_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv(
            "Base URL",
            &format!("http://127.0.0.1:{}/v1", app.settings.port),
        ),
        kv("API Key", &app.settings.api_key),
    ];
    if app.settings.lan_enabled {
        lines.push(kv_colored("局域网", "已开启", Color::Green));
        lines.push(kv("LAN Key", &app.settings.lan_key));
    } else {
        lines.push(kv_colored("局域网", "关闭", Color::DarkGray));
    }
    lines.push(kv(
        "模型数量",
        &format!("{} 个（按 r 刷新）", app.models.len()),
    ));
    lines.push(kv("已存账号", &format!("{} 个", app.accounts.len())));
    match &app.occupant {
        crate::health::Occupant::Nothing => lines.push(Line::from(Span::styled(
            "  按 s 启动服务",
            Style::default().fg(Color::DarkGray),
        ))),
        crate::health::Occupant::Wbhub { version } if app.owns_server() => {
            lines.push(Line::from(Span::styled(
                "  本进程提供（按 s 停止）",
                Style::default().fg(Color::Green),
            )))
        }
        crate::health::Occupant::Wbhub { version } => lines.push(Line::from(vec![
            Span::styled("  由外部实例提供（wbhub ", Style::default().fg(Color::Cyan)),
            Span::styled(version.clone(), Style::default().fg(Color::Cyan)),
            Span::styled(
                "），本界面只负责查看；按 s 会另起一个并撞端口",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        crate::health::Occupant::Something(hint) => lines.push(Line::from(Span::styled(
            format!("  端口被别的程序占着：{hint}"),
            Style::default().fg(Color::Yellow),
        ))),
    }
    lines
}

fn overview_tip_lines(app: &App) -> Vec<Line<'static>> {
    let base = format!("http://127.0.0.1:{}/v1", app.settings.port);
    let example_model = app
        .models
        .iter()
        .find(|m| m.is_free())
        .or_else(|| app.models.first())
        .map(|m| m.id.clone())
        .unwrap_or_else(|| "（按 2 看模型列表）".to_string());
    vec![
        Line::from(Span::styled(
            "在任意 OpenAI 兼容客户端里这样填：",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::raw("  baseUrl  "),
            Span::styled(base, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::raw("  apiKey   "),
            Span::styled(
                app.settings.api_key.clone(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::raw("  model    "),
            Span::styled(example_model, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(Span::styled(
            "按 s 启停服务。要让局域网设备也能用，去「5 设置」打开局域网，",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "再用那里显示的 LAN Key。建议用 tmux 挂着跑，息屏后会休眠。",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

/// 一行的「标签 + 值」。标签按显示宽度对齐到 14 列。
const LABEL: usize = 14;

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(pad(key, LABEL), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

fn kv_colored(key: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(pad(key, LABEL), Style::default().fg(Color::DarkGray)),
        Span::styled(value.to_string(), Style::default().fg(color)),
    ])
}

fn draw_models(frame: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec![
        Cell::from("模型"),
        Cell::from("倍率"),
        Cell::from("上下文"),
        Cell::from("标记"),
    ])
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .models
        .iter()
        .map(|model| {
            let rate = model.rate();
            let rate_style = if model.is_free() {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            let mut marks: Vec<String> = model.badges.clone();
            if model.reasoning.supports {
                marks.push("推理".into());
            }
            if model.supports_images {
                marks.push("图片".into());
            }
            Row::new(vec![
                Cell::from(model.id.clone()),
                Cell::from(if rate.is_empty() {
                    "-".to_string()
                } else {
                    rate
                })
                .style(rate_style),
                Cell::from(thousands(model.context_window)),
                Cell::from(marks.join(" ")).style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(32),
            Constraint::Length(9),
            Constraint::Length(13),
            Constraint::Min(8),
        ],
    )
    .header(header)
    .block(section(&format!("模型（{}）", app.models.len())));

    if app.models.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  还没有模型数据。先登录，再按 r 刷新。",
                Style::default().fg(Color::DarkGray),
            )))
            .block(section("模型")),
            area,
        );
        return;
    }
    frame.render_widget(table, area);
}

fn draw_calls(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let summary = &app.summary;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("累计 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} 次", summary.calls),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  成功 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", summary.succeeded),
                Style::default().fg(Color::Green),
            ),
            Span::styled("  失败 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", summary.failed),
                Style::default().fg(if summary.failed > 0 {
                    Color::Red
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled("  Tokens ", Style::default().fg(Color::DarkGray)),
            Span::raw(thousands(summary.prompt_tokens + summary.completion_tokens)),
            Span::styled("  已消耗 ", Style::default().fg(Color::DarkGray)),
            Span::styled(credits(summary.credits), Style::default().fg(Color::Yellow)),
        ]))
        .block(section("汇总")),
        rows[0],
    );

    let header = Row::new(vec![
        Cell::from("时间"),
        Cell::from("模型"),
        Cell::from("结果"),
        Cell::from("输入"),
        Cell::from("输出"),
        Cell::from("积分"),
        Cell::from("说明"),
    ])
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    let visible = rows[1].height.saturating_sub(3) as usize;
    let rows_data: Vec<Row> = app
        .calls
        .iter()
        .take(visible.max(1))
        .map(|record| {
            let when = chrono::DateTime::from_timestamp_millis(record.timestamp)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            let detail = record.detail.as_deref().unwrap_or("");
            Row::new(vec![
                Cell::from(when).style(Style::default().fg(Color::DarkGray)),
                Cell::from(fit(&record.model, 22)),
                Cell::from(outcome_label(&record.outcome)).style(outcome_style(&record.outcome)),
                Cell::from(thousands(record.prompt_tokens)),
                Cell::from(thousands(record.completion_tokens)),
                Cell::from(credits(record.credits)),
                Cell::from(fit(detail, 30)).style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    if rows_data.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  还没有调用记录。",
                Style::default().fg(Color::DarkGray),
            )))
            .block(section("最近调用")),
            rows[1],
        );
        return;
    }

    frame.render_widget(
        Table::new(
            rows_data,
            [
                Constraint::Length(11),
                Constraint::Length(24),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Min(10),
            ],
        )
        .header(header)
        .block(section("最近调用")),
        rows[1],
    );
}

fn draw_growth(frame: &mut Frame, area: Rect, app: &App) {
    if !app.growth.available {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "  国际版账号没有成长中心。",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  签到与额度仍然可以用：按 1 回到概览看积分。",
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(section("成长中心")),
            area,
        );
        return;
    }

    let view = &app.growth;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(pad("连登", LABEL), Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} 天", view.days),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if view.next_tier.is_empty() {
                    String::new()
                } else {
                    format!(
                        "    下一档 {} 还差 {} 天",
                        view.next_tier, view.next_tier_remaining
                    )
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        kv("补签卡", &format!("{} 张", view.makeup_cards)),
        kv(
            "可兑换",
            if view.redeemable.is_empty() {
                "无".to_string()
            } else {
                view.redeemable.join("、")
            }
            .as_str(),
        ),
        kv("抽奖次数", &format!("{}", view.lottery_chances)),
        Line::from(""),
    ];

    match &view.travel {
        Some(travel) if travel.state.is_empty() => {
            lines.push(kv_colored("猫猫", "还没领养（按 t 领养）", Color::DarkGray))
        }
        Some(travel) if travel.traveling() => lines.push(Line::from(vec![
            Span::styled(pad("猫猫", LABEL), Style::default().fg(Color::DarkGray)),
            Span::styled("在路上", Style::default().fg(Color::Yellow)),
            Span::raw(format!(
                "    {} 到站（约 {} 分钟），到站后按 t 领取",
                crate::cli::clock_of(travel.arrive_at),
                travel.minutes_until(crate::store::now_seconds())
            )),
        ])),
        Some(travel) if travel.arrived() => {
            lines.push(kv_colored("猫猫", "已到站（按 t 领取奖励）", Color::Green))
        }
        Some(travel) => lines.push(kv_colored(
            "猫猫",
            &format!("在家（今日已派出：{}）", travel.daily_limit_reached),
            Color::DarkGray,
        )),
        None => lines.push(kv_colored("猫猫", "状态不可用", Color::DarkGray)),
    }

    match &view.night {
        Some(task) => lines.push(Line::from(vec![
            Span::styled(pad("夜猫子", LABEL), Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}/{}", task.current, task.target)),
            Span::styled(
                format!("    {}", task.accept_status),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                if view.night_window {
                    "    现在可以上报".to_string()
                } else {
                    "    23:00–08:00 才计分".to_string()
                },
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        None => lines.push(kv_colored("夜猫子", "没有这个任务", Color::DarkGray)),
    }

    lines.push(kv(
        "成长任务",
        &format!("{} 项，已完成 {}", view.tasks_total, view.tasks_claimed),
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  c 签到    d 日常奖励（补签 → 礼包 → 兑换 → 抽奖）    t 猫猫旅行    n 夜猫子",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .block(section("成长中心"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_settings(frame: &mut Frame, area: Rect, app: &App) {
    let lan = if app.settings.lan_enabled {
        Span::styled("已开启", Style::default().fg(Color::Green))
    } else {
        Span::styled("关闭", Style::default().fg(Color::DarkGray))
    };
    let verbose = if app.settings.verbose {
        Span::styled("开", Style::default().fg(Color::Green))
    } else {
        Span::styled("关", Style::default().fg(Color::DarkGray))
    };

    let normal = Style::default();
    let selected = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let mut rows: Vec<Row> = Vec::new();
    let mut add = |label: &str, value: Vec<Span<'static>>, hint: &str| {
        let index = rows.len();
        let style = if index == app.settings_row {
            selected
        } else {
            normal
        };
        let mut cells = vec![Cell::from(label.to_string())];
        cells.push(Cell::from(Line::from(value)));
        cells.push(Cell::from(hint.to_string()).style(Style::default().fg(Color::DarkGray)));
        rows.push(Row::new(cells).style(style));
    };

    // 正在编辑的那一行把输入框画出来，光标位置用一个竖线示意。
    let cell = |row: usize, text: String| -> Vec<Span<'static>> {
        match &app.editing {
            Some((editing_row, buffer)) if *editing_row == row => vec![
                Span::styled(buffer.clone(), Style::default().fg(Color::Yellow)),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ],
            _ => vec![Span::raw(text)],
        }
    };

    add(
        "端口",
        cell(0, app.settings.port.to_string()),
        "Enter 编辑，再回车确认",
    );
    add("局域网", vec![lan], "Enter 开关，需要重启服务");
    add(
        "调用记录上限",
        cell(2, app.settings.call_log_limit.to_string()),
        "Enter 编辑，再回车确认",
    );
    add(
        "API Key",
        vec![Span::raw(fit(&app.settings.api_key, 34))],
        "k 重新生成",
    );
    add(
        "LAN Key",
        vec![Span::raw(fit(&app.settings.lan_key, 34))],
        "k 重新生成",
    );
    add(
        "模型刷新间隔",
        cell(3, app.settings.refresh_minutes.to_string()),
        "Enter 编辑（分钟），再回车确认",
    );
    add("服务日志", vec![verbose], "Enter 开关");

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(40),
                Constraint::Min(20),
            ],
        )
        .block(section(&format!("设置 · wbhub {}", crate::build_version()))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_respects_double_width_characters() {
        // 每个汉字占两格，所以 5 格宽只能放下两个字加省略号。
        let clipped = fit("中文昵称", 5);
        assert_eq!(clipped.chars().count(), 3);
        assert_eq!(fit("中文昵称", 8), "中文昵称");
    }

    #[test]
    fn outcomes_map_to_colours() {
        assert_eq!(outcome_label("ok"), "成功");
        assert_eq!(outcome_label("aborted"), "中断");
        assert_eq!(outcome_label("failed"), "失败");
    }
}
