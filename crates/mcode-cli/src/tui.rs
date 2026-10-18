//! TUI 主循环 — ratatui + crossterm。
//! 职责：终端生命周期、按键处理（选择器 > 权限 > 补全 > 历史 > 编辑）、
//! 回合 spawn（对齐桌面版 start_turn：cancel 复位 + 历史锁超时防御）、goal 裁决 spawn、渲染分发。

use crate::app::{ChatApp, Effect, Turn};
use crate::inputlog;
use crate::runtime::{UiEvent, build_harness, event_sink};
use crate::view;
use anyhow::{Result, bail};
use mcode_core::agent::completion::{CompletionState, InputHistory, Trigger};
use mcode_core::agent::context::{MAX_GOAL_ROUNDS, check_goal_complete};
use mcode_core::agent::runner::{MAX_TOOL_TURNS, TurnOpts, run_agent_turn};
use mcode_core::commands::filter_commands;
use mcode_core::config::{Config, DesktopConfig, ThinkingEffort};
use mcode_core::permission::Decision;
use mcode_core::sessions::SessionStore;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::io::stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// 历史锁饿死防御超时（对齐桌面版）
const HIST_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const TICK_MS: u64 = 250;
/// 状态栏动词每 4s 轮换（对齐 TS StatusBar）
const STATUS_VERBS: [&str; 6] = [
    "Brewing",
    "Simmering",
    "Caramelizing",
    "Marinating",
    "Cooking",
    "Perfuming",
];

/// 启动预置：恢复指定会话 / 直接打开会话选择器；cwd 供全局 resume 覆盖工作目录
#[derive(Default)]
pub struct Preload {
    pub session_id: Option<String>,
    pub open_sessions: bool,
    pub cwd: Option<std::path::PathBuf>,
}

pub fn run(cfg: Config, cwd: std::path::PathBuf, preload: Preload) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async_run(cfg, cwd, preload))
}

