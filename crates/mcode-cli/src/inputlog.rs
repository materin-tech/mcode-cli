//! 输入历史持久化 — ~/.mcode/history.jsonl（{text, cwd, ts} 每行一条，对齐 util/input-history.ts）。
//! `load` 语义逐字对齐 TS readInputHistory：cwd 优先（各自新→旧）→ 全局（新→旧），
//! 去重保留最新位置，截取最近 100 条，再反转回旧→新供 InputHistory::extend 消费。
//! `stats` 对齐 TS historyStats，供 /history 诊断。

use mcode_core::util::mcode_dir;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// 召回容量（对齐 TS readInputHistory 默认 limit=100）
const RECALL_CAP: usize = 100;

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    text: String,
    cwd: String,
    ts: i64,
}

fn path() -> std::path::PathBuf {
    mcode_dir().join("history.jsonl")
}

fn read_entries() -> Vec<Entry> {
    let Ok(f) = std::fs::File::open(path()) else {
        return vec![];
    };
    BufReader::new(f)
        .lines()
        .map_while(|l| l.ok())
        .filter_map(|l| serde_json::from_str::<Entry>(&l).ok())
        .filter(|e| !e.text.is_empty())
        .collect()
}

/// 读取召回序列（旧 → 新）：cwd 优先、去重保留最新、截最近 RECALL_CAP 条。
/// （TS 的 result 是新→旧；core InputHistory 是旧→新栈，故最后 reverse 一次）
pub fn load(cwd: &Path) -> Vec<String> {
    let all: Vec<(String, String)> = read_entries()
        .into_iter()
        .map(|e| (e.text, e.cwd))
        .collect();
    recall_order(all, &cwd.display().to_string())
}

/// 纯排序口径（可测）：cwd 条目优先，各自新→旧，去重保留最新位置，截 RECALL_CAP 后转旧→新
fn recall_order(all: Vec<(String, String)>, cwd: &str) -> Vec<String> {
    let mut current: Vec<(String, String)> =
        all.iter().filter(|(_, c)| c == cwd).cloned().collect();
    current.reverse();
    let mut global: Vec<(String, String)> = all.iter().filter(|(_, c)| c != cwd).cloned().collect();
    global.reverse();
    // 新→旧推送去重：最新位置优先保留（对齐 TS 的 seen-set 语义）
    let mut newest_first: Vec<String> = vec![];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (text, _) in current.into_iter().chain(global) {
        if seen.insert(text.clone()) {
            newest_first.push(text);
        }
    }
    newest_first.truncate(RECALL_CAP);
    newest_first.reverse();
    newest_first
}

/// /history 诊断数据（对齐 TS historyStats：文件路径 + 本 cwd 条数 + 全局条数）
pub struct HistoryStats {
    pub file: std::path::PathBuf,
    pub cwd_entries: usize,
    pub global_entries: usize,
}

pub fn stats(cwd: &Path) -> HistoryStats {
    let cwd_s = cwd.display().to_string();
    let mut s = HistoryStats {
        file: path(),
        cwd_entries: 0,
        global_entries: 0,
    };
    for e in read_entries() {
        s.global_entries += 1;
        if e.cwd == cwd_s {
            s.cwd_entries += 1;
        }
    }
    s
}

/// 追加一条（文件不存在自动建目录）
pub fn append(text: &str, cwd: &Path) {
    if text.is_empty() {
        return;
    }
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let e = Entry {
        text: text.to_string(),
        cwd: cwd.display().to_string(),
        ts: chrono::Utc::now().timestamp(),
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{}", serde_json::to_string(&e).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_order_cwd_first_dedupe_latest() {
        // 写入序（旧→新）：2 条其他目录 + 3 条本目录；"shared" 两个目录都有 → 本目录最新位保留
        let all = vec![
            ("g1".to_string(), "/proj/b".to_string()),
            ("g2".to_string(), "/proj/b".to_string()),
            ("u1".to_string(), "/proj/a".to_string()),
            ("shared".to_string(), "/proj/b".to_string()),
            ("shared".to_string(), "/proj/a".to_string()),
        ];
        // 期望召回（旧→新）：g1、g2、u1、shared（cwd 版本压掉 b 目录版本）
        assert_eq!(
            recall_order(all, "/proj/a"),
            vec!["g1", "g2", "u1", "shared"]
        );
    }

    #[test]
    fn recall_order_caps_at_100_keeping_newest() {
        let all: Vec<(String, String)> = (0..150)
            .map(|i| (format!("t{i}"), "/proj/a".to_string()))
            .collect();
        let out = recall_order(all, "/proj/a");
        assert_eq!(out.len(), RECALL_CAP);
        // 截最近 100 条：最旧 50 条被丢弃
        assert_eq!(out.first().unwrap(), "t50");
        assert_eq!(out.last().unwrap(), "t149");
    }
}
