//! mcode — 终端 AI coding agent（纯 Rust 重写 mcode TS CLI，制品为单一 CLI 二进制）。
//!
//! 用法：
//!   mcode                     交互 TUI
//!   mcode --resume [id]       恢复会话（无 id 列出最近会话进入选择）
//!   mcode --continue          恢复最近会话
//!   mcode add_provider        从环境变量配置供应商（MCODE_BASE_URL / MCODE_API_KEY_ENV / MCODE_API_KEY / MCODE_MODELS / MCODE_MULTIMODAL）
//!   mcode switch <name>       切换活跃供应商
//!   mcode reset               清空供应商配置
//!   mcode token               显示最近会话 token 统计
//!   mcode --version           版本

mod app;
mod chat;
mod inputlog;
mod runtime;
mod tui;
mod view;

use anyhow::{Result, bail};
use mcode_core::config::Config;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sub: Vec<String> = vec![];
    let mut resume_id: Option<String> = None;
    let mut continue_flag = false;
    let mut resume_no_arg = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-v" => {
                println!("mcode {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--resume" => {
                // 后续若跟非 flag 参数则视为 session id
                match args.get(i + 1) {
                    Some(v) if !v.starts_with('-') => {
                        resume_id = Some(v.clone());
                        i += 1;
                    }
                    _ => resume_no_arg = true,
                }
            }
            "--continue" => continue_flag = true,
            other => sub.push(other.to_string()),
        }
        i += 1;
    }

    if let Some(cmd) = sub.first() {
        return run_subcommand(cmd, &sub[1..]);
    }

    let cfg = load_config_for_tui()?;
    let cwd = std::env::current_dir()?;

    let preload = if continue_flag {
        let list = mcode_core::sessions::SessionStore::list(&cwd)?;
        let newest = list.first().map(|m| m.id.clone());
        tui::Preload {
            session_id: newest,
            open_sessions: false,
        }
    } else if resume_no_arg {
        tui::Preload {
            session_id: None,
            open_sessions: true,
        }
    } else {
        tui::Preload {
            session_id: resume_id,
            open_sessions: false,
        }
    };

    tui::run(cfg, cwd, preload)
}

fn load_config_for_tui() -> Result<Config> {
    // 损坏的 config.json 直接报错退出（不静默回退后覆盖用户配置）
    Config::try_load_from(&mcode_core::util::config_path())
}

fn run_subcommand(cmd: &str, rest: &[String]) -> Result<()> {
    match cmd {
        "add_provider" => add_provider(rest),
        "switch" => switch_provider(rest),
        "reset" => reset_providers(),
        "token" => token_stats(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => bail!("未知子命令 {other}（mcode help 查看用法）"),
    }
}

/// 从环境变量创建供应商（对齐 TS runAddProvider）
fn add_provider(rest: &[String]) -> Result<()> {
    let name = rest
        .first()
        .cloned()
        .or_else(|| std::env::var("MCODE_PROVIDER_NAME").ok())
        .unwrap_or_else(|| "default".into());
    let base_url =
        std::env::var("MCODE_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let key_env = match std::env::var("MCODE_API_KEY_ENV") {
        Ok(v) => v,
        Err(_) => match std::env::var("MCODE_API_KEY") {
            // 直接给了明文 key → 存为字面值（Provider::resolve_api_key 的明文兜底语义）
            Ok(k) => k,
            Err(_) => {
                bail!("需要环境变量 MCODE_API_KEY_ENV（环境变量名）或 MCODE_API_KEY（明文 key）")
            }
        },
    };
    let models: Vec<String> = std::env::var("MCODE_MODELS")
        .unwrap_or_else(|_| "gpt-5".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let multimodal: Vec<String> = std::env::var("MCODE_MULTIMODAL")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut cfg = Config::load();
    cfg.providers.retain(|p| p.name != name);
    let model0 = models.first().cloned().unwrap_or_default();
    cfg.providers.push(mcode_core::config::Provider {
        name: name.clone(),
        base_url,
        api_key_env: key_env,
        models: models.clone(),
        multimodal_models: multimodal,
    });
    cfg.active_provider = Some(name.clone());
    cfg.active_model = Some(model0);
    cfg.save()?;
    println!(
        "✓ 供应商 {name} 已配置（models: {}）并设为活跃",
        models.join(", ")
    );
    Ok(())
}

fn switch_provider(rest: &[String]) -> Result<()> {
    let name = rest
        .first()
        .ok_or_else(|| anyhow::anyhow!("用法：mcode switch <provider-name>"))?;
    let mut cfg = Config::load();
    let p = cfg
        .providers
        .iter_mut()
        .find(|p| p.name == *name)
        .ok_or_else(|| anyhow::anyhow!("供应商 {name} 不存在（先 add_provider）"))?;
    let model = p.models.first().cloned().unwrap_or_default();
    cfg.active_provider = Some(name.clone());
    cfg.active_model = Some(model);
    cfg.save()?;
    println!("✓ 活跃供应商 → {name}");
    Ok(())
}

fn reset_providers() -> Result<()> {
    let mut cfg = Config::load();
    cfg.providers.clear();
    cfg.active_provider = None;
    cfg.active_model = None;
    cfg.save()?;
    println!("✓ 供应商配置已清空");
    Ok(())
}

/// token 统计（诚实口径：读当前目录最近会话的累计值；TS 版的 70/30 拆分是编造，不移植）
fn token_stats() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let list = mcode_core::sessions::SessionStore::list(&cwd)?;
    match list.first() {
        Some(meta) => {
            println!(
                "最近会话：{}\n  tokens（input+output）：{}\n  turns：{}\n  updated：{}",
                meta.title, meta.tokens_total, meta.turns, meta.updated
            );
        }
        None => println!("当前目录暂无会话记录"),
    }
    Ok(())
}

fn print_help() {
    println!(
        "mcode — 终端 AI coding agent

用法：
  mcode                     交互 TUI
  mcode --resume [id]       恢复会话
  mcode --continue          恢复最近会话
  mcode add_provider        环境变量配置供应商（MCODE_BASE_URL / MCODE_API_KEY_ENV / MCODE_MODELS ...）
  mcode switch <name>       切换活跃供应商
  mcode reset               清空供应商配置
  mcode token               最近会话 token 统计
  mcode --version           版本"
    );
}