async fn async_run(cfg: Config, cwd: std::path::PathBuf, preload: Preload) -> Result<()> {
    if !crossterm::tty::IsTty::is_tty(&stdout()) {
        bail!(
            "mcode 需要交互式终端（TTY）。非交互使用请用子命令：mcode add_provider / switch / reset / token"
        );
    }

    // 全局 resume：工作目录切到会话原属项目（对齐 TS listAllSessions/loadSessionGlobal 语义）
    let cwd = preload.cwd.clone().unwrap_or(cwd);

    // 内置资产物化（必须在 build_harness 之前——skills 加载读 ~/.mcode/assets）
    let materialized = crate::assets::materialize_bundled_assets();

    let hist = Arc::new(Mutex::new(
        mcode_core::agent::history::HistoryStore::default(),
    ));
    let mut app = ChatApp::new(cfg.clone(), cwd.clone(), hist);
    // 顶部横幅提示（对齐 TS Banner：version/cwd/model/mode + hint）
    app.transcript.items.push(crate::chat::ChatItem::System {
        text: format!(
            "▛▀▘ MCode v{} · {} · {} · {}\n{}",
            app.version,
            cwd.display(),
            cfg.active_model().unwrap_or_else(|| "no model".into()),
            cfg.default_mode.label(),
            crate::locales::strings(cfg.language).banner_hint
        ),
        error: false,
    });
    for tier in &materialized {
        app.transcript.items.push(crate::chat::ChatItem::System {
            text: format!("✓ 内置资产已物化 {tier} → ~/.mcode/assets"),
            error: false,
        });
    }
    if let Some(id) = &preload.session_id
        && let Err(e) = app.load_session(id)
    {
        bail!("恢复会话 {id} 失败：{e:#}");
    }

    // harness：provider 未就绪也允许进 TUI（引导接管），但回合会被 pre-flight 拦下
    let mut ui_rx = None;
    if cfg.provider_ready().is_ok() {
        match build_harness(&cfg, &cwd).await {
            Ok(h) => {
                for (name, r) in &h.mcp_report {
                    if let Err(e) = r {
                        app.transcript.items.push(crate::chat::ChatItem::System {
                            text: format!("⚠ MCP {name} 连接失败：{e:#}"),
                            error: false,
                        });
                    }
                }
                ui_rx = h.take_rx();
                app.attach_harness(h);
            }
            Err(e) => {
                app.transcript.items.push(crate::chat::ChatItem::System {
                    text: format!("✗ 运行时初始化失败：{e:#}（/doctor 查看诊断）"),
                    error: true,
                });
            }
        }
    } else {
        app.transcript.items.push(crate::chat::ChatItem::System {
            text: crate::locales::strings(cfg.language)
                .no_provider_warning
                .to_string(),
            error: true,
        });
    }

    // 终端
    let mut terminal = ratatui::init();
    // 按键线程：阻塞读 → mpsc
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        // read() 出错（终端关闭）或通道断开即退出线程
        while let Ok(ev) = crossterm::event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut ui = UiState::new(app, ui_rx, preload.open_sessions);
    // 首次启动（无 provider）→ 引导向导接管（对齐 TS providers.length === 0）
    if cfg.providers.is_empty() {
        ui.guide = Some(crate::guide::Guide::new(cfg.language));
    }
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    let mut quit = false;

    while !quit {
        // 会话选择器数据惰性加载（本地为空 → 全局跨项目兜底，对齐 TS listAllSessions）
        if ui.sessions.is_none() && ui.want_sessions {
            let mut list = SessionStore::list(&ui.app.cwd).unwrap_or_default();
            if list.is_empty() {
                list = mcode_core::sessions::SessionStore::list_all();
                if !list.is_empty() {
                    ui.app.transcript.items.push(crate::chat::ChatItem::System {
                        text: format!("本地无会话 — 显示全局 {} 条", list.len()),
                        error: false,
                    });
                }
            }
            ui.sessions = Some((list, 0));
            ui.want_sessions = false;
        }
        terminal.draw(|f| draw(f, &mut ui))?;
        // select 只做接收（分支 future 的借用随 select 结束），处理放外面避免借用冲突
        enum Action {
            Tick,
            Key(Option<crossterm::event::Event>),
            Ui(Option<UiEvent>),
        }
        let action = tokio::select! {
            _ = tick.tick() => Action::Tick,
            ev = key_rx.recv() => Action::Key(ev),
            ue = recv_ui(&mut ui.ui_rx) => Action::Ui(ue),
        };
        match action {
            Action::Tick => {}
            Action::Key(Some(ke)) => quit = handle_event(&mut ui, ke).await,
            Action::Key(None) => quit = true, // 按键线程退出
            Action::Ui(Some(ue)) => {
                let effect = ui.app.on_ui_event(ue);
                quit = apply_effect(&mut ui, effect).await.unwrap_or(true);
            }
            Action::Ui(None) => {}
        }
    }

    ratatui::restore();
    Ok(())
}

/// 只借用 receiver 字段的接收 future（select 分支内不触碰 app 状态）
async fn recv_ui(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<UiEvent>>,
) -> Option<UiEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// 渲染 + 输入的界面状态（app 之外的易变 UI 态）
struct UiState {
    app: ChatApp,
    ui_rx: Option<tokio::sync::mpsc::UnboundedReceiver<UiEvent>>,
    input: String,
    /// 光标位置（char 索引）
    cursor: usize,
    completion: CompletionState,
    input_history: InputHistory,
    reasoning_expanded: bool,
    scroll_from_bottom: usize,
    busy_since: Option<Instant>,
    model_picker: Option<(Vec<String>, usize)>,
    sessions: Option<(Vec<mcode_core::sessions::SessionMeta>, usize)>,
    want_sessions: bool,
    files: Vec<String>,
    /// 首次启动引导（providers 为空时接管界面）
    guide: Option<crate::guide::Guide>,
}

