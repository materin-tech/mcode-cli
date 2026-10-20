//! 首次启动引导 — TS ui/GuidedSetup.tsx 的 Rust 移植。
//! 流程：语言 → 供应商四步（name/baseURL/apiKeyEnv/models）→ 多模态↑↓切换 → 默认权限模式 → 完成。
//! 触发：config.json 无 providers 时接管整个界面（对齐 TS：providers.length === 0 渲染 GuidedSetup）。

use crate::locales::strings;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mcode_core::config::{Language, Mode, Provider};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// 列表顺序（对齐 TS constants 的 LANGUAGES / MODES）
const LANGUAGES: [Language; 5] = [
    Language::ZhCn,
    Language::ZhTw,
    Language::EnUs,
    Language::JaJp,
    Language::KoKr,
];

const MODES: [Mode; 4] = [Mode::Passon, Mode::Manual, Mode::Auto, Mode::Plan];

// ---------- 步进类型 ----------

/// 单步推进动作
pub enum GuideAction {
    None,
    /// 引导完成，携带最终配置
    Completed(GuideResult),
}

/// 完成时交付的配置（provider.models 已拆分、multimodal 已并入 multimodal_models）
pub struct GuideResult {
    pub language: Language,
    pub provider: Provider,
    pub default_mode: Mode,
}

/// 文本步字段
#[derive(Clone, Copy, PartialEq)]
enum TextField {
    Name,
    BaseUrl,
    ApiKeyEnv,
    Models,
}

/// 引导状态机（步骤顺序对齐 TS Step 联合类型；无 Done 态 —— TS 在最后一步 onComplete 后直接卸载组件）
enum Step {
    Language,
    Text(TextField),
    Multimodal,
    Mode,
}

/// 引导向导（providers 为空时接管整个 TUI）
pub struct Guide {
    step: Step,
    language: Language,
    select_index: usize,
    text: String,
    name: String,
    base_url: String,
    api_key_env: String,
    models: String,
    multimodal: bool,
}

impl Guide {
    pub fn new(default_language: Language) -> Self {
        let select_index = LANGUAGES
            .iter()
            .position(|l| *l == default_language)
            .unwrap_or(0);
        Self {
            step: Step::Language,
            language: default_language,
            select_index,
            text: String::new(),
            name: String::new(),
            base_url: String::new(),
            api_key_env: String::new(),
            models: String::new(),
            multimodal: false,
        }
    }

    /// 按键推进状态机；完成时返回 Completed
    pub fn handle_key(&mut self, key: KeyEvent) -> GuideAction {
        match &self.step {
            Step::Multimodal => {
                if matches!(key.code, KeyCode::Up | KeyCode::Down) {
                    self.multimodal = !self.multimodal;
                }
                if key.code == KeyCode::Enter {
                    self.select_index = 0;
                    self.step = Step::Mode;
                }
                GuideAction::None
            }
            Step::Language | Step::Mode => {
                let count = match &self.step {
                    Step::Language => LANGUAGES.len(),
                    _ => MODES.len(),
                };
                match key.code {
                    KeyCode::Up => {
                        self.select_index = (self.select_index + count - 1) % count.max(1);
                    }
                    KeyCode::Down => {
                        self.select_index = (self.select_index + 1) % count.max(1);
                    }
                    KeyCode::Enter => {
                        if matches!(self.step, Step::Language) {
                            self.language = LANGUAGES[self.select_index.min(LANGUAGES.len() - 1)];
                            self.select_index = 0;
                            self.step = Step::Text(TextField::Name);
                        } else {
                            return GuideAction::Completed(self.finish());
                        }
                    }
                    _ => {}
                }
                GuideAction::None
            }
            Step::Text(field) => {
                if key.code == KeyCode::Enter {
                    let value = self.text.trim().to_string();
                    if value.is_empty() {
                        return GuideAction::None; // TS：空值不推进
                    }
                    match field {
                        TextField::Name => self.name = value,
                        TextField::BaseUrl => self.base_url = value,
                        TextField::ApiKeyEnv => self.api_key_env = value,
                        TextField::Models => self.models = value,
                    }
                    self.text.clear();
                    self.select_index = 0;
                    self.step = match field {
                        TextField::Name => Step::Text(TextField::BaseUrl),
                        TextField::BaseUrl => Step::Text(TextField::ApiKeyEnv),
                        TextField::ApiKeyEnv => Step::Text(TextField::Models),
                        TextField::Models => Step::Multimodal,
                    };
                    return GuideAction::None;
                }
                if matches!(key.code, KeyCode::Backspace | KeyCode::Delete) {
                    self.text.pop();
                    return GuideAction::None;
                }
                // 可打印字符追加（Ctrl 组合不捕获）
                if let KeyCode::Char(c) = key.code
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                {
                    self.text.push(c);
                }
                GuideAction::None
            }
        }
    }

