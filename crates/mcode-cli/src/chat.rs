//! 转录模型 — ChatItem 流 + 事件归约器（对齐 mcode/src/ui/App.tsx 的 emit 开关）。
//! 纯逻辑，不依赖终端：AgentEvent 从头部进入，ChatItem 从尾部渲染。
//! 行为对齐要点：
//! - reasoning 增量并入末尾 reasoning 项（无则新建）
//! - 文本增量：某阶段首个 delta 时，把累计的工具计数刷成一条 activity 行
//! - tool_start 推 `(running)` 占位，tool_result 回扫同名占位并替换结果
//! - done：无流式文本则用 event.reply 兜底；追加 `Cooked for … · done HH:MM` footer（去重）

use mcode_core::agent::AgentEvent;
use std::time::Instant;

/// 转录条目上限（对齐 TS MAX_ITEMS = 300）
const MAX_ITEMS: usize = 300;
const TRIM_MARK: &str = "… older transcript trimmed";

#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    User {
        text: String,
    },
    /// 模型思考流（subagent 来源带 label）
    Reasoning {
        text: String,
        label: Option<String>,
    },
    /// 工具活动摘要行（"read 2 files, ran 8 shell commands"）
    Activity {
        text: String,
    },
    Assistant {
        text: String,
    },
    System {
        text: String,
        error: bool,
    },
    Tool {
        name: String,
        detail: String,
        ok: bool,
    },
    Footer {
        text: String,
    },
}

pub struct Transcript {
    pub items: Vec<ChatItem>,
    /// 当前回合开始时刻（footer 用）
    turn_started: Option<Instant>,
    assistant_buf: String,
    /// 本回合是否已有流式文本（决定 done 是否用 reply 兜底）
    streamed: bool,
    /// 工具调用计数（activity 摘要源）
    tool_counts: Vec<(String, usize)>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            turn_started: None,
            assistant_buf: String::new(),
            streamed: false,
            tool_counts: Vec::new(),
        }
    }

    pub fn push_user(&mut self, text: &str) {
        self.items.push(ChatItem::User { text: text.into() });
        self.trim();
    }

    /// 回合开始：复位阶段缓冲
    pub fn begin_turn(&mut self) {
        self.turn_started = Some(Instant::now());
        self.assistant_buf.clear();
        self.streamed = false;
        self.tool_counts.clear();
    }

    /// 单一归约入口（对齐 App.tsx emit switch）
    pub fn on_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::Reasoning { text, scope } => {
                let label = scope.clone();
                if let Some(ChatItem::Reasoning { .. }) = self.items.last() {
                    if let Some(ChatItem::Reasoning { text: t, .. }) = self.items.last_mut() {
                        t.push_str(text);
                    }
                } else {
                    self.items.push(ChatItem::Reasoning {
                        text: text.clone(),
                        label,
                    });
                }
            }
            AgentEvent::TextDelta(delta) => {
                // 阶段首个 delta：先刷工具计数为 activity 行
                if self.assistant_buf.is_empty() {
                    self.flush_activity();
                }
                if !self.streamed {
                    self.streamed = true;
                }
                if matches!(self.items.last(), Some(ChatItem::Assistant { .. })) {
                    if let Some(ChatItem::Assistant { text }) = self.items.last_mut() {
                        text.push_str(delta);
                    }
                } else {
                    self.assistant_buf.push_str(delta);
                    let buf = self.assistant_buf.clone();
                    self.items.push(ChatItem::Assistant { text: buf });
                }
            }
            AgentEvent::ToolStart { name, .. } => {
                // 工具开始 → 助手缓冲复位（下一阶段文本另起一项）
                self.assistant_buf.clear();
                match self.tool_counts.iter_mut().find(|(n, _)| n == name) {
                    Some((_, c)) => *c += 1,
                    None => self.tool_counts.push((name.clone(), 1)),
                }
                self.items.push(ChatItem::Tool {
                    name: name.clone(),
                    detail: "(running)".into(),
                    ok: true,
                });
            }
            AgentEvent::ToolResult {
                name,
                success,
                output,
                display,
                ..
            } => {
                // 回扫同名 (running) 占位并替换
                let detail = match display {
                    Some(_) => summarize_display(output),
                    None => summarize_output(output),
                };
                for it in self.items.iter_mut().rev() {
                    if let ChatItem::Tool {
                        name: n,
                        detail: d,
                        ok,
                    } = it
                        && n == name && d == "(running)" {
                            *d = detail;
                            *ok = *success;
                            break;
                        }
                }
            }
            AgentEvent::Done { reply } => {
                // 无流式文本 → reply 兜底为 assistant 项
                if !self.streamed && !reply.trim().is_empty() {
                    self.items.push(ChatItem::Assistant {
                        text: reply.clone(),
                    });
                }
                let footer = match self.turn_started.take() {
                    Some(t) => format!(
                        "Cooked for {} · done {}",
                        format_duration(t.elapsed().as_secs()),
                        clock_now()
                    ),
                    None => format!("done {}", clock_now()),
                };
                // footer 去重（上一条已是 footer 则不重复）
                if !matches!(self.items.last(), Some(ChatItem::Footer { .. })) {
                    self.items.push(ChatItem::Footer { text: footer });
                }
                self.assistant_buf.clear();
            }
            AgentEvent::Error { message } => {
                self.items.push(ChatItem::System {
                    text: message.clone(),
                    error: true,
                });
                self.assistant_buf.clear();
            }
            AgentEvent::Retrying {
                attempt,
                max,
                reason,
            } => {
                self.items.push(ChatItem::System {
                    text: format!("⟳ 重试 {attempt}/{max}：{reason}"),
                    error: false,
                });
            }
            AgentEvent::Compacted { note } => {
                self.items.push(ChatItem::System {
                    text: note.clone(),
                    error: false,
                });
            }
            AgentEvent::Usage(_) => {}
        }
        self.trim();
    }

    /// 工具计数 → activity 行（对齐 summarizeActivity；无计数则不刷）
    fn flush_activity(&mut self) {
        if self.tool_counts.is_empty() {
            return;
        }
        let text = summarize_activity(&self.tool_counts);
        self.items.push(ChatItem::Activity { text });
        self.tool_counts.clear();
    }

    fn trim(&mut self) {
        let mut trimmed = false;
        while self.items.len() > MAX_ITEMS {
            self.items.remove(0);
            trimmed = true;
        }
        if trimmed {
            // 首项插裁剪标记（已存在则不重复）
            if !matches!(self.items.first(), Some(ChatItem::System { text, .. }) if text == TRIM_MARK)
            {
                self.items.insert(
                    0,
                    ChatItem::System {
                        text: TRIM_MARK.into(),
                        error: false,
                    },
                );
            }
        }
    }

    /// 供 /clear 等使用
    pub fn clear(&mut self) {
        self.items.clear();
        self.assistant_buf.clear();
        self.streamed = false;
        self.tool_counts.clear();
        self.turn_started = None;
    }
}

