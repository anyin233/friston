//! 设置: Obsidian, capture, captions, minutes providers, local models and the index.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, IconName, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_i18n::{tr, trf};
use mm_llm::{MinutesBackend, antigravity};
use mm_service::{AppConfig, RefineBackend};

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
    embed_dir: Entity<InputState>,
    embed_url: Entity<InputState>,
    hf_token: Entity<InputState>,
    llm_dir: Entity<InputState>,
    spacing: Entity<InputState>,
    refine_model: Entity<InputState>,
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
    model_sels: [Entity<SelectState<Vec<Choice>>>; 3],
    /// Items, selection and language last pushed into each picker.
    model_sigs: [Vec<String>; 3],
}

/// The three model pickers on this page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelPicker {
    /// Codex model for minutes.
    Codex = 0,
    /// Antigravity model for minutes.
    AgyMinutes = 1,
    /// Antigravity model for the post-meeting refine (must hear audio).
    AgyRefine = 2,
}

impl ModelPicker {
    const ALL: [ModelPicker; 3] = [Self::Codex, Self::AgyMinutes, Self::AgyRefine];

    /// Stores a picked model id in the config.
    ///
    /// Params: `cfg`; `id` – `""` means the CLI's default (Codex only).
    fn store(self, cfg: &mut AppConfig, id: String) {
        match self {
            Self::Codex => cfg.codex_model = Some(id).filter(|m| !m.is_empty()),
            Self::AgyMinutes if !id.is_empty() => cfg.antigravity_minutes_model = id,
            Self::AgyRefine if !id.is_empty() => cfg.antigravity_model = id,
            _ => {}
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
            embed_dir,
            embed_url,
            hf_token,
            llm_dir,
            spacing,
            refine_model,
            mics,
            mic_sig: vec![],
            windows,
            window_sig: vec![],
            embed_dir_seen: cfg.embedding_model_dir.clone(),
            byok_kind: kind,
            model_sels,
            model_sigs: Default::default(),
        }
    }

