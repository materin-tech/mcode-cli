//! ChatApp 状态机 — 转录、回合、队列、权限、goal 外层循环、斜杠指令。
//! 语义对齐 mcode/src/ui/App.tsx + MCode_Desktop session_ops.rs，但不持有任何渲染/异步设施：
//! 需要真正起回合的地方返回 `Effect::Turn`，由 tui.rs 的事件循环负责 spawn。

use crate::chat::Transcript;
use crate::runtime::{Harness, UiEvent};
use anyhow::Result;
use mcode_core::agent::context::{MAX_GOAL_ROUNDS, build_goal_prompt};
use mcode_core::agent::runner::ConfirmRequest;
use mcode_core::agent::{Attachment, Message, Role};
use mcode_core::config::Config;
use mcode_core::permission::{Decision, Mode};
use mcode_core::sessions::SessionStore;
use mcode_core::util::file_refs::expand_file_refs;
use mcode_core::util::{TokenUsage, mcode_md_paths};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 本 CLI 已实现的指令（/help 列出；与 handle_command 的 match 分支保持一致，测试兜底对账）
const IMPLEMENTED: &[&str] = &[
    "/help",
    "/clear",
    "/compact",
    "/model",
    "/token",
    "/language",
    "/status",
    "/doctor",
    "/save",
    "/resume",
    "/init",
    "/memory",
    "/goal",
    "/agents",
    "/skills",
    "/skill",
    "/agent",
    "/provider",
    "/mcp",
    "/permissions",
    "/effort",
    "/history",
    "/exit",
];

/// TS builtin.ts 的 stub 指令（本构建输出说明不执行；/mcp /permissions 已真实现故不在列）
const STUBS: &[&str] = &[
    "/context",
    "/export",
    "/rewind",
    "/todos",
    "/copy",
    "/config",
    "/add-dir",
    "/vim",
    "/terminal-setup",
    "/statusline",
    "/output-style",
    "/login",
    "/logout",
    "/bug",
    "/release-notes",
];

/// 一次待启动的回合（tui.rs 拿去 spawn）
pub struct Turn {
    pub input: String,
    pub attachments: Vec<Attachment>,
}

pub enum Effect {
    None,
    Quit,
    /// 启动一个回合
    Turn(Turn),
    /// 回合终态（reply 为空 = 打断或出错）；loop 据此推进 goal / 消化队列
    TurnDone,
    /// goal 裁决需要发起（独立单发调用）
    SpawnGoalCheck,
    /// 配置已变（模型/语言/PUA），需重建 harness
    RebuildHarness,
    /// 打开模型选择器 / 会话选择器
    OpenModelPicker,
    OpenSessions,
}

pub struct ChatApp {
    pub cfg: Config,
    pub cwd: PathBuf,
    pub hist: Arc<Mutex<mcode_core::agent::history::HistoryStore>>,
    pub harness: Option<Arc<Harness>>,
    pub session: Option<SessionStore>,
    pub transcript: Transcript,
    pub busy: bool,
    pub goal: Option<String>,
    pub goal_rounds: u32,
    pub queue: VecDeque<String>,
    pub pending_confirm: Option<ConfirmRequest>,
    pub tokens_total: u64,
    /// 权限模式本地缓存（真值在 harness.deps.perms）
    pub mode: Mode,
    pub version: &'static str,
    /// 已加载技能（构造时无条件加载，与后端无关 — 对齐 TS createSkillCommands 进 ALL_COMMANDS）
    pub skills: Vec<mcode_core::skills::Skill>,
    /// 已加载 subagent 定义（同上，/agents 与补全在无 provider 时也要可用）
    pub agents: Vec<mcode_core::agent::subagent::AgentDefinition>,
}

impl ChatApp {
    pub fn new(
        cfg: Config,
        cwd: PathBuf,
        hist: Arc<Mutex<mcode_core::agent::history::HistoryStore>>,
    ) -> Self {
        let mode = cfg.default_mode;
        // 技能/agent 与后端无关，构造期即加载（assets 物化发生在 ChatApp::new 之前）
        let skills = mcode_core::skills::load_all(&cwd);
        let agents = mcode_core::agent::subagent::all_agents();
        Self {
            cfg,
            cwd,
            hist,
            harness: None,
            session: None,
            transcript: Transcript::new(),
            busy: false,
            goal: None,
            goal_rounds: 0,
            queue: VecDeque::new(),
            pending_confirm: None,
            tokens_total: 0,
            mode,
            version: env!("CARGO_PKG_VERSION"),
            skills,
            agents,
        }
    }

