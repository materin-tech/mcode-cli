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
use mcode_core::agent::runner::{TurnOpts, run_agent_turn};
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
    // 括号粘贴模式：终端把整段粘贴包成单个 Event::Paste（ESC[200~…ESC[201~），
    // 粘贴里的换行不会被抓成回车键逐行提交（对齐 TS 版多行粘贴；不支持的老终端行为不变）
    let _ = crossterm::execute!(stdout(), crossterm::event::EnableBracketedPaste);
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

    let _ = crossterm::execute!(stdout(), crossterm::event::DisableBracketedPaste);
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
    /// 多行粘贴登记表（token, content）：粘贴折叠为占位 token 入框，提交时展开还原全文
    pastes: Vec<(String, String)>,
    /// 下一个粘贴占位的编号（#1、#2…，会话内递增）
    next_paste_id: usize,
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
            pastes: Vec::new(),
            next_paste_id: 1,
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

    /// 登记一段多行粘贴，返回占位 token（对齐主流终端 agent：`[Pasted text #N +M lines]`）。
    /// 行数按去尾换行后的内容计（"a\n" 视为 1 行）。
    fn register_paste(&mut self, content: &str) -> String {
        let id = self.next_paste_id;
        self.next_paste_id += 1;
        let trimmed = content.trim_end_matches('\n');
        let lines = trimmed.split('\n').count();
        let token = format!("[Pasted text #{id} +{lines} lines]");
        self.pastes.push((token.clone(), content.to_string()));
        token
    }

    /// 提交前展开：把输入中的占位 token 还原为粘贴全文
    fn expand_tokens(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (token, content) in &self.pastes {
            if out.contains(token.as_str()) {
                out = out.replace(token.as_str(), content);
            }
        }
        out
    }

    /// 光标左侧是否恰好压在某个 token 末尾 → 返回该 token（整体删除用）
    fn token_ending_at(&self, cursor: usize) -> Option<String> {
        let prefix: String = self
            .input
            .chars()
            .take(cursor.min(self.input.chars().count()))
            .collect();
        self.pastes
            .iter()
            .find(|(token, _)| prefix.ends_with(token.as_str()))
            .map(|(token, _)| token.clone())
    }

    /// 粘贴载荷入框：多行折叠为占位 token，单行原样插入
    fn insert_paste(&mut self, cleaned: &str) {
        if cleaned.is_empty() {
            return;
        }
        if cleaned.contains('\n') {
            let token = self.register_paste(cleaned);
            self.insert(&token);
        } else {
            self.insert(cleaned);
        }
    }

    fn insert(&mut self, s: &str) {
        let char_len = self.input.chars().count();
        let pos = self.cursor.min(char_len);
        let head: String = self.input.chars().take(pos).collect();
        let tail: String = self.input.chars().skip(pos).collect();
        self.input = format!("{head}{s}{tail}");
        self.cursor = pos + s.chars().count();
        // 文件列表只有 @ 补全用得到：仅当本次输入引入 '@' 才重扫。此前每击键
        // 全量重扫工作区（对齐 TS onAtTrigger 的"触发时扫描"语义走偏了），
        // 深目录上每敲一个字符卡一次
        if s.contains('@') {
            self.files = list_workspace_files(&self.app.cwd);
        }
        self.sync_completion();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let pos = self.cursor.min(self.input.chars().count());
        // 光标压在粘贴 token 末尾：整体删除（一次退格移除整个占位）
        if let Some(token) = self.token_ending_at(pos) {
            let token_len = token.chars().count();
            let head: String = self.input.chars().take(pos - token_len).collect();
            let tail: String = self.input.chars().skip(pos).collect();
            self.input = format!("{head}{tail}");
            self.cursor = pos - token_len;
            self.sync_completion();
            return;
        }
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
        // 提交即展开：历史与模型都拿粘贴全文（token 只在编辑时存在）
        let expanded = self.expand_tokens(&text);
        if !expanded.trim().is_empty() {
            self.input_history.push(expanded.trim().to_string());
            inputlog::append(expanded.trim(), &self.app.cwd);
        }
        expanded
    }
}

