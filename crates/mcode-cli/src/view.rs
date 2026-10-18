//! 渲染层 — ChatItem → 终端行。纯函数，换行逻辑可测试。
//! 前缀用 ASCII（宽度可靠），颜色承载语义。

use crate::chat::ChatItem;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 按显示宽度贪心换行（CJK 友好：按字符切，不做拉丁断词优化）。
/// 尊重显式 `\n`；width 为 0 时按 1 处理。
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    for para in s.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = para.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let end = (i + width).min(chars.len());
            out.push(chars[i..end].iter().collect());
            i = end;
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn span(text: impl Into<String>, color: Color, bold: bool) -> Span<'static> {
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(text.into(), style)
}

/// 单个 ChatItem → 终端行（已按 width 换行）
pub fn item_lines(item: &ChatItem, width: usize, reasoning_expanded: bool) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2); // 留边
    match item {
        ChatItem::User { text } => {
            let mut out = vec![Line::from(span("> user", Color::Cyan, true))];
            for l in wrap(text, inner) {
                out.push(Line::from(span(l, Color::White, false)));
            }
            out
        }
        ChatItem::Assistant { text } => wrap(text, inner)
            .into_iter()
            .map(|l| Line::from(span(l, Color::White, false)))
            .collect(),
        ChatItem::Reasoning { text, label } => {
            let tag = match label {
                Some(a) => format!("thinking · {a}"),
                None => "thinking".to_string(),
            };
            let all: Vec<String> = wrap(text, inner);
            let mut out = vec![];
            if reasoning_expanded {
                out.push(Line::from(span(
                    format!("· {tag} · {} lines · ctrl+o collapse", all.len()),
                    Color::DarkGray,
                    false,
                )));
                for l in all {
                    out.push(Line::from(span(l, Color::DarkGray, false)));
                }
            } else {
                let shown: Vec<String> = all.iter().rev().take(5).rev().cloned().collect();
                let hidden = all.len().saturating_sub(shown.len());
                let head = if hidden > 0 {
                    format!("· {tag} · {} lines (+{hidden} · ctrl+o expand)", all.len())
                } else {
                    format!("· {tag} · {} lines", all.len())
                };
                out.push(Line::from(span(head, Color::DarkGray, false)));
                for l in shown {
                    out.push(Line::from(span(l, Color::DarkGray, false)));
                }
            }
            out
        }
        ChatItem::Activity { text } => vec![Line::from(span(
            format!("· {text}"),
            Color::DarkGray,
            false,
        ))],
        ChatItem::System { text, error } => {
            let color = if *error { Color::Red } else { Color::Yellow };
            wrap(text, inner)
                .into_iter()
                .map(|l| Line::from(span(l, color, false)))
                .collect()
        }
        ChatItem::Tool { name, detail, ok } => {
            let color = if *ok { Color::Green } else { Color::Red };
            let text = format!("[{name}] {detail}");
            wrap(&text, inner.saturating_sub(2))
                .into_iter()
                .map(|l| Line::from(span(format!("  {l}"), color, false)))
                .collect()
        }
        ChatItem::Footer { text } => vec![Line::from(span(
            format!("── {text}"),
            Color::DarkGray,
            false,
        ))],
    }
}

/// 全转录 → 终端行
pub fn transcript_lines(
    items: &[ChatItem],
    width: usize,
    reasoning_expanded: bool,
) -> Vec<Line<'static>> {
    let mut out = vec![];
    for item in items {
        out.extend(item_lines(item, width, reasoning_expanded));
    }
    if out.is_empty() {
        out.push(Line::from(span(
            "输入问题或 /help 查看指令；@ 引用文件，/ 调指令",
            Color::DarkGray,
            false,
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_newlines_and_width() {
        assert_eq!(wrap("ab\ncd", 10), vec!["ab", "cd"]);
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap("中文换行测试", 2), vec!["中文", "换行", "测试"]);
        assert_eq!(wrap("", 4), vec![""]);
    }

    #[test]
    fn reasoning_collapsed_shows_tail_five() {
        let text = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let item = ChatItem::Reasoning { text, label: None };
        let lines = item_lines(&item, 80, false);
        // 头部 + 最后 5 行
        assert_eq!(lines.len(), 6);
        let head = lines[0].spans[0].content.clone();
        assert!(head.contains("+5"), "应标注隐藏行数: {head}");
        let tail = lines.last().unwrap().spans[0].content.clone();
        assert!(tail.contains("line9"));
        // 展开后全量
        let expanded = item_lines(&item, 80, true);
        assert_eq!(expanded.len(), 11);
    }

    #[test]
    fn tool_failed_is_red_semantics() {
        let item = ChatItem::Tool {
            name: "bash".into(),
            detail: "boom".into(),
            ok: false,
        };
        let lines = item_lines(&item, 80, false);
        assert!(lines[0].spans[0].content.contains("[bash] boom"));
    }
}
