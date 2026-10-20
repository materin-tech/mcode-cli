//! 管理指令 — /provider /skill /agent /mcp 的增删改查。
//! 消费 mcode-core 的管理原语（create/install/remove），双端（CLI TUI + 桌面）共用同一底层。

use crate::app::{ChatApp, Effect};

/// /provider /skill /agent 的统一入口（/mcp 的增删改见 run_mcp）
pub fn run(app: &mut ChatApp, cmd: &str, arg: &str) -> Effect {
    match cmd {
        "/provider" => provider(app, arg),
        "/skill" => skill(app, arg),
        "/agent" => agent(app, arg),
        _ => {
            app.notice(format!("未知管理指令 {cmd}"), true);
            Effect::None
        }
    }
}

/// skill/agent 增删后刷新运行时缓存（技能斜杠补全、/agents 列表即时可见）
fn refresh_extensions(app: &mut ChatApp) {
    app.skills = mcode_core::skills::load_all(&app.cwd);
    app.agents = mcode_core::agent::subagent::all_agents();
}

fn split_sub(arg: &str) -> (&str, &str) {
    arg.split_once(' ').unwrap_or((arg, ""))
}

fn provider(app: &mut ChatApp, arg: &str) -> Effect {
    let (sub, rest) = split_sub(arg);
    let rest = rest.trim();
    match sub {
        "" | "list" => {
            let lines: Vec<String> = app
                .cfg
                .providers
                .iter()
                .map(|p| {
                    let active = app.cfg.active_provider.as_deref() == Some(p.name.as_str());
                    format!(
                        "{} {} · {} · key: ${} · models: {}",
                        if active { "▸" } else { " " },
                        p.name,
                        p.base_url,
                        p.api_key_env,
                        p.models.join(", ")
                    )
                })
                .collect();
            if lines.is_empty() {
                app.notice(
                    "无 provider —— /provider add <名称> <baseURL> <apiKeyEnv> <模型,逗号分隔>",
                    false,
                );
            } else {
                app.notice(
                    format!("providers（~/.mcode/config.json）：\n{}", lines.join("\n")),
                    false,
                );
            }
            Effect::None
        }
        "add" => {
            // add <name> <baseURL> <apiKeyEnv> <models逗号分隔> [multimodal逗号分隔] [responses|chat]
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if !(4..=6).contains(&parts.len()) {
                app.notice(
                    "用法：/provider add <名称> <baseURL> <apiKeyEnv> <模型,逗号分隔> [multimodal模型,逗号分隔] [responses|chat]",
                    true,
                );
                return Effect::None;
            }
            let protocol = match parts.get(5).map(|s| mcode_core::config::Protocol::parse(s)) {
                Some(None) => {
                    app.notice("✗ protocol 只支持 responses | chat".to_string(), true);
                    return Effect::None;
                }
                p => p.flatten().unwrap_or_default(),
            };
            let csv = |s: &str| -> Vec<String> {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            };
            let p = mcode_core::config::Provider {
                name: parts[0].to_string(),
                base_url: parts[1].to_string(),
                api_key_env: parts[2].to_string(),
                models: csv(parts[3]),
                multimodal_models: parts.get(4).map(|s| csv(s)).unwrap_or_default(),
                protocol,
            };
            match app.cfg.add_provider(p) {
                Ok(()) => {
                    if let Err(e) = app.cfg.save() {
                        app.notice(format!("✗ 保存 config.json 失败：{e:#}"), true);
                        return Effect::None;
                    }
                    app.notice(
                        format!("✓ provider `{}` 已写入（首个自动激活）", parts[0]),
                        false,
                    );
                    Effect::RebuildHarness
                }
                Err(e) => {
                    app.notice(format!("✗ {e:#}"), true);
                    Effect::None
                }
            }
        }
        "use" => {
            let name = rest;
            if name.is_empty() {
                app.notice("用法：/provider use <名称>", true);
                return Effect::None;
            }
            match app.cfg.set_active_provider(name) {
                Ok(()) => {
                    if let Err(e) = app.cfg.save() {
                        app.notice(format!("✗ 保存失败：{e:#}"), true);
                        return Effect::None;
                    }
                    let model = app.cfg.active_model().unwrap_or_default();
                    app.notice(format!("✓ 激活 `{name}` · 模型 → {model}"), false);
                    Effect::RebuildHarness
                }
                Err(e) => {
                    app.notice(format!("✗ {e:#}"), true);
                    Effect::None
                }
            }
        }
        "remove" => {
            let name = rest;
            if name.is_empty() {
                app.notice("用法：/provider remove <名称>", true);
            }
            match app.cfg.remove_provider(name) {
                Ok(()) => {
                    if let Err(e) = app.cfg.save() {
                        app.notice(format!("✗ 保存失败：{e:#}"), true);
                        return Effect::None;
                    }
                    app.notice(format!("✓ 已移除 `{name}`（active 已回落）"), false);
                    Effect::RebuildHarness
                }
                Err(e) => {
                    app.notice(format!("✗ {e:#}"), true);
                    Effect::None
                }
            }
        }
        _ => {
            app.notice(
                "用法：/provider [list] | add <名称> <baseURL> <apiKeyEnv> <模型,逗号分隔> | use <名称> | remove <名称>",
                true,
            );
            Effect::None
        }
    }
}

