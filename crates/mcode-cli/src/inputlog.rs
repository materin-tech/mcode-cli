//! 输入历史持久化 — ~/.mcode/history.jsonl（{text, cwd, ts} 每行一条，对齐 util/input-history.ts）。
//! 召回语义在 core 的 InputHistory；这里只管装载与追加。

use mcode_core::util::mcode_dir;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// 装载上限（对齐 TS cap 100）
const LOAD_CAP: usize = 100;

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    text: String,
    cwd: String,
    ts: i64,
}

fn path() -> std::path::PathBuf {
    mcode_dir().join("history.jsonl")
}

/// 读取历史（旧 → 新），只取最近 LOAD_CAP 条
pub fn load() -> Vec<String> {
    let Ok(f) = std::fs::File::open(path()) else {
        return vec![];
    };
    let all: Vec<String> = BufReader::new(f)
        .lines()
        .map_while(|l| l.ok())
        .filter_map(|l| serde_json::from_str::<Entry>(&l).ok())
        .filter(|e| !e.text.is_empty())
        .map(|e| e.text)
        .collect();
    let start = all.len().saturating_sub(LOAD_CAP);
    all[start..].to_vec()
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
