//! 设置: Obsidian, capture, captions, minutes providers, local models and the index.

use std::path::PathBuf;

use gpui_kit::base::TestTextExt as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, IconName, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_i18n::{tr, trf};
use mm_llm::{
    AsrMode, AsrModelList, AsrProvider, DEFAULT_GEMINI_LIVE_MODEL, DEFAULT_GEMINI_REFINE_MODEL,
    DEFAULT_OPENAI_LIVE_MODEL, DEFAULT_OPENAI_REFINE_MODEL, MinutesBackend, asr_pricing, byok,
};
use mm_service::{AppConfig, CloudLiveMode, LiveAsrBackend, RefineBackend};
use mm_store::ProviderUsageSummary;

use crate::logic;
use crate::state::{AppState, Loadable, NoticeKind};
use crate::ui::{self, Choice};

/// Value of the "no window" choice.
const NO_WINDOW: &str = "";

/// Text fields of the settings page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Vault,
    Folder,
    Model,
    BaseUrl,
    EmbedDir,
    EmbedUrl,
    LlmDir,
    Spacing,
    RefineModel,
    LiveOpenAiModel,
    LiveGeminiModel,
    LiveOpenAiUtteranceModel,
    LiveGeminiUtteranceModel,
    RefineOpenAiModel,
    RefineGeminiModel,
}

/// The 设置 page.
pub struct SettingsPage {
    app: Entity<AppState>,
    /// Input placeholders, rewritten when the interface language changes.
    placeholders: ui::Placeholders,
    vault: Entity<InputState>,
    folder: Entity<InputState>,
    model: Entity<InputState>,
    base_url: Entity<InputState>,
    api_key: Entity<InputState>,
    asr_openai_key: Entity<InputState>,
    asr_openai_admin_key: Entity<InputState>,
    asr_gemini_key: Entity<InputState>,
    embed_dir: Entity<InputState>,
    embed_url: Entity<InputState>,
    hf_token: Entity<InputState>,
    llm_dir: Entity<InputState>,
    spacing: Entity<InputState>,
    refine_model: Entity<InputState>,
    live_openai_model: Entity<InputState>,
    live_gemini_model: Entity<InputState>,
    live_openai_utterance_model: Entity<InputState>,
    live_gemini_utterance_model: Entity<InputState>,
    refine_openai_model: Entity<InputState>,
    refine_gemini_model: Entity<InputState>,
    mics: Entity<SelectState<Vec<Choice>>>,
    mic_sig: Vec<String>,
    /// Window to record with the next meeting (moved here from the 会议中 page).
    windows: Entity<SelectState<Vec<Choice>>>,
    window_sig: Vec<String>,
    /// Embedding folder last shown in `embed_dir`, to notice changes made elsewhere.
    embed_dir_seen: Option<PathBuf>,
    /// BYOK kind chosen in the form (may differ from config until a model is typed).
    byok_kind: &'static str,
    /// Model pickers, in [`ModelPicker`] order.
    model_sels: [Entity<SelectState<Vec<Choice>>>; 2],
    /// Items, selection and language last pushed into each picker.
    model_sigs: [Vec<String>; 2],
    /// Cloud transcription model pickers, in [`AsrModelPicker`] order.
    asr_sels: [Entity<SelectState<Vec<Choice>>>; 6],
    /// Items, selection and language last pushed into each cloud ASR picker.
    asr_sigs: [Vec<String>; 6],
}

/// The Codex model pickers on this page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelPicker {
    /// Codex model for minutes.
    CodexMinutes = 0,
    /// Codex model for the unified post-meeting refine + minutes pass.
    CodexRefine = 1,
}

impl ModelPicker {
    const ALL: [ModelPicker; 2] = [Self::CodexMinutes, Self::CodexRefine];

    /// Stores a picked model id in the config.
    ///
    /// Params: `cfg`; `id` – `""` means Codex's default model.
    fn store(self, cfg: &mut AppConfig, id: String) {
        let picked = Some(id).filter(|m| !m.is_empty());
        match self {
            Self::CodexMinutes => cfg.codex_model = picked,
            Self::CodexRefine => cfg.codex_refine_model = picked,
        }
    }
}

/// The cloud transcription model pickers on this page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AsrModelPicker {
    LiveOpenAi = 0,
    LiveGemini = 1,
    LiveOpenAiUtterance = 2,
    LiveGeminiUtterance = 3,
    RefineOpenAi = 4,
    RefineGemini = 5,
}

impl AsrModelPicker {
    const ALL: [AsrModelPicker; 6] = [
        Self::LiveOpenAi,
        Self::LiveGemini,
        Self::LiveOpenAiUtterance,
        Self::LiveGeminiUtterance,
        Self::RefineOpenAi,
        Self::RefineGemini,
    ];

    fn meta(self) -> (AsrProvider, AsrMode) {
        match self {
            Self::LiveOpenAi => (AsrProvider::OpenAi, AsrMode::Live),
            Self::LiveGemini => (AsrProvider::Gemini, AsrMode::Live),
            Self::LiveOpenAiUtterance | Self::RefineOpenAi => {
                (AsrProvider::OpenAi, AsrMode::Refine)
            }
            Self::LiveGeminiUtterance | Self::RefineGemini => {
                (AsrProvider::Gemini, AsrMode::Refine)
            }
        }
    }

    fn saved(self, cfg: &AppConfig) -> String {
        match self {
            Self::LiveOpenAi => cfg.live_openai_model.clone().unwrap_or_default(),
            Self::LiveGemini => cfg.live_gemini_model.clone().unwrap_or_default(),
            Self::LiveOpenAiUtterance => {
                cfg.live_openai_utterance_model.clone().unwrap_or_default()
            }
            Self::LiveGeminiUtterance => {
                cfg.live_gemini_utterance_model.clone().unwrap_or_default()
            }
            Self::RefineOpenAi => cfg.refine_openai_model.clone().unwrap_or_default(),
            Self::RefineGemini => cfg.refine_gemini_model.clone().unwrap_or_default(),
        }
    }

    fn store(self, cfg: &mut AppConfig, id: String) {
        let picked = Some(id).filter(|m| !m.is_empty());
        match self {
            Self::LiveOpenAi => cfg.live_openai_model = picked,
            Self::LiveGemini => cfg.live_gemini_model = picked,
            Self::LiveOpenAiUtterance => cfg.live_openai_utterance_model = picked,
            Self::LiveGeminiUtterance => cfg.live_gemini_utterance_model = picked,
            Self::RefineOpenAi => cfg.refine_openai_model = picked,
            Self::RefineGemini => cfg.refine_gemini_model = picked,
        }
    }

    fn input(self, page: &SettingsPage) -> &Entity<InputState> {
        match self {
            Self::LiveOpenAi => &page.live_openai_model,
            Self::LiveGemini => &page.live_gemini_model,
            Self::LiveOpenAiUtterance => &page.live_openai_utterance_model,
            Self::LiveGeminiUtterance => &page.live_gemini_utterance_model,
            Self::RefineOpenAi => &page.refine_openai_model,
            Self::RefineGemini => &page.refine_gemini_model,
        }
    }
}

/// Displays an optional path in a text field.
///
/// Params: `p`.
/// Returns: the text.
fn path_text(p: &Option<PathBuf>) -> String {
    p.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Parses a path field; empty means "not set" and `~/` expands to the home folder.
///
/// Params: `s`.
/// Returns: the path.
fn parse_path(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(match s.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => PathBuf::from(s),
    })
}

