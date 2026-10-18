// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 补全状态机 — 1:1 移植自 CLI/desktop-prototype 的 CompletionState 设计（MCode.md 2.2）
//!
//! 纯逻辑、无 UI 依赖：`/` 与 `@` 触发、过滤、↑↓ 循环、tab 补全、Esc 关闭。
//! UI 层（gpui）只做渲染，所有按键都先过这里。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    Slash,
    At,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionState {
    pub trigger: Option<Trigger>,
    pub query: String,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// 展示名（指令名或文件路径）
    pub name: String,
    pub desc: String,
    pub is_dir: bool,
}

impl CompletionState {
    /// 根据光标前最后一个 token 更新触发状态（对齐 prototype 的 updateTrigger 正则语义）
    pub fn sync_from_input(&mut self, input: &str) {
        let token = input.split_whitespace().next_back().unwrap_or("");
        let mut chars = token.chars();
        match (chars.next(), input.is_empty()) {
            (Some('/'), false) => {
                self.trigger = Some(Trigger::Slash);
                self.query = chars.as_str().to_string();
                self.selected = 0;
            }
            (Some('@'), false) => {
                self.trigger = Some(Trigger::At);
                self.query = chars.as_str().to_string();
                self.selected = 0;
            }
            _ => {
                self.trigger = None;
                self.query.clear();
            }
        }
    }

    pub fn is_open(&self) -> bool {
        self.trigger.is_some()
    }

    pub fn move_down(&mut self, len: usize) {
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn move_up(&mut self, len: usize) {
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    /// tab/enter 补全：返回替换后的完整输入
    pub fn apply(&self, input: &str, candidate_name: &str) -> String {
        let Some(t) = self.trigger else {
            return input.to_string();
        };
        let prefix = match t {
            Trigger::Slash => "/",
            Trigger::At => "@",
        };
        match input.rsplit_once(char::is_whitespace) {
            Some((head, _)) => format!("{head} {prefix}{candidate_name} "),
            None => format!("{}{} ", prefix, candidate_name),
        }
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }
}

/// 输入历史召回（↑/↓，跨会话持久化语义由上层存储实现）
#[derive(Debug, Default)]
pub struct InputHistory {
    entries: Vec<String>,
    idx: Option<usize>,
    draft: String,
}

impl InputHistory {
    pub fn push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.entries.retain(|e| e != &text); // 连续重复去重折叠
        self.entries.push(text);
        self.idx = None;
    }

    /// 装载既有历史（旧 → 新顺序），启动时从 history.jsonl 恢复用
    pub fn extend(&mut self, entries: impl IntoIterator<Item = String>) {
        self.entries.extend(entries);
    }

    /// 返回 Some(填充文本) 或 None（无动作）
    pub fn recall_up(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let i = match self.idx {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
        };
        self.idx = Some(i);
        Some(self.entries[i].clone())
    }

    pub fn recall_down(&mut self) -> Option<String> {
        let i = self.idx?;
        if i + 1 < self.entries.len() {
            self.idx = Some(i + 1);
            Some(self.entries[i + 1].clone())
        } else {
            self.idx = None;
            Some(self.draft.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_trigger_and_filter_semantics() {
        let mut st = CompletionState::default();
        st.sync_from_input("/");
        assert_eq!(st.trigger, Some(Trigger::Slash));
        assert_eq!(st.query, "");

        st.sync_from_input("/to");
        assert_eq!(st.query, "to");

        st.sync_from_input("运行 /to"); // token 前缀也触发
        assert_eq!(st.trigger, Some(Trigger::Slash));
        assert_eq!(st.query, "to");

        st.sync_from_input("hello world");
        assert_eq!(st.trigger, None);
    }

    #[test]
    fn at_trigger_apply_replaces_token() {
        let mut st = CompletionState::default();
        st.sync_from_input("看下 @src/ap");
        assert_eq!(st.trigger, Some(Trigger::At));
        let out = st.apply("看下 @src/ap", "src/api/client.ts");
        assert_eq!(out, "看下 @src/api/client.ts ");
    }

    #[test]
    fn selection_wraps() {
        let mut st = CompletionState::default();
        st.sync_from_input("/");
        st.move_down(3);
        st.move_down(3);
        st.move_down(3);
        assert_eq!(st.selected, 0);
        st.move_up(3);
        assert_eq!(st.selected, 2);
    }

    #[test]
    fn history_recall_with_draft() {
        let mut h = InputHistory::default();
        h.push("a".into());
        h.push("b".into());
        assert_eq!(h.recall_up(""), Some("b".into()));
        assert_eq!(h.recall_up(""), Some("a".into()));
        assert_eq!(h.recall_up(""), Some("a".into())); // 顶端停住
        assert_eq!(h.recall_down(), Some("b".into()));
        assert_eq!(h.recall_down(), Some("".into())); // 回到草稿
    }
}
