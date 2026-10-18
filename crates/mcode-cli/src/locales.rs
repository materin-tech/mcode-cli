//! UI 字符串表 — TS ui/locales.ts 的 Rust 移植（26 键 × 5 语言）。
//! 组件只消费键；新增语言 = 加一个表，不改组件。语言同时注入 system prompt（core 的 language_directive）。

use mcode_core::config::Language;

/// 全量键集（对齐 TS StringKey；字段顺序即表构造顺序）。
/// allow(dead_code)：TS locales.ts 中 inputPlaceholder/thinking/toolRunning/errorPrefix/mode_*
/// 亦为"定义未消费"的完整性键，表保持全量以维持"新增语言 = 加一张表"的扩展口径。
#[allow(dead_code)]
pub struct Strings {
    pub banner_hint: &'static str,
    pub input_placeholder: &'static str,
    pub thinking: &'static str,
    pub mode_hint: &'static str,
    pub mode_passon: &'static str,
    pub mode_manual: &'static str,
    pub mode_auto: &'static str,
    pub mode_plan: &'static str,
    pub no_provider_warning: &'static str,
    pub no_api_key_warning: &'static str,
    pub slash_hint: &'static str,
    pub at_hint: &'static str,
    pub tool_running: &'static str,
    pub permission_ask: &'static str,
    pub permission_allow: &'static str,
    pub permission_deny: &'static str,
    pub error_prefix: &'static str,
    pub guide_welcome: &'static str,
    pub guide_language: &'static str,
    pub guide_provider_name: &'static str,
    pub guide_base_url: &'static str,
    pub guide_api_key_env: &'static str,
    pub guide_models: &'static str,
    pub guide_mode: &'static str,
    pub guide_done: &'static str,
    pub command_not_found: &'static str,
}

const ZH_CN: Strings = Strings {
    banner_hint: "输入 / 查看指令，@ 引用文件，shift+tab 切换模式",
    input_placeholder: "输入消息…",
    thinking: "thinking …",
    mode_hint: "mode",
    mode_passon: "passon（全自动）",
    mode_manual: "manual（逐次确认）",
    mode_auto: "auto（白名单自动）",
    mode_plan: "plan（只读计划）",
    no_provider_warning: "未配置模型供应商，请先完成引导或运行 mcode add_provider <name>",
    no_api_key_warning: "API Key 未设置：请先 export 环境变量后再发送消息",
    slash_hint: "指令补全：↑↓ 选择，tab 补全，回车执行",
    at_hint: "文件/目录补全：↑↓ 选择，tab 补全",
    tool_running: "运行中",
    permission_ask: "是否允许执行？",
    permission_allow: "允许",
    permission_deny: "拒绝",
    error_prefix: "错误",
    guide_welcome: "欢迎使用 MCode —— 首次启动引导",
    guide_language: "选择语言环境（↑↓ 选择，回车确认）",
    guide_provider_name: "供应商名称（如 openai）",
    guide_base_url: "API baseURL（如 https://api.openai.com/v1）",
    guide_api_key_env: "API Key：环境变量名（推荐，如 GLM_API_KEY），或直接粘贴 Key",
    guide_models: "可用模型，逗号分隔（如 gpt-5,gpt-5-mini）",
    guide_mode: "选择默认权限模式（↑↓ 选择，回车确认）",
    guide_done: "初始化完成，开始对话吧。",
    command_not_found: "未知指令",
};