impl UiState {
    fn new(
        app: ChatApp,
        ui_rx: Option<tokio::sync::mpsc::UnboundedReceiver<UiEvent>>,
        open_sessions: bool,
    ) -> Self {
        let mut input_history = InputHistory::default();
        input_history.extend(inputlog::load(&app.cwd));
        let files = list_workspace_files(&app.cwd);
        Self {
            app,
            ui_rx,
            input: String::new(),
            cursor: 0,
            completion: CompletionState::default(),
            input_history,
            reasoning_expanded: false,
            scroll_from_bottom: 0,
            busy_since: None,
            model_picker: None,
            sessions: None,
            want_sessions: open_sessions,
            files,
            guide: None,
        }
    }

    fn sync_completion(&mut self) {
        self.completion.sync_from_input(&self.input);
    }

    fn insert(&mut self, s: &str) {
        let char_len = self.input.chars().count();
        let pos = self.cursor.min(char_len);
        let head: String = self.input.chars().take(pos).collect();
        let tail: String = self.input.chars().skip(pos).collect();
        self.input = format!("{head}{s}{tail}");
        self.cursor = pos + s.chars().count();
        self.files = list_workspace_files(&self.app.cwd); // @ 触发时重扫（对齐 TS onAtTrigger）
        self.sync_completion();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let pos = self.cursor.min(self.input.chars().count());
        let head: String = self.input.chars().take(pos - 1).collect();
        let tail: String = self.input.chars().skip(pos).collect();
        self.input = format!("{head}{tail}");
        self.cursor = pos - 1;
        self.sync_completion();
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.input.chars().count() as i32;
        let next = (self.cursor as i32 + delta).clamp(0, len);
        self.cursor = next as usize;
    }

    fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.completion.close();
        if !text.trim().is_empty() {
            self.input_history.push(text.trim().to_string());
            inputlog::append(text.trim(), &self.app.cwd);
        }
        text
    }
}

/// 补全候选（/ 指令目录 或 @ 工作区文件）
fn candidates(ui: &UiState) -> Vec<mcode_core::agent::completion::Candidate> {
    use mcode_core::agent::completion::Candidate;
    match ui.completion.trigger {
        Some(Trigger::Slash) => {
            let mut out: Vec<Candidate> = filter_commands(&ui.completion.query)
                .into_iter()
                .map(|c| Candidate {
                    name: c.name.trim_start_matches('/').to_string(),
                    desc: c.desc.to_string(),
                    is_dir: false,
                })
                .collect();
            // 技能同名斜杠指令（对齐 TS createSkillCommands：/skill-name [args]；
            // 查 app.skills 无 harness 门槛 —— 无 provider 时补全也要可见）
            {
                let q = ui.completion.query.to_lowercase();
                for s in &ui.app.skills {
                    if s.entry.name.to_lowercase().starts_with(&q) {
                        out.push(Candidate {
                            name: s.entry.name.clone(),
                            desc: format!("skill: {}", s.entry.description),
                            is_dir: false,
                        });
                    }
                }
            }
            out
        }
        Some(Trigger::At) => {
            let q = ui.completion.query.to_lowercase();
            ui.files
                .iter()
                .filter(|f| q.is_empty() || f.to_lowercase().contains(&q))
                .take(50)
                .map(|f| Candidate {
                    name: f.clone(),
                    desc: String::new(),
                    is_dir: f.ends_with('/'),
                })
                .collect()
        }
        None => vec![],
    }
}

