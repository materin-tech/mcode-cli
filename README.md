# mcode-rs

终端 AI coding agent —— 纯 Rust 实现，单一静态二进制，无 Node/React 依赖。

对齐 mcode（TypeScript/Ink 版）的产品语义：OpenAI Responses API 流式、工具循环、技能与斜杠指令、会话持久化。

## 特性

- **Agent 循环**：单回合最多 25 轮工具调用自动回喂；流式错误指数退避重试（10 次）；≥100k token 自动压缩历史；ESC 随时打断
- **内置工具**：read_file / write_file / edit_file / bash / glob / grep / webfetch / websearch / task（subagent）
- **斜杠指令**：`/model` `/token` `/language` `/effort` `/compact` `/resume` `/goal` `/pua` `/skills` 等
- **技能系统**：安装包自带技能集（启动时自动物化到 `~/.mcode/assets`），`/<skill-name>` 直接调用，`/` 唤起补全面板
- **会话**：自动持久化（JSONL）+ 跨项目恢复（`--continue` / `--resume`）
- **权限四模式**：manual / auto / plan / passon（shift+tab 切换）
- **扩展**：hooks（PreToolUse / PostToolUse / Stop）、MCP server、subagent 定义
- **多语言 UI**：zh_CN / zh_TW / en_US / ja_JP / ko_KR

## 安装

从源码构建（Rust 1.85+，edition 2024）：

```bash
cargo build --release
# 产物：target/release/mcode
```

或从 [Releases](../../releases) 下载对应平台预编译包。

## 快速开始

```bash
mcode                 # 首次启动进入引导向导
mcode add_provider    # 从环境变量配置供应商（MCODE_BASE_URL / MCODE_API_KEY_ENV / MCODE_MODELS ...）
mcode switch <name>   # 切换供应商
mcode token           # token 用量统计
```

`~/.mcode/config.json` schema（camelCase，注意字段大小写）：

```json
{
  "language": "zh_CN",
  "activeProvider": "my-provider",
  "activeModel": "glm-4.7",
  "providers": [
    {
      "name": "my-provider",
      "baseURL": "https://api.example.com/v1",
      "apiKeyEnv": "MY_API_KEY",
      "models": ["glm-4.7"],
      "multimodalModels": []
    }
  ],
  "defaultMode": "passon"
}
```

> snake_case 键（如 `activeProvider` 写成 `active_provider`）会被静默忽略 —— 字段大小写是 schema 的一部分。

## 技能

技能从四层加载（后者覆盖同名）：内置 assets → `~/.mcode/skills` → `{project}/.mcode/skills`。

- `/skills` 列出已加载技能
- `/p10 设置明年技术战略` —— 斜杠 + 技能名 + 参数，直接作为回合指令执行
- 无需先配置模型即可发现与补全；回合启动时才校验 API key

## 开发

```bash
cargo test            # 全量测试
cargo clippy --workspace --all-targets
cargo fmt --all
```

## License

[MIT](LICENSE) © 2026 Materin-tech