fn skill(app: &mut ChatApp, arg: &str) -> Effect {
    let (sub, rest) = split_sub(arg);
    let rest = rest.trim();
    match sub {
        "" => {
            // 列表优先（对齐桌面扩展页）；管理动作用法在尾部提示
            let list: Vec<String> = app
                .skills
                .iter()
                .map(|s| format!("  • {} [{}]", s.entry.name, s.entry.source))
                .collect();
            let mut lines = vec![format!(
                "已加载技能（{}）— /skill create <名称> <描述> | install <目录> | remove <名称>：",
                app.skills.len()
            )];
            lines.extend(list.iter().take(30).cloned());
            if list.len() > 30 {
                lines.push(format!("  … 还有 {} 条", list.len() - 30));
            }
            app.notice(lines.join("\n"), false);
        }
        "create" => {
            // create <name> <description...>；正文给最小模板，用户随后自行编辑
            let (name, desc) = rest.split_once(' ').unwrap_or((rest, ""));
            if name.is_empty() || desc.trim().is_empty() {
                app.notice("用法：/skill create <名称> <描述...>", true);
                return Effect::None;
            }
            let body = format!("# {name}\n\n待补充：逐步披露的完整指令内容。");
            match mcode_core::skills::create_skill(name, desc.trim(), &body, Some(&app.cwd)) {
                Ok(p) => {
                    app.notice(format!("✓ 已创建 {}", p.display()), false);
                    refresh_extensions(app);
                }
                Err(e) => app.notice(format!("✗ {e:#}"), true),
            }
        }
        "install" => {
            if rest.is_empty() {
                app.notice("用法：/skill install <含 SKILL.md 的目录>", true);
                return Effect::None;
            }
            let src = std::path::PathBuf::from(rest);
            // 先项目级，存在同名冲突由原语报错
            match mcode_core::skills::install_skill_from_dir(&src, Some(&app.cwd)) {
                Ok(p) => {
                    app.notice(format!("✓ 已安装到 {}（项目级）", p.display()), false);
                    refresh_extensions(app);
                }
                Err(e) => {
                    // 项目级失败 → 回落用户级重试（错误信息保留首轮原因）
                    match mcode_core::skills::install_skill_from_dir(&src, None) {
                        Ok(p) => {
                            app.notice(format!("✓ 已安装到 {}（用户级）", p.display()), false);
                            refresh_extensions(app);
                        }
                        Err(_) => app.notice(format!("✗ 安装失败：{e:#}"), true),
                    }
                }
            }
        }
        "remove" => {
            if rest.is_empty() {
                app.notice("用法：/skill remove <名称>", true);
                return Effect::None;
            }
            // 项目级优先，未命中再试用户级
            match mcode_core::skills::remove_skill(rest, Some(&app.cwd)) {
                Ok(true) => {
                    app.notice(format!("✓ 已移除项目级技能 `{rest}`"), false);
                    refresh_extensions(app);
                }
                Ok(false) => match mcode_core::skills::remove_skill(rest, None) {
                    Ok(true) => {
                        app.notice(format!("✓ 已移除用户级技能 `{rest}`"), false);
                        refresh_extensions(app);
                    }
                    Ok(false) => app.notice(format!("✗ 未找到技能 `{rest}`"), true),
                    Err(e) => app.notice(format!("✗ {e:#}"), true),
                },
                Err(e) => app.notice(format!("✗ {e:#}"), true),
            }
        }
        _ => {
            app.notice("未知子命令，用法：/skill create|install|remove", true);
        }
    }
    Effect::None
}