/// workspace 文件清单：前 2 层目录、上限 200（对齐 util/files.ts listWorkspaceFiles）
fn list_workspace_files(cwd: &std::path::Path) -> Vec<String> {
    const SKIP: [&str; 6] = [
        "node_modules",
        ".git",
        "dist",
        "target",
        ".DS_Store",
        "__pycache__",
    ];
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(cwd) else {
        return out;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if SKIP.contains(&name.as_str()) || name.starts_with('.') {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            out.push(format!("{name}/"));
            // 第二层
            if let Ok(sub) = std::fs::read_dir(e.path()) {
                let mut subs: Vec<_> = sub.filter_map(|x| x.ok()).collect();
                subs.sort_by_key(|x| x.file_name());
                for s in subs {
                    let sn = s.file_name().to_string_lossy().to_string();
                    if SKIP.contains(&sn.as_str()) || sn.starts_with('.') {
                        continue;
                    }
                    let sdir = s.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    out.push(format!("{name}/{sn}{}", if sdir { "/" } else { "" }));
                    if out.len() >= 200 {
                        return out;
                    }
                }
            }
        } else {
            out.push(name);
        }
        if out.len() >= 200 {
            break;
        }
    }
    out
}

/// 按键处理，返回 true = 退出
async fn handle_event(ui: &mut UiState, ev: crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let key = match ev {
        Event::Key(k) => k,
        Event::Paste(text) => {
            ui.insert(&text);
            return false;
        }
        Event::Resize(_, _) => return false,
        _ => return false,
    };
    let KeyEvent {
        code, modifiers, ..
    } = key;

    // -- 首次启动引导（接管一切按键；对齐 TS providers 为空时渲染 GuidedSetup）--
    if let Some(guide) = ui.guide.as_mut() {
        // ctrl+c 始终可退出
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match guide.handle_key(key) {
            crate::guide::GuideAction::None => {}
            crate::guide::GuideAction::Completed(result) => {
                ui.guide = None;
                ui.app.complete_guide(result);
                // provider 就绪 → 立即建 harness（等价 /model 的 RebuildHarness 路径）
                return apply_effect(ui, Effect::RebuildHarness)
                    .await
                    .unwrap_or(false);
            }
        }
        return false;
    }

    // -- 模型选择器 --
    if let Some((models, sel)) = ui.model_picker.as_mut() {
        match code {
            KeyCode::Up => *sel = (*sel + models.len().saturating_sub(1)) % models.len().max(1),
            KeyCode::Down => *sel = (*sel + 1) % models.len().max(1),
            KeyCode::Esc => ui.model_picker = None,
            KeyCode::Enter => {
                let model = models.get(*sel).cloned();
                ui.model_picker = None;
                if let Some(m) = model
                    && ui.app.set_model(&m)
                {
                    ui.app.transcript.items.push(crate::chat::ChatItem::System {
                        text: format!("✓ 模型 → {m}（重建运行时生效）"),
                        error: false,
                    });
                    return apply_effect(ui, Effect::RebuildHarness)
                        .await
                        .unwrap_or(false);
                }
            }
            _ => {}
        }
        return false;
    }

    // -- 会话选择器 --
    if let Some((list, sel)) = ui.sessions.as_mut() {
        match code {
            KeyCode::Up => *sel = (*sel + list.len().saturating_sub(1)) % list.len().max(1),
            KeyCode::Down => *sel = (*sel + 1) % list.len().max(1),
            KeyCode::Esc => ui.sessions = None,
            KeyCode::Enter => {
                let picked = list.get(*sel).cloned();
                ui.sessions = None;
                if let Some(s) = picked {
                    // 全局兜底列表里的会话可能属于其他项目 → 以 meta.cwd 读取并切换工作目录
                    let same_project = s.cwd == ui.app.cwd.display().to_string();
                    let mut new_cwd = None;
                    let res = if same_project {
                        ui.app.load_session(&s.id)
                    } else {
                        let target = std::path::PathBuf::from(&s.cwd);
                        let r = ui.app.load_session_from(&target, &s.id);
                        if r.is_ok() {
                            new_cwd = Some(target);
                        }
                        r
                    };
                    if let Some(cwd) = new_cwd {
                        ui.app.cwd = cwd;
                    }
                    match res {
                        Ok(_) => {}
                        Err(e) => ui.app.transcript.items.push(crate::chat::ChatItem::System {
                            text: format!("恢复会话失败：{e:#}"),
                            error: true,
                        }),
                    }
                }
            }
            _ => {}
        }
        return false;
    }

    // -- 权限提示 --
    if ui.app.pending_confirm.is_some() {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => ui.app.confirm(Decision::Allow),
            KeyCode::Char('a') | KeyCode::Char('A') => ui.app.confirm(Decision::AllowAlways),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ui.app.interrupt(),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return true,
            _ => {}
        }
        return false;
    }

    // -- 补全面板 --
    let cands = candidates(ui);
    if ui.completion.is_open() && !cands.is_empty() {
        match code {
            KeyCode::Up => {
                ui.completion.move_up(cands.len());
                return false;
            }
            KeyCode::Down => {
                ui.completion.move_down(cands.len());
                return false;
            }
            KeyCode::Tab => {
                let sel = ui.completion.selected.min(cands.len() - 1);
                let name = cands[sel].name.clone();
                ui.input = ui.completion.apply(&ui.input, &name);
                ui.cursor = ui.input.chars().count();
                ui.sync_completion();
                return false;
            }
            _ => {}
        }
    }

    match code {
        KeyCode::Enter => {
            let text = ui.take_input();
            ui.scroll_from_bottom = 0;
            let effect = ui.app.submit(&text);
            return apply_effect(ui, effect).await.unwrap_or(false);
        }
        KeyCode::Up => {
            if let Some(prev) = ui.input_history.recall_up(&ui.input) {
                ui.input = prev;
                ui.cursor = ui.input.chars().count();
                ui.sync_completion();
            }
        }
        KeyCode::Down => {
            if let Some(next) = ui.input_history.recall_down() {
                ui.input = next;
                ui.cursor = ui.input.chars().count();
                ui.sync_completion();
            }
        }
        KeyCode::Tab => { /* 无补全时忽略 */ }
        KeyCode::BackTab => {
            // shift+tab：循环权限模式（真值在 harness.deps.perms，锁被持有时跳过）
            if let Some(h) = &ui.app.harness
                && let Ok(mut p) = h.deps.perms.try_lock()
            {
                p.cycle_mode();
                ui.app.mode = p.mode;
            }
        }
        KeyCode::Esc => {
            if ui.completion.is_open() {
                ui.completion.close();
            } else {
                ui.app.interrupt();
                ui.busy_since = if ui.app.busy { ui.busy_since } else { None };
            }
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            if ui.app.busy {
                ui.app.interrupt();
            } else {
                return true;
            }
        }
        KeyCode::Char('o') if modifiers.contains(KeyModifiers::CONTROL) => {
            ui.reasoning_expanded = !ui.reasoning_expanded;
        }
        KeyCode::PageUp => {
            ui.scroll_from_bottom = ui.scroll_from_bottom.saturating_sub(10);
        }
        KeyCode::PageDown => {
            ui.scroll_from_bottom = ui.scroll_from_bottom.saturating_add(10);
        }
        KeyCode::Backspace => ui.backspace(),
        KeyCode::Left => ui.move_cursor(-1),
        KeyCode::Right => ui.move_cursor(1),
        KeyCode::Home => ui.cursor = 0,
        KeyCode::End => ui.cursor = ui.input.chars().count(),
        KeyCode::Char(c) => {
            if modifiers.contains(KeyModifiers::SHIFT) && c == 'Z' {
                // shift+tab 由 crossterm 报为 BackTab；此处兜底忽略
            } else if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                ui.insert(&c.to_string());
            }
        }
        _ => {}
    }
    false
}