/// 补全候选（/ 指令目录 或 @ 工作区文件）
/// @ 候选：只展示查询目录的立即子项（对齐 Claude Code 的层级下钻）。
/// query 按最后一个 '/' 拆成目录前缀 + 层内过滤词："" → 根目录一层；
/// "src/" → src 的子项；"src/ma" → src 子项中含 "ma" 的。目录候选带尾随 '/'。
fn at_candidates(files: &[String], query: &str) -> Vec<mcode_core::agent::completion::Candidate> {
    use mcode_core::agent::completion::Candidate;
    let q = query.to_lowercase();
    let (dir, filter) = match q.rfind('/') {
        Some(i) => (&q[..i + 1], &q[i + 1..]),
        None => ("", q.as_str()),
    };
    files
        .iter()
        .filter(|f| {
            if !f.to_lowercase().starts_with(dir) || f.len() <= dir.len() {
                return false;
            }
            let name = &f[dir.len()..];
            let core = name.strip_suffix('/').unwrap_or(name);
            // 立即子项：名字部分不含更深的 '/'
            !core.contains('/') && (filter.is_empty() || core.to_lowercase().contains(filter))
        })
        .take(50)
        .map(|f| Candidate {
            name: f.clone(),
            desc: String::new(),
            is_dir: f.ends_with('/'),
        })
        .collect()
}

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
        Some(Trigger::At) => at_candidates(&ui.files, &ui.completion.query),
        None => vec![],
    }
}

/// workspace 文件清单：前 2 层目录、上限 200（对齐 util/files.ts listWorkspaceFiles）
/// @ 候选的工作区扫描：有界递归（深度 8、总量 200），目录带尾随 '/'。
/// 层级展示交给 at_candidates 按查询目录逐层过滤，这里只攒全量扁平表。
fn list_workspace_files(cwd: &std::path::Path) -> Vec<String> {
    const SKIP: [&str; 6] = [
        "node_modules",
        ".git",
        "dist",
        "target",
        ".DS_Store",
        "__pycache__",
    ];
    const MAX_DEPTH: usize = 8;
    const MAX_ENTRIES: usize = 200;
    fn walk(dir: &std::path::Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
        if depth >= MAX_DEPTH || out.len() >= MAX_ENTRIES {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            if SKIP.contains(&name.as_str()) || name.starts_with('.') || out.len() >= MAX_ENTRIES {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                out.push(format!("{prefix}{name}/"));
                walk(&e.path(), &format!("{prefix}{name}/"), depth + 1, out);
            } else {
                out.push(format!("{prefix}{name}"));
            }
        }
    }
    let mut out = vec![];
    walk(cwd, "", 0, &mut out);
    out
}

/// 按键处理，返回 true = 退出
async fn handle_event(ui: &mut UiState, ev: crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let key = match ev {
        Event::Key(k) => k,
        Event::Paste(text) => {
            let cleaned = normalize_paste(&text);
            ui.insert_paste(&cleaned);
            return false;
        }
        Event::Resize(_, _) => return false,
        _ => return false,
    };
    // Windows 上 crossterm 对每次按键发 Press + Release 两个事件（macOS 后端只发 Press），
    // 不过滤会把每个字符处理两遍（实测：终端输入双字符）。自动重复在 Windows 后端
    // 是多条 Press 事件，不受影响；Release 一律忽略。
    if key.kind != crossterm::event::KeyEventKind::Press {
        return false;
    }
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
                ui.input = ui.completion.apply(&ui.input, &cands[sel]);
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
            // 有内容：跨行移动光标（文本编辑器语义）；空框：历史回填
            if !ui.input.is_empty() {
                ui.cursor = cursor_vertical_move(&ui.input, ui.cursor, -1);
            } else if let Some(prev) = ui.input_history.recall_up(&ui.input) {
                ui.input = prev;
                ui.cursor = ui.input.chars().count();
                ui.sync_completion();
            }
        }
        KeyCode::Down => {
            if !ui.input.is_empty() {
                ui.cursor = cursor_vertical_move(&ui.input, ui.cursor, 1);
            } else if let Some(next) = ui.input_history.recall_down() {
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
        // 折叠态内容视为整体：横移键不动（先 ↑ 展开），避免在不可见位置插入
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
                max_turns: 0, // 主会话不设轮次上限，死循环处理直到任务完成
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
    // 输入框高度随内容行数自适应（多行粘贴折叠为占位 token，框体通常单行）
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(input_box_height(&ui.input, INPUT_MAX_ROWS)),
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
        // 滑动窗口：候选项超过 8 条时随选中项滚动，选中项始终可见
        let (start, end) = completion_window(cands.len(), sel, 8);
        let items: Vec<ListItem> = cands[start..end]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let style = if start + i == sel {
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
    let (title, tcolor) = if ui.app.busy {
        (" Esc 打断 ", Color::Red)
    } else {
        (" 输入 ", Color::DarkGray)
    };
    // 多行输入：首行带提示符，后续行两格缩进（\n 是文本换行，不是提交）；
    // 多行粘贴已折叠为占位 token，框内通常单行（历史回填的多行条目仍整块显示）
    let lines: Vec<Line> = ui
        .input
        .split('\n')
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                Line::from(format!("{prefix}{l}"))
            } else {
                Line::from(format!("  {l}"))
            }
        })
        .collect();
    let visible = area.height.saturating_sub(2) as usize;
    let (crow, ccol) = input_row_col(&ui.input, ui.cursor);
    // 光标行超出可视区时滚动，保证光标始终可见
    let top = if visible == 0 || crow < visible {
        0
    } else {
        crow + 1 - visible
    };
    let para = Paragraph::new(ratatui::text::Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, Style::default().fg(tcolor))),
        )
        .scroll((top as u16, 0));
    f.render_widget(para, area);
    // 真实光标定位（反色单元格近似），随行/列移动
    let row = crow.saturating_sub(top) as u16;
    if row < visible as u16 {
        let indent = if crow == 0 {
            prefix.chars().count() as u16
        } else {
            2
        };
        let x = (area.x + 1 + indent + ccol as u16).min(area.x + area.width.saturating_sub(2));
        let y = area.y + 1 + row;
        if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
            cell.set_style(Style::default().bg(Color::Gray).fg(Color::Black));
        }
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