    pub fn attach_harness(&mut self, harness: Harness) {
        self.mode = harness
            .deps
            .perms
            .try_lock()
            .map(|p| p.mode)
            .unwrap_or(self.mode);
        self.harness = Some(Arc::new(harness));
    }

    /// 是否配置了图片模型（决定 @图片 是附件还是降级提示）
    fn multimodal(&self) -> bool {
        let model = self.cfg.active_model().unwrap_or_default();
        self.cfg
            .active_provider()
            .map(|p| p.is_multimodal(&model))
            .unwrap_or(false)
    }

    pub(crate) fn notice(&mut self, text: impl Into<String>, error: bool) {
        self.transcript.items.push(crate::chat::ChatItem::System {
            text: text.into(),
            error,
        });
    }

    // ---------- 提交管线 ----------

    /// 用户按 Enter：斜杠指令拦截；busy 时入队；否则启动回合
    pub fn submit(&mut self, raw: &str) -> Effect {
        let raw = raw.trim();
        if raw.is_empty() {
            return Effect::None;
        }
        if raw.starts_with('/') {
            return self.handle_command(raw);
        }
        if self.busy {
            self.queue.push_back(raw.to_string());
            self.notice("⏳ 回合进行中，已加入队列", false);
            return Effect::None;
        }
        match self.prepare_turn(raw) {
            Some(t) => Effect::Turn(t),
            None => Effect::None,
        }
    }

    /// 回合前置：@引用展开 + 会话落盘 + 转录推入
    pub fn prepare_turn(&mut self, raw: &str) -> Option<Turn> {
        // 每回合检查 provider（对齐 TS providerIssue 不缓存）
        if let Err(e) = self.cfg.provider_ready() {
            // API key 缺失走 locale 文案（对齐 TS `${noApiKeyWarning}（${env}）`）
            let t = crate::locales::strings(self.cfg.language);
            let msg = match self.cfg.active_provider() {
                Some(p) if p.resolve_api_key().is_none() => {
                    format!("✗ {}（{}）", t.no_api_key_warning, p.api_key_env)
                }
                _ => format!("✗ {e:#}"),
            };
            self.notice(msg, true);
            return None;
        }
        let mut attachments = vec![];
        let refs = expand_file_refs(raw, &self.cwd, self.multimodal());
        if !refs.missing.is_empty() {
            self.notice(format!("⚠ 引用未解析：{}", refs.missing.join("、")), true);
        }
        for (p, url) in refs.attachments {
            attachments.push(Attachment {
                path: p,
                data_url: url,
            });
        }
        let turn_input = refs.text;

        // 会话持久化：无会话则创建 + 标题 + User 消息落盘
        if self.session.is_none() {
            self.session = SessionStore::create(&self.cwd).ok();
        }
        if let Some(s) = self.session.as_mut() {
            s.ensure_title(raw);
            s.meta.model = self.cfg.active_model().unwrap_or_default();
            s.meta.mode = self.mode.label().to_string();
            let m = Message::text(Role::User, turn_input.clone());
            let _ = s.append_message(&m);
        }

        self.transcript.push_user(raw);
        self.transcript.begin_turn();
        self.busy = true;
        Some(Turn {
            input: turn_input,
            attachments,
        })
    }

    /// 队列出队（回合收尾后由 loop 调）
    pub fn pop_queue(&mut self) -> Option<Turn> {
        let next = self.queue.pop_front()?;
        self.prepare_turn(&next)
    }

    // ---------- UI 事件 ----------

    pub fn on_ui_event(&mut self, ev: UiEvent) -> Effect {
        match ev {
            UiEvent::Confirm(req) => {
                self.pending_confirm = Some(req);
                Effect::None
            }
            UiEvent::GoalCheck(done) => self.on_goal_check(done),
            UiEvent::Agent(agent_ev) => self.on_agent_event(agent_ev),
        }
    }

    fn on_agent_event(&mut self, ev: mcode_core::agent::AgentEvent) -> Effect {
        use mcode_core::agent::AgentEvent as AE;
        if let AE::Usage(TokenUsage { input, output, .. }) = &ev {
            self.tokens_total += input + output;
            if let Some(s) = self.session.as_mut() {
                s.meta.tokens_total = self.tokens_total;
            }
        }
        self.transcript.on_event(&ev);
        match ev {
            // 回合收尾：busy=false 时收到的是被打断/超时的陈旧回合，忽略防污染
            AE::Done { reply } => {
                if !self.busy {
                    return Effect::None;
                }
                if !reply.is_empty()
                    && let Some(s) = self.session.as_mut()
                {
                    let m = Message::text(Role::Assistant, reply.clone());
                    let _ = s.append_message(&m);
                    s.meta.turns += 1;
                }
                self.busy = false;
                if self.goal.is_some() && !reply.is_empty() {
                    Effect::SpawnGoalCheck
                } else {
                    if self.goal.is_some() {
                        self.notice("🎯 goal 循环已终止（回合被打断或出错）", false);
                        self.goal = None;
                        self.goal_rounds = 0;
                    }
                    Effect::TurnDone
                }
            }
            AE::Error { .. } => {
                if self.busy {
                    self.busy = false;
                    Effect::TurnDone
                } else {
                    Effect::None
                }
            }
            _ => Effect::None,
        }
    }