/// 效果落地，返回 true = 退出
async fn apply_effect(ui: &mut UiState, effect: Effect) -> Result<bool> {
    match effect {
        Effect::None => {}
        Effect::Quit => return Ok(true),
        Effect::Turn(t) => {
            ui.busy_since = Some(Instant::now());
            spawn_turn(ui, t).await;
        }
        Effect::TurnDone => {
            ui.busy_since = None;
            // 队列消化
            if let Some(t) = ui.app.pop_queue() {
                ui.busy_since = Some(Instant::now());
                spawn_turn(ui, t).await;
            }
        }
        Effect::SpawnGoalCheck => {
            spawn_goal_check(ui).await;
        }
        Effect::RebuildHarness => {
            let cfg = ui.app.cfg.clone();
            let cwd = ui.app.cwd.clone();
            match build_harness(&cfg, &cwd).await {
                Ok(h) => {
                    ui.ui_rx = h.take_rx();
                    ui.app.attach_harness(h);
                }
                Err(e) => {
                    ui.app.transcript.items.push(crate::chat::ChatItem::System {
                        text: format!("✗ 重建运行时失败：{e:#}"),
                        error: true,
                    });
                }
            }
        }
        Effect::OpenModelPicker => {
            let models = ui
                .app
                .cfg
                .active_provider()
                .map(|p| p.models.clone())
                .unwrap_or_default();
            if models.is_empty() {
                ui.app.transcript.items.push(crate::chat::ChatItem::System {
                    text: "未配置供应商模型".into(),
                    error: true,
                });
            } else {
                ui.model_picker = Some((models, 0));
            }
        }
        Effect::OpenSessions => ui.want_sessions = true,
    }
    Ok(false)
}