impl SettingsPage {
    /// Creates the page with fields filled from the config.
    ///
    /// Params: `app`; `window`; `cx`.
    /// Returns: the page.
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cfg = app.read(cx).cfg.clone();
        let (kind, model, base) =
            cfg.byok
                .as_ref()
                .map(logic::byok_parts)
                .unwrap_or(("", String::new(), String::new()));
        let mk = |value: String,
                  placeholder: fn() -> &'static str,
                  placeholders: &mut ui::Placeholders,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
            placeholders.input(&input, placeholder);
            input
        };
        let mut placeholders = ui::Placeholders::default();
        let vault = mk(
            path_text(&cfg.vault_path),
            || {
                tr!(
                    "Obsidian 库文件夹，例如 ~/Documents/Work Notes",
                    "Obsidian vault folder, e.g. ~/Documents/Work Notes"
                )
            },
            &mut placeholders,
            window,
            cx,
        );
        let folder = mk(
            cfg.meetings_folder.clone(),
            || "Meetings",
            &mut placeholders,
            window,
            cx,
        );
        let model = mk(
            model,
            || {
                tr!(
                    "模型，例如 claude-opus-5-5、gpt-5.5",
                    "Model, e.g. claude-opus-5-5 or gpt-5.5"
                )
            },
            &mut placeholders,
            window,
            cx,
        );
        let base_url = mk(
            base,
            || {
                tr!(
                    "接口地址，例如 https://openrouter.ai/api/v1",
                    "Endpoint, e.g. https://openrouter.ai/api/v1"
                )
            },
            &mut placeholders,
            window,
            cx,
        );
        let api_key = cx.new(|cx| InputState::new(window, cx).masked(true));
        let embed_dir = mk(
            path_text(&cfg.embedding_model_dir),
            || {
                tr!(
                    "EmbeddingGemma 2 模型文件夹",
                    "EmbeddingGemma 2 model folder"
                )
            },
            &mut placeholders,
            window,
            cx,
        );
        let embed_url = mk(
            cfg.embedding_model_url.clone(),
            || {
                tr!(
                    "模型地址，例如 https://huggingface.co/mlx-community/embeddinggemma-2-bf16",
                    "Model URL, e.g. https://huggingface.co/mlx-community/embeddinggemma-2-bf16"
                )
            },
            &mut placeholders,
            window,
            cx,
        );
        let hf_token = cx.new(|cx| InputState::new(window, cx).masked(true));
        let llm_dir = mk(
            path_text(&cfg.local_llm_dir),
            || {
                tr!(
                    "本地纪要模型（MLX）文件夹",
                    "Local minutes model (MLX) folder"
                )
            },
            &mut placeholders,
            window,
            cx,
        );
        let spacing = mk(
            logic::spacing_text(cfg.spatial_spacing_cm),
            || tr!("选填，例如 14", "Optional, e.g. 14"),
            &mut placeholders,
            window,
            cx,
        );
        let refine_model = mk(
            cfg.refine_asr_model.clone().unwrap_or_default(),
            || {
                tr!(
                    "默认 mlx-community/Qwen3-ASR-1.7B-8bit",
                    "Default: mlx-community/Qwen3-ASR-1.7B-8bit"
                )
            },
            &mut placeholders,
            window,
            cx,
        );
        let live_openai_model = mk(
            cfg.live_openai_model.clone().unwrap_or_default(),
            || DEFAULT_OPENAI_LIVE_MODEL,
            &mut placeholders,
            window,
            cx,
        );
        let live_gemini_model = mk(
            cfg.live_gemini_model.clone().unwrap_or_default(),
            || DEFAULT_GEMINI_LIVE_MODEL,
            &mut placeholders,
            window,
            cx,
        );
        let live_openai_utterance_model = mk(
            cfg.live_openai_utterance_model.clone().unwrap_or_default(),
            || DEFAULT_OPENAI_REFINE_MODEL,
            &mut placeholders,
            window,
            cx,
        );
        let live_gemini_utterance_model = mk(
            cfg.live_gemini_utterance_model.clone().unwrap_or_default(),
            || DEFAULT_GEMINI_REFINE_MODEL,
            &mut placeholders,
            window,
            cx,
        );
        let refine_openai_model = mk(
            cfg.refine_openai_model.clone().unwrap_or_default(),
            || DEFAULT_OPENAI_REFINE_MODEL,
            &mut placeholders,
            window,
            cx,
        );
        let refine_gemini_model = mk(
            cfg.refine_gemini_model.clone().unwrap_or_default(),
            || DEFAULT_GEMINI_REFINE_MODEL,
            &mut placeholders,
            window,
            cx,
        );
        let asr_openai_key = cx.new(|cx| InputState::new(window, cx).masked(true));
        let asr_openai_admin_key = cx.new(|cx| InputState::new(window, cx).masked(true));
        let asr_gemini_key = cx.new(|cx| InputState::new(window, cx).masked(true));
        let mics = cx.new(|cx| {
            SelectState::new(
                vec![Choice::new(
                    "",
                    tr!("系统默认麦克风", "System default microphone"),
                )],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let windows = cx.new(|cx| {
            SelectState::new(
                vec![Choice::new(
                    NO_WINDOW,
                    tr!("不录屏，只录声音", "Audio only, no screen recording"),
                )],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });

        let model_sels = ModelPicker::ALL
            .map(|_| cx.new(|cx| SelectState::new(Vec::<Choice>::new(), None, window, cx)));
        for (sel, picker) in model_sels.iter().zip(ModelPicker::ALL) {
            cx.subscribe(sel, move |this, _, ev: &SelectEvent<Vec<Choice>>, cx| {
                let SelectEvent::Confirm(Some(id)) = ev else {
                    return;
                };
                let id = id.clone();
                this.app
                    .update(cx, |a, cx| a.update_config(cx, |c| picker.store(c, id)));
            })
            .detach();
        }
        let asr_sels = AsrModelPicker::ALL
            .map(|_| cx.new(|cx| SelectState::new(Vec::<Choice>::new(), None, window, cx)));
        for (sel, picker) in asr_sels.iter().zip(AsrModelPicker::ALL) {
            cx.subscribe_in(
                sel,
                window,
                move |this, _, ev: &SelectEvent<Vec<Choice>>, window, cx| {
                    let SelectEvent::Confirm(Some(id)) = ev else {
                        return;
                    };
                    let id = id.clone();
                    let inp = picker.input(this).clone();
                    let val_for_input = id.clone();
                    inp.update(cx, |s, cx| s.set_value(val_for_input, window, cx));
                    this.app
                        .update(cx, |a, cx| a.update_config(cx, |c| picker.store(c, id)));
                },
            )
            .detach();
        }
        // Model lists come from the CLIs; ask once the page exists (never inside its creation,
        // which may run while the app state is being updated).
        let app_for_lists = app.clone();
        cx.defer_in(window, move |this, window, cx| {
            app_for_lists.update(cx, |a, cx| a.ensure_model_lists(cx));
            this.sync_model_pickers(window, cx);
        });

        for (input, field) in [
            (&vault, Field::Vault),
            (&folder, Field::Folder),
            (&model, Field::Model),
            (&base_url, Field::BaseUrl),
            (&embed_dir, Field::EmbedDir),
            (&embed_url, Field::EmbedUrl),
            (&llm_dir, Field::LlmDir),
            (&spacing, Field::Spacing),
            (&refine_model, Field::RefineModel),
            (&live_openai_model, Field::LiveOpenAiModel),
            (&live_gemini_model, Field::LiveGeminiModel),
            (
                &live_openai_utterance_model,
                Field::LiveOpenAiUtteranceModel,
            ),
            (
                &live_gemini_utterance_model,
                Field::LiveGeminiUtteranceModel,
            ),
            (&refine_openai_model, Field::RefineOpenAiModel),
            (&refine_gemini_model, Field::RefineGeminiModel),
        ] {
            cx.subscribe(input, move |this, state, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let v = state.read(cx).value().to_string();
                    this.commit(field, v, cx);
                }
            })
            .detach();
        }
        cx.subscribe(&mics, |this, _, ev: &SelectEvent<Vec<Choice>>, cx| {
            let SelectEvent::Confirm(v) = ev;
            let uid = v.clone().filter(|s| !s.is_empty());
            this.app
                .update(cx, |a, cx| a.update_config(cx, |c| c.microphone_uid = uid));
        })
        .detach();
        cx.subscribe(&windows, |this, _, ev: &SelectEvent<Vec<Choice>>, cx| {
            let SelectEvent::Confirm(v) = ev;
            let id = v.as_deref().and_then(|s| s.parse::<u32>().ok());
            this.app.update(cx, |a, cx| {
                a.selected_window = id;
                cx.notify();
            });
        })
        .detach();
        cx.observe_in(&app, window, |this, _, window, cx| {
            this.sync_embed_dir(window, cx);
            this.sync_mics(window, cx);
            this.sync_windows(window, cx);
            this.sync_model_pickers(window, cx)
        })
        .detach();
        placeholders.input(&api_key, || {
            tr!(
                "粘贴 API 密钥，保存后不会再显示",
                "Paste an API key. It won’t be shown again after saving."
            )
        });
        placeholders.input(&asr_openai_key, || {
            tr!(
                "粘贴 OpenAI API 密钥（sk-…），保存后不会再显示",
                "Paste OpenAI API key (sk-…). Hidden after saving."
            )
        });
        placeholders.input(&asr_openai_admin_key, || {
            tr!(
                "粘贴 OpenAI Admin 密钥（sk-admin-…，可选）",
                "Paste OpenAI Admin key (sk-admin-…, optional)"
            )
        });
        placeholders.input(&asr_gemini_key, || {
            tr!(
                "粘贴 Google Gemini API 密钥（AIza…），保存后不会再显示",
                "Paste Google Gemini API key (AIza…). Hidden after saving."
            )
        });
        placeholders.input(&hf_token, || {
            tr!(
                "粘贴 Hugging Face 令牌（hf_…），保存后不会再显示",
                "Paste a Hugging Face token (hf_…). It won’t be shown again after saving."
            )
        });
        placeholders.sync(window, cx);
        Self {
            app,
            placeholders,
            vault,
            folder,
            model,
            base_url,
            api_key,
            asr_openai_key,
            asr_openai_admin_key,
            asr_gemini_key,
            embed_dir,
            embed_url,
            hf_token,
            llm_dir,
            spacing,
            refine_model,
            live_openai_model,
            live_gemini_model,
            live_openai_utterance_model,
            live_gemini_utterance_model,
            refine_openai_model,
            refine_gemini_model,
            mics,
            mic_sig: vec![],
            windows,
            window_sig: vec![],
            embed_dir_seen: cfg.embedding_model_dir.clone(),
            byok_kind: kind,
            model_sels,
            model_sigs: Default::default(),
            asr_sels,
            asr_sigs: Default::default(),
        }
    }

    /// Applies `(items, selected)` choices to `sels` whenever their rendered signature changes.
    fn apply_picker_lists<const N: usize>(
        sels: &[Entity<SelectState<Vec<Choice>>>; N],
        sigs: &mut [Vec<String>; N],
        lists: [(Vec<(String, String)>, String); N],
        lang: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (i, (items, selected)) in lists.into_iter().enumerate() {
            // Labels are part of the signature: the language switch relabels the default entry.
            let sig: Vec<String> = items
                .iter()
                .flat_map(|(id, l)| [id.clone(), l.clone()])
                .chain([selected.clone(), lang.to_string()])
                .collect();
            if sig == sigs[i] {
                continue;
            }
            sigs[i] = sig;
            let choices: Vec<Choice> = items
                .into_iter()
                .map(|(id, l)| Choice::new(id, l))
                .collect();
            sels[i].update(cx, |s, cx| {
                s.set_items(choices, window, cx);
                s.set_selected_value(&selected, window, cx);
            });
        }
    }

    /// Keeps the model pickers in step with Codex / cloud ASR model lists and the config.
    ///
    /// Params: `window`; `cx`.
    fn sync_model_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = mm_i18n::current().code().to_string();
        let (codex_lists, asr_lists) = {
            let app = self.app.read(cx);
            let cfg = &app.cfg;
            let all = |_: &str| true;
            let listed = app.codex_models.ready().map(Vec::as_slice);
            let default_label = Some(tr!("Codex 默认模型", "Codex default model"));
            let codex_minutes_saved = cfg.codex_model.clone().unwrap_or_default();
            let codex_refine_saved = cfg.codex_refine_model.clone().unwrap_or_default();
            let codex_lists = [
                (
                    logic::model_choices(listed, &[], all, &codex_minutes_saved, default_label),
                    codex_minutes_saved,
                ),
                (
                    logic::model_choices(listed, &[], all, &codex_refine_saved, default_label),
                    codex_refine_saved,
                ),
            ];
            let asr_lists = AsrModelPicker::ALL.map(|picker| {
                let (provider, mode) = picker.meta();
                let provider_models = match provider {
                    AsrProvider::OpenAi => app.openai_asr_models.ready(),
                    AsrProvider::Gemini => app.gemini_asr_models.ready(),
                };
                let listed = provider_models.map(|m| m.for_mode(mode));
                let default_id = asr_pricing::default_model(provider, mode);
                let default_label = trf!("默认（{default_id}）", "Default ({default_id})");
                let selected = picker.saved(cfg);
                (
                    logic::model_choices(listed, &[], all, &selected, Some(&default_label)),
                    selected,
                )
            });
            (codex_lists, asr_lists)
        };
        Self::apply_picker_lists(
            &self.model_sels,
            &mut self.model_sigs,
            codex_lists,
            &lang,
            window,
            cx,
        );
        Self::apply_picker_lists(
            &self.asr_sels,
            &mut self.asr_sigs,
            asr_lists,
            &lang,
            window,
            cx,
        );
        cx.notify();
    }

    /// A model picker with a button that asks Codex for its list again.
    ///
    /// Params: `picker`; `list` – the CLI's list state, for the status line; `cx`.
    /// Returns: the picker element.
    fn model_picker(
        &self,
        picker: ModelPicker,
        list: &Loadable<Vec<mm_llm::ModelOption>>,
        cx: &mut Context<Self>,
    ) -> Div {
        let note = match list {
            Loadable::Loading | Loadable::Idle => {
                Some(tr!("正在获取模型列表…", "Getting the model list…").to_string())
            }
            Loadable::Failed(e) => Some(trf!(
                "获取模型列表失败，先显示已保存的模型：{}",
                "Couldn’t get the model list, so only saved models are shown: {}",
                e.message
            )),
            Loadable::Ready(_) => None,
        };
        let app = self.app.clone();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(Select::new(&self.model_sels[picker as usize]).small()),
                    )
                    .child(
                        Button::new(("model-refresh", picker as usize))
                            .small()
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label(tr!("刷新列表", "Refresh list"))
                            .on_click(move |_, _, cx| {
                                app.update(cx, |a, cx| a.refresh_codex_models(cx))
                            }),
                    ),
            )
            .when_some(note, |el, n| el.child(ui::muted(n, cx)))
    }

    /// Human-readable Codex CLI sign-in status line shared by the refine and minutes sections.
    ///
    /// Params: `app`.
    /// Returns: localized status line.
    fn codex_status_line(app: &AppState) -> String {
        match &app.codex {
            Loadable::Ready(c) => match (&c.path, c.signed_in) {
                (None, _) => tr!(
                    "未安装 Codex CLI。安装后可用 ChatGPT 账号处理录音与纪要。",
                    "Codex CLI isn’t installed. Install it to use your ChatGPT account."
                )
                .to_string(),
                (Some(p), true) => trf!(
                    "Codex 已登录{}（{}）",
                    "Codex signed in{} ({})",
                    c.label
                        .as_deref()
                        .map(|l| trf!("：{l}", ": {l}"))
                        .unwrap_or_default(),
                    p.display()
                ),
                (Some(_), false) => trf!(
                    "Codex 未登录。在终端运行 codex login 后点“重新检测”。{}",
                    "Codex isn’t signed in. Run codex login in Terminal, then click Check again.{}",
                    c.error
                        .as_deref()
                        .map(|e| trf!("（{e}）", " ({e})"))
                        .unwrap_or_default()
                ),
            },
            Loadable::Failed(e) => e.message.clone(),
            _ => tr!("正在检测 Codex…", "Checking Codex…").to_string(),
        }
    }

    /// Shows a model folder set elsewhere (a finished download) in the folder field.
    ///
    /// Params: `window`; `cx`.
    fn sync_embed_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.app.read(cx).cfg.embedding_model_dir.clone();
        if dir != self.embed_dir_seen {
            let text = path_text(&dir);
            self.embed_dir_seen = dir;
            self.embed_dir
                .update(cx, |s, cx| s.set_value(text, window, cx));
        }
    }

    /// Keeps the microphone select in step with the probed inputs.
    fn sync_mics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        if let Loadable::Ready(inputs) = &app.inputs {
            let mut items = vec![Choice::new(
                "",
                tr!("系统默认麦克风", "System default microphone"),
            )];
            items.extend(inputs.iter().map(|i| {
                let spatial = if i.supports_spatial_cues() {
                    tr!("，可判断方位", ", detects direction")
                } else {
                    ""
                };
                let (name, n) = (&i.name, i.channels);
                let channels = logic::count_en(n as u64, "channel", "channels");
                Choice::new(
                    i.uid.clone(),
                    trf!(
                        "{name}（{n} 声道{spatial}）",
                        "{name} ({channels}{spatial})"
                    ),
                )
            }));
            // The language is part of the signature so a switch relabels the built-in choices.
            let sig: Vec<String> = items
                .iter()
                .map(|c| c.value.clone())
                .chain([mm_i18n::current().code().to_string()])
                .collect();
            if sig != self.mic_sig {
                self.mic_sig = sig;
                let selected = app.cfg.microphone_uid.clone().unwrap_or_default();
                self.mics.update(cx, |s, cx| {
                    s.set_items(items, window, cx);
                    s.set_selected_value(&selected, window, cx);
                });
            }
        }
        cx.notify();
    }

    /// Keeps the window select in step with the windows ScreenCaptureKit can record.
    ///
    /// Params: `window`; `cx`.
    fn sync_windows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        let Loadable::Ready(src) = &app.sources else {
            return;
        };
        let mut items = vec![Choice::new(
            NO_WINDOW,
            tr!("不录屏，只录声音", "Audio only, no screen recording"),
        )];
        items.extend(src.windows.iter().map(|w| {
            let title = if w.title.is_empty() {
                w.app_name.clone()
            } else {
                format!("{} — {}", w.app_name, w.title)
            };
            Choice::new(w.window_id.to_string(), title)
        }));
        // The language is part of the signature so a switch relabels the built-in choice.
        let sig: Vec<String> = items
            .iter()
            .map(|c| c.value.clone())
            .chain([mm_i18n::current().code().to_string()])
            .collect();
        if sig != self.window_sig {
            self.window_sig = sig;
            let selected = app
                .selected_window
                .map(|w| w.to_string())
                .unwrap_or_default();
            self.windows.update(cx, |s, cx| {
                s.set_items(items, window, cx);
                s.set_selected_value(&selected, window, cx);
            });
        }
    }

    /// Writes a text field into the config.
    ///
    /// Params: `field`; `value`; `cx`.
    fn commit(&mut self, field: Field, value: String, cx: &mut Context<Self>) {
        let kind = self.byok_kind;
        let model = self.model.read(cx).value().to_string();
        let base = self.base_url.read(cx).value().to_string();
        self.app.update(cx, |a, cx| match field {
            Field::Vault => {
                let p = parse_path(&value);
                if let Some(dir) = &p
                    && !dir.is_dir()
                {
                    a.notify(
                        NoticeKind::Warning,
                        trf!("文件夹不存在：{}", "Folder not found: {}", dir.display()),
                        cx,
                    );
                    return;
                }
                a.update_config(cx, |c| c.vault_path = p);
            }
            Field::Folder => {
                let f = value.trim().trim_matches('/').to_string();
                let f = if f.is_empty() {
                    "Meetings".to_string()
                } else {
                    f
                };
                a.update_config(cx, |c| c.meetings_folder = f);
            }
            Field::Model | Field::BaseUrl => {
                let p = logic::byok_from_form(kind, &model, &base);
                a.update_config(cx, |c| c.byok = p);
            }
            Field::EmbedDir => a.update_config(cx, |c| c.embedding_model_dir = parse_path(&value)),
            Field::EmbedUrl => {
                // An emptied field goes back to the suggested repo rather than to nothing.
                let url = match value.trim() {
                    "" => mm_service::hub::DEFAULT_EMBEDDING_REPO_URL.to_string(),
                    v => v.to_string(),
                };
                a.update_config(cx, |c| c.embedding_model_url = url);
            }
            Field::LlmDir => a.update_config(cx, |c| c.local_llm_dir = parse_path(&value)),
            Field::RefineModel => {
                let model = Some(value.trim().to_string()).filter(|v| !v.is_empty());
                a.update_config(cx, |c| c.refine_asr_model = model);
            }
            Field::LiveOpenAiModel => {
                let model = Some(value.trim().to_string()).filter(|v| !v.is_empty());
                a.update_config(cx, |c| c.live_openai_model = model);
            }
            Field::LiveGeminiModel => {
                let model = Some(value.trim().to_string()).filter(|v| !v.is_empty());
                a.update_config(cx, |c| c.live_gemini_model = model);
            }
            Field::LiveOpenAiUtteranceModel => {
                let model = Some(value.trim().to_string()).filter(|v| !v.is_empty());
                a.update_config(cx, |c| c.live_openai_utterance_model = model);
            }
            Field::LiveGeminiUtteranceModel => {
                let model = Some(value.trim().to_string()).filter(|v| !v.is_empty());
                a.update_config(cx, |c| c.live_gemini_utterance_model = model);
            }
            Field::RefineOpenAiModel => {
                let model = Some(value.trim().to_string()).filter(|v| !v.is_empty());
                a.update_config(cx, |c| c.refine_openai_model = model);
            }
            Field::RefineGeminiModel => {
                let model = Some(value.trim().to_string()).filter(|v| !v.is_empty());
                a.update_config(cx, |c| c.refine_gemini_model = model);
            }
            Field::Spacing => match logic::parse_spacing_cm(&value) {
                Ok(cm) => a.update_config(cx, |c| c.spatial_spacing_cm = cm),
                Err(msg) => a.notify(NoticeKind::Warning, msg, cx),
            },
        });
    }

    /// A labelled row.
    fn row(label: &str, hint: Option<String>, control: impl IntoElement, cx: &App) -> Div {
        let label_text: SharedString = label.to_string().into();
        h_flex()
            .gap_4()
            .items_start()
            .py_2()
            .child(
                v_flex()
                    .w(px(220.))
                    .flex_none()
                    .child(
                        div()
                            .text_sm()
                            .child(label_text.clone().test_text(label_text)),
                    )
                    .when_some(hint, |el, h| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(h),
                        )
                    }),
            )
            .child(div().flex_1().min_w_0().child(control))
    }

    /// A switch bound to a boolean config field.
    fn switch(
        &self,
        id: &'static str,
        label: &'static str,
        value: bool,
        set: fn(&mut AppConfig, bool),
    ) -> Switch {
        let app = self.app.clone();
        Switch::new(id)
            .checked(value)
            .label(label)
            .on_click(move |v, _, cx| {
                let v = *v;
                app.update(cx, |a, cx| a.update_config(cx, |c| set(c, v)));
            })
    }

    /// Segmented choice buttons.
    fn segmented<T: Clone + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: Vec<(T, &'static str)>,
        current: T,
        on_pick: impl Fn(&mut AppState, T, &mut Context<AppState>) + Clone + 'static,
    ) -> Div {
        h_flex()
            .flex_wrap()
            .gap_1()
            .children(options.into_iter().enumerate().map(|(i, (v, label))| {
                let app = self.app.clone();
                let pick = on_pick.clone();
                let active = v == current;
                let b = Button::new((id, i)).small().label(label);
                let b = if active { b.primary() } else { b.outline() };
                b.on_click(move |_, _, cx| {
                    let v = v.clone();
                    let pick = pick.clone();
                    app.update(cx, |a, cx| pick(a, v, cx));
                })
            }))
    }

    /// Interface language section. Each language is named in its own script so a user who
    /// landed in the wrong one can still find theirs.
    fn general(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let mut options: Vec<(Option<mm_i18n::Lang>, &'static str)> =
            vec![(None, tr!("跟随系统", "System default"))];
        options.extend(mm_i18n::Lang::ALL.map(|l| (Some(l), l.native_name())));
        ui::card(cx)
            .child(ui::section_title(tr!("通用", "General")))
            .child(Self::row(
                tr!("界面语言", "Language"),
                Some(
                    tr!(
                        "也用于写入 Obsidian 笔记的标题和提示。纪要正文的语言在下方“纪要”里设置。",
                        "Also used for headings and hints the app writes into Obsidian notes. The minutes language is set under Minutes below."
                    )
                    .into(),
                ),
                self.segmented("ui-lang", options, cfg.ui_language, |a, v, cx| {
                    a.update_config(cx, |c| c.ui_language = v)
                }),
                cx,
            ))
    }

    /// Obsidian section.
    fn obsidian(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.clone();
        let vault_input = self.vault.clone();
        ui::card(cx)
            .child(ui::section_title(tr!("Obsidian 库", "Obsidian vault")))
            .child(Self::row(
                tr!("库文件夹", "Vault folder"),
                Some(
                    tr!(
                        "会议笔记、纪要和截图都写进这个库。",
                        "Meeting notes, minutes and screenshots are written to this vault."
                    )
                    .into(),
                ),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.vault).id("settings-vault")))
                    .child(
                        Button::new("discover")
                            .outline()
                            .label(tr!("自动发现", "Find automatically"))
                            .on_click(cx.listener(move |_, _, window, cx| {
                                let mut probe = AppConfig::default();
                                if probe.adopt_obsidian_vault() {
                                    let p = probe.vault_path.clone();
                                    vault_input
                                        .update(cx, |s, cx| s.set_value(path_text(&p), window, cx));
                                    app.update(cx, |a, cx| {
                                        a.update_config(cx, |c| c.vault_path = p.clone());
                                        a.notify(
                                            NoticeKind::Success,
                                            trf!(
                                                "已使用 Obsidian 库：{}",
                                                "Using Obsidian vault: {}",
                                                path_text(&p)
                                            ),
                                            cx,
                                        );
                                    });
                                } else {
                                    app.update(cx, |a, cx| {
                                        a.notify(
                                            NoticeKind::Warning,
                                            tr!(
                                                "没有找到 Obsidian 登记的库，请手动填写文件夹。",
                                                "No vault registered with Obsidian was found. Enter the folder manually."
                                            ),
                                            cx,
                                        )
                                    });
                                }
                            })),
                    ),
                cx,
            ))
            .child(Self::row(
                tr!("会议笔记文件夹", "Meeting notes folder"),
                Some(tr!("库里的相对路径", "Path relative to the vault").into()),
                Input::new(&self.folder).id("settings-folder"),
                cx,
            ))
            .child(ui::muted(
                match &cfg.vault_path {
                    Some(v) => trf!("当前：{}", "Current: {}", v.display()),
                    None => tr!(
                        "还没有设置库：笔记不会写入文件。",
                        "No vault set yet, so notes aren’t written to files."
                    )
                    .into(),
                },
                cx,
            ))
    }

    /// Capture and caption section.
    fn capture(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = cfg.caption_language.clone();
        let (
            recording,
            codex_line,
            codex_models,
            openai_key_present,
            gemini_key_present,
            openai_asr_models,
            gemini_asr_models,
        ) = {
            let app = self.app.read(cx);
            (
                app.live.is_some() || app.live_starting,
                Self::codex_status_line(app),
                app.codex_models.clone(),
                app.openai_key_present,
                app.gemini_key_present,
                app.openai_asr_models.clone(),
                app.gemini_asr_models.clone(),
            )
        };
        let a_mics = self.app.clone();
        let a_src = self.app.clone();
        let a_codex = self.app.clone();
        ui::card(cx)
            .child(ui::section_title(tr!("录制与字幕", "Recording and captions")))
            .when(recording, |el| {
                el.child(ui::muted(
                    tr!(
                        "正在记录：这里的修改从下一场记录开始生效。",
                        "Recording now: changes here apply from the next recording."
                    ),
                    cx,
                ))
            })
            .child(Self::row(
                tr!("麦克风", "Microphone"),
                Some(
                    tr!(
                        "两个及以上声道的实体麦克风可以判断方位。",
                        "Physical microphones with two or more channels can tell direction."
                    )
                    .into(),
                ),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(Select::new(&self.mics).small()))
                    .child(
                        Button::new("refresh-mics")
                            .small()
                            .outline()
                            .icon(IconName::RefreshCw)
                            .tooltip(tr!("重新读取麦克风", "Reload microphones"))
                            .on_click(move |_, _, cx| a_mics.update(cx, |a, cx| a.refresh_inputs(cx))),
                    ),
                cx,
            ))
            .child(Self::row(
                tr!("录制窗口", "Window"),
                Some(
                    tr!(
                        "只对下一场记录有效，不会保存。需要屏幕录制权限。",
                        "Applies to the next recording only and isn’t saved. Needs screen recording permission."
                    )
                    .into(),
                ),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(Select::new(&self.windows).small()))
                    .child(
                        Button::new("refresh-windows")
                            .small()
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label(tr!("读取窗口", "Load windows"))
                            .on_click(move |_, _, cx| a_src.update(cx, |a, cx| a.refresh_sources(cx))),
                    ),
                cx,
            ))
            .child(Self::row(
                tr!("方位", "Direction"),
                Some(
                    tr!(
                        "麦克风有两个声道时，会议中会标出每个人偏左、居中还是偏右。",
                        "With a two-channel microphone, meetings show whether each person is left, center or right."
                    )
                    .into(),
                ),
                self.switch(
                    "s-flip",
                    tr!("左右翻转", "Swap left and right"),
                    cfg.spatial_flip,
                    |c, v| c.spatial_flip = v,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("麦克风间距（厘米）", "Mic spacing (cm)"),
                Some(
                    tr!(
                        "选填。填了能算出实际角度，不填也能判断左右。",
                        "Optional. With it the app can compute real angles; without it, left and right still work."
                    )
                    .into(),
                ),
                Input::new(&self.spacing).small(),
                cx,
            ))
            .child(Self::row(
                "",
                None,
                self.switch(
                    "s-sys",
                    tr!("录系统声音（远程参会者）", "Record system audio (remote participants)"),
                    cfg.system_audio,
                    |c, v| c.system_audio = v,
                ),
                cx,
            ))
            .child(Self::row(
                "",
                None,
                self.switch(
                    "s-screen",
                    tr!("录制会议窗口画面", "Record the meeting window"),
                    cfg.record_screen,
                    |c, v| c.record_screen = v,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("字幕语言", "Caption language"),
                None,
                self.segmented(
                    "cap-lang",
                    logic::caption_languages()
                        .iter()
                        .map(|(v, l)| (v.map(String::from), *l))
                        .collect(),
                    lang,
                    |a, v, cx| a.update_config(cx, |c| c.caption_language = v),
                ),
                cx,
            ))
            .child(Self::row(
                "",
                None,
                self.switch(
                    "s-float",
                    tr!("会议中显示浮窗字幕", "Show floating captions during meetings"),
                    cfg.floating_captions,
                    |c, v| c.floating_captions = v,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("实时字幕方式", "Live captions with"),
                Some(tr!(
                    "本地：Qwen3-ASR 1.7B 在本机运行。OpenAI / Gemini：通过云端转写（可选低延迟 VAD 门控实时流，或本地草稿 + 断句按需上传），仍在本机做 VAD 断句、声纹、方位与说话人区分；断网或缺少密钥时自动回退到本地 Qwen3-ASR。",
                    "Local: Qwen3-ASR 1.7B runs on this Mac. OpenAI / Gemini: cloud transcription with either VAD-gated realtime streaming or low-cost per-utterance HTTP upload, keeping local VAD, voiceprints, spatial cues, and diarization; falls back to local Qwen3-ASR if offline or unconfigured."
                ).into()),
                self.segmented(
                    "live-asr-backend",
                    vec![
                        (LiveAsrBackend::Local, logic::live_asr_backend_label(LiveAsrBackend::Local)),
                        (LiveAsrBackend::OpenAi, logic::live_asr_backend_label(LiveAsrBackend::OpenAi)),
                        (LiveAsrBackend::Gemini, logic::live_asr_backend_label(LiveAsrBackend::Gemini)),
                    ],
                    cfg.live_asr_backend,
                    |a, v, cx| {
                        a.update_config(cx, |c| c.live_asr_backend = v);
                        if matches!(v, LiveAsrBackend::OpenAi | LiveAsrBackend::Gemini) {
                            a.ensure_model_lists(cx);
                        }
                    },
                ),
                cx,
            ))
            .when(cfg.live_asr_backend != LiveAsrBackend::Local, |el| {
                el.child(Self::row(
                    tr!("云端上传策略", "Cloud upload mode"),
                    Some(tr!(
                        "实时流式：保持按需 WebSocket，本地 Silero VAD + RMS 门控跳过静音与短噪点，空闲自动断开，延迟最低。按句上传：本地 Silero VAD 断句（中文可配本机 CPU Zipformer 即时草稿），仅在每句话结束时通过 REST 上传，费用低约 4–6 倍且无常驻长连接。",
                        "Realtime stream: VAD + RMS gated WebSocket with lazy connect and idle auto-close for lowest-latency partials. Per-utterance: local Silero VAD (+ CPU Chinese Zipformer draft) uploads only completed speech via REST (~4–6x lower cost, no persistent WebSockets)."
                    ).into()),
                    self.segmented(
                        "live-cloud-mode",
                        vec![
                            (
                                CloudLiveMode::Streaming,
                                logic::cloud_live_mode_label(CloudLiveMode::Streaming),
                            ),
                            (
                                CloudLiveMode::Utterance,
                                logic::cloud_live_mode_label(CloudLiveMode::Utterance),
                            ),
                        ],
                        cfg.live_cloud_mode,
                        |a, v, cx| a.update_config(cx, |c| c.live_cloud_mode = v),
                    ),
                    cx,
                ))
            })
            .when(
                cfg.live_asr_backend == LiveAsrBackend::OpenAi
                    && cfg.live_cloud_mode == CloudLiveMode::Streaming,
                |el| {
                    el.child(self.cloud_asr_model_row(
                        AsrModelPicker::LiveOpenAi,
                        AsrProvider::OpenAi,
                        AsrMode::Live,
                        logic::effective_live_model(cfg),
                        openai_key_present,
                        &openai_asr_models,
                        "live-openai",
                        &self.live_openai_model,
                        cx,
                    ))
                },
            )
            .when(
                cfg.live_asr_backend == LiveAsrBackend::OpenAi
                    && cfg.live_cloud_mode == CloudLiveMode::Utterance,
                |el| {
                    el.child(self.cloud_asr_model_row(
                        AsrModelPicker::LiveOpenAiUtterance,
                        AsrProvider::OpenAi,
                        AsrMode::Refine,
                        logic::effective_live_model(cfg),
                        openai_key_present,
                        &openai_asr_models,
                        "live-openai",
                        &self.live_openai_utterance_model,
                        cx,
                    ))
                },
            )
            .when(
                cfg.live_asr_backend == LiveAsrBackend::Gemini
                    && cfg.live_cloud_mode == CloudLiveMode::Streaming,
                |el| {
                    el.child(self.cloud_asr_model_row(
                        AsrModelPicker::LiveGemini,
                        AsrProvider::Gemini,
                        AsrMode::Live,
                        logic::effective_live_model(cfg),
                        gemini_key_present,
                        &gemini_asr_models,
                        "live-gemini",
                        &self.live_gemini_model,
                        cx,
                    ))
                },
            )
            .when(
                cfg.live_asr_backend == LiveAsrBackend::Gemini
                    && cfg.live_cloud_mode == CloudLiveMode::Utterance,
                |el| {
                    el.child(self.cloud_asr_model_row(
                        AsrModelPicker::LiveGeminiUtterance,
                        AsrProvider::Gemini,
                        AsrMode::Refine,
                        logic::effective_live_model(cfg),
                        gemini_key_present,
                        &gemini_asr_models,
                        "live-gemini",
                        &self.live_gemini_utterance_model,
                        cx,
                    ))
                },
            )
            .child(Self::row(
                "",
                None,
                self.switch(
                    "s-refine",
                    tr!(
                        "会后用大模型精转写录音",
                        "Re-transcribe the recording with a larger model after the meeting"
                    ),
                    cfg.refine_after_meeting,
                    |c, v| c.refine_after_meeting = v,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("精转方式", "Refine with"),
                Some(tr!(
                    "本地：Qwen3-ASR 在本机重新转写。Codex 一体化：直接把录音路径交给本机 Codex，在一次运行里完成转写、区分说话人和制作会议纪要。OpenAI / Gemini：分段上传音频到云端转写；任何云端或 Codex 路线失败时都自动回退到本地精转。",
                    "Local: Qwen3-ASR re-transcribes on this Mac. Codex (transcribe + minutes): passes recordings to local Codex in one pass. OpenAI / Gemini: uploads speech chunks to cloud APIs; any cloud or Codex failure falls back to local Qwen3-ASR."
                ).into()),
                self.segmented(
                    "refine-backend",
                    vec![
                        (RefineBackend::Local, logic::refine_backend_label(RefineBackend::Local)),
                        (RefineBackend::Codex, logic::refine_backend_label(RefineBackend::Codex)),
                        (RefineBackend::OpenAi, logic::refine_backend_label(RefineBackend::OpenAi)),
                        (RefineBackend::Gemini, logic::refine_backend_label(RefineBackend::Gemini)),
                    ],
                    cfg.refine_backend,
                    |a, v, cx| {
                        a.update_config(cx, |c| c.refine_backend = v);
                        if matches!(v, RefineBackend::OpenAi | RefineBackend::Gemini) {
                            a.ensure_model_lists(cx);
                        }
                    },
                ),
                cx,
            ))
            .when(cfg.refine_backend == RefineBackend::Local, |el| {
                el.child(Self::row(
                    tr!("精转模型", "Refine model"),
                    Some(tr!(
                        "本地精转可填更大的 Qwen3-ASR 模型：Hugging Face 仓库名或本地文件夹，名字里要带 0.6B 或 1.7B。留空用默认。",
                        "Enter a larger local Qwen3-ASR model: a Hugging Face repo or local folder containing 0.6B or 1.7B. Leave blank for the default."
                    ).into()),
                    Input::new(&self.refine_model)
                        .id("settings-refine-model")
                        .small(),
                    cx,
                ))
            })
            .when(cfg.refine_backend == RefineBackend::OpenAi, |el| {
                el.child(self.cloud_asr_model_row(
                    AsrModelPicker::RefineOpenAi,
                    AsrProvider::OpenAi,
                    AsrMode::Refine,
                    logic::effective_refine_model(cfg),
                    openai_key_present,
                    &openai_asr_models,
                    "refine-openai",
                    &self.refine_openai_model,
                    cx,
                ))
            })
            .when(cfg.refine_backend == RefineBackend::Gemini, |el| {
                el.child(self.cloud_asr_model_row(
                    AsrModelPicker::RefineGemini,
                    AsrProvider::Gemini,
                    AsrMode::Refine,
                    logic::effective_refine_model(cfg),
                    gemini_key_present,
                    &gemini_asr_models,
                    "refine-gemini",
                    &self.refine_gemini_model,
                    cx,
                ))
            })
            .when(cfg.refine_backend == RefineBackend::Codex, |el| {
                let picker = self.model_picker(ModelPicker::CodexRefine, &codex_models, cx);
                el.child(Self::row(
                    tr!("Codex 精转模型", "Codex refine model"),
                    Some(tr!(
                        "向 Codex 传入音频路径，由它自主调用转写工具、区分说话人并生成会议纪要。",
                        "Gives Codex the recording path to transcribe, separate speakers, and write minutes."
                    ).into()),
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(ui::muted(codex_line, cx))
                                .child(
                                    Button::new("codex-refine-refresh")
                                        .small()
                                        .outline()
                                        .icon(IconName::RefreshCw)
                                        .label(tr!("重新检测", "Check again"))
                                        .on_click(move |_, _, cx| {
                                            a_codex.update(cx, |a, cx| a.refresh_codex(cx))
                                        }),
                                ),
                        )
                        .child(picker),
                    cx,
                ))
            })
    }

    /// Cloud transcription model picker row (dynamic Select + refresh + editable field + rate hint).
    #[allow(clippy::too_many_arguments)]
    fn cloud_asr_model_row(
        &self,
        picker: AsrModelPicker,
        provider: AsrProvider,
        mode: AsrMode,
        active_model: String,
        key_present: bool,
        models_state: &Loadable<AsrModelList>,
        prefix: &'static str,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Div {
        let is_live_utterance = matches!(
            picker,
            AsrModelPicker::LiveOpenAiUtterance | AsrModelPicker::LiveGeminiUtterance
        );
        let label = match picker {
            AsrModelPicker::LiveOpenAi
            | AsrModelPicker::LiveGemini
            | AsrModelPicker::LiveOpenAiUtterance
            | AsrModelPicker::LiveGeminiUtterance => tr!("实时字幕模型", "Live caption model"),
            AsrModelPicker::RefineOpenAi | AsrModelPicker::RefineGemini => {
                tr!("精转模型", "Refine model")
            }
        };
        let hint = if is_live_utterance {
            match provider {
                AsrProvider::OpenAi => tr!(
                    "通过 OpenAI 转写接口按完成语句上传（16 kHz WAV），中文可选本机 CPU Zipformer 即时草稿；失败时自动回退到本地 Qwen3-ASR。",
                    "Uploads completed speech utterances as 16 kHz WAV via OpenAI audio transcriptions, with optional local CPU Zipformer draft for Mandarin; falls back locally on error."
                ),
                AsrProvider::Gemini => tr!(
                    "通过 Gemini REST 接口按完成语句上传（16 kHz WAV），中文可选本机 CPU Zipformer 即时草稿；失败时自动回退到本地 Qwen3-ASR。",
                    "Uploads completed speech utterances as 16 kHz WAV via Gemini REST, with optional local CPU Zipformer draft for Mandarin; falls back locally on error."
                ),
            }
        } else {
            match (provider, mode) {
                (AsrProvider::OpenAi, AsrMode::Live) => tr!(
                    "通过 OpenAI Realtime 转写（24 kHz PCM16），本地 VAD + RMS 门控按需连接并跳过尾音静音；失败时自动回退到本地 Qwen3-ASR。",
                    "Uses OpenAI Realtime transcription (24 kHz PCM16) with local VAD + RMS gating, lazy connect, and trailing-silence freeze; falls back locally on error."
                ),
                (AsrProvider::Gemini, AsrMode::Live) => tr!(
                    "通过 Gemini Live API 实时转写（16 kHz PCM16），本地 VAD + RMS 门控按需连接并跳过尾音静音；失败时自动回退到本地 Qwen3-ASR。",
                    "Uses Gemini Live API transcription (16 kHz PCM16) with local VAD + RMS gating, lazy connect, and trailing-silence freeze; falls back locally on error."
                ),
                (_, AsrMode::Refine) => tr!(
                    "会后按录音切片上传转写；失败时自动回退到本地 Qwen3-ASR。配置密钥后自动拉取可用模型列表，也可点刷新或手动输入模型 ID。",
                    "Fetches available models automatically once an API key is saved; refresh anytime or enter any compatible model ID. Falls back locally on error."
                ),
            }
        };
        let rate_hint = logic::model_rate_hint(provider, mode, &active_model);
        let provider_name = match provider {
            AsrProvider::OpenAi => "OpenAI",
            AsrProvider::Gemini => "Google Gemini",
        };
        let (list_status_text, list_status_warn): (SharedString, bool) = if !key_present {
            let msg = match provider {
                AsrProvider::OpenAi => tr!(
                    "尚未保存 OpenAI API 密钥：在下方保存后会自动拉取可用模型列表，也可先手动输入模型 ID。",
                    "No OpenAI API key saved yet: add one below to auto-fetch available models, or enter a model ID manually."
                ),
                AsrProvider::Gemini => tr!(
                    "尚未保存 Gemini API 密钥：在下方保存后会自动拉取可用模型列表，也可先手动输入模型 ID。",
                    "No Gemini API key saved yet: add one below to auto-fetch available models, or enter a model ID manually."
                ),
            };
            (msg.into(), true)
        } else {
            match models_state {
                Loadable::Loading => (
                    match provider {
                        AsrProvider::OpenAi => tr!(
                            "正在从 OpenAI 拉取可用模型列表…",
                            "Fetching available models from OpenAI…"
                        ),
                        AsrProvider::Gemini => tr!(
                            "正在从 Google Gemini 拉取可用模型列表…",
                            "Fetching available models from Google Gemini…"
                        ),
                    }
                    .into(),
                    false,
                ),
                Loadable::Ready(list) => {
                    let count = list.for_mode(mode).len();
                    if count > 0 {
                        (
                            trf!(
                                "已从 {provider_name} 拉取 {count} 个可用模型",
                                "Loaded {count} models from {provider_name}"
                            )
                            .into(),
                            false,
                        )
                    } else {
                        (
                            trf!(
                                "未在 {provider_name} 账号中匹配到可用转写模型，可在上方手动输入模型 ID。",
                                "No matching transcription models found in your {provider_name} account; enter a model ID manually above."
                            )
                            .into(),
                            true,
                        )
                    }
                }
                Loadable::Failed(e) => (
                    trf!(
                        "拉取模型列表失败：{}。可点“刷新列表”重试，或在上方手动输入模型 ID。",
                        "Couldn’t fetch model list: {}. Click Refresh list to retry, or enter a model ID above.",
                        e.message
                    )
                    .into(),
                    true,
                ),
                Loadable::Idle => (
                    tr!(
                        "点“刷新列表”从服务商拉取可用模型，或手动输入模型 ID。",
                        "Click Refresh list to fetch models from the provider, or enter a model ID."
                    )
                    .into(),
                    false,
                ),
            }
        };
        let app = self.app.clone();
        let loading = matches!(models_state, Loadable::Loading);
        Self::row(
            label,
            Some(hint.into()),
            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .id(SharedString::from(format!("{prefix}-select")))
                                .test_support()
                                .flex_1()
                                .min_w_0()
                                .child(Select::new(&self.asr_sels[picker as usize]).small()),
                        )
                        .child(
                            Button::new(SharedString::from(format!("{prefix}-refresh")))
                                .small()
                                .outline()
                                .icon(IconName::RefreshCw)
                                .disabled(!key_present || loading)
                                .label(tr!("刷新列表", "Refresh list"))
                                .on_click(move |_, _, cx| {
                                    app.update(cx, |a, cx| a.refresh_asr_models(provider, cx))
                                }),
                        ),
                )
                .child(
                    Input::new(input)
                        .id(SharedString::from(format!("{prefix}-input")))
                        .small(),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(if list_status_warn {
                            cx.theme().warning
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(list_status_text.clone().test_text(list_status_text)),
                )
                .child(ui::muted(rate_hint, cx)),
            cx,
        )
    }

    /// Keychain input + Save/Delete buttons + status line for one cloud ASR account.
    fn asr_key_control(
        &self,
        id_prefix: &'static str,
        input: &Entity<InputState>,
        account: &'static str,
        status_line: String,
        cx: &App,
    ) -> Div {
        let app_save = self.app.clone();
        let app_del = self.app.clone();
        let in_save = input.clone();
        let in_del = input.clone();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(input).mask_toggle()))
                    .child(
                        Button::new(SharedString::from(format!("{id_prefix}-save")))
                            .outline()
                            .label(tr!("保存密钥", "Save key"))
                            .on_click(move |_, window, cx| {
                                let k = in_save.read(cx).value().trim().to_string();
                                if k.is_empty() {
                                    return;
                                }
                                app_save.update(cx, |a, cx| a.write_asr_key(account, Some(k), cx));
                                in_save.update(cx, |s, cx| s.set_value("", window, cx));
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id_prefix}-del")))
                            .ghost()
                            .label(tr!("删除密钥", "Delete key"))
                            .on_click(move |_, window, cx| {
                                in_del.update(cx, |s, cx| s.set_value("", window, cx));
                                app_del.update(cx, |a, cx| a.write_asr_key(account, None, cx));
                            }),
                    ),
            )
            .child(ui::muted(status_line, cx))
    }

    /// One provider's local cumulative spend summary (this calendar month + all-time total).
    fn asr_spend_row(
        &self,
        id_prefix: &'static str,
        title: &'static str,
        provider_id: &'static str,
        summary: ProviderUsageSummary,
        cx: &App,
    ) -> Div {
        let month_dur = logic::format_duration_minutes(summary.month.audio_ms);
        let month_usd = logic::format_usd(summary.month.cost_usd);
        let month_calls = summary.month.calls;
        let total_dur = logic::format_duration_minutes(summary.total.audio_ms);
        let total_usd = logic::format_usd(summary.total.cost_usd);
        let total_calls = summary.total.calls;
        let app = self.app.clone();
        h_flex()
            .gap_3()
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .gap(px(2.))
                    .child(div().text_sm().child(SharedString::from(title).test_text(title)))
                    .child(ui::muted(
                        trf!(
                            "本月：{month_dur} · 约 {month_usd}（{month_calls} 次）    累计：{total_dur} · 约 {total_usd}（{total_calls} 次）",
                            "This month: {month_dur} · ~{month_usd} ({month_calls} calls)    All-time: {total_dur} · ~{total_usd} ({total_calls} calls)"
                        ),
                        cx,
                    )),
            )
            .child(
                Button::new(SharedString::from(format!("{id_prefix}-reset")))
                    .small()
                    .ghost()
                    .label(tr!("重置", "Reset"))
                    .on_click(move |_, _, cx| {
                        app.update(cx, |a, cx| a.clear_asr_usage(Some(provider_id), cx));
                    }),
            )
    }

    /// Cloud ASR Keychain keys, local spend accumulator, and optional OpenAI org spend sync.
    fn cloud_asr(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (
            openai_key_present,
            gemini_key_present,
            openai_admin_key_present,
            openai_asr_models,
            gemini_asr_models,
            openai_usage,
            gemini_usage,
            openai_org_costs,
        ) = {
            let app = self.app.read(cx);
            (
                app.openai_key_present,
                app.gemini_key_present,
                app.openai_admin_key_present,
                app.openai_asr_models.clone(),
                app.gemini_asr_models.clone(),
                app.openai_usage,
                app.gemini_usage,
                app.openai_org_costs.clone(),
            )
        };
        let key_status = |present: bool| {
            if present {
                tr!(
                    "钥匙串里已有密钥（不会显示）。",
                    "A key is saved in Keychain (hidden)."
                )
                .to_string()
            } else {
                tr!("还没有保存密钥。", "No key saved yet.").to_string()
            }
        };
        let asr_key_status = |present: bool, models: &Loadable<AsrModelList>| {
            if !present {
                return tr!("还没有保存密钥。", "No key saved yet.").to_string();
            }
            let base = key_status(true);
            let suffix = match models {
                Loadable::Idle => String::new(),
                Loadable::Loading => tr!(
                    " · 正在拉取可用转写模型…",
                    " · Fetching transcription models…"
                )
                .to_string(),
                Loadable::Ready(list) => {
                    let live_n = list.live.len();
                    let refine_n = list.refine.len();
                    trf!(
                        " · 已拉取 {live_n} 个实时 / {refine_n} 个精转模型",
                        " · Loaded {live_n} live / {refine_n} refine models"
                    )
                }
                Loadable::Failed(_) => {
                    tr!(" · 拉取模型列表失败", " · Failed to fetch models").to_string()
                }
            };
            format!("{base}{suffix}")
        };
        let org_costs_line = match &openai_org_costs {
            Loadable::Idle => {
                if openai_admin_key_present {
                    tr!(
                        "已保存 Admin 密钥，点击右侧“同步本月组织花费”向 OpenAI 官方拉取实时账单。",
                        "Admin key saved. Click “Sync org spend” to fetch this month’s OpenAI billing total."
                    )
                    .to_string()
                } else {
                    tr!(
                        "可选：仅用于查询 OpenAI 组织账单（/v1/organization/costs）；普通转写只需上方的 OpenAI API 密钥。",
                        "Optional: only used to query OpenAI org billing (/v1/organization/costs); standard transcription only needs the OpenAI API key above."
                    )
                    .to_string()
                }
            }
            Loadable::Loading => tr!(
                "正在查询 OpenAI 本月组织花费…",
                "Querying OpenAI org spend…"
            )
            .to_string(),
            Loadable::Ready(r) => trf!(
                "OpenAI 组织本月已出账：{}（含各 API 项目）",
                "OpenAI org billed this month: {} (across API projects)",
                logic::format_usd(r.total_usd)
            ),
            Loadable::Failed(e) => trf!("查询失败：{}", "Sync failed: {}", e.message),
        };
        let syncing_costs = matches!(openai_org_costs, Loadable::Loading);
        let a_costs = self.app.clone();

        ui::card(cx)
            .child(ui::section_title(tr!(
                "云端转写密钥与花费累计",
                "Cloud transcription keys & spend"
            )))
            .child(Self::row(
                "OpenAI API Key",
                Some(
                    tr!(
                        "独立保存在 macOS 钥匙串；若纪要也使用 OpenAI 则自动共用。",
                        "Stored in the macOS Keychain independently of Minutes (shared if Minutes also uses OpenAI)."
                    )
                    .into(),
                ),
                self.asr_key_control(
                    "asr-openai-key",
                    &self.asr_openai_key,
                    byok::OPENAI_KEY_ACCOUNT,
                    asr_key_status(openai_key_present, &openai_asr_models),
                    cx,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("OpenAI Admin 密钥（可选）", "OpenAI Admin key (optional)"),
                Some(
                    tr!(
                        "仅组织 Admin 密钥（sk-admin-…）可调用 OpenAI Costs API。",
                        "Only an Organization Admin key (sk-admin-…) can query the OpenAI Costs API."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_2()
                    .child(self.asr_key_control(
                        "asr-openai-admin-key",
                        &self.asr_openai_admin_key,
                        byok::OPENAI_ADMIN_KEY_ACCOUNT,
                        key_status(openai_admin_key_present),
                        cx,
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("sync-openai-costs")
                                    .small()
                                    .outline()
                                    .icon(IconName::RefreshCw)
                                    .disabled(syncing_costs)
                                    .label(tr!("同步本月组织花费", "Sync org spend"))
                                    .on_click(move |_, _, cx| {
                                        a_costs.update(cx, |a, cx| a.sync_openai_costs(cx));
                                    }),
                            )
                            .child(div().flex_1().child(ui::muted(org_costs_line, cx))),
                    ),
                cx,
            ))
            .child(Self::row(
                "Google Gemini API Key",
                Some(
                    tr!(
                        "独立保存在 macOS 钥匙串；可与上方的 Claude / OpenAI 纪要同时使用。",
                        "Stored in the macOS Keychain independently; works alongside Claude or OpenAI minutes."
                    )
                    .into(),
                ),
                self.asr_key_control(
                    "asr-gemini-key",
                    &self.asr_gemini_key,
                    byok::GEMINI_KEY_ACCOUNT,
                    asr_key_status(gemini_key_present, &gemini_asr_models),
                    cx,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("本机转写花费累计", "Local transcription spend"),
                Some(
                    tr!(
                        "按本机实际送往云端的音频时长、token 与模型官方单价估算累计，不做手动预算倒推。",
                        "Accumulated locally from sent audio duration, tokens, and model rates."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_3()
                    .child(self.asr_spend_row(
                        "spend-openai",
                        "OpenAI",
                        AsrProvider::OpenAi.as_str(),
                        openai_usage,
                        cx,
                    ))
                    .child(self.asr_spend_row(
                        "spend-gemini",
                        "Google Gemini",
                        AsrProvider::Gemini.as_str(),
                        gemini_usage,
                        cx,
                    ))
                    .child(ui::muted(
                        tr!(
                            "说明：Google Gemini Developer API 不提供余额/账单查询接口，因此仅统计本机累计用量与参考费率；OpenAI 若有权限可在上方通过 Admin 密钥同步官方月账单。",
                            "Note: Google’s Gemini Developer API provides no balance or billing endpoint, so Gemini shows local cumulative usage and rate estimates; OpenAI supports official org spend sync with an optional Admin key."
                        ),
                        cx,
                    )),
                cx,
            ))
    }

    /// Minutes provider section: the backend pick, then the settings of the backends it may use.
    fn minutes(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let backend = cfg.minutes_backend;
        let app = self.app.read(cx);
        let codex_line = Self::codex_status_line(app);
        let key_line = if cfg.byok.is_none() {
            tr!(
                "选择提供方并填写模型后可以保存密钥。",
                "Choose a provider and enter a model to save a key."
            )
        } else if app.byok_key_present {
            tr!(
                "钥匙串里已有密钥（不会显示）。",
                "A key is saved in Keychain (hidden)."
            )
        } else {
            tr!("还没有保存密钥。", "No key saved yet.")
        };
        let codex_models = app.codex_models.clone();
        let unified_codex_note = (cfg.refine_after_meeting && cfg.refine_backend == RefineBackend::Codex)
            .then(|| {
                tr!(
                    "已开启 Codex 一体化精转：会后将在精转写阶段一并生成纪要；这里的设置用于手动重新生成纪要或回退。",
                    "Codex unified refine is active: post-meeting minutes are generated during the refine pass; this setting is used for manual regeneration or fallback."
                )
            });
        let current_kind = self.byok_kind;
        let a1 = self.app.clone();
        let a2 = self.app.clone();
        let key = self.api_key.clone();
        let key2 = self.api_key.clone();
        let minutes_lang = cfg.minutes_language.clone();

        let codex_row =
            matches!(backend, MinutesBackend::Auto | MinutesBackend::Codex).then(|| {
                let picker = self.model_picker(ModelPicker::CodexMinutes, &codex_models, cx);
                Self::row(
                tr!("Codex（ChatGPT 账号）", "Codex (ChatGPT account)"),
                Some(
                    tr!(
                        "用本机 Codex 登录的 ChatGPT 账号写纪要。",
                        "Writes minutes with the ChatGPT account signed in to Codex on this Mac."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_2()
                    .when(backend == MinutesBackend::Auto, |el| {
                        el.child(self.switch(
                            "s-codex",
                            tr!(
                                "没有 API 密钥时使用 Codex",
                                "Use Codex when there is no API key"
                            ),
                            cfg.use_codex,
                            |c, v| c.use_codex = v,
                        ))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(ui::muted(codex_line, cx))
                            .child(
                                Button::new("codex-refresh")
                                    .small()
                                    .outline()
                                    .icon(IconName::RefreshCw)
                                    .label(tr!("重新检测", "Check again"))
                                    .on_click(move |_, _, cx| {
                                        a1.update(cx, |a, cx| a.refresh_codex(cx))
                                    }),
                            ),
                    )
                    .child(picker),
                cx,
            )
            });

        let key_row = matches!(backend, MinutesBackend::Auto | MinutesBackend::ApiKey).then(|| {
            Self::row(
                tr!("API 密钥", "API key"),
                Some(
                    tr!(
                        "密钥只保存在 macOS 钥匙串。",
                        "Keys are stored only in the macOS Keychain."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .children(logic::byok_kinds().into_iter().enumerate().map(
                                |(i, (k, label))| {
                                    let b = Button::new(("byok", i)).small().label(label);
                                    let b = if k == current_kind {
                                        b.primary()
                                    } else {
                                        b.outline()
                                    };
                                    b.on_click(cx.listener(move |this, _, _, cx| {
                                        this.byok_kind = k;
                                        let model = this.model.read(cx).value().to_string();
                                        this.commit(Field::Model, model, cx);
                                        cx.notify();
                                    }))
                                },
                            )),
                    )
                    .when(!current_kind.is_empty(), |el| {
                        el.child(Input::new(&self.model).id("settings-byok-model"))
                            .when(current_kind == "openai_compatible", |el| {
                                el.child(Input::new(&self.base_url).id("settings-byok-base"))
                            })
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .child(Input::new(&self.api_key).mask_toggle()),
                                    )
                                    .child(
                                        Button::new("key-save")
                                            .outline()
                                            .label(tr!("保存密钥", "Save key"))
                                            .on_click(cx.listener(move |_, _, window, cx| {
                                                let k = key.read(cx).value().trim().to_string();
                                                if k.is_empty() {
                                                    return;
                                                }
                                                a2.update(cx, |a, cx| {
                                                    a.write_byok_key(Some(k), cx)
                                                });
                                                key.update(cx, |s, cx| s.set_value("", window, cx));
                                            })),
                                    )
                                    .child(
                                        Button::new("key-del")
                                            .ghost()
                                            .label(tr!("删除密钥", "Delete key"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                key2.update(cx, |s, cx| {
                                                    s.set_value("", window, cx)
                                                });
                                                this.app
                                                    .update(cx, |a, cx| a.write_byok_key(None, cx));
                                            })),
                                    ),
                            )
                            .child(ui::muted(key_line, cx))
                    }),
                cx,
            )
        });

        ui::card(cx)
            .child(ui::section_title(tr!("纪要", "Minutes")))
            .child(Self::row(
                "",
                None,
                self.switch(
                    "s-auto",
                    tr!(
                        "会议结束后自动生成纪要",
                        "Generate minutes when a meeting ends"
                    ),
                    cfg.auto_minutes,
                    |c, v| c.auto_minutes = v,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("纪要语言", "Minutes language"),
                None,
                self.segmented(
                    "min-lang",
                    logic::minutes_languages()
                        .iter()
                        .map(|(v, l)| (v.to_string(), *l))
                        .collect(),
                    minutes_lang,
                    |a, v, cx| a.update_config(cx, |c| c.minutes_language = v),
                ),
                cx,
            ))
            .child(Self::row(
                tr!("纪要生成方式", "Write minutes with"),
                Some(
                    tr!(
                        "自动：依次尝试 API 密钥、Codex、本地模型。指定某一项时，它用不了或失败会改用自动顺序。",
                        "Automatic tries an API key, then Codex, then the local model. If the one you pick can’t run or fails, the automatic order takes over."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_1()
                    .child(self.segmented(
                        "min-backend",
                        [
                            MinutesBackend::Auto,
                            MinutesBackend::ApiKey,
                            MinutesBackend::Codex,
                        ]
                        .map(|b| (b, logic::minutes_backend_label(b)))
                        .to_vec(),
                        backend,
                        |a, v, cx| a.update_config(cx, |c| c.minutes_backend = v),
                    ))
                    .when_some(unified_codex_note, |el, n| el.child(ui::muted(n, cx))),
                cx,
            ))
            .children(codex_row)
            .children(key_row)
    }

    /// Local models and index section.
    /// macOS privacy grants with one-click requests and a deep link to System Settings.
    fn permissions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let (summary, missing) = match &app.permissions {
            Loadable::Ready(p) => {
                let missing = p.missing();
                let summary = if missing.is_empty() {
                    tr!(
                        "麦克风、日历、屏幕与系统录音都已授权。",
                        "Microphone, calendars, and screen and system audio recording are all allowed."
                    )
                    .to_string()
                } else {
                    trf!(
                        "还没有授权：{}。授权后无需重新构建，签名固定时授权会一直保留。",
                        "Not yet allowed: {}. No rebuild is needed after granting, and access persists while the signature stays the same.",
                        missing
                            .iter()
                            .map(|(_, l)| *l)
                            .collect::<Vec<_>>()
                            .join(tr!("、", ", "))
                    )
                };
                (summary, missing)
            }
            Loadable::Loading => (tr!("正在检查…", "Checking…").to_string(), vec![]),
            Loadable::Idle => (tr!("尚未检查。", "Not checked yet.").to_string(), vec![]),
            Loadable::Failed(e) => (e.message.clone(), vec![]),
        };
        let handle = self.app.clone();
        let mut buttons = h_flex().gap_2().items_center().flex_wrap();
        for (kind, label) in missing {
            let h = handle.clone();
            buttons = buttons.child(
                Button::new(SharedString::from(format!("perm-{kind}")))
                    .small()
                    .outline()
                    .label(trf!("请求{label}", "Request {label}"))
                    .on_click(move |_, _, cx| h.update(cx, |a, cx| a.request_permission(kind, cx))),
            );
        }
        let h = handle.clone();
        buttons = buttons
            .child(
                Button::new("perm-settings")
                    .small()
                    .ghost()
                    .label(tr!("打开系统设置", "Open System Settings"))
                    .on_click(|_, _, cx| {
                        cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
                    }),
            )
            .child(
                Button::new("perm-refresh")
                    .small()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .on_click(move |_, _, cx| h.update(cx, |a, cx| a.refresh_permissions(cx))),
            );
        ui::card(cx)
            .child(ui::section_title(tr!("系统权限", "System permissions")))
            .child(Self::row(
                tr!("授权状态", "Status"),
                Some(
                    tr!(
                        "屏幕与系统录音用于录制会议窗口和远程参会者的声音。",
                        "Screen and system audio recording capture the meeting window and remote participants’ voices."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_2()
                    .child(ui::muted(summary, cx))
                    .child(buttons),
                cx,
            ))
    }

    fn models(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let embed_line = match &app.embed_status {
            Loadable::Ready(s) if s.loaded => trf!(
                "已加载：{}（{}）",
                "Loaded: {} ({})",
                s.directory.clone().unwrap_or_default(),
                s.modalities.join(tr!("、", ", "))
            ),
            Loadable::Ready(_) | Loadable::Idle => tr!(
                "未加载：搜索只用关键词。",
                "Not loaded. Search uses keywords only."
            )
            .to_string(),
            Loadable::Loading => tr!("正在加载…", "Loading…").to_string(),
            Loadable::Failed(e) => e.message.clone(),
        };
        let llm_line = match &app.llm_status {
            Loadable::Ready(s) if s.loaded => {
                trf!(
                    "已加载：{}",
                    "Loaded: {}",
                    s.directory.clone().unwrap_or_default()
                )
            }
            Loadable::Ready(_) | Loadable::Idle => tr!(
                "未加载：没有密钥和 Codex 时，生成纪要前会自动加载。",
                "Not loaded. Without a key or Codex, it loads automatically before generating minutes."
            )
            .to_string(),
            Loadable::Loading => tr!("正在加载…", "Loading…").to_string(),
            Loadable::Failed(e) => e.message.clone(),
        };
        let stats = app
            .index_stats
            .map(|s| {
                let (sources, chunks, embedded) = (s.sources, s.chunks, s.embedded);
                let sources_en = logic::count_en(sources, "source", "sources");
                let chunks_en = logic::count_en(chunks, "chunk", "chunks");
                trf!(
                    "{sources} 个来源，{chunks} 个片段，其中 {embedded} 个已用当前模型嵌入",
                    "{sources_en}, {chunks_en}, {embedded} embedded with the current model"
                )
            })
            .unwrap_or_else(|| tr!("正在统计…", "Counting…").into());
        let engine_line = trf!(
            "{}；{}",
            "{}; {}",
            app.meeting_state
                .describe(tr!("字幕引擎 ", "Caption engine ")),
            app.embed_state
                .describe(tr!("嵌入引擎 ", "Embedding engine "))
        );
        let download_line = app
            .model_download
            .as_ref()
            .map(|d| logic::download_line(&d.repo, &d.progress));
        let downloading = download_line.is_some();
        let token_line = if app.hf_token_present {
            tr!("令牌已保存在钥匙串。", "Token saved in Keychain.")
        } else if std::env::var_os("HF_TOKEN").is_some() {
            tr!(
                "钥匙串里没有令牌，将使用环境变量 HF_TOKEN。",
                "No token in Keychain; the HF_TOKEN environment variable will be used."
            )
        } else {
            tr!(
                "未设置：只能下载公开模型。",
                "Not set. Only public models can be downloaded."
            )
        };
        let (e_in, l_in) = (self.embed_dir.clone(), self.llm_dir.clone());
        let u_in = self.embed_url.clone();
        let (t_in, t_in2) = (self.hf_token.clone(), self.hf_token.clone());
        let (a7, a8, a9) = (self.app.clone(), self.app.clone(), self.app.clone());
        let (a1, a2, a3, a4, a5, a6) = (
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
        );
        ui::card(cx)
            .child(ui::section_title(tr!("本地模型与搜索索引", "Local models and search index")))
            .child(Self::row(
                tr!("嵌入模型", "Embedding model"),
                Some(
                    tr!(
                        "EmbeddingGemma 2，用于语义搜索。",
                        "EmbeddingGemma 2, for semantic search."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_2()
                    .child(Input::new(&self.embed_dir))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("embed-load")
                                    .small()
                                    .outline()
                                    .label(tr!("加载", "Load"))
                                    .on_click(move |_, _, cx| {
                                        let dir = parse_path(&e_in.read(cx).value());
                                        a1.update(cx, |a, cx| match dir {
                                            Some(d) => a.load_embedding(d, cx),
                                            None => a.notify(
                                                NoticeKind::Warning,
                                                tr!(
                                                    "请先填写模型文件夹。",
                                                    "Enter the model folder first."
                                                ),
                                                cx,
                                            ),
                                        });
                                    }),
                            )
                            .child(
                                Button::new("embed-unload")
                                    .small()
                                    .ghost()
                                    .label(tr!("卸载", "Unload"))
                                    .on_click(move |_, _, cx| {
                                        a2.update(cx, |a, cx| a.unload_embedding(cx))
                                    }),
                            ),
                    )
                    .child(ui::muted(embed_line, cx)),
                cx,
            ))
            .child(Self::row(
                tr!("从 Hugging Face 下载", "Download from Hugging Face"),
                Some(trf!(
                    "下载到 {}，完成后自动加载。",
                    "Downloads to {} and loads when finished.",
                    mm_service::models_dir(&app.data_dir).display()
                )),
                v_flex()
                    .gap_2()
                    .child(Input::new(&self.embed_url))
                    .child(
                        h_flex()
                            .gap_2()
                            .when(!downloading, |el| {
                                el.child(
                                    Button::new("embed-download")
                                        .small()
                                        .outline()
                                        .icon(IconName::ArrowDown)
                                        .label(tr!("下载并加载", "Download and load"))
                                        .on_click(move |_, _, cx| {
                                            let url = u_in.read(cx).value().to_string();
                                            a7.update(cx, |a, cx| a.download_embedding(url, cx));
                                        }),
                                )
                            })
                            .when(downloading, |el| {
                                el.child(
                                    Button::new("embed-download-cancel")
                                        .small()
                                        .ghost()
                                        .label(tr!("取消下载", "Cancel download"))
                                        .on_click(move |_, _, cx| {
                                            a8.update(cx, |a, cx| a.cancel_download(cx))
                                        }),
                                )
                            }),
                    )
                    .when_some(download_line, |el, line| el.child(ui::muted(line, cx))),
                cx,
            ))
            .child(Self::row(
                "",
                Some(tr!(
                    "没有填写模型文件夹时生效：本机已有（包括 Hugging Face 缓存）就直接加载，没有就从上面的地址下载。",
                    "Applies when no model folder is set: loads a copy already on this Mac (including the Hugging Face cache), otherwise downloads from the URL above."
                ).into()),
                self.switch(
                    "s-embed-auto",
                    tr!("启动时自动准备嵌入模型", "Prepare the embedding model at launch"),
                    app.cfg.auto_download_embedding,
                    |c, v| c.auto_download_embedding = v,
                ),
                cx,
            ))
            .child(Self::row(
                tr!("Hugging Face 令牌", "Hugging Face token"),
                Some(
                    tr!(
                        "下载需要授权的模型时使用，只保存在 macOS 钥匙串。",
                        "Used to download gated models. Stored only in the macOS Keychain."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.hf_token).mask_toggle()),
                            )
                            .child(
                                Button::new("hf-token-save")
                                    .outline()
                                    .label(tr!("保存令牌", "Save token"))
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        let t = t_in.read(cx).value().trim().to_string();
                                        if t.is_empty() {
                                            return;
                                        }
                                        a9.update(cx, |a, cx| a.write_hf_token(Some(t), cx));
                                        t_in.update(cx, |s, cx| s.set_value("", window, cx));
                                    })),
                            )
                            .child(
                                Button::new("hf-token-del")
                                    .ghost()
                                    .label(tr!("删除令牌", "Delete token"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        t_in2.update(cx, |s, cx| s.set_value("", window, cx));
                                        this.app.update(cx, |a, cx| a.write_hf_token(None, cx));
                                    })),
                            ),
                    )
                    .child(ui::muted(token_line, cx)),
                cx,
            ))
            .child(Self::row(
                tr!("本地纪要模型", "Local minutes model"),
                Some(tr!("离线生成纪要（MLX）。", "Generates minutes offline (MLX).").into()),
                v_flex()
                    .gap_2()
                    .child(Input::new(&self.llm_dir))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("llm-load")
                                    .small()
                                    .outline()
                                    .label(tr!("加载", "Load"))
                                    .on_click(move |_, _, cx| {
                                        let dir = parse_path(&l_in.read(cx).value());
                                        a3.update(cx, |a, cx| match dir {
                                            Some(d) => a.load_local_llm(d, cx),
                                            None => a.notify(
                                                NoticeKind::Warning,
                                                tr!(
                                                    "请先填写模型文件夹。",
                                                    "Enter the model folder first."
                                                ),
                                                cx,
                                            ),
                                        });
                                    }),
                            )
                            .child(
                                Button::new("llm-unload")
                                    .small()
                                    .ghost()
                                    .label(tr!("卸载", "Unload"))
                                    .on_click(move |_, _, cx| {
                                        a4.update(cx, |a, cx| a.unload_local_llm(cx))
                                    }),
                            ),
                    )
                    .child(ui::muted(llm_line, cx)),
                cx,
            ))
            .child(Self::row(
                tr!("搜索索引", "Search index"),
                None,
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::muted(stats, cx))
                    .child(
                        Button::new("stats-refresh")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .on_click(move |_, _, cx| a5.update(cx, |a, cx| a.refresh_stats(cx))),
                    ),
                cx,
            ))
            .child(Self::row(
                tr!("引擎", "Engines"),
                None,
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::muted(engine_line, cx))
                    .child(
                        Button::new("engine-restart")
                            .small()
                            .outline()
                            .label(tr!("重新启动字幕引擎", "Restart caption engine"))
                            .on_click(move |_, _, cx| {
                                a6.update(cx, |a, cx| a.restart_meeting_engine(cx))
                            }),
                    ),
                cx,
            ))
            .child(ui::muted(
                trf!(
                    "数据文件夹：{}",
                    "Data folder: {}",
                    app.data_dir.display()
                ),
                cx,
            ))
    }
}

impl Render for SettingsPage {
    /// Renders all sections.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.placeholders.sync(window, cx);
        let cfg = self.app.read(cx).cfg.clone();
        v_flex()
            .id("settings-scroll")
            .test_support()
            .size_full()
            .p_6()
            .gap_4()
            .overflow_y_scrollbar()
            .child(ui::page_head(
                tr!("设置", "Settings"),
                tr!(
                    "修改后立即保存。文字输入框按回车或离开输入框时保存。",
                    "Changes save immediately. Text fields save when you press Return or leave the field."
                ),
                cx,
            ))
            .child(self.general(&cfg, cx))
            .child(self.permissions(cx))
            .child(self.obsidian(&cfg, cx))
            .child(self.capture(&cfg, cx))
            .child(self.cloud_asr(cx))
            .child(self.minutes(&cfg, cx))
            .child(self.models(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{PathBuf, parse_path, path_text};

    #[test]
    fn paths_parse_and_display() {
        assert_eq!(parse_path("  "), None);
        assert_eq!(parse_path("/a b"), Some(PathBuf::from("/a b")));
        assert!(parse_path("~/x").unwrap().ends_with("x"));
        assert_eq!(path_text(&None), "");
    }
}