const EN_US: Strings = Strings {
    banner_hint: "Type / for commands, @ for files, shift+tab to switch mode",
    input_placeholder: "Type a message…",
    thinking: "thinking …",
    mode_hint: "mode",
    mode_passon: "passon (full auto)",
    mode_manual: "manual (confirm each)",
    mode_auto: "auto (allowlist)",
    mode_plan: "plan (read-only)",
    no_provider_warning: "No model provider configured — finish the setup guide or run mcode add_provider <name>",
    no_api_key_warning: "API key not set: export the environment variable first, then send a message",
    slash_hint: "Command completion: ↑↓ select, tab complete, enter run",
    at_hint: "File/dir completion: ↑↓ select, tab complete",
    tool_running: "running",
    permission_ask: "Allow execution?",
    permission_allow: "allow",
    permission_deny: "deny",
    error_prefix: "error",
    guide_welcome: "Welcome to MCode — first launch setup",
    guide_language: "Choose language (↑↓ select, enter confirm)",
    guide_provider_name: "Provider name (e.g. openai)",
    guide_base_url: "API baseURL (e.g. https://api.openai.com/v1)",
    guide_api_key_env: "API key: env var name (recommended, e.g. GLM_API_KEY), or paste the key",
    guide_models: "Available models, comma separated (e.g. gpt-5,gpt-5-mini)",
    guide_mode: "Choose default permission mode (↑↓ select, enter confirm)",
    guide_done: "Setup complete. Start chatting.",
    command_not_found: "unknown command",
};

// zh_TW / ja_JP / ko_KR：对齐 TS 的 `...zh_CN` / `...en_US` 展开结果（基表 + 覆盖键）
const ZH_TW: Strings = Strings {
    banner_hint: "輸入 / 查看指令，@ 引用檔案，shift+tab 切換模式",
    input_placeholder: "輸入訊息…",
    no_provider_warning: "未設定模型供應商，請先完成引導或執行 mcode add_provider <name>",
    guide_welcome: "歡迎使用 MCode —— 首次啟動引導",
    guide_language: "選擇語言環境（↑↓ 選擇，Enter 確認）",
    guide_done: "初始化完成，開始對話吧。",
    command_not_found: "未知指令",
    ..ZH_CN
};

const JA_JP: Strings = Strings {
    banner_hint: "「/」でコマンド、「@」でファイル参照、shift+tab でモード切替",
    input_placeholder: "メッセージを入力…",
    thinking: "思考中 …",
    no_provider_warning: "モデルプロバイダが未設定です。セットアップガイドを完了するか mcode add_provider <name> を実行してください",
    guide_welcome: "MCode へようこそ —— 初回セットアップ",
    guide_language: "言語を選択（↑↓ で選択、Enter で確定）",
    guide_done: "セットアップ完了。会話を始めましょう。",
    command_not_found: "不明なコマンド",
    ..EN_US
};

const KO_KR: Strings = Strings {
    banner_hint: "/ 로 명령어, @ 로 파일 참조, shift+tab 으로 모드 전환",
    input_placeholder: "메시지 입력…",
    thinking: "생각 중 …",
    no_provider_warning: "모델 공급자가 설정되지 않았습니다. 설정 가이드를 완료하거나 mcode add_provider <name> 을 실행하세요",
    guide_welcome: "MCode에 오신 것을 환영합니다 —— 첫 실행 설정",
    guide_language: "언어 선택（↑↓ 선택, Enter 확인）",
    guide_done: "설정 완료. 대화를 시작하세요.",
    command_not_found: "알 수 없는 명령어",
    ..EN_US
};

/// 语言 → 字符串表（未知值兜底 en_US，对齐 TS `LOCALES[language] ?? LOCALES.en_US`）
pub fn strings(lang: Language) -> &'static Strings {
    match lang {
        Language::ZhCn => &ZH_CN,
        Language::ZhTw => &ZH_TW,
        Language::EnUs => &EN_US,
        Language::JaJp => &JA_JP,
        Language::KoKr => &KO_KR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_languages_have_full_tables() {
        for lang in [
            Language::ZhCn,
            Language::ZhTw,
            Language::EnUs,
            Language::JaJp,
            Language::KoKr,
        ] {
            let s = strings(lang);
            assert!(!s.banner_hint.is_empty());
            assert!(!s.guide_done.is_empty());
            assert!(!s.command_not_found.is_empty());
        }
        // zh_TW 继承 zh_CN 的未覆盖键（对齐 TS spread 语义）
        assert_eq!(
            strings(Language::ZhTw).thinking,
            strings(Language::ZhCn).thinking
        );
        // ja/ko 继承 en_US
        assert_eq!(
            strings(Language::JaJp).mode_plan,
            strings(Language::EnUs).mode_plan
        );
    }
}
