// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 会话持久化 — 对齐 MCode.md 5.1：JSONL 消息流 + meta + resume + rewind 检查点

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

use crate::agent::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub mode: String,
    pub started: String,
    pub updated: String,
    pub turns: u32,
    pub tokens_total: u64,
}

impl Default for SessionMeta {
    fn default() -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: "未命名会话".into(),
            cwd: std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            model: String::new(),
            mode: "passon".into(),
            started: now.clone(),
            updated: now,
            turns: 0,
            tokens_total: 0,
        }
    }
}

pub struct SessionStore {
    pub meta: SessionMeta,
    jsonl_path: PathBuf,
    meta_path: PathBuf,
}

impl SessionStore {
    pub fn create(cwd: &std::path::Path) -> Result<Self> {
        let dir = crate::util::sessions_dir(cwd);
        std::fs::create_dir_all(&dir)?;
        let meta = SessionMeta {
            cwd: cwd.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let s = Self {
            jsonl_path: dir.join(format!("{}.jsonl", meta.id)),
            meta_path: dir.join(format!("{}.meta.json", meta.id)),
            meta,
        };
        s.flush_meta()?;
        Ok(s)
    }

    /// 列出当前目录的历史会话（resume 选择器数据源，按 updated 倒序）
    pub fn list(cwd: &std::path::Path) -> Result<Vec<SessionMeta>> {
        let dir = crate::util::sessions_dir(cwd);
        let mut metas = vec![];
        if !dir.exists() {
            return Ok(metas);
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".meta.json")
                && let Ok(bytes) = std::fs::read(entry.path())
                    && let Ok(m) = serde_json::from_slice::<SessionMeta>(&bytes) {
                        metas.push(m);
                    }
        }
        metas.sort_by(|a, b| b.updated.cmp(&a.updated));
        Ok(metas)
    }

    pub fn append_message(&mut self, m: &Message) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.jsonl_path)?;
        writeln!(f, "{}", serde_json::to_string(m)?)?;
        self.meta.updated = chrono::Utc::now().to_rfc3339();
        self.flush_meta()
    }

    /// 恢复历史会话：读 JSONL 消息流（resume 数据源）
    pub fn load(cwd: &std::path::Path, id: &str) -> Result<(Self, Vec<Message>)> {
        let dir = crate::util::sessions_dir(cwd);
        let jsonl_path = dir.join(format!("{id}.jsonl"));
        let meta_path = dir.join(format!("{id}.meta.json"));
        let meta: SessionMeta = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
        let mut messages = vec![];
        for line in std::fs::read_to_string(&jsonl_path)?.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(m) = serde_json::from_str::<Message>(line) {
                messages.push(m);
            }
        }
        let s = Self {
            meta,
            jsonl_path,
            meta_path,
        };
        Ok((s, messages))
    }

    /// 会话标题取首轮用户输入截断（对齐 CLI）
    pub fn ensure_title(&mut self, first_user_text: &str) {
        if self.meta.title == "未命名会话" && !first_user_text.is_empty() {
            let t: String = first_user_text.chars().take(24).collect();
            self.meta.title = t;
            let _ = self.flush_meta();
        }
    }

    /// 检查点：写文件 / bash 修改前留存快照标记（rewind 数据源）
    pub fn add_checkpoint(&mut self, desc: &str) -> Result<()> {
        let ck_path = self.jsonl_path.with_extension("checkpoints.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ck_path)?;
        writeln!(
            f,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "desc": desc,
            }))?
        )?;
        Ok(())
    }

    fn flush_meta(&self) -> Result<()> {
        std::fs::write(&self.meta_path, serde_json::to_string_pretty(&self.meta)?)?;
        Ok(())
    }
}