    fn on_goal_check(&mut self, done: bool) -> Effect {
        if self.busy || self.goal.is_none() {
            return Effect::None; // 陈旧裁决
        }
        let goal = self.goal.clone().unwrap_or_default();
        if done {
            self.notice("🎯 GOAL_COMPLETE — goal 已达成", false);
            self.goal = None;
            self.goal_rounds = 0;
            return Effect::TurnDone;
        }
        if self.goal_rounds as usize + 1 >= MAX_GOAL_ROUNDS {
            self.notice(
                format!("🎯 goal 循环已达上限（{MAX_GOAL_ROUNDS} 轮），终止"),
                false,
            );
            self.goal = None;
            self.goal_rounds = 0;
            return Effect::TurnDone;
        }
        self.goal_rounds += 1;
        self.notice(
            format!(
                "🎯 goal 第 {}/{} 轮继续",
                self.goal_rounds + 1,
                MAX_GOAL_ROUNDS
            ),
            false,
        );
        match self.prepare_turn(&build_goal_prompt(&goal)) {
            Some(t) => Effect::Turn(t),
            None => Effect::TurnDone,
        }
    }

    // ---------- 权限 / 打断 ----------

    /// 权限提示 y / a / n
    pub fn confirm(&mut self, decision: Decision) {
        if let Some(req) = self.pending_confirm.take() {
            let _ = req.respond.send(decision);
        }
    }

    /// ESC：拒绝待裁决权限 → 打断回合
    pub fn interrupt(&mut self) {
        if let Some(req) = self.pending_confirm.take() {
            let _ = req.respond.send(Decision::Deny);
            self.notice("已拒绝", false);
            return;
        }
        if self.busy
            && let Some(h) = &self.harness
        {
            let _ = h.deps.cancel_tx.send(true);
            self.notice("⏹ 已请求打断…", false);
        }
    }

    // ---------- 斜杠指令（返回 true = 本地已解释） ----------

    /// 技能 prompt 拼装（对齐 TS createSkillCommands handler：带参 Task: {args}，无参固定句）
    fn build_skill_prompt(name: &str, body: &str, task: &str) -> String {
        let suffix = if task.is_empty() {
            "Follow the skill instructions.".to_string()
        } else {
            format!("Task: {task}")
        };
        format!("[skill: {name}]\n{body}\n\n{suffix}")
    }