/// spawn 回合（对齐桌面版 start_turn：cancel 复位 + 锁超时防御）
async fn spawn_turn(ui: &mut UiState, t: Turn) {
    let Some(h) = ui.app.harness.clone() else {
        ui.app.transcript.items.push(crate::chat::ChatItem::System {
            text: "未初始化完成，无法启动回合（/doctor 查看诊断）".into(),
            error: true,
        });
        ui.app.busy = false;
        return;
    };
    // 复位打断信号（只在上一回合终态后调用，复位安全——见桌面版注释）
    let _ = h.deps.cancel_tx.send(false);
    let deps = h.deps.clone();
    let hist = ui.app.hist.clone();
    let sink = event_sink(h.ui_tx.clone());
    let goal = ui.app.goal.clone();
    let effort = current_effort();
    tokio::spawn(async move {
        let mut hs =
            match tokio::time::timeout(HIST_LOCK_TIMEOUT, hist.lock()).await {
                Ok(g) => g,
                Err(_) => {
                    let _ = h.ui_tx.send(UiEvent::Agent(mcode_core::agent::AgentEvent::Error {
                    message: format!(
                        "回合历史锁被占用超过 {}s（上一回合可能未正常收尾），本次回合已放弃",
                        HIST_LOCK_TIMEOUT.as_secs()
                    ),
                }));
                    let _ = h
                        .ui_tx
                        .send(UiEvent::Agent(mcode_core::agent::AgentEvent::Done {
                            reply: String::new(),
                        }));
                    return;
                }
            };
        run_agent_turn(
            &deps,
            &t.input,
            TurnOpts {
                instructions: None,
                max_turns: MAX_TOOL_TURNS,
                goal,
                tools_filter: None,
                attachments: t.attachments,
                effort,
                history: &mut hs,
                emit: sink,
            },
        )
        .await;
    });
}

/// spawn goal 完成裁判（独立单发调用，结果经 GoalCheck 回主循环）
async fn spawn_goal_check(ui: &mut UiState) {
    let Some(h) = ui.app.harness.clone() else {
        return;
    };
    let Some(goal) = ui.app.goal.clone() else {
        return;
    };
    let backend = h.deps.backend.clone();
    let cancel = h.deps.cancel_tx.subscribe();
    let ui_tx = h.ui_tx.clone();
    tokio::spawn(async move {
        let done = check_goal_complete(backend.as_ref(), &goal, cancel).await;
        let _ = ui_tx.send(UiEvent::GoalCheck(done));
    });
}

fn current_effort() -> ThinkingEffort {
    DesktopConfig::load().thinking_effort()
}

// ---------- 渲染 ----------