fn agent(app: &mut ChatApp, arg: &str) -> Effect {
    let (sub, rest) = split_sub(arg);
    let rest = rest.trim();
    match sub {
        "" => {
            let user_dir = mcode_core::util::mcode_dir().join("agents");
            let lines: Vec<String> = app
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
            app.notice(
                format!(
                    "可用 subagent（{}）— /agent create <名称> <描述> | install <.md> | remove <名称>：\n{}",
                    app.agents.len(),
                    lines.join("\n")
                ),
                false,
            );
        }
        "create" => {
            let (name, desc) = rest.split_once(' ').unwrap_or((rest, ""));
            if name.is_empty() || desc.trim().is_empty() {
                app.notice("用法：/agent create <名称> <描述...>", true);
                return Effect::None;
            }
            let prompt = format!("You are {name}. {desc}\n\n待补充：完整的职责边界与工作方式。");
            match mcode_core::agent::subagent::create_agent(name, desc.trim(), &prompt, None, None)
            {
                Ok(p) => {
                    app.notice(format!("✓ 已创建 {}", p.display()), false);
                    refresh_extensions(app);
                }
                Err(e) => app.notice(format!("✗ {e:#}"), true),
            }
        }
        "install" => {
            if rest.is_empty() {
                app.notice("用法：/agent install <.md 文件路径>", true);
                return Effect::None;
            }
            match mcode_core::agent::subagent::install_agent_from_file(
                &std::path::PathBuf::from(rest),
                None,
            ) {
                Ok(p) => {
                    app.notice(format!("✓ 已安装 {}", p.display()), false);
                    refresh_extensions(app);
                }
                Err(e) => app.notice(format!("✗ {e:#}"), true),
            }
        }
        "remove" => {
            if rest.is_empty() {
                app.notice("用法：/agent remove <名称>", true);
                return Effect::None;
            }
            match mcode_core::agent::subagent::remove_agent(rest, None) {
                Ok(true) => {
                    app.notice(format!("✓ 已移除 `{rest}`"), false);
                    refresh_extensions(app);
                }
                Ok(false) => app.notice(format!("✗ 未找到 `{rest}`（内置 agent 不可移除）"), true),
                Err(e) => app.notice(format!("✗ {e:#}"), true),
            }
        }
        _ => {
            app.notice("未知子命令，用法：/agent create|install|remove", true);
        }
    }
    Effect::None
}

/// /mcp 的增删改（列表分支保留在 app.rs /mcp 原样）；返回 Ok(()) 表示 arg 为空交回列表
pub fn run_mcp(app: &mut ChatApp, arg: &str) -> Option<Effect> {
    let (sub, rest) = split_sub(arg);
    let rest = rest.trim();
    match sub {
        "" => None, // 无子命令 → 调用方走原有列表展示
        "add" => {
            // add <name> <command> [args...]
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() < 2 {
                app.notice("用法：/mcp add <名称> <command> [args...]", true);
                return Some(Effect::None);
            }
            let mut desktop = mcode_core::config::DesktopConfig::load();
            desktop.upsert_mcp_server(mcode_core::config::McpServerConfig {
                name: parts[0].to_string(),
                command: parts[1].to_string(),
                args: parts[2..].iter().map(|s| s.to_string()).collect(),
                env: Default::default(),
                enabled: true,
            });
            if let Err(e) = desktop.save() {
                app.notice(format!("✗ 保存 desktop.json 失败：{e:#}"), true);
                return Some(Effect::None);
            }
            app.notice(
                format!("✓ MCP server `{}` 已写入 desktop.json（重启会话生效，env 请编辑 desktop.json）", parts[0]),
                false,
            );
            Some(Effect::RebuildHarness)
        }
        "remove" => {
            if rest.is_empty() {
                app.notice("用法：/mcp remove <名称>", true);
                return Some(Effect::None);
            }
            let mut desktop = mcode_core::config::DesktopConfig::load();
            if desktop.remove_mcp_server(rest) {
                if let Err(e) = desktop.save() {
                    app.notice(format!("✗ 保存 desktop.json 失败：{e:#}"), true);
                    return Some(Effect::None);
                }
                app.notice(format!("✓ 已移除 MCP server `{rest}`"), false);
                Some(Effect::RebuildHarness)
            } else {
                app.notice(format!("✗ 未找到 MCP server `{rest}`"), true);
                Some(Effect::None)
            }
        }
        sub @ ("on" | "off") => {
            if rest.is_empty() {
                app.notice("用法：/mcp on|off <名称>", true);
                return Some(Effect::None);
            }
            let enabled = sub == "on";
            let mut desktop = mcode_core::config::DesktopConfig::load();
            if let Err(e) = desktop.set_mcp_enabled(rest, enabled) {
                app.notice(format!("✗ {e:#}"), true);
                return Some(Effect::None);
            }
            if let Err(e) = desktop.save() {
                app.notice(format!("✗ 保存 desktop.json 失败：{e:#}"), true);
                return Some(Effect::None);
            }
            app.notice(
                format!(
                    "✓ MCP server `{rest}` 已{}",
                    if enabled {
                        "启用（重建后注入工具）"
                    } else {
                        "停用"
                    }
                ),
                false,
            );
            Some(Effect::RebuildHarness)
        }
        _ => {
            app.notice("用法：/mcp [列表] | add <名称> <command> [args...] | remove <名称> | on|off <名称>", true);
            Some(Effect::None)
        }
    }
}