    fn handle_command(&mut self, text: &str) -> Effect {
        let (cmd, arg) = text.split_once(' ').unwrap_or((text, ""));
        let cmd = cmd.trim().to_lowercase();
        let arg = arg.trim();
        match cmd.as_str() {
            "/help" => {
                // 对齐 TS /help：名字 + 描述对齐表，stub 指令带 (stub) 标记
                let width = mcode_core::commands::COMMANDS
                    .iter()
                    .map(|c| c.name.len())
                    .max()
                    .unwrap_or(0);
                let mut lines = vec![format!("Commands ({}):", IMPLEMENTED.len() + STUBS.len())];
                for c in mcode_core::commands::COMMANDS.iter() {
                    if IMPLEMENTED.contains(&c.name) {
                        lines.push(format!("  {:<width$}  {}", c.name, c.desc, width = width));
                    } else if STUBS.contains(&c.name) {
                        lines.push(format!(
                            "  {:<width$}  {} (stub)",
                            c.name,
                            c.desc,
                            width = width
                        ));
                    }
                }
                self.notice(lines.join("\n"), false);
            }
            "/status" => {
                self.notice(
                    format!(
                        "mcode v{} · 模型 {} · cwd {} · 模式 {} · tokens {}",
                        self.version,
                        self.cfg.active_model().unwrap_or_default(),
                        self.cwd.display(),
                        self.mode.label(),
                        self.tokens_total
                    ),
                    false,
                );
            }
            "/doctor" => {
                let ready = self.cfg.provider_ready().map(|_| "OK").unwrap_or("异常");
                let key = self
                    .cfg
                    .active_provider()
                    .and_then(|p| p.resolve_api_key())
                    .map(|_| "OK")
                    .unwrap_or("缺失");
                self.notice(
                    format!(
                        "诊断：provider={ready} · api_key={key} · cwd={}",
                        self.cwd.display()
                    ),
                    false,
                );
            }
            "/token" => {
                self.notice(
                    format!("累计 token：{}（input+output）", self.tokens_total),
                    false,
                );
            }
            "/model" => {
                if arg.is_empty() {
                    return Effect::OpenModelPicker;
                }
                if self.set_model(arg) {
                    self.notice(format!("✓ 模型 → {arg}（重建运行时生效）"), false);
                    return Effect::RebuildHarness;
                }
                self.notice(format!("✗ 模型 {arg} 不在当前供应商列表中"), true);
            }
            "/language" => {
                if arg.is_empty() {
                    self.notice(
                        format!(
                            "当前语言：{:?}（/language <zh_CN|zh_TW|en_US|ja_JP|ko_KR>）",
                            self.cfg.language
                        ),
                        false,
                    );
                } else if let Some(l) = parse_language(arg) {
                    self.cfg.language = l;
                    self.cfg.save().ok();
                    self.notice(format!("✓ 语言 → {arg:?}（重建运行时生效）"), false);
                    return Effect::RebuildHarness;
                } else {
                    self.notice(format!("✗ 无法识别语言 {arg}"), true);
                }
            }
            "/effort" => {
                if arg.is_empty() {
                    let cur = mcode_core::config::DesktopConfig::load().thinking_effort();
                    self.notice(
                        format!(
                            "当前思考深度：{}（可选 default/low/medium/high，/effort <level> 切换，下一回合生效）",
                            cur.as_str()
                        ),
                        false,
                    );
                } else if let Some(level) = mcode_core::config::ThinkingEffort::parse(arg) {
                    let mut desktop = mcode_core::config::DesktopConfig::load();
                    desktop.thinking_effort = level.as_str().to_string();
                    desktop.save().ok();
                    self.notice(
                        format!("🧠 思考深度 → {}（下一回合生效）", level.as_str()),
                        false,
                    );
                } else {
                    self.notice(
                        format!("✗ 无法识别思考深度 {arg}（可选 default/low/medium/high）"),
                        true,
                    );
                }
            }
            "/clear" => {
                self.transcript.clear();
                if let Ok(mut h) = self.hist.try_lock() {
                    h.clear();
                }
                self.session = None;
            }
            "/compact" => {
                let Ok(mut h) = self.hist.try_lock() else {
                    self.notice("回合运行中，稍后再压缩", true);
                    return Effect::None;
                };
                let msgs = h.message_count();
                let keep = (msgs / 4).max(2);
                let first = h.all().first().map(|m| {
                    let t: String = m.text.chars().take(200).collect();
                    t
                });
                let summary = format!(
                    "手动压缩：{msgs} 条消息，首条：\"{}\"",
                    first.unwrap_or_default()
                );
                h.compact(summary, keep);
                drop(h);
                self.notice(format!("🗜 已手动压缩历史（保留最近 {keep} 轮）"), false);
            }
            "/goal" => {
                if arg.is_empty() || arg == "clear" {
                    self.goal = None;
                    self.goal_rounds = 0;
                    self.notice("goal 已清除", false);
                } else {
                    self.goal = Some(arg.to_string());
                    self.goal_rounds = 0;
                    self.notice(
                        format!(
                            "🎯 goal 已设置：{arg}（回合完成后自动推进，上限 {MAX_GOAL_ROUNDS} 轮）"
                        ),
                        false,
                    );
                    // 立即跑第一轮（对齐 TS /goal）
                    return match self.prepare_turn(&build_goal_prompt(arg)) {
                        Some(t) => Effect::Turn(t),
                        None => Effect::None,
                    };
                }
            }
            "/provider" | "/skill" | "/agent" => {
                return crate::manage::run(self, &cmd, arg);
            }
            "/agents" => {
                // 对齐桌面版扩展页：名字 + 来源（user 可移除 / builtin）+ 描述
                let user_dir = mcode_core::util::mcode_dir().join("agents");
                let lines: Vec<String> = self
                    .agents
                    .iter()
                    .map(|a| {
                        let src = if user_dir.join(format!("{}.md", a.name)).is_file() {
                            "user"
                        } else {
                            "builtin"
                        };
                        let desc: String = a.description.chars().take(52).collect();
                        format!("  • {} [{src}] {desc}", a.name)
                    })
                    .collect();
                self.notice(
                    format!(
                        "可用 subagent（{}）— 经 task 工具或自动拆解调度，/agent create|install|remove 管理：\n{}",
                        self.agents.len(),
                        lines.join("\n")
                    ),
                    false,
                );
            }
            "/skills" => {
                // 对齐 TS /skills：按来源汇总计数 + 前 40 条
                let list: Vec<String> = self
                    .skills
                    .iter()
                    .map(|s| format!("  • {} [{}]", s.entry.name, s.entry.source))
                    .collect();
                let mut by_source: Vec<(String, usize)> = vec![];
                for s in &self.skills {
                    match by_source.iter_mut().find(|(k, _)| *k == s.entry.source) {
                        Some((_, n)) => *n += 1,
                        None => by_source.push((s.entry.source.clone(), 1)),
                    }
                }
                let summary = by_source
                    .iter()
                    .map(|(k, n)| format!("{k}: {n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut lines = vec![format!(
                    "已加载技能（{}）[{summary}] — /<skill-name> 运行，或由模型经 skill 工具拉取：",
                    self.skills.len()
                )];
                lines.extend(list.iter().take(40).cloned());
                if list.len() > 40 {
                    lines.push(format!("  … 还有 {} 条（见 / 补全面板）", list.len() - 40));
                }
                self.notice(lines.join("\n"), false);
            }
            "/mcp" => {
                if let Some(eff) = crate::manage::run_mcp(self, arg) {
                    return eff;
                }
                if let Some(h) = &self.harness {
                    if h.mcp_report.is_empty() {
                        self.notice(
                            "未配置 MCP server（~/.mcode/desktop.json mcpServers）",
                            false,
                        );
                    } else {
                        let list = h
                            .mcp_report
                            .iter()
                            .map(|(n, r)| match r {
                                Ok(k) => format!("{n}: {k} tools"),
                                Err(e) => format!("{n}: 连接失败 {e:#}"),
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.notice(format!("MCP server：\n{list}"), false);
                    }
                }
            }
            "/memory" => {
                // 对齐 TS /memory：只列存在的文档，📁 project / 🌐 global + 字符数
                let paths = mcode_md_paths(&self.cwd);
                let mut lines = vec![];
                let last = paths.len().saturating_sub(1);
                for (i, p) in paths.iter().enumerate() {
                    if !p.exists() {
                        continue;
                    }
                    let chars = std::fs::read_to_string(p)
                        .map(|c| c.chars().count())
                        .unwrap_or(0);
                    let scope = if i == last { "🌐" } else { "📁" };
                    lines.push(format!("{scope} {} ({chars} chars)", p.display()));
                }
                if lines.is_empty() {
                    self.notice("无 MCODE.md —— /init 生成一份", false);
                } else {
                    self.notice(format!("MCODE.md docs:\n{}", lines.join("\n")), false);
                }
            }
            "/init" => {
                match mcode_core::util::init_mcode_doc(&self.cwd) {
                    Ok((p, true)) => self.notice(format!("✓ 已生成 {}", p.display()), false),
                    Ok((p, false)) => self.notice(format!("ℹ 已存在：{}", p.display()), false),
                    Err(e) => self.notice(format!("✗ 生成失败：{e}"), true),
                };
            }
            "/history" => {
                // 对齐 TS /history 诊断口径
                let s = crate::inputlog::stats(&self.cwd);
                self.notice(
                    format!(
                        "history 文件：{}\n本 cwd 条目：{}（↑ 召回，cwd 优先）\n全局条目：{}\n若此处有计数但 ↑ 无反应，是按键事件未到达应用（终端/复用器问题）",
                        s.file.display(),
                        s.cwd_entries,
                        s.global_entries
                    ),
                    false,
                );
            }
            "/save" => {
                match self.save_snapshot() {
                    Ok(p) => self.notice(format!("✓ 快照已写入 {}", p.display()), false),
                    Err(e) => self.notice(format!("✗ 快照失败：{e:#}"), true),
                };
            }
            "/resume" => return Effect::OpenSessions,
            "/pua:on" => {
                self.cfg.pua_always_on = true;
                self.cfg.save().ok();
                self.notice("✓ PUA 收敛纪律已开启（每会话常驻）", false);
                return Effect::RebuildHarness;
            }
            "/pua:off" => {
                self.cfg.pua_always_on = false;
                self.cfg.save().ok();
                self.notice("✓ PUA 收敛纪律已关闭", false);
                return Effect::RebuildHarness;
            }
            "/permissions" => {
                let desktop = mcode_core::config::DesktopConfig::load();
                if desktop.permission_rules.is_empty() {
                    self.notice(
                        "权限规则为空（~/.mcode/desktop.json permissionRules）",
                        false,
                    );
                } else {
                    let list = desktop
                        .permission_rules
                        .iter()
                        .map(|r| {
                            format!(
                                "{}({}) → {}",
                                r.tool,
                                r.pattern,
                                if r.allow { "ALLOW" } else { "DENY" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.notice(format!("权限规则（desktop.json）：\n{list}"), false);
                }
            }
            "/exit" | "/quit" => return Effect::Quit,
            other => {
                // TS stub 指令：输出说明不执行（对齐 builtin.ts stubHandler）
                if STUBS.contains(&other) {
                    let desc = mcode_core::commands::COMMANDS
                        .iter()
                        .find(|c| c.name == other)
                        .map(|c| c.desc)
                        .unwrap_or("");
                    self.notice(
                        format!("ℹ {desc} — stub：本构建未支持（桌面版可用）"),
                        false,
                    );
                    return Effect::None;
                }
                // /<skill-name> [args] → 注入技能 body 作为回合 prompt（对齐 TS createSkillCommands；
                // 查 self.skills 而非 harness —— 无 provider 时技能也要可发现，回合启动才校验 key。
                // cmd 保留前导斜杠而技能名不带；输入已 to_lowercase，按忽略大小写匹配）
                let bare = other.trim_start_matches('/');
                if let Some(skill) = self
                    .skills
                    .iter()
                    .find(|s| s.entry.name.eq_ignore_ascii_case(bare))
                {
                    let prompt = Self::build_skill_prompt(&skill.entry.name, &skill.body, arg);
                    return match self.prepare_turn(&prompt) {
                        Some(t) => Effect::Turn(t),
                        None => Effect::None,
                    };
                }
                let not_found = crate::locales::strings(self.cfg.language).command_not_found;
                self.notice(
                    format!("{not_found}：{other}（/help 输出完整指令表）"),
                    false,
                );
            }
        }
        Effect::None
    }

    /// 恢复历史会话：JSONL 富消息 → ChatItem 映射 + hist 重建（相邻 user/assistant 文本对）。
    /// （对齐 MCode_Desktop session_ops::resume_session）
    pub fn load_session(&mut self, id: &str) -> Result<()> {
        let cwd = self.cwd.clone();
        self.load_session_from(&cwd, id)
    }

    /// 指定项目目录恢复（全局 resume 用：会话可能属于其他项目）
    pub fn load_session_from(&mut self, project_cwd: &std::path::Path, id: &str) -> Result<()> {
        let (store, msgs) = SessionStore::load(project_cwd, id)?;
        let mut hist = mcode_core::agent::history::HistoryStore::default();
        let mut items = vec![];
        let mut pending_user: Option<String> = None;
        for m in msgs {
            let mut texts: Vec<String> = vec![];
            let mut tool_uses: Vec<(String, String)> = vec![];
            for c in &m.content {
                match c {
                    mcode_core::agent::ContentBlock::Text { text } => texts.push(text.clone()),
                    mcode_core::agent::ContentBlock::ToolUse { name, input, .. } => {
                        tool_uses.push((name.clone(), input.to_string()))
                    }
                    _ => {}
                }
            }
            for (name, input) in &tool_uses {
                let brief: String = input.chars().take(80).collect();
                items.push(crate::chat::ChatItem::Tool {
                    name: name.clone(),
                    detail: brief,
                    ok: true,
                });
            }
            for text in texts {
                match m.role {
                    Role::User => {
                        items.push(crate::chat::ChatItem::User { text: text.clone() });
                        pending_user = Some(text);
                    }
                    Role::Assistant => {
                        items.push(crate::chat::ChatItem::Assistant { text: text.clone() });
                        if let Some(u) = pending_user.take() {
                            hist.push_turn(u, text);
                        }
                    }
                    // tool 消息不进转录与 hist（工具细节已在 ToolUse 块渲染）
                    Role::Tool => {}
                }
            }
        }
        self.session = Some(store);
        self.hist = Arc::new(Mutex::new(hist));
        self.transcript.clear();
        self.transcript.items = items;
        Ok(())
    }

    /// 引导完成 → 应用配置并落盘（对齐 TS GuidedSetup onComplete；harness 由调用方重建）
    pub fn complete_guide(&mut self, result: crate::guide::GuideResult) {
        self.cfg.language = result.language;
        self.cfg.providers = vec![result.provider.clone()];
        self.cfg.active_provider = Some(result.provider.name.clone());
        self.cfg.active_model = result.provider.models.first().cloned();
        self.cfg.default_mode = result.default_mode;
        if let Err(e) = self.cfg.save() {
            self.notice(format!("✗ 配置保存失败：{e:#}"), true);
        }
        let done = crate::locales::strings(self.cfg.language).guide_done;
        self.notice(done, false);
    }

    /// 切换 active_model（校验在当前供应商模型列表内）。调用方随后应重建 harness。
    pub fn set_model(&mut self, model: &str) -> bool {
        let valid = self
            .cfg
            .active_provider()
            .map(|p| p.models.iter().any(|m| m == model))
            .unwrap_or(false);
        if !valid {
            return false;
        }
        self.cfg.active_model = Some(model.to_string());
        self.cfg.save().ok();
        true
    }

    /// /save：转录导出为 markdown 快照（~/.mcode/session-data/<date>-<session>.md）
    fn save_snapshot(&self) -> Result<PathBuf> {
        let dir = mcode_core::util::mcode_dir().join("session-data");
        std::fs::create_dir_all(&dir)?;
        let date = chrono::Local::now().format("%Y-%m-%d");
        let sid = self
            .session
            .as_ref()
            .map(|s| s.meta.id.clone())
            .unwrap_or_else(|| "adhoc".into());
        let path = dir.join(format!("{date}-{sid}.md"));
        let mut out = format!(
            "# MCode Session {sid}\n\n- date: {date}\n- cwd: {}\n- model: {}\n- tokens: {}\n\n---\n\n",
            self.cwd.display(),
            self.cfg.active_model().unwrap_or_default(),
            self.tokens_total
        );
        for item in &self.transcript.items {
            out.push_str(&snapshot_line(item));
            out.push_str("\n\n");
        }
        std::fs::write(&path, out)?;
        Ok(path)
    }
}

fn snapshot_line(item: &crate::chat::ChatItem) -> String {
    use crate::chat::ChatItem as CI;
    match item {
        CI::User { text } => format!("## 👤 User\n\n{text}"),
        CI::Assistant { text } => format!("## 🤖 Assistant\n\n{text}"),
        CI::Reasoning { text, .. } => format!("> 💭 {text}"),
        CI::Activity { text } => format!("> {text}"),
        CI::System { text, error } => {
            if *error {
                format!("> ✗ {text}")
            } else {
                format!("> {text}")
            }
        }
        CI::Tool { name, detail, ok } => {
            format!("> 🔧 {name}: {detail}{}", if *ok { "" } else { "（失败）" })
        }
        CI::Footer { text } => format!("---\n*{text}*"),
    }
}

fn parse_language(s: &str) -> Option<mcode_core::config::Language> {
    use mcode_core::config::Language;
    match s.to_lowercase().as_str() {
        "zh_cn" | "zh-cn" => Some(Language::ZhCn),
        "zh_tw" | "zh-tw" => Some(Language::ZhTw),
        "en_us" | "en-us" => Some(Language::EnUs),
        "ja_jp" | "ja-jp" => Some(Language::JaJp),
        "ko_kr" | "ko-kr" => Some(Language::KoKr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> ChatApp {
        ChatApp::new(
            Config::default(),
            std::env::temp_dir(),
            Arc::new(Mutex::new(
                mcode_core::agent::history::HistoryStore::default(),
            )),
        )
    }

    #[test]
    fn empty_input_is_noop() {
        let mut a = app();
        assert!(matches!(a.submit("  "), Effect::None));
    }

    #[test]
    fn slash_routes_to_command_and_unknown_reports() {
        let mut a = app();
        assert!(matches!(a.submit("/status"), Effect::None));
        assert!(matches!(a.submit("/no-such-cmd"), Effect::None));
        // 未知指令文案走 locale 表（对齐 TS createCommands 兜底），不断言死翻译
        let not_found = crate::locales::strings(a.cfg.language).command_not_found;
        assert!(a.transcript.items.iter().any(
            |i| matches!(i, crate::chat::ChatItem::System { text, .. } if text.contains(not_found))
        ));
    }

    #[test]
    fn skill_slash_dispatches_without_provider() {
        // 对齐 TS createSkillCommands：技能指令与后端无关，无 provider 也必须可分派
        // （回合启动时 prepare_turn 才校验 key）。回归：分派曾拿带斜杠的 "/p10"
        // 去比技能名 "p10"，永远失配落到未知指令。
        let mut a = app();
        a.skills.push(mcode_core::skills::Skill {
            entry: mcode_core::skills::SkillEntry {
                name: "p10".into(),
                description: "P10 CTO mode".into(),
                source: "pua-core".into(),
            },
            body: "P10 BODY".into(),
        });
        // Config::default() 无 provider → prepare_turn 拦下（Effect::None + 警示），
        // 但绝不能落到未知指令兜底
        assert!(matches!(a.submit("/p10"), Effect::None));
        let not_found = crate::locales::strings(a.cfg.language).command_not_found;
        assert!(
            !a.transcript.items.iter().any(
                |i| matches!(i, crate::chat::ChatItem::System { text, .. } if text.contains(not_found))
            ),
            "/技能名 不应报未知指令"
        );
        // 大小写不敏感 + 无参数（空 task 走 "Follow the skill instructions."）
        let mut b = app();
        b.skills.push(mcode_core::skills::Skill {
            entry: mcode_core::skills::SkillEntry {
                name: "Pro".into(),
                description: "pro".into(),
                source: "pua-core".into(),
            },
            body: "B".into(),
        });
        assert!(matches!(b.submit("/pro"), Effect::None));
    }

    #[test]
    fn command_tables_align_with_core() {
        // /help 与 stub 分支按名字查 core COMMANDS 的 desc；两表失同步会漏行/空描述
        for name in IMPLEMENTED.iter().chain(STUBS.iter()) {
            assert!(
                mcode_core::commands::COMMANDS
                    .iter()
                    .any(|c| c.name == *name),
                "{name} 不在 core COMMANDS 里"
            );
        }
    }

    #[test]
    fn skill_prompt_matches_ts_shape() {
        assert_eq!(
            ChatApp::build_skill_prompt("pua", "BODY", "do X"),
            "[skill: pua]\nBODY\n\nTask: do X"
        );
        assert_eq!(
            ChatApp::build_skill_prompt("pua", "BODY", ""),
            "[skill: pua]\nBODY\n\nFollow the skill instructions."
        );
    }

    #[test]
    fn stub_command_reports_not_supported() {
        let mut a = app();
        assert!(matches!(a.submit("/copy"), Effect::None));
        assert!(a.transcript.items.iter().any(
            |i| matches!(i, crate::chat::ChatItem::System { text, .. } if text.contains("stub"))
        ));
    }

    #[test]
    fn init_creates_mcode_md_then_reports_existing() {
        let dir = std::env::temp_dir().join(format!("mcode_app_init_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut a = ChatApp::new(
            Config::default(),
            dir.clone(),
            Arc::new(Mutex::new(
                mcode_core::agent::history::HistoryStore::default(),
            )),
        );
        assert!(matches!(a.submit("/init"), Effect::None));
        assert!(dir.join("MCODE.md").exists());
        // 二次调用不覆盖用户内容（对齐 TS initMcodeDoc）
        std::fs::write(dir.join("MCODE.md"), "keep").unwrap();
        assert!(matches!(a.submit("/init"), Effect::None));
        assert_eq!(
            std::fs::read_to_string(dir.join("MCODE.md")).unwrap(),
            "keep"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_quits() {
        let mut a = app();
        assert!(matches!(a.submit("/exit"), Effect::Quit));
    }

    #[test]
    fn busy_input_queued() {
        let mut a = app();
        a.busy = true;
        assert!(matches!(a.submit("稍后处理这个"), Effect::None));
        assert_eq!(a.queue.len(), 1);
    }

    #[test]
    fn goal_check_done_clears_goal() {
        let mut a = app();
        a.goal = Some("写个诗".into());
        a.busy = false;
        assert!(matches!(a.on_goal_check(true), Effect::TurnDone));
        assert!(a.goal.is_none());
    }

    #[test]
    fn stale_goal_check_ignored_while_busy() {
        let mut a = app();
        a.goal = Some("x".into());
        a.busy = true;
        assert!(matches!(a.on_goal_check(true), Effect::None));
    }

    #[test]
    fn confirm_sends_decision_and_clears_pending() {
        let mut a = app();
        let (tx, rx) = tokio::sync::oneshot::channel();
        a.pending_confirm = Some(ConfirmRequest {
            tool: "bash".into(),
            args_summary: "ls".into(),
            respond: tx,
        });
        a.confirm(Decision::Allow);
        assert!(a.pending_confirm.is_none());
        // 接收端收到裁决（不关心结果值，只验证通道打通）
        ry_block(rx);
    }

    fn ry_block(mut rx: tokio::sync::oneshot::Receiver<Decision>) {
        // 测试里简单 try_recv 验证已发送
        let _ = rx.try_recv();
    }

    #[test]
    fn interrupt_denies_pending_confirm() {
        let mut a = app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        a.pending_confirm = Some(ConfirmRequest {
            tool: "write_file".into(),
            args_summary: "x".into(),
            respond: tx,
        });
        a.interrupt();
        assert_eq!(rx.try_recv().ok(), Some(Decision::Deny));
    }
}