/// 工具计数 → "read 2 files, ran 8 shell commands"（对齐 util/activity.ts）
/// 工具计数 → "read 2 files, ran 8 shell commands"（对齐 util/activity.ts；保持首次出现顺序——
/// TS 的 Object.entries 对字符串键保插入序，BTreeMap 会按字母序打乱短语顺序）
pub fn summarize_activity(counts: &[(String, usize)]) -> String {
    let mut parts: Vec<String> = vec![];
    for (name, n) in counts {
        let n = *n;
        if n == 0 {
            continue;
        }
        let part = match name.as_str() {
            "read_file" => format!("read {n} {}", plural(n, "file", "files")),
            "write_file" => format!("wrote {n} {}", plural(n, "file", "files")),
            "edit_file" => format!("edited {n} {}", plural(n, "file", "files")),
            "bash" => format!("ran {n} shell {}", plural(n, "command", "commands")),
            "glob" | "grep" => format!("searched {n} {}", plural(n, "time", "times")),
            "task" => format!("dispatched {n} {}", plural(n, "subagent", "subagents")),
            "skill" => format!("loaded {n} {}", plural(n, "skill", "skills")),
            other => format!("{other} ×{n}"),
        };
        parts.push(part);
    }
    parts.join(", ")
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// 秒 → "2m 56s" / "45s"（对齐 formatDuration）
pub fn format_duration(total_seconds: u64) -> String {
    let secs = total_seconds;
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    let s = secs % 60;
    if s == 0 {
        format!("{m}m")
    } else {
        format!("{m}m {s}s")
    }
}

/// 当前时刻 HH:MM（对齐 clockNow）
pub fn clock_now() -> String {
    let now = chrono::Local::now();
    now.format("%H:%M").to_string()
}

/// 工具输出摘要（TUI 单行展示；超长截断）
fn summarize_output(output: &str) -> String {
    let first_line = output.lines().next().unwrap_or_default();
    truncate_chars(first_line, 120)
}

fn summarize_display(output: &str) -> String {
    truncate_chars(output, 120)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_text(s: &str) -> AgentEvent {
        AgentEvent::TextDelta(s.into())
    }

    #[test]
    fn text_deltas_merge_into_one_assistant_item() {
        let mut t = Transcript::new();
        t.begin_turn();
        t.on_event(&ev_text("你好"));
        t.on_event(&ev_text("，世界"));
        assert_eq!(t.items.len(), 1);
        assert_eq!(
            t.items[0],
            ChatItem::Assistant {
                text: "你好，世界".into()
            }
        );
    }

    #[test]
    fn tool_running_then_result_replaces_placeholder() {
        let mut t = Transcript::new();
        t.begin_turn();
        t.on_event(&AgentEvent::ToolStart {
            name: "bash".into(),
            args: serde_json::json!({}),
            scope: None,
        });
        assert_eq!(
            t.items.last(),
            Some(&ChatItem::Tool {
                name: "bash".into(),
                detail: "(running)".into(),
                ok: true
            })
        );
        t.on_event(&AgentEvent::ToolResult {
            name: "bash".into(),
            success: true,
            output: "hello\nworld".into(),
            display: None,
            scope: None,
        });
        assert_eq!(
            t.items.last(),
            Some(&ChatItem::Tool {
                name: "bash".into(),
                detail: "hello".into(),
                ok: true
            })
        );
    }

    #[test]
    fn activity_flushed_before_first_text_after_tools() {
        let mut t = Transcript::new();
        t.begin_turn();
        t.on_event(&AgentEvent::ToolStart {
            name: "read_file".into(),
            args: serde_json::json!({}),
            scope: None,
        });
        t.on_event(&AgentEvent::ToolResult {
            name: "read_file".into(),
            success: true,
            output: "x".into(),
            display: None,
            scope: None,
        });
        t.on_event(&AgentEvent::ToolStart {
            name: "bash".into(),
            args: serde_json::json!({}),
            scope: None,
        });
        t.on_event(&AgentEvent::ToolResult {
            name: "bash".into(),
            success: true,
            output: "y".into(),
            display: None,
            scope: None,
        });
        t.on_event(&ev_text("结果如下"));
        // [Tool, Tool, Activity, Assistant]——对齐 TS：工具行保留，阶段切换时刷出 activity 汇总
        assert_eq!(
            t.items[0],
            ChatItem::Tool {
                name: "read_file".into(),
                detail: "x".into(),
                ok: true
            }
        );
        assert_eq!(
            t.items[1],
            ChatItem::Tool {
                name: "bash".into(),
                detail: "y".into(),
                ok: true
            }
        );
        assert_eq!(
            t.items[2],
            ChatItem::Activity {
                text: "read 1 file, ran 1 shell command".into()
            }
        );
        assert_eq!(
            t.items[3],
            ChatItem::Assistant {
                text: "结果如下".into()
            }
        );
    }

    #[test]
    fn done_without_stream_uses_reply() {
        let mut t = Transcript::new();
        t.begin_turn();
        t.on_event(&AgentEvent::Done {
            reply: "最终回复".into(),
        });
        assert!(matches!(&t.items[0], ChatItem::Assistant { text } if text == "最终回复"));
        assert!(matches!(t.items.last(), Some(ChatItem::Footer { .. })));
    }

    #[test]
    fn footer_not_duplicated() {
        let mut t = Transcript::new();
        t.begin_turn();
        t.on_event(&AgentEvent::Done { reply: "a".into() });
        let last = t.items.last().cloned();
        // 模拟下一条 Done（不会出现在真实流，但验证去重逻辑）
        t.turn_started = Some(Instant::now());
        t.on_event(&AgentEvent::Done {
            reply: String::new(),
        });
        assert_eq!(t.items.last(), last.as_ref());
    }

    #[test]
    fn reasoning_appends_to_trailing_item() {
        let mut t = Transcript::new();
        t.begin_turn();
        t.on_event(&AgentEvent::Reasoning {
            text: "思考".into(),
            scope: None,
        });
        t.on_event(&AgentEvent::Reasoning {
            text: "中".into(),
            scope: None,
        });
        assert_eq!(t.items.len(), 1);
        assert_eq!(
            t.items[0],
            ChatItem::Reasoning {
                text: "思考中".into(),
                label: None
            }
        );
    }

    #[test]
    fn error_becomes_system_item() {
        let mut t = Transcript::new();
        t.begin_turn();
        t.on_event(&AgentEvent::Error {
            message: "boom".into(),
        });
        assert_eq!(
            t.items[0],
            ChatItem::System {
                text: "boom".into(),
                error: true
            }
        );
    }

    #[test]
    fn duration_format() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(176), "2m 56s");
        assert_eq!(format_duration(120), "2m");
    }

    #[test]
    fn activity_phrases() {
        // 顺序敏感：对齐 TS Object.entries 的插入序
        let c = vec![("read_file".to_string(), 2), ("bash".to_string(), 8)];
        assert_eq!(summarize_activity(&c), "read 2 files, ran 8 shell commands");
    }
}