fn draw(f: &mut ratatui::Frame, ui: &mut UiState) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(area);
    draw_transcript(f, chunks[0], ui);
    draw_input(f, chunks[1], ui);
    draw_status(f, chunks[2], ui);
    if ui.app.pending_confirm.is_some() {
        draw_confirm(f, area, ui);
    }
    if ui.model_picker.is_some() {
        draw_model_picker(f, area, ui);
    }
    if let Some((list, sel)) = &ui.sessions {
        draw_sessions(f, area, list, *sel);
    }
    if let Some(guide) = &ui.guide {
        crate::guide::draw(f, area, guide);
    }
}

fn draw_transcript(f: &mut ratatui::Frame, area: Rect, ui: &mut UiState) {
    // 补全面板占转录区底部
    let cands = candidates(ui);
    let panel_h = if ui.completion.is_open() && !cands.is_empty() {
        (cands.len().min(8) as u16) + 2
    } else {
        0
    };
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(panel_h)]).split(area);
    let list_area = chunks[0];

    let width = list_area.width.saturating_sub(2) as usize;
    let lines = view::transcript_lines(&ui.app.transcript.items, width, ui.reasoning_expanded);
    let h = list_area.height.saturating_sub(2) as usize;
    ui.scroll_from_bottom = ui.scroll_from_bottom.min(lines.len().saturating_sub(1));
    let top = lines
        .len()
        .saturating_sub(h)
        .saturating_sub(ui.scroll_from_bottom);
    let title = format!(
        " mcode v{} · {} · {} · {} ",
        ui.app.version,
        ui.app.cwd.display(),
        ui.app.cfg.active_model().unwrap_or_default(),
        ui.app.mode.label()
    );
    let para = Paragraph::new(ratatui::text::Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, Style::default().fg(Color::Cyan))),
        )
        .scroll((top as u16, 0));
    f.render_widget(para, list_area);

    if panel_h > 0 {
        let sel = ui.completion.selected;
        let items: Vec<ListItem> = cands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let style = if i == sel {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default()
                };
                let desc = if c.desc.is_empty() {
                    c.name.clone()
                } else {
                    format!("{:<18} {}", c.name, c.desc)
                };
                ListItem::new(Line::from(Span::styled(desc, style)))
            })
            .collect();
        // 面板标题按触发器区分（对齐 TS InputBox：trigger === '/' ? slashHint : atHint）
        let t = crate::locales::strings(ui.app.cfg.language);
        let hint = match ui.completion.trigger {
            Some(Trigger::Slash) => t.slash_hint,
            _ => t.at_hint,
        };
        let list = List::new(items).block(Block::default().borders(Borders::ALL).title(
            Span::styled(format!(" {hint} "), Style::default().fg(Color::DarkGray)),
        ));
        f.render_widget(list, chunks[1]);
    }
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, ui: &UiState) {
    let prefix = if ui.app.busy { "… " } else { "> " };
    let shown = format!("{}{}", prefix, ui.input);
    let (title, tcolor) = if ui.app.busy {
        (" Esc 打断 ", Color::Red)
    } else {
        (" 输入 ", Color::DarkGray)
    };
    let para = Paragraph::new(shown).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, Style::default().fg(tcolor))),
    );
    // 假光标：ratatui 提供 cursor 定位（terminal.show_cursor 配合），这里用反色 span 近似
    f.render_widget(para, area);
    // 真实光标定位（反色单元格近似）
    let inner_x = area.x + 1 + (prefix.chars().count() + ui.cursor) as u16;
    let clamped_x = inner_x.min(area.x + area.width.saturating_sub(2));
    if let Some(cell) = f.buffer_mut().cell_mut((clamped_x, area.y + 1)) {
        cell.set_style(Style::default().bg(Color::Gray).fg(Color::Black));
    }
}