    /// 组装最终配置（对齐 TS onComplete：models 拆分去空、multimodal 决定 multimodalModels）
    fn finish(&self) -> GuideResult {
        let models: Vec<String> = self
            .models
            .split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        GuideResult {
            language: self.language,
            provider: Provider {
                name: self.name.clone(),
                base_url: self.base_url.clone(),
                api_key_env: self.api_key_env.clone(),
                multimodal_models: if self.multimodal {
                    models.clone()
                } else {
                    vec![]
                },
                models,
                // 引导暂不提供协议选择；私有化部署（vLLM 等仅 Chat Completions）
                // 需 /provider add 末位参数或手改 config.json 的 protocol 为 "chat"
                protocol: mcode_core::config::Protocol::default(),
            },
            default_mode: MODES[self.select_index.min(MODES.len() - 1)],
        }
    }
}

/// '› ' 选中光标 + 高亮（对齐 TS 的 i === selectIndex 渲染）
fn sel_span(selected: bool, text: impl Into<String>, color: Color) -> Span<'static> {
    let prefix = if selected { "› " } else { "  " };
    Span::styled(
        format!("{prefix}{}", text.into()),
        Style::default().fg(if selected { color } else { Color::default() }),
    )
}

/// 引导界面（providers 为空时替代整个聊天界面渲染）
pub fn draw(f: &mut ratatui::Frame, area: Rect, guide: &Guide) {
    let t = strings(guide.language);
    let rect = crate::tui::centered_rect(70, 18, area);
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        t.guide_welcome,
        Style::default().fg(Color::Cyan),
    ))];

    match &guide.step {
        Step::Language => {
            lines.push(Line::from(t.guide_language));
            for (i, lang) in LANGUAGES.iter().enumerate() {
                lines.push(Line::from(sel_span(
                    i == guide.select_index,
                    format!("{lang:?}"),
                    Color::Cyan,
                )));
            }
        }
        Step::Text(field) => {
            let hint = match field {
                TextField::Name => t.guide_provider_name,
                TextField::BaseUrl => t.guide_base_url,
                TextField::ApiKeyEnv => t.guide_api_key_env,
                TextField::Models => t.guide_models,
            };
            lines.push(Line::from(hint));
            lines.push(Line::from(Span::styled(
                format!("› {}▌", guide.text),
                Style::default().fg(Color::Green),
            )));
        }
        Step::Multimodal => {
            // 多模态提示文案对齐 TS 硬编码中文（TS 未做本地化）
            lines.push(Line::from("这些模型是否支持图片输入（多模态）？"));
            lines.push(Line::from(sel_span(
                guide.multimodal,
                "支持（多模态图片）",
                Color::Cyan,
            )));
            lines.push(Line::from(sel_span(
                !guide.multimodal,
                "不支持（纯文本）",
                Color::Cyan,
            )));
            lines.push(Line::from(Span::styled(
                "不支持时图片不会上传，仅提示；之后可在 config.json 的 multimodalModels 中修改",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Step::Mode => {
            lines.push(Line::from(t.guide_mode));
            for (i, m) in MODES.iter().enumerate() {
                let color = if matches!(m, Mode::Passon) {
                    Color::Red
                } else {
                    Color::Cyan
                };
                lines.push(Line::from(sel_span(
                    i == guide.select_index,
                    label_of(m),
                    color,
                )));
            }
        }
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" setup "),
    );
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(para, rect);
}

/// 模式标签（引导里展示 locale 化标签；labels 与 StatusBar 一致）
fn label_of(m: &Mode) -> &'static str {
    let t = strings(Language::ZhCn);
    match m {
        Mode::Passon => t.mode_passon,
        Mode::Manual => t.mode_manual,
        Mode::Auto => t.mode_auto,
        Mode::Plan => t.mode_plan,
    }
}