/// 粘贴内容换行归一：\r\n / \r → \n。
/// 换行是文本（输入框内换行显示），只有回车键（KeyCode::Enter）才提交——
/// 对齐 TS 版多行粘贴语义（回车键 ≠ 换行符）。
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 补全面板滑动窗口：候选项超过 max 时面板整体滚动，选中项始终可见
/// （对齐 TS CompletionPanel 的 start = max(0, min(sel - max + 1, len - max))）。
/// 返回渲染切片区间 (start, end)。
fn completion_window(len: usize, sel: usize, max: usize) -> (usize, usize) {
    if len == 0 || max == 0 {
        return (0, 0);
    }
    if len <= max {
        return (0, len);
    }
    let start = (sel + 1).saturating_sub(max).min(len - max);
    (start, start + max)
}

/// 字符游标 → (行, 列)：行按 \n 计，列为行内字符偏移
fn input_row_col(input: &str, cursor: usize) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    for (i, ch) in input.chars().enumerate() {
        if i >= cursor {
            break;
        }
        if ch == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

/// 光标垂直跨行移动（文本编辑器语义）：保留列、目标列 clamp 到目标行行尾，
/// 已在首行/末行则原地不动。delta 为 -1（上）/ 1（下）。
fn cursor_vertical_move(input: &str, cursor: usize, delta: i32) -> usize {
    let (row, col) = input_row_col(input, cursor);
    let total = input.split('\n').count() as i32;
    let target = (row as i32 + delta).clamp(0, total - 1);
    if target == row as i32 {
        return cursor;
    }
    let target = target as usize;
    let target_col = input
        .split('\n')
        .nth(target)
        .map_or(0, |l| l.chars().count())
        .min(col);
    let prefix: usize = input
        .split('\n')
        .take(target)
        .map(|l| l.chars().count() + 1)
        .sum();
    prefix + target_col
}

/// 输入框总高度 = 内容行数（至少 1，封顶 max_rows）+ 上下边框 2
fn input_box_height(input: &str, max_rows: u16) -> u16 {
    let rows = input.split('\n').count() as u16;
    rows.max(1).min(max_rows.max(1)) + 2
}

/// 输入框内容行数上限（超出后内部滚动，避免长粘贴吃满整屏）
const INPUT_MAX_ROWS: u16 = 12;

#[cfg(test)]
mod cursor_tests {
    use super::*;

    const MULTI: &str = "AB\nCD\nEF";

    #[test]
    fn up_moves_to_previous_line_clamping_column() {
        // 光标在第三行 F 后（index 8）→ 上移到第二行，列 2 未超行尾 → C 后
        assert_eq!(cursor_vertical_move(MULTI, 8, -1), 5);
        // 第二行 D 后（index 5）→ 上移到第一行，列 2 → B 后
        assert_eq!(cursor_vertical_move(MULTI, 5, -1), 2);
        // 列超目标行尾时 clamp：光标在第二行列 2，上移到第一行（长 2）→ 列 2
        assert_eq!(cursor_vertical_move("A\nBCD", 5, -1), 1);
    }

    #[test]
    fn down_moves_to_next_line_clamping_column() {
        assert_eq!(cursor_vertical_move(MULTI, 2, 1), 5);
        assert_eq!(cursor_vertical_move(MULTI, 5, 1), 8);
    }

    #[test]
    fn at_edges_cursor_stays_put() {
        // 首行再上移、末行再下移 → 原地不动
        assert_eq!(cursor_vertical_move(MULTI, 2, -1), 2);
        assert_eq!(cursor_vertical_move(MULTI, 8, 1), 8);
    }

    #[test]
    fn single_line_and_empty_input_are_noops() {
        assert_eq!(cursor_vertical_move("hello", 3, -1), 3);
        assert_eq!(cursor_vertical_move("hello", 3, 1), 3);
        assert_eq!(cursor_vertical_move("", 0, -1), 0);
        assert_eq!(cursor_vertical_move("", 0, 1), 0);
    }

    #[test]
    fn cursor_on_newline_moves_from_row_end() {
        // 光标停在第一行末（'\n' 上，index 2）→ 上移不动、下移到第二行行首列 clamp
        assert_eq!(cursor_vertical_move(MULTI, 2, -1), 2);
        assert_eq!(cursor_vertical_move(MULTI, 2, 1), 2 + 1 + 2);
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn paste_normalizes_crlf_and_cr_to_lf() {
        assert_eq!(normalize_paste("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(normalize_paste("plain"), "plain");
        assert_eq!(normalize_paste(""), "");
    }

    #[test]
    fn completion_window_shows_all_when_under_max() {
        assert_eq!(completion_window(0, 0, 8), (0, 0));
        assert_eq!(completion_window(5, 3, 8), (0, 5));
        assert_eq!(completion_window(8, 7, 8), (0, 8));
    }

    #[test]
    fn at_candidates_show_only_first_level_on_at() {
        let files = vec![
            "README.md".to_string(),
            "src/".to_string(),
            "src/api/".to_string(),
            "src/api/client.ts".to_string(),
            "src/main.rs".to_string(),
        ];
        let cands1 = at_candidates(&files, "");
        let names: Vec<&str> = cands1.iter().map(|c| c.name.as_str()).collect();
        // 只有一层：src 的子项不可见
        assert_eq!(names, vec!["README.md", "src/"]);
    }

    #[test]
    fn at_candidates_descend_into_selected_dir() {
        let files = vec![
            "README.md".to_string(),
            "src/".to_string(),
            "src/api/".to_string(),
            "src/api/client.ts".to_string(),
            "src/main.rs".to_string(),
        ];
        // 选中 src/ 后查询变 "src/"：展示第二层
        let cands2 = at_candidates(&files, "src/");
        let names: Vec<&str> = cands2.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["src/api/", "src/main.rs"]);
        // 第三层同理
        let cands3 = at_candidates(&files, "src/api/");
        let names: Vec<&str> = cands3.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["src/api/client.ts"]);
    }

    #[test]
    fn at_candidates_filter_within_level() {
        let files = vec![
            "src/".to_string(),
            "src/main.rs".to_string(),
            "src/models/".to_string(),
        ];
        let cands4 = at_candidates(&files, "src/ma");
        let names: Vec<&str> = cands4.iter().map(|c| c.name.as_str()).collect();
        // 过滤词只作用于当前层，且大小写不敏感
        assert_eq!(names, vec!["src/main.rs"]);
        let cands5 = at_candidates(&files, "SRC/MO");
        let names: Vec<&str> = cands5.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["src/models/"]);
    }

    #[test]
    fn completion_window_scrolls_to_keep_selection_visible() {
        // 12 个候选项、可见 8 条：选中前 8 条窗口钉在头部
        assert_eq!(completion_window(12, 0, 8), (0, 8));
        assert_eq!(completion_window(12, 7, 8), (0, 8));
        // 选中越过后 8 条起，窗口整体下滚，选中项始终在窗内
        assert_eq!(completion_window(12, 8, 8), (1, 9));
        assert_eq!(completion_window(12, 11, 8), (4, 12));
    }

    #[test]
    fn input_row_col_maps_cursor_across_lines() {
        assert_eq!(input_row_col("", 0), (0, 0));
        assert_eq!(input_row_col("ab\ncd", 0), (0, 0));
        assert_eq!(input_row_col("ab\ncd", 2), (0, 2));
        // 游标落在 \n 之后 → 下一行行首
        assert_eq!(input_row_col("ab\ncd", 3), (1, 0));
        assert_eq!(input_row_col("ab\ncd", 5), (1, 2));
        // 越界按末尾夹紧
        assert_eq!(input_row_col("ab", 99), (0, 2));
    }

    #[test]
    fn input_box_height_grows_with_lines_and_caps() {
        assert_eq!(input_box_height("", 12), 3); // 1 行 + 上下边框
        assert_eq!(input_box_height("a\nb\nc", 12), 5);
        let many = "x\n".repeat(50);
        assert_eq!(input_box_height(&many, 12), 14); // 封顶 12 + 2
    }
}

#[cfg(test)]
mod paste_token_tests {
    use super::*;

    /// UiState::new 依赖 ChatApp/文件扫描，构造最小状态（同 app.rs 测试的 app() 口径）
    fn ui_for_test() -> UiState {
        let app = ChatApp::new(
            Config::default(),
            std::env::temp_dir(),
            Arc::new(Mutex::new(
                mcode_core::agent::history::HistoryStore::default(),
            )),
        );
        UiState::new(app, None, false)
    }

    #[test]
    fn multiline_paste_becomes_placeholder_token() {
        let mut ui = ui_for_test();
        ui.insert_paste("首行\n次行\n三行");
        assert_eq!(ui.input, "[Pasted text #1 +3 lines]");
        assert_eq!(ui.cursor, ui.input.chars().count());
        // 输入框内容不含粘贴原文，token 已登记
        assert!(!ui.input.contains("次行"));
        assert_eq!(ui.pastes.len(), 1);
    }

    #[test]
    fn single_line_paste_stays_raw() {
        let mut ui = ui_for_test();
        ui.insert_paste("单行内容");
        assert_eq!(ui.input, "单行内容");
        assert!(ui.pastes.is_empty());
    }

    #[test]
    fn backspace_removes_whole_token_at_once() {
        let mut ui = ui_for_test();
        ui.insert("前缀 ");
        ui.insert_paste("a\nb");
        assert!(ui.input.contains("[Pasted text #1 +2 lines]"));
        ui.backspace(); // 光标在 token 末尾：一次退格整体删除
        assert_eq!(ui.input, "前缀 ");
        assert_eq!(ui.cursor, "前缀 ".chars().count());
        // 再退格回到普通逐字符删除
        ui.backspace();
        assert_eq!(ui.input, "前缀");
    }

    #[test]
    fn backspace_inside_or_after_token_text_stays_charwise() {
        let mut ui = ui_for_test();
        ui.insert_paste("a\nb");
        // 光标移到 token 中间：逐字符删除
        ui.move_cursor(-3);
        ui.backspace();
        assert!(ui.input.starts_with("[Pasted"));
    }

    #[test]
    fn take_input_expands_tokens_to_full_content() {
        let mut ui = ui_for_test();
        ui.insert("看这段：");
        ui.insert_paste("L1\nL2\nL3");
        let text = ui.take_input();
        assert_eq!(text, "看这段：L1\nL2\nL3");
        // 历史/后续提交拿全文；token 只在编辑期存在
        assert!(text.contains("L2"));
        assert!(!text.contains("[Pasted text"));
    }

    #[test]
    fn numbered_tokens_increment_per_paste() {
        let mut ui = ui_for_test();
        ui.insert_paste("a\nb");
        ui.insert(" + ");
        ui.insert_paste("c\nd\ne");
        assert_eq!(
            ui.input,
            "[Pasted text #1 +2 lines] + [Pasted text #2 +3 lines]"
        );
        assert_eq!(ui.expand_tokens(&ui.input), "a\nb + c\nd\ne");
    }
}