    /// Keeps the model pickers in step with the CLIs' model lists and the config.
    ///
    /// Params: `window`; `cx`.
    fn sync_model_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        let cfg = &app.cfg;
        let agy_fallback = antigravity::models();
        let all = |_: &str| true;
        let codex_saved = cfg.codex_model.clone().unwrap_or_default();
        let lists = [
            (
                logic::model_choices(
                    app.codex_models.ready().map(Vec::as_slice),
                    &[],
                    all,
                    &codex_saved,
                    Some(tr!("Codex 默认模型", "Codex default model")),
                ),
                codex_saved.clone(),
            ),
            (
                logic::model_choices(
                    app.agy_models.ready().map(Vec::as_slice),
                    &agy_fallback,
                    all,
                    &cfg.antigravity_minutes_model,
                    None,
                ),
                cfg.antigravity_minutes_model.clone(),
            ),
            (
                logic::model_choices(
                    app.agy_models.ready().map(Vec::as_slice),
                    &agy_fallback,
                    antigravity::refine_capable,
                    &cfg.antigravity_model,
                    None,
                ),
                cfg.antigravity_model.clone(),
            ),
        ];
        let lang = mm_i18n::current().code().to_string();
        for (i, (items, selected)) in lists.into_iter().enumerate() {
            // Labels are part of the signature: the language switch relabels the default entry.
            let sig: Vec<String> = items
                .iter()
                .flat_map(|(id, l)| [id.clone(), l.clone()])
                .chain([selected.clone(), lang.clone()])
                .collect();
            if sig == self.model_sigs[i] {
                continue;
            }
            self.model_sigs[i] = sig;
            let choices: Vec<Choice> = items
                .into_iter()
                .map(|(id, l)| Choice::new(id, l))
                .collect();
            self.model_sels[i].update(cx, |s, cx| {
                s.set_items(choices, window, cx);
                s.set_selected_value(&selected, window, cx);
            });
        }
        cx.notify();
    }

    /// A model picker with a button that asks the CLI for its list again.
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
                                app.update(cx, |a, cx| match picker {
                                    ModelPicker::Codex => a.refresh_codex_models(cx),
                                    _ => a.refresh_agy_models(cx),
                                })
                            }),
                    ),
            )
            .when_some(note, |el, n| el.child(ui::muted(n, cx)))
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
            Field::Spacing => match logic::parse_spacing_cm(&value) {
                Ok(cm) => a.update_config(cx, |c| c.spatial_spacing_cm = cm),
                Err(msg) => a.notify(NoticeKind::Warning, msg, cx),
            },
        });
    }

    /// A labelled row.
    fn row(label: &str, hint: Option<String>, control: impl IntoElement, cx: &App) -> Div {
        h_flex()
            .gap_4()
            .items_start()
            .py_2()
            .child(
                v_flex()
                    .w(px(220.))
                    .flex_none()
                    .child(div().text_sm().child(label.to_string()))
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
                    .child(div().flex_1().child(Input::new(&self.vault)))
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
                Input::new(&self.folder),
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
        let recording = {
            let app = self.app.read(cx);
            app.live.is_some() || app.live_starting
        };
        let a_mics = self.app.clone();
        let a_src = self.app.clone();
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
                    "本地：Qwen3-ASR 在本机重新转写。Antigravity：Gemini 直接听录音；录音会上传到 Google，失败时自动改用本地。纪要也选 Antigravity 且模型相同时，15 分钟内的会议一次完成转写和纪要。",
                    "Local: Qwen3-ASR re-transcribes on this Mac. Antigravity: Gemini listens to the recording; the recording is uploaded to Google, and the app falls back to local if it fails. If minutes also use Antigravity with the same model, meetings up to 15 minutes get the transcript and minutes in one run."
                ).into()),
                self.segmented(
                    "refine-backend",
                    vec![
                        (RefineBackend::Local, tr!("本地 Qwen3-ASR", "Local Qwen3-ASR")),
                        (RefineBackend::Antigravity, "Antigravity · Gemini"),
                    ],
                    cfg.refine_backend,
                    |a, v, cx| a.update_config(cx, |c| c.refine_backend = v),
                ),
                cx,
            ))
            .when(cfg.refine_backend == RefineBackend::Local, |el| {
                el.child(Self::row(
                    tr!("精转模型", "Refine model"),
                    Some(tr!(
                        "实时字幕固定用快速模型。这里可以填更大的 Qwen3-ASR 模型：Hugging Face 仓库名或本地文件夹，名字里要带 0.6B 或 1.7B。留空用默认。",
                        "Live captions always use the fast model. Enter a larger Qwen3-ASR model here: a Hugging Face repo or a local folder whose name contains 0.6B or 1.7B. Leave blank for the default."
                    ).into()),
                    Input::new(&self.refine_model).small(),
                    cx,
                ))
            })
            .when(cfg.refine_backend == RefineBackend::Antigravity, |el| {
                let agy_line = match antigravity::find_agy() {
                    Some(p) => trf!(
                        "已找到 agy（{}）。第一次使用前，在终端运行一次 agy 并用 Google 账号登录。",
                        "Found agy ({}). Before first use, run agy once in Terminal and sign in with your Google account.",
                        p.display()
                    ),
                    None => tr!(
                        "没有找到 agy。安装 Antigravity CLI 后再选这一项，否则会改用本地精转。",
                        "agy not found. Install the Antigravity CLI before choosing this; otherwise the app refines locally."
                    )
                    .to_string(),
                };
                let picker = self.model_picker(
                    ModelPicker::AgyRefine,
                    &self.app.read(cx).agy_models.clone(),
                    cx,
                );
                el.child(Self::row(
                    tr!("Gemini 模型", "Gemini model"),
                    Some(agy_line),
                    picker,
                    cx,
                ))
            })
    }

    /// Minutes provider section: the backend pick, then the settings of the backends it may use.
    fn minutes(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let backend = cfg.minutes_backend;
        let app = self.app.read(cx);
        let codex_line = match &app.codex {
            Loadable::Ready(c) => match (&c.path, c.signed_in) {
                (None, _) => tr!(
                    "未安装 Codex CLI。安装后可用 ChatGPT 账号生成纪要。",
                    "Codex CLI isn’t installed. Install it to generate minutes with your ChatGPT account."
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
        };
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
        let agy_models = app.agy_models.clone();
        let agy_line = match antigravity::find_agy() {
            Some(p) => trf!(
                "已找到 agy（{}）。第一次使用前，在终端运行一次 agy 并登录。",
                "Found agy ({}). Before first use, run agy once in Terminal and sign in.",
                p.display()
            ),
            None => tr!(
                "没有找到 agy。安装 Antigravity CLI 后再选这一项，否则会改用自动顺序。",
                "agy not found. Install the Antigravity CLI before choosing this; otherwise the automatic order is used."
            )
            .to_string(),
        };
        // How the refine pass and the minutes share agy (see `merges_minutes_into_refine`).
        let agy_refine_note = (cfg.refine_after_meeting
            && cfg.refine_backend == RefineBackend::Antigravity)
            .then(|| {
                if cfg.antigravity_minutes_model == cfg.antigravity_model {
                    tr!(
                        "和精转用同一个模型：15 分钟内的会议一次完成转写和纪要，更长的会议转写后再写纪要。",
                        "Same model as the refine: meetings up to 15 minutes get the transcript and minutes in one run; longer ones get minutes after the transcript."
                    )
                } else {
                    tr!(
                        "和精转模型不同：精转写完成后再用这个模型写纪要。",
                        "Different from the refine model: this model writes the minutes after the refine."
                    )
                }
            });
        let current_kind = self.byok_kind;
        let a1 = self.app.clone();
        let a2 = self.app.clone();
        let key = self.api_key.clone();
        let key2 = self.api_key.clone();
        let minutes_lang = cfg.minutes_language.clone();

        let codex_row =
            matches!(backend, MinutesBackend::Auto | MinutesBackend::Codex).then(|| {
                let picker = self.model_picker(ModelPicker::Codex, &codex_models, cx);
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

        let agy_row = (backend == MinutesBackend::Antigravity).then(|| {
            let picker = self.model_picker(ModelPicker::AgyMinutes, &agy_models, cx);
            Self::row(
                tr!("Antigravity 模型", "Antigravity model"),
                Some(
                    tr!(
                        "通过 agy 写纪要，可选 Gemini、Claude 等模型；转写文字会发送给 Google。",
                        "Writes minutes through agy with Gemini, Claude or another model; the transcript text is sent to Google."
                    )
                    .into(),
                ),
                v_flex()
                    .gap_2()
                    .child(picker)
                    .child(ui::muted(agy_line, cx))
                    .when_some(agy_refine_note, |el, n| el.child(ui::muted(n, cx))),
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
                        el.child(Input::new(&self.model))
                            .when(current_kind == "openai_compatible", |el| {
                                el.child(Input::new(&self.base_url))
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
                self.segmented(
                    "min-backend",
                    [
                        MinutesBackend::Auto,
                        MinutesBackend::ApiKey,
                        MinutesBackend::Codex,
                        MinutesBackend::Antigravity,
                    ]
                    .map(|b| (b, logic::minutes_backend_label(b)))
                    .to_vec(),
                    backend,
                    |a, v, cx| a.update_config(cx, |c| c.minutes_backend = v),
                ),
                cx,
            ))
            .children(codex_row)
            .children(agy_row)
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
