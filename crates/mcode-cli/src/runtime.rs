//! 运行时桥接 — 移植自 MCode_Desktop/crates/mcode-ui/src/runtime.rs（去除 gpui 依赖）。
//! tokio 任务跑 harness 回合，TUI 主循环经 mpsc 收 UiEvent（Agent 事件 + 权限确认 + goal 裁决）。

use anyhow::Result;
use mcode_core::agent::ModelBackend;
use mcode_core::agent::context::load_project_doc;
use mcode_core::agent::runner::{AgentDeps, ConfirmRequest, ConfirmSender};
use mcode_core::agent::subagent::{TaskTool, all_agents};
use mcode_core::api::ResponsesBackend;
use mcode_core::hooks::HookEngine;
use mcode_core::permission::PermissionManager;
use mcode_core::tools::{EventSink, ToolRegistry};
use mcode_core::util::TokenLedger;
use mcode_core::{agent::AgentEvent, config::Config};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// UI 统一事件流
pub enum UiEvent {
    /// agent 回合事件（TextDelta / ToolStart / Done …）
    Agent(AgentEvent),
    /// 权限确认请求（确认泵从 runner 转发）
    Confirm(ConfirmRequest),
    /// goal 外层循环裁判结果（true = 已达成）
    GoalCheck(bool),
}

/// harness 组装结果：deps 交给回合任务，事件流交给 TUI 泵
pub struct Harness {
    pub deps: Arc<AgentDeps>,
    pub ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    /// UI 事件接收端 — App 经 take_rx 接管（take 后为 None，重复接管安全）
    rx_slot: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<UiEvent>>>,
    /// cancel 通道 keepalive 接收端（见 build_harness_async 注释）
    _cancel_keepalive: tokio::sync::watch::Receiver<bool>,
    pub mcp_report: Vec<(String, Result<usize>)>,
}

impl Harness {
    /// 接管事件接收端，用于挂接 TUI 事件泵
    pub fn take_rx(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<UiEvent>> {
        self.rx_slot.lock().ok()?.take()
    }
}

/// 构建 harness：后端 + 工具注册表（内置/web/skill/task/MCP）+ 权限 + hooks + ledger。
/// MCP 连接为真实进程握手（initialize → tools/list）。
pub async fn build_harness(cfg: &Config, cwd: &PathBuf) -> Result<Harness> {
    build_harness_with_backend(cfg, cwd, None).await
}

/// 测试注入用：显式给后端（绕过 ResponsesBackend 的真实 HTTP 客户端），其余组装路径完全一致。
pub async fn build_harness_with_backend(
    cfg: &Config,
    cwd: &PathBuf,
    backend_override: Option<Arc<dyn ModelBackend>>,
) -> Result<Harness> {
    cfg.provider_ready()?;
    let backend: Arc<dyn ModelBackend> = match backend_override {
        Some(b) => b,
        None => {
            let provider = cfg
                .active_provider()
                .ok_or_else(|| anyhow::anyhow!("未配置供应商"))?
                .clone();
            let model = cfg.active_model().unwrap_or_default();
            Arc::new(ResponsesBackend::new(provider, model)?)
        }
    };

    let (confirm_tx, mut confirm_rx): (ConfirmSender, _) = tokio::sync::mpsc::channel(16);
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();

    let desktop = mcode_core::config::DesktopConfig::load();
    let perms = PermissionManager::new(cfg.default_mode, desktop.permission_rules.clone());

    let registry = Arc::new(ToolRegistry::with_builtins());
    registry.register(Arc::new(mcode_core::tools::web::WebFetch));
    registry.register(Arc::new(mcode_core::tools::web::WebSearch));
    registry.register(Arc::new(mcode_core::tools::skill_tool::SkillTool));

    // keepalive 接收端：保证 cancel 通道恒有接收者，interrupt 的 send 永不失败。
    // 否则"回合任务尚未 subscribe 时用户按 ESC"存在竞态：send(Err: 无接收者) 丢失，
    // 任务随后照常启动并挂死（桌面版实测复现的"ESC 后无响应"根因之一）。
    let (cancel_tx, _cancel_keepalive) = tokio::sync::watch::channel(false);
    let skills = mcode_core::skills::load_all(cwd);
    let deps = Arc::new(AgentDeps {
        cfg: cfg.clone(),
        backend,
        registry: registry.clone(),
        perms: Arc::new(tokio::sync::Mutex::new(perms)),
        ledger: Arc::new(tokio::sync::Mutex::new(TokenLedger::default())),
        hooks: Arc::new(HookEngine::load(cwd)),
        confirm: confirm_tx,
        cancel_tx,
        cwd: cwd.clone(),
        skills,
        agents: Arc::new(all_agents()),
        project_doc: load_project_doc(cwd),
    });

    // task 工具（需要 deps 自引用，deps 构建后注册）
    let agents = deps.agents.clone();
    deps.registry.register(Arc::new(TaskTool {
        deps: deps.clone(),
        agents,
    }));

    // MCP：真实连接 + 工具注册（失败的 server 记入报告，不阻断）
    let mcp_report = mcode_core::mcp::attach_all(&desktop.mcp_servers, &deps.registry).await;

    // 确认泵：ConfirmRequest → UI 事件流
    let pump_tx = ui_tx.clone();
    tokio::spawn(async move {
        while let Some(req) = confirm_rx.recv().await {
            if pump_tx.send(UiEvent::Confirm(req)).is_err() {
                break;
            }
        }
    });

    Ok(Harness {
        deps,
        ui_tx,
        rx_slot: Mutex::new(Some(ui_rx)),
        _cancel_keepalive,
        mcp_report,
    })
}

/// 事件汇：AgentEvent → UI 事件流
pub fn event_sink(tx: tokio::sync::mpsc::UnboundedSender<UiEvent>) -> EventSink {
    Arc::new(move |ev| {
        let _ = tx.send(UiEvent::Agent(ev));
    })
}
