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
use mm_service::AppConfig;

use crate::logic::{self, BYOK_KINDS, CAPTION_LANGUAGES, MINUTES_LANGUAGES};
use crate::state::{AppState, Loadable, NoticeKind};
use crate::ui::{self, Choice};

/// Text fields of the settings page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Vault,
    Folder,
    Model,
    BaseUrl,
    EmbedDir,
    LlmDir,
}

/// The 设置 page.
pub struct SettingsPage {
    app: Entity<AppState>,
    vault: Entity<InputState>,
    folder: Entity<InputState>,
    model: Entity<InputState>,
    base_url: Entity<InputState>,
    api_key: Entity<InputState>,
    embed_dir: Entity<InputState>,
    llm_dir: Entity<InputState>,
    mics: Entity<SelectState<Vec<Choice>>>,
    mic_sig: Vec<String>,
    /// BYOK kind chosen in the form (may differ from config until a model is typed).
    byok_kind: &'static str,
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
                  placeholder: &'static str,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let vault = mk(
            path_text(&cfg.vault_path),
            "Obsidian 库文件夹，例如 ~/Documents/Work Notes",
            window,
            cx,
        );
        let folder = mk(cfg.meetings_folder.clone(), "Meetings", window, cx);
        let model = mk(model, "模型，例如 claude-opus-5-5、gpt-5.5", window, cx);
        let base_url = mk(
            base,
            "接口地址，例如 https://openrouter.ai/api/v1",
            window,
            cx,
        );
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("粘贴 API 密钥，保存后不会再显示")
                .masked(true)
        });
        let embed_dir = mk(
            path_text(&cfg.embedding_model_dir),
            "EmbeddingGemma 2 模型文件夹",
            window,
            cx,
        );
        let llm_dir = mk(
            path_text(&cfg.local_llm_dir),
            "本地纪要模型（MLX）文件夹",
            window,
            cx,
        );
        let mics = cx.new(|cx| {
            SelectState::new(
                vec![Choice::new("", "系统默认麦克风")],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });

        for (input, field) in [
            (&vault, Field::Vault),
            (&folder, Field::Folder),
            (&model, Field::Model),
            (&base_url, Field::BaseUrl),
            (&embed_dir, Field::EmbedDir),
            (&llm_dir, Field::LlmDir),
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
        cx.observe_in(&app, window, |this, _, window, cx| {
            this.sync_mics(window, cx)
        })
        .detach();
        Self {
            app,
            vault,
            folder,
            model,
            base_url,
            api_key,
            embed_dir,
            llm_dir,
            mics,
            mic_sig: vec![],
            byok_kind: kind,
        }
    }

    /// Keeps the microphone select in step with the probed inputs.
    fn sync_mics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        if let Loadable::Ready(inputs) = &app.inputs {
            let mut items = vec![Choice::new("", "系统默认麦克风")];
            items.extend(inputs.iter().map(|i| {
                let spatial = if i.supports_spatial_cues() {
                    "，可判断方位"
                } else {
                    ""
                };
                Choice::new(
                    i.uid.clone(),
                    format!("{}（{} 声道{spatial}）", i.name, i.channels),
                )
            }));
            let sig: Vec<String> = items.iter().map(|c| c.value.clone()).collect();
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
                        format!("文件夹不存在：{}", dir.display()),
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
            Field::LlmDir => a.update_config(cx, |c| c.local_llm_dir = parse_path(&value)),
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

    /// Obsidian section.
    fn obsidian(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.clone();
        let vault_input = self.vault.clone();
        ui::card(cx)
            .child(ui::section_title("Obsidian 库"))
            .child(Self::row(
                "库文件夹",
                Some("会议笔记、纪要和截图都写进这个库。".into()),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.vault)))
                    .child(
                        Button::new("discover")
                            .outline()
                            .label("自动发现")
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
                                            format!("已使用 Obsidian 库：{}", path_text(&p)),
                                            cx,
                                        );
                                    });
                                } else {
                                    app.update(cx, |a, cx| {
                                        a.notify(
                                            NoticeKind::Warning,
                                            "没有找到 Obsidian 登记的库，请手动填写文件夹。",
                                            cx,
                                        )
                                    });
                                }
                            })),
                    ),
                cx,
            ))
            .child(Self::row(
                "会议笔记文件夹",
                Some("库里的相对路径".into()),
                Input::new(&self.folder),
                cx,
            ))
            .child(ui::muted(
                match &cfg.vault_path {
                    Some(v) => format!("当前：{}", v.display()),
                    None => "还没有设置库：笔记不会写入文件。".into(),
                },
                cx,
            ))
    }

    /// Capture and caption section.
    fn capture(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = cfg.caption_language.clone();
        ui::card(cx)
            .child(ui::section_title("录制与字幕"))
            .child(Self::row(
                "默认麦克风",
                Some("两个及以上声道的实体麦克风可以判断方位。".into()),
                Select::new(&self.mics).small(),
                cx,
            ))
            .child(Self::row(
                "",
                None,
                self.switch(
                    "s-sys",
                    "录系统声音（远程参会者）",
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
                    "录制会议窗口画面",
                    cfg.record_screen,
                    |c, v| c.record_screen = v,
                ),
                cx,
            ))
            .child(Self::row(
                "字幕语言",
                None,
                self.segmented(
                    "cap-lang",
                    CAPTION_LANGUAGES
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
                    "会议中显示浮窗字幕",
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
                    "会后用大模型精转写录音",
                    cfg.refine_after_meeting,
                    |c, v| c.refine_after_meeting = v,
                ),
                cx,
            ))
    }

    /// Minutes provider section.
    fn minutes(&self, cfg: &AppConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let codex_line = match &app.codex {
            Loadable::Ready(c) => match (&c.path, c.signed_in) {
                (None, _) => "未安装 Codex CLI。安装后可用 ChatGPT 账号生成纪要。".to_string(),
                (Some(p), true) => format!(
                    "Codex 已登录{}（{}）",
                    c.label
                        .as_deref()
                        .map(|l| format!("：{l}"))
                        .unwrap_or_default(),
                    p.display()
                ),
                (Some(_), false) => format!(
                    "Codex 未登录。在终端运行 codex login 后点“重新检测”。{}",
                    c.error
                        .as_deref()
                        .map(|e| format!("（{e}）"))
                        .unwrap_or_default()
                ),
            },
            Loadable::Failed(e) => e.message.clone(),
            _ => "正在检测 Codex…".to_string(),
        };
        let key_line = if cfg.byok.is_none() {
            "选择提供方并填写模型后可以保存密钥。".to_string()
        } else if app.byok_key_present {
            "钥匙串里已有密钥（不会显示）。".to_string()
        } else {
            "还没有保存密钥。".to_string()
        };
        let current_kind = self.byok_kind;
        let a1 = self.app.clone();
        let a2 = self.app.clone();
        let key = self.api_key.clone();
        let key2 = self.api_key.clone();
        let minutes_lang = cfg.minutes_language.clone();
        ui::card(cx)
            .child(ui::section_title("纪要"))
            .child(Self::row(
                "",
                None,
                self.switch(
                    "s-auto",
                    "会议结束后自动生成纪要",
                    cfg.auto_minutes,
                    |c, v| c.auto_minutes = v,
                ),
                cx,
            ))
            .child(Self::row(
                "纪要语言",
                None,
                self.segmented(
                    "min-lang",
                    MINUTES_LANGUAGES
                        .iter()
                        .map(|(v, l)| (v.to_string(), *l))
                        .collect(),
                    minutes_lang,
                    |a, v, cx| a.update_config(cx, |c| c.minutes_language = v),
                ),
                cx,
            ))
            .child(Self::row(
                "Codex（ChatGPT 账号）",
                Some("没有 API 密钥时使用本机 Codex。".into()),
                v_flex()
                    .gap_2()
                    .child(self.switch(
                        "s-codex",
                        "允许通过 Codex 生成纪要",
                        cfg.use_codex,
                        |c, v| c.use_codex = v,
                    ))
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
                                    .label("重新检测")
                                    .on_click(move |_, _, cx| {
                                        a1.update(cx, |a, cx| a.refresh_codex(cx))
                                    }),
                            ),
                    ),
                cx,
            ))
            .child(Self::row(
                "API 密钥（优先使用）",
                Some("密钥只保存在 macOS 钥匙串。".into()),
                v_flex()
                    .gap_2()
                    .child(h_flex().gap_1().children(BYOK_KINDS.iter().enumerate().map(
                        |(i, (k, label))| {
                            let k: &'static str = k;
                            let b = Button::new(("byok", i)).small().label(*label);
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
                    )))
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
                                            .label("保存密钥")
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
                                        Button::new("key-del").ghost().label("删除密钥").on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                key2.update(cx, |s, cx| {
                                                    s.set_value("", window, cx)
                                                });
                                                this.app
                                                    .update(cx, |a, cx| a.write_byok_key(None, cx));
                                            }),
                                        ),
                                    ),
                            )
                            .child(ui::muted(key_line, cx))
                    }),
                cx,
            ))
    }

    /// Local models and index section.
    fn models(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let embed_line = match &app.embed_status {
            Loadable::Ready(s) if s.loaded => format!(
                "已加载：{}（{}）",
                s.directory.clone().unwrap_or_default(),
                s.modalities.join("、")
            ),
            Loadable::Ready(_) | Loadable::Idle => "未加载：搜索只用关键词。".to_string(),
            Loadable::Loading => "正在加载…".to_string(),
            Loadable::Failed(e) => e.message.clone(),
        };
        let llm_line = match &app.llm_status {
            Loadable::Ready(s) if s.loaded => {
                format!("已加载：{}", s.directory.clone().unwrap_or_default())
            }
            Loadable::Ready(_) | Loadable::Idle => {
                "未加载：没有密钥和 Codex 时，生成纪要前会自动加载。".to_string()
            }
            Loadable::Loading => "正在加载…".to_string(),
            Loadable::Failed(e) => e.message.clone(),
        };
        let stats = app
            .index_stats
            .map(|s| {
                format!(
                    "{} 个来源，{} 个片段，其中 {} 个已用当前模型嵌入",
                    s.sources, s.chunks, s.embedded
                )
            })
            .unwrap_or_else(|| "正在统计…".into());
        let engine_line = format!(
            "{}；{}",
            app.meeting_state.describe("字幕引擎 "),
            app.embed_state.describe("嵌入引擎 ")
        );
        let (e_in, l_in) = (self.embed_dir.clone(), self.llm_dir.clone());
        let (a1, a2, a3, a4, a5, a6) = (
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
            self.app.clone(),
        );
        ui::card(cx)
            .child(ui::section_title("本地模型与搜索索引"))
            .child(Self::row(
                "嵌入模型",
                Some("EmbeddingGemma 2，用于语义搜索。".into()),
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
                                    .label("加载")
                                    .on_click(move |_, _, cx| {
                                        let dir = parse_path(&e_in.read(cx).value());
                                        a1.update(cx, |a, cx| match dir {
                                            Some(d) => a.load_embedding(d, cx),
                                            None => a.notify(
                                                NoticeKind::Warning,
                                                "请先填写模型文件夹。",
                                                cx,
                                            ),
                                        });
                                    }),
                            )
                            .child(
                                Button::new("embed-unload")
                                    .small()
                                    .ghost()
                                    .label("卸载")
                                    .on_click(move |_, _, cx| {
                                        a2.update(cx, |a, cx| a.unload_embedding(cx))
                                    }),
                            ),
                    )
                    .child(ui::muted(embed_line, cx)),
                cx,
            ))
            .child(Self::row(
                "本地纪要模型",
                Some("离线生成纪要（MLX）。".into()),
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
                                    .label("加载")
                                    .on_click(move |_, _, cx| {
                                        let dir = parse_path(&l_in.read(cx).value());
                                        a3.update(cx, |a, cx| match dir {
                                            Some(d) => a.load_local_llm(d, cx),
                                            None => a.notify(
                                                NoticeKind::Warning,
                                                "请先填写模型文件夹。",
                                                cx,
                                            ),
                                        });
                                    }),
                            )
                            .child(
                                Button::new("llm-unload")
                                    .small()
                                    .ghost()
                                    .label("卸载")
                                    .on_click(move |_, _, cx| {
                                        a4.update(cx, |a, cx| a.unload_local_llm(cx))
                                    }),
                            ),
                    )
                    .child(ui::muted(llm_line, cx)),
                cx,
            ))
            .child(Self::row(
                "搜索索引",
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
                "引擎",
                None,
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::muted(engine_line, cx))
                    .child(
                        Button::new("engine-restart")
                            .small()
                            .outline()
                            .label("重新启动字幕引擎")
                            .on_click(move |_, _, cx| {
                                a6.update(cx, |a, cx| a.restart_meeting_engine(cx))
                            }),
                    ),
                cx,
            ))
            .child(ui::muted(
                format!("数据文件夹：{}", app.data_dir.display()),
                cx,
            ))
    }
}

impl Render for SettingsPage {
    /// Renders all sections.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = self.app.read(cx).cfg.clone();
        v_flex()
            .id("settings-scroll")
            .size_full()
            .p_6()
            .gap_4()
            .overflow_y_scrollbar()
            .child(ui::page_head(
                "设置",
                "修改后立即保存。文字输入框按回车或离开输入框时保存。",
                cx,
            ))
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