fn draw_status(f: &mut ratatui::Frame, area: Rect, ui: &UiState) {
    let mut left = vec![];
    if ui.app.busy {
        let elapsed = ui.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let verb = STATUS_VERBS[((elapsed / 4) as usize) % STATUS_VERBS.len()];
        left.push(Span::styled(
            format!(" · {verb}… ({elapsed}s · {} tokens) ", ui.app.tokens_total),
            Style::default().fg(Color::Cyan),
        ));
    }
    if ui.app.goal.is_some() {
        left.push(Span::styled(
            format!(
                " /goal {}-{}/{} ",
                ui.app.goal_rounds + 1,
                MAX_GOAL_ROUNDS,
                "active"
            ),
            Style::default().fg(Color::Magenta),
        ));
    }
    let mode_style = if ui.app.mode == mcode_core::permission::Mode::Passon {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let right = Line::from(Span::styled(
        format!(
            "{}: {} (shift+tab) · ctrl+o reasoning ",
            crate::locales::strings(ui.app.cfg.language).mode_hint,
            ui.app.mode.label()
        ),
        mode_style,
    ));
    let line = Line::from(left);
    let line_w = line.width() as u16;
    let mut spans = line.spans;
    let right_w = right.width() as u16;
    let pad = area.width.saturating_sub(line_w + right_w);
    spans.push(Span::raw(" ".repeat(pad as usize)));
    spans.extend(right.spans);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_confirm(f: &mut ratatui::Frame, area: Rect, ui: &UiState) {
    let Some(req) = &ui.app.pending_confirm else {
        return;
    };
    let t = crate::locales::strings(ui.app.cfg.language);
    let rect = centered_rect(60, 8, area);
    let text = vec![
        Line::from(Span::styled(
            format!("⚠ {} `{}`", t.permission_ask, req.tool),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            req.args_summary.clone(),
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "y = {} · a = always · n = {} (Esc)",
                t.permission_allow, t.permission_deny
            ),
            Style::default().fg(Color::Yellow),
        )),
    ];
    let para = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" permission "),
    );
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(para, rect);
}

fn draw_model_picker(f: &mut ratatui::Frame, area: Rect, ui: &UiState) {
    let Some((models, sel)) = &ui.model_picker else {
        return;
    };
    let rect = centered_rect(50, (models.len() as u16 + 2).min(14), area);
    let items: Vec<ListItem> = models
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let style = if i == *sel {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(m.clone(), style)))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" model (↑↓ · Enter · Esc) "),
        )
        .highlight_symbol("> ");
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_stateful_widget(
        list,
        rect,
        &mut ratatui::widgets::ListState::default().with_selected(Some(*sel)),
    );
}

fn draw_sessions(
    f: &mut ratatui::Frame,
    area: Rect,
    list: &[mcode_core::sessions::SessionMeta],
    sel: usize,
) {
    if list.is_empty() {
        let rect = centered_rect(50, 5, area);
        let para = Paragraph::new("当前目录没有历史会话")
            .block(Block::default().borders(Borders::ALL).title(" resume "));
        f.render_widget(ratatui::widgets::Clear, rect);
        f.render_widget(para, rect);
        return;
    }
    let rect = centered_rect(64, (list.len() as u16 + 2).min(16), area);
    let items: Vec<ListItem> = list
        .iter()
        .map(|s| {
            // 全局列表可能含其他项目的会话 → 显示 cwd 便于辨认
            ListItem::new(Line::from(Span::raw(format!(
                "{} · {} · {} turns · {} tokens · {}",
                s.title, s.updated, s.turns, s.tokens_total, s.cwd
            ))))
        })
        .collect();
    let lw = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" resume (↑↓ · Enter · Esc) "),
        )
        .highlight_symbol("> ");
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_stateful_widget(
        lw,
        rect,
        &mut ratatui::widgets::ListState::default().with_selected(Some(sel)),
    );
}

pub(crate) fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let w = r.width * percent_x / 100;
    let x = r.x + (r.width.saturating_sub(w)) / 2;
    let h = height.min(r.height.saturating_sub(2));
    let y = r.y + (r.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
