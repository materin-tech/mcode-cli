//! 内置资产（assets/ecc-core + assets/pua-core）— 编译期嵌入二进制，启动时一次性物化到
//! `~/.mcode/assets`。语义对齐 TS materializeEccCore/materializePuaCore 与桌面版
//! materialize_bundled_assets：整组目录已存在则跳过，绝不覆盖既有用户文件（可删可改）。
//!
//! 单二进制制品的关键：assets/ 只存在于源码仓库，运行时不需要随二进制分发。

use include_dir::{Dir, include_dir};
use mcode_core::util::mcode_dir;
use std::path::Path;

/// 仓库根 assets/（ecc-core: 71 agents + 311 skills；pua-core: 12 技能）
static BUNDLED: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../assets");

/// 物化内置资产组到 `~/.mcode/assets/<tier>`，返回本次新落地的组名（已存在 → 空表）。
/// 失败的组静默跳过（技能缺失不阻断启动，/skills 与 /doctor 可见）。
pub fn materialize_bundled_assets() -> Vec<&'static str> {
    let dst_root = mcode_dir().join("assets");
    let mut done = vec![];
    for tier in BUNDLED.dirs() {
        let Some(name) = tier.path().file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if dst_root.join(name).is_dir() {
            continue; // 已物化（或用户自建）：整组跳过
        }
        if write_dir(tier, &dst_root).is_ok() {
            done.push(name);
        }
    }
    done
}

/// 深度递归写出嵌入目录组。include_dir 0.7 的 `files()` 只遍历本层（非递归），
/// 必须沿 `entries()` 下钻；嵌入根 Dir 的 path 为空，子项路径形如 `ecc-core/...`，
/// 直接拼到 dst_root 下即与 `dst_root.join(name).is_dir()` 的整组跳过检查对齐。
fn write_dir(dir: &Dir, dst_root: &Path) -> std::io::Result<()> {
    for e in dir.entries() {
        match e {
            include_dir::DirEntry::Dir(d) => write_dir(d, dst_root)?,
            include_dir::DirEntry::File(f) => {
                let target = dst_root.join(f.path());
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, f.contents())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// include_dir 的 files() 非递归 —— 测试口径按 entries() 深度计数
    fn count_files(dir: &Dir, pred: &impl Fn(&Path) -> bool) -> usize {
        dir.entries()
            .iter()
            .map(|e| match e {
                include_dir::DirEntry::Dir(d) => count_files(d, pred),
                include_dir::DirEntry::File(f) => usize::from(pred(f.path())),
            })
            .sum()
    }

    #[test]
    fn bundled_assets_are_embedded() {
        use std::path::Component;
        // 按路径段匹配目录归属，不依赖 include_dir 的前缀表示
        let in_dir = |dir: &str, p: &Path| {
            p.components()
                .any(|c| c == Component::Normal(std::ffi::OsStr::new(dir)))
        };
        // ecc-core 与 pua-core 两组都在
        let names: Vec<_> = BUNDLED
            .dirs()
            .filter_map(|d| d.path().file_name())
            .collect();
        assert!(names.contains(&std::ffi::OsStr::new("ecc-core")));
        assert!(names.contains(&std::ffi::OsStr::new("pua-core")));
        // ecc-core/skills 有相当数量 SKILL.md（311 个技能目录）
        let skill_count = count_files(&BUNDLED, &|p| {
            p.ends_with("SKILL.md") && in_dir("ecc-core", p)
        });
        assert!(
            skill_count > 300,
            "ecc-core skills 应嵌入 300+，实际 {skill_count}"
        );
        // agents 同理（磁盘实测 71 个，与嵌入数一致）
        let agent_count = count_files(&BUNDLED, &|p| {
            p.extension() == Some("md".as_ref()) && in_dir("agents", p)
        });
        assert!(
            agent_count > 50,
            "ecc-core agents 应嵌入 50+，实际 {agent_count}"
        );
    }
}
