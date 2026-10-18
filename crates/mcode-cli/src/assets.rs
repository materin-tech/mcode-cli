//! 内置资产（assets/ecc-core + assets/pua-core）— 编译期嵌入二进制，启动时一次性物化到
//! `~/.mcode/assets`。语义对齐 TS materializeEccCore/materializePuaCore 与桌面版
//! materialize_bundled_assets：整组目录已存在则跳过，绝不覆盖既有用户文件（可删可改）。
//!
//! 单二进制制品的关键：assets/ 只存在于源码仓库，运行时不需要随二进制分发。

use include_dir::{Dir, include_dir};
use mcode_core::util::mcode_dir;
use std::path::Path;

/// 仓库根 assets/（ecc-core: 115 agents + 311 skills；pua-core: 12 技能）
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

/// 递归写出嵌入目录（include_dir 的 files() 已是深度遍历，父目录按需创建）
fn write_dir(dir: &Dir, dst_root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst_root.join(dir.path()))?;
    for f in dir.files() {
        let target = dst_root.join(f.path());
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, f.contents())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_assets_are_embedded() {
        // ecc-core 与 pua-core 两组都在
        let names: Vec<_> = BUNDLED.dirs().iter().filter_map(|d| d.path().file_name()).collect();
        assert!(names.contains(&std::ffi::OsStr::new("ecc-core")));
        assert!(names.contains(&std::ffi::OsStr::new("pua-core")));
        // ecc-core/skills 有相当数量 SKILL.md（311 个技能目录）
        let skill_count = BUNDLED
            .find("ecc-core/skills")
            .map(|d| d.files().iter().filter(|f| f.path().ends_with("SKILL.md")).count())
            .unwrap_or(0);
        assert!(skill_count > 300, "ecc-core skills 应嵌入 300+，实际 {skill_count}");
        // agents 同理（115 个）
        let agent_count = BUNDLED
            .find("ecc-core/agents")
            .map(|d| d.files().iter().filter(|f| f.path().extension() == Some("md".as_ref())).count())
            .unwrap_or(0);
        assert!(agent_count > 100, "ecc-core agents 应嵌入 100+，实际 {agent_count}");
    }
}
