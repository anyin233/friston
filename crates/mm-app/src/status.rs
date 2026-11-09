//! What the sidebar's status lights show: one red/yellow/green light per resource the app
//! loads at startup (Obsidian vault, caption engine, embedding model) plus how minutes are
//! generated.
//!
//! Pure on purpose: the workbench copies the relevant `AppState` fields into a
//! [`StatusInput`] and renders the [`StatusRow`]s, so every rule here is unit tested.

use std::collections::BTreeMap;
use std::path::Path;

use mm_i18n::{tr, trf};
use mm_llm::{ByokProvider, MinutesBackend};
use mm_service::engines::{EmbedStatus, LlmStatus, SpeechModelsStatus};

use crate::pipeline::CodexStatus;
use crate::state::{EngineState, Loadable};

/// Traffic-light state of one resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Light {
    /// Not loaded (not configured, missing or failed).
    Red,
    /// Loading.
    Yellow,
    /// Loaded and usable.
    Green,
}

/// One light in the sidebar and one block of its hover card.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRow {
    /// Heading in the hover card.
    pub title: &'static str,
    pub light: Light,
    /// Main value: vault name, model name or why it is not loaded.
    pub value: String,
    /// Secondary lines: paths, companion models, errors.
    pub details: Vec<String>,
}

/// The `AppState` fields the lights depend on, borrowed for one render.
pub struct StatusInput<'a> {
    pub vault: Option<&'a Path>,
    /// Whether `vault` is an existing folder (checked by the caller, so this stays pure).
    pub vault_exists: bool,
    pub meeting_state: &'a EngineState,
    /// `initialize`'s `capabilities.models`, so the card can name a model still loading.
    pub meeting_models: &'a BTreeMap<String, String>,
    pub speech: &'a Loadable<SpeechModelsStatus>,
    pub embed: &'a Loadable<EmbedStatus>,
    pub embed_dir: Option<&'a Path>,
    pub minutes: MinutesInput<'a>,
}

/// What decides who writes the minutes (same policy as `mm_llm::plan_providers`).
pub struct MinutesInput<'a> {
    /// The backend the user picked.
    pub backend: MinutesBackend,
    /// `agy --model` id for minutes.
    pub antigravity_model: &'a str,
    /// Whether the `agy` executable was found.
    pub agy_found: bool,
    /// The Antigravity refine writes the minutes in the same run (short meetings).
    pub merged_with_refine: bool,
    /// BYOK provider, only when its key is in the Keychain.
    pub byok: Option<&'a ByokProvider>,
    /// The automatic chain may use Codex.
    pub use_codex: bool,
    pub codex: &'a Loadable<CodexStatus>,
    /// Codex model for minutes; `None` is Codex's default.
    pub codex_model: Option<&'a str>,
    pub llm: &'a Loadable<LlmStatus>,
    pub llm_dir: Option<&'a Path>,
}

/// Builds the rows in display order.
///
/// Params: `input` – current state.
/// Returns: the rows.
pub fn rows(input: &StatusInput) -> Vec<StatusRow> {
    vec![
        vault_row(input.vault, input.vault_exists),
        caption_row(input.meeting_state, input.meeting_models, input.speech),
        embed_row(input.embed, input.embed_dir),
        minutes_row(&input.minutes),
    ]
}

/// Readable model name from a repo id or folder.
///
/// Hugging Face snapshot folders (`…/models--owner--name/snapshots/<hash>`) are named after
/// the repo, not the hash; repo ids keep only the part after the owner.
///
/// Params: `id` – repo id (`owner/name`) or folder path.
/// Returns: the short name.
pub fn model_name(id: &str) -> String {
    let path = Path::new(id);
    if path.is_absolute() {
        let parts: Vec<String> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if let Some(i) = parts.iter().rposition(|p| p == "snapshots")
            && i > 0
            && let Some(repo) = parts[i - 1].strip_prefix("models--")
        {
            return repo.rsplit("--").next().unwrap_or(repo).to_string();
        }
        return path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| id.to_string());
    }
    id.rsplit('/').next().unwrap_or(id).to_string()
}

/// Obsidian vault: green when the folder exists, red when unset or gone.
fn vault_row(vault: Option<&Path>, exists: bool) -> StatusRow {
    let (light, value, details) = match vault {
        None => (
            Light::Red,
            tr!("未设置", "Not set").to_string(),
            vec![
                tr!(
                    "在设置中选择 Obsidian 库",
                    "Choose an Obsidian vault in Settings"
                )
                .into(),
            ],
        ),
        Some(p) => {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.display().to_string());
            let path = p.display().to_string();
            if exists {
                (Light::Green, name, vec![path])
            } else {
                (
                    Light::Red,
                    trf!("{name}（文件夹不存在）", "{name} (folder not found)"),
                    vec![path],
                )
            }
        }
    };
    StatusRow {
        title: tr!("Obsidian 库", "Obsidian vault"),
        light,
        value,
        details,
    }
}

/// Role name of a live caption model in the card.
///
/// Params: `role` – engine role id from `speechModels/status` (matched, never translated).
/// Returns: the localized label, or the id itself for roles this build does not know.
fn role_label(role: &str) -> &str {
    match role {
        "vad" => tr!("断句", "Segmentation"),
        "speakerEmbedding" => tr!("声纹", "Voiceprints"),
        "liveDiarization" => tr!("说话人分离", "Speaker separation"),
        "draft" => tr!("草稿字幕", "Draft captions"),
        "prefixDecoder" => tr!("前缀解码", "Prefix decoding"),
        other => other,
    }
}

/// Caption engine: green only once the live models are resident, not merely when the
/// process answered `initialize`, because the first recording would otherwise still wait.
fn caption_row(
    state: &EngineState,
    advertised: &BTreeMap<String, String>,
    speech: &Loadable<SpeechModelsStatus>,
) -> StatusRow {
    let asr = |models: &BTreeMap<String, String>| {
        models
            .get("liveASR")
            .map(|m| model_name(m))
            .unwrap_or_else(|| "Qwen3-ASR".into())
    };
    let (light, value, details) = match (state, speech) {
        (EngineState::Starting, _) => (
            Light::Yellow,
            tr!("引擎启动中", "Engine starting").into(),
            vec![],
        ),
        (EngineState::Ready, Loadable::Ready(s)) => {
            let mut details: Vec<String> = s
                .models
                .iter()
                .filter(|(role, _)| role.as_str() != "liveASR")
                .map(|(role, id)| trf!("{}：{}", "{}: {}", role_label(role), model_name(id)))
                .collect();
            if !s.unavailable.is_empty() {
                let missing: Vec<&str> = s.unavailable.iter().map(|r| role_label(r)).collect();
                details.push(trf!(
                    "未加载：{}",
                    "Not loaded: {}",
                    missing.join(tr!("、", ", "))
                ));
            }
            (Light::Green, asr(&s.models), details)
        }
        (EngineState::Ready, Loadable::Loading) => (
            Light::Yellow,
            asr(advertised),
            vec![
                tr!(
                    "正在加载模型，首次使用需要下载",
                    "Loading models; the first run downloads them"
                )
                .into(),
            ],
        ),
        (EngineState::Ready, Loadable::Failed(e)) => (
            Light::Red,
            asr(advertised),
            vec![trf!(
                "模型加载失败：{}",
                "Couldn’t load models: {}",
                e.message
            )],
        ),
        (EngineState::Ready, Loadable::Idle) => (
            Light::Red,
            asr(advertised),
            vec![tr!("模型未加载", "Models not loaded").into()],
        ),
        (other, _) => (Light::Red, other.describe(tr!("引擎", "Engine")), vec![]),
    };
    StatusRow {
        title: tr!("字幕引擎", "Caption engine"),
        light,
        value,
        details,
    }
}

/// Shared light rule for the two models in `mm-embedd`.
///
/// Params: `loading`/`loaded` – status; `loaded_dir` – folder the engine reports;
/// `dir` – configured folder; `error` – failure message.
/// Returns: light, value and details.
fn model_light(
    loading: bool,
    loaded: bool,
    loaded_dir: Option<&str>,
    dir: Option<&Path>,
    error: Option<&str>,
) -> (Light, String, Vec<String>) {
    let configured = dir.map(|d| d.display().to_string());
    let name = loaded_dir
        .map(str::to_string)
        .or(configured.clone())
        .map(|d| model_name(&d));
    match (loading, loaded, error) {
        (true, _, _) => (
            Light::Yellow,
            name.unwrap_or_else(|| tr!("加载中", "Loading").into()),
            vec![tr!("正在加载", "Loading…").into()],
        ),
        (_, true, _) => (
            Light::Green,
            name.unwrap_or_else(|| tr!("已加载", "Loaded").into()),
            loaded_dir
                .map(str::to_string)
                .or(configured)
                .into_iter()
                .collect(),
        ),
        (_, _, Some(e)) => (
            Light::Red,
            name.unwrap_or_else(|| tr!("未加载", "Not loaded").into()),
            vec![trf!("加载失败：{e}", "Couldn’t load: {e}")],
        ),
        _ => match name {
            Some(n) => (Light::Red, n, vec![tr!("未加载", "Not loaded").into()]),
            None => (
                Light::Red,
                tr!("未设置", "Not set").into(),
                vec![
                    tr!(
                        "在设置中选择或下载模型",
                        "Choose or download a model in Settings"
                    )
                    .into(),
                ],
            ),
        },
    }
}

/// Embedding model (EmbeddingGemma 2 in `mm-embedd`).
fn embed_row(status: &Loadable<EmbedStatus>, dir: Option<&Path>) -> StatusRow {
    let ready = status.ready();
    let (light, value, details) = model_light(
        matches!(status, Loadable::Loading),
        ready.is_some_and(|s| s.loaded),
        ready.and_then(|s| s.directory.as_deref()),
        dir,
        match status {
            Loadable::Failed(e) => Some(e.message.as_str()),
            _ => None,
        },
    );
    StatusRow {
        title: tr!("嵌入模型", "Embedding model"),
        light,
        value,
        details,
    }
}

/// Provider name and model of a BYOK configuration.
///
/// Params: `p`.
/// Returns: e.g. `OpenAI · gpt-5`.
fn byok_label(p: &ByokProvider) -> String {
    let (name, model) = match p {
        ByokProvider::OpenAi { model } => ("OpenAI", model),
        ByokProvider::Anthropic { model } => ("Anthropic", model),
        ByokProvider::Gemini { model } => ("Gemini", model),
        ByokProvider::OpenAiCompatible { model, .. } => {
            (tr!("OpenAI 兼容", "OpenAI-compatible"), model)
        }
    };
    if model.trim().is_empty() {
        name.to_string()
    } else {
        format!("{name} · {model}")
    }
}

/// The local minutes model's own light (also in `mm-embedd`).
///
/// Params: `status`; `dir` – configured folder.
/// Returns: light, model name and details.
fn local_llm(status: &Loadable<LlmStatus>, dir: Option<&Path>) -> (Light, String, Vec<String>) {
    let ready = status.ready();
    model_light(
        matches!(status, Loadable::Loading),
        ready.is_some_and(|s| s.loaded),
        ready.and_then(|s| s.directory.as_deref()),
        dir,
        match status {
            Loadable::Failed(e) => Some(e.message.as_str()),
            _ => None,
        },
    )
}

/// Names Codex with its model.
///
/// Params: `model` – picked model, `None` for Codex's default.
/// Returns: e.g. "Codex（ChatGPT · gpt-6-luna）".
fn codex_value(model: Option<&str>) -> String {
    match model {
        Some(m) => trf!("Codex（ChatGPT · {m}）", "Codex (ChatGPT · {m})"),
        None => tr!("Codex（ChatGPT）", "Codex (ChatGPT)").into(),
    }
}

/// Who writes minutes in the automatic order: BYOK → Codex → local.
///
/// Green names the provider that will run; yellow means Codex (or the local model, when it is
/// the only option) is still being checked or loaded; red means nothing can write minutes.
///
/// Params: `m`; `skip` – an explicitly picked backend this order stands in for, left out.
/// Returns: light, value and details.
fn minutes_provider(m: &MinutesInput, skip: MinutesBackend) -> (Light, String, Vec<String>) {
    let configured_local = m.llm_dir.is_some() || m.llm.ready().is_some_and(|s| s.loaded);
    let local = local_llm(m.llm, m.llm_dir);
    // A loaded or configured local model is worth naming even when a cloud provider wins.
    let local_note = || -> Vec<String> {
        if !configured_local {
            return vec![];
        }
        let state = match local.0 {
            Light::Green => tr!("已加载", "loaded"),
            Light::Yellow => tr!("加载中", "loading"),
            Light::Red => tr!("未加载", "not loaded"),
        };
        vec![trf!(
            "本地模型（备用）：{}（{state}）",
            "Local model (backup): {} ({state})",
            local.1
        )]
    };
    if let Some(p) = m.byok.filter(|_| skip != MinutesBackend::ApiKey) {
        return (
            Light::Green,
            trf!("API 密钥 · {}", "API key · {}", byok_label(p)),
            local_note(),
        );
    }
    let mut codex_note = None;
    if m.use_codex && skip != MinutesBackend::Codex {
        match m.codex {
            Loadable::Ready(c) if c.usable_path().is_some() => {
                let mut details: Vec<String> = c
                    .label
                    .iter()
                    .map(|l| trf!("账号：{l}", "Account: {l}"))
                    .collect();
                details.extend(local_note());
                return (Light::Green, codex_value(m.codex_model), details);
            }
            Loadable::Loading | Loadable::Idle => {
                return (
                    Light::Yellow,
                    codex_value(m.codex_model),
                    vec![tr!("正在检测 Codex 登录状态", "Checking Codex sign-in…").into()],
                );
            }
            Loadable::Ready(c) => {
                codex_note = Some(match (&c.path, &c.error) {
                    (None, _) => tr!("未找到 Codex", "Codex not found").to_string(),
                    (_, Some(e)) => trf!("Codex 不可用：{e}", "Codex unavailable: {e}"),
                    _ => tr!(
                        "Codex 未登录（运行 codex login）",
                        "Codex not signed in (run codex login)"
                    )
                    .to_string(),
                });
            }
            Loadable::Failed(e) => {
                codex_note = Some(trf!("Codex 不可用：{}", "Codex unavailable: {}", e.message))
            }
        }
    }
    if configured_local {
        let (light, name, mut details) = local;
        details.extend(codex_note);
        return (
            light,
            trf!("本地模型 · {name}", "Local model · {name}"),
            details,
        );
    }
    let mut details: Vec<String> = codex_note.into_iter().collect();
    details.push(
        tr!(
            "在设置中添加 API 密钥、登录 Codex 或加载本地纪要模型",
            "Add an API key, sign in to Codex or load a local minutes model in Settings"
        )
        .into(),
    );
    (
        Light::Red,
        tr!("没有可用的纪要生成方式", "No way to write minutes").into(),
        details,
    )
}

/// The automatic order standing in for a pick that cannot run: never better than yellow,
/// so the light shows the pick is not being honoured.
///
/// Params: `m`; `pick` – the explicit backend; `why` – why it cannot run.
/// Returns: light, value and details.
fn stand_in(m: &MinutesInput, pick: MinutesBackend, why: String) -> (Light, String, Vec<String>) {
    let (light, value, mut details) = minutes_provider(m, pick);
    details.insert(0, why);
    let light = if light == Light::Green {
        Light::Yellow
    } else {
        light
    };
    (light, value, details)
}

/// The note naming what runs when an explicit pick fails.
///
/// Params: `m`; `pick`.
/// Returns: "失败时改用：…".
fn fallback_note(m: &MinutesInput, pick: MinutesBackend) -> String {
    let (_, fallback, _) = minutes_provider(m, pick);
    trf!("失败时改用：{fallback}", "Falls back to: {fallback}")
}

/// Minutes: always shown, naming the provider that will write them.
///
/// An explicit pick that can run is green and lists its fallback, because any failure there
/// drops to the automatic order; a pick that cannot run shows the stand-in in yellow.
fn minutes_row(m: &MinutesInput) -> StatusRow {
    let (light, value, details) = match m.backend {
        MinutesBackend::Auto => minutes_provider(m, MinutesBackend::Auto),
        MinutesBackend::ApiKey => match m.byok {
            Some(p) => (
                Light::Green,
                trf!("API 密钥 · {}", "API key · {}", byok_label(p)),
                vec![fallback_note(m, MinutesBackend::ApiKey)],
            ),
            None => stand_in(
                m,
                MinutesBackend::ApiKey,
                tr!(
                    "选了 API 密钥，但还没有保存密钥",
                    "API key is picked, but no key is saved"
                )
                .into(),
            ),
        },
        MinutesBackend::Codex => match m.codex {
            Loadable::Ready(c) if c.usable_path().is_some() => {
                let mut details: Vec<String> = c
                    .label
                    .iter()
                    .map(|l| trf!("账号：{l}", "Account: {l}"))
                    .collect();
                details.push(fallback_note(m, MinutesBackend::Codex));
                (Light::Green, codex_value(m.codex_model), details)
            }
            Loadable::Loading | Loadable::Idle => (
                Light::Yellow,
                codex_value(m.codex_model),
                vec![tr!("正在检测 Codex 登录状态", "Checking Codex sign-in…").into()],
            ),
            _ => stand_in(
                m,
                MinutesBackend::Codex,
                tr!(
                    "选了 Codex，但它没有安装或未登录",
                    "Codex is picked, but it isn’t installed or signed in"
                )
                .into(),
            ),
        },
        MinutesBackend::Antigravity if m.agy_found => {
            let value = if m.antigravity_model.trim().is_empty() {
                "Antigravity".to_string()
            } else {
                format!("Antigravity · {}", m.antigravity_model)
            };
            let mut details = Vec::new();
            if m.merged_with_refine {
                details.push(
                    tr!(
                        "15 分钟内的会议在精转写时一并生成纪要",
                        "Meetings up to 15 minutes get minutes during the refine pass"
                    )
                    .into(),
                );
            }
            details.push(fallback_note(m, MinutesBackend::Antigravity));
            (Light::Green, value, details)
        }
        MinutesBackend::Antigravity => stand_in(
            m,
            MinutesBackend::Antigravity,
            tr!(
                "选了 Antigravity，但没有找到 agy",
                "Antigravity is picked, but agy wasn’t found"
            )
            .into(),
        ),
    };
    StatusRow {
        title: tr!("纪要", "Minutes"),
        light,
        value,
        details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::LoadError;
    use std::path::PathBuf;

    /// Owned backing data for a [`StatusInput`].
    struct Fixture {
        vault: Option<PathBuf>,
        vault_exists: bool,
        meeting_state: EngineState,
        meeting_models: BTreeMap<String, String>,
        speech: Loadable<SpeechModelsStatus>,
        embed: Loadable<EmbedStatus>,
        embed_dir: Option<PathBuf>,
        llm: Loadable<LlmStatus>,
        llm_dir: Option<PathBuf>,
        backend: MinutesBackend,
        antigravity_model: String,
        agy_found: bool,
        merged_with_refine: bool,
        byok: Option<ByokProvider>,
        use_codex: bool,
        codex: Loadable<CodexStatus>,
        codex_model: Option<String>,
    }

    impl Fixture {
        /// Fresh launch: nothing configured, engine starting.
        fn new() -> Self {
            Self {
                vault: None,
                vault_exists: false,
                meeting_state: EngineState::Starting,
                meeting_models: BTreeMap::new(),
                speech: Loadable::Idle,
                embed: Loadable::Idle,
                embed_dir: None,
                llm: Loadable::Idle,
                llm_dir: None,
                backend: MinutesBackend::Auto,
                antigravity_model: String::new(),
                agy_found: false,
                merged_with_refine: false,
                byok: None,
                use_codex: false,
                codex: Loadable::Idle,
                codex_model: None,
            }
        }

        /// The minutes row for the current fixture.
        fn minutes(&self) -> StatusRow {
            self.rows().pop().expect("minutes row is always last")
        }

        /// Rows for the current fixture.
        fn rows(&self) -> Vec<StatusRow> {
            rows(&StatusInput {
                vault: self.vault.as_deref(),
                vault_exists: self.vault_exists,
                meeting_state: &self.meeting_state,
                meeting_models: &self.meeting_models,
                speech: &self.speech,
                embed: &self.embed,
                embed_dir: self.embed_dir.as_deref(),
                minutes: MinutesInput {
                    backend: self.backend,
                    antigravity_model: &self.antigravity_model,
                    agy_found: self.agy_found,
                    merged_with_refine: self.merged_with_refine,
                    byok: self.byok.as_ref(),
                    use_codex: self.use_codex,
                    codex: &self.codex,
                    codex_model: self.codex_model.as_deref(),
                    llm: &self.llm,
                    llm_dir: self.llm_dir.as_deref(),
                },
            })
        }
    }

    fn lights(rows: &[StatusRow]) -> Vec<(&'static str, Light)> {
        rows.iter().map(|r| (r.title, r.light)).collect()
    }

    #[test]
    fn fresh_launch_is_red_except_the_starting_engine() {
        let rows = Fixture::new().rows();
        assert_eq!(
            lights(&rows),
            vec![
                ("Obsidian 库", Light::Red),
                ("字幕引擎", Light::Yellow),
                ("嵌入模型", Light::Red),
                ("纪要", Light::Red),
            ]
        );
        assert_eq!(rows[0].value, "未设置");
    }

    #[test]
    fn caption_engine_turns_green_only_after_preload() {
        let mut f = Fixture::new();
        f.meeting_state = EngineState::Ready;
        f.meeting_models
            .insert("liveASR".into(), "mlx-community/Qwen3-ASR-1.7B-4bit".into());
        f.speech = Loadable::Loading;
        let row = &f.rows()[1];
        assert_eq!(
            row.light,
            Light::Yellow,
            "process up but models still loading"
        );
        assert_eq!(
            row.value, "Qwen3-ASR-1.7B-4bit",
            "names the advertised model"
        );

        f.speech = Loadable::Ready(SpeechModelsStatus {
            models: BTreeMap::from([
                ("liveASR".into(), "mlx-community/Qwen3-ASR-1.7B-4bit".into()),
                ("vad".into(), "aufklarer/Silero-VAD-v5-MLX".into()),
            ]),
            unavailable: vec!["draft".into()],
        });
        let row = &f.rows()[1];
        assert_eq!(row.light, Light::Green);
        assert!(row.details.contains(&"断句：Silero-VAD-v5-MLX".to_string()));
        assert!(row.details.contains(&"未加载：草稿字幕".to_string()));

        f.speech = Loadable::Failed(LoadError::from("offline".to_string()));
        assert_eq!(f.rows()[1].light, Light::Red);
        f.meeting_state = EngineState::Missing;
        assert_eq!(f.rows()[1].light, Light::Red);
    }

    #[test]
    fn vault_light_needs_an_existing_folder() {
        let mut f = Fixture::new();
        f.vault = Some("/Users/me/Notes".into());
        assert_eq!(f.rows()[0].light, Light::Red);
        assert!(f.rows()[0].value.contains("不存在"));
        f.vault_exists = true;
        let row = &f.rows()[0];
        assert_eq!((row.light, row.value.as_str()), (Light::Green, "Notes"));
    }

    #[test]
    fn model_rows_follow_load_state() {
        let mut f = Fixture::new();
        f.embed_dir = Some("/m/embeddinggemma-2".into());
        f.embed = Loadable::Loading;
        assert_eq!(f.rows()[2].light, Light::Yellow);
        f.embed = Loadable::Ready(EmbedStatus {
            loaded: true,
            directory: Some("/m/embeddinggemma-2".into()),
            modalities: vec![],
        });
        let row = &f.rows()[2];
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Green, "embeddinggemma-2")
        );
        f.embed = Loadable::Ready(EmbedStatus::default());
        assert_eq!(f.rows()[2].light, Light::Red, "unloaded by the user");
    }

    #[test]
    fn minutes_name_the_cloud_provider_in_green() {
        let mut f = Fixture::new();
        f.use_codex = true;
        f.codex = Loadable::Loading;
        assert_eq!(f.minutes().light, Light::Yellow, "still probing Codex");

        f.codex = Loadable::Ready(CodexStatus {
            path: Some("/opt/homebrew/bin/codex".into()),
            signed_in: true,
            label: Some("me@example.com".into()),
            error: None,
        });
        let row = f.minutes();
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Green, "Codex（ChatGPT）")
        );
        assert_eq!(row.details, vec!["账号：me@example.com".to_string()]);

        f.byok = Some(ByokProvider::Anthropic {
            model: "claude-sonnet-5-5".into(),
        });
        let row = f.minutes();
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Green, "API 密钥 · Anthropic · claude-sonnet-5-5"),
            "BYOK wins over Codex, as in select_provider"
        );
    }

    #[test]
    fn minutes_fall_back_to_the_local_model() {
        let mut f = Fixture::new();
        f.use_codex = true;
        f.codex = Loadable::Ready(CodexStatus {
            path: Some("/bin/codex".into()),
            ..Default::default()
        });
        f.llm_dir = Some("/m/Qwen3-4B".into());
        f.llm = Loadable::Failed(LoadError::from("oom".to_string()));
        let row = f.minutes();
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Red, "本地模型 · Qwen3-4B")
        );
        assert!(row.details.contains(&"加载失败：oom".to_string()));
        assert!(row.details.iter().any(|d| d.contains("未登录")));

        f.llm = Loadable::Ready(LlmStatus {
            loaded: true,
            directory: Some("/m/Qwen3-4B".into()),
        });
        assert_eq!(f.minutes().light, Light::Green);
    }

    #[test]
    fn local_model_is_noted_as_backup_behind_a_cloud_provider() {
        let mut f = Fixture::new();
        f.byok = Some(ByokProvider::OpenAi {
            model: String::new(),
        });
        f.llm_dir = Some("/m/Qwen3-4B".into());
        f.llm = Loadable::Loading;
        let row = f.minutes();
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Green, "API 密钥 · OpenAI")
        );
        assert_eq!(
            row.details,
            vec!["本地模型（备用）：Qwen3-4B（加载中）".to_string()]
        );
    }

    #[test]
    fn antigravity_writes_minutes_and_lists_its_fallback() {
        let mut f = Fixture::new();
        f.backend = MinutesBackend::Antigravity;
        f.antigravity_model = "gemini-3-pro".into();
        f.agy_found = true;
        f.merged_with_refine = true;
        let row = f.minutes();
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Green, "Antigravity · gemini-3-pro")
        );
        assert_eq!(row.details[1], "失败时改用：没有可用的纪要生成方式");

        // Without agy the automatic order stands in, in yellow, and says why.
        f.agy_found = false;
        f.byok = Some(ByokProvider::OpenAi {
            model: "gpt-6".into(),
        });
        let row = f.minutes();
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Yellow, "API 密钥 · OpenAI · gpt-6")
        );
        assert!(row.details[0].contains("没有找到 agy"));
    }

    #[test]
    fn explicit_codex_names_its_model_and_skips_itself_as_fallback() {
        let mut f = Fixture::new();
        f.backend = MinutesBackend::Codex;
        f.use_codex = true;
        f.codex_model = Some("gpt-6-luna".into());
        f.codex = Loadable::Ready(CodexStatus {
            path: Some("/bin/codex".into()),
            signed_in: true,
            label: None,
            error: None,
        });
        let row = f.minutes();
        assert_eq!(
            (row.light, row.value.as_str()),
            (Light::Green, "Codex（ChatGPT · gpt-6-luna）")
        );
        assert_eq!(
            row.details,
            vec!["失败时改用：没有可用的纪要生成方式".to_string()]
        );

        f.backend = MinutesBackend::ApiKey;
        let row = f.minutes();
        assert_eq!(row.light, Light::Yellow, "no key saved");
        assert_eq!(row.value, "Codex（ChatGPT · gpt-6-luna）");
    }

    /// English rows keep the same composition rules: role labels, list separators,
    /// the backup note and the Antigravity fallback all switch language together.
    #[test]
    fn rows_read_naturally_in_english() {
        mm_i18n::with_lang(mm_i18n::Lang::En, || {
            let mut f = Fixture::new();
            assert_eq!(f.rows()[0].title, "Obsidian vault");
            assert_eq!(f.rows()[0].value, "Not set");

            f.vault = Some("/Users/me/Notes".into());
            assert_eq!(f.rows()[0].value, "Notes (folder not found)");

            f.meeting_state = EngineState::Ready;
            f.speech = Loadable::Ready(SpeechModelsStatus {
                models: BTreeMap::from([
                    ("liveASR".into(), "mlx-community/Qwen3-ASR-1.7B-4bit".into()),
                    ("vad".into(), "aufklarer/Silero-VAD-v5-MLX".into()),
                ]),
                unavailable: vec!["draft".into(), "prefixDecoder".into()],
            });
            let row = &f.rows()[1];
            assert_eq!(row.title, "Caption engine");
            assert!(
                row.details
                    .contains(&"Segmentation: Silero-VAD-v5-MLX".to_string())
            );
            assert!(
                row.details
                    .contains(&"Not loaded: Draft captions, Prefix decoding".to_string())
            );

            f.byok = Some(ByokProvider::OpenAiCompatible {
                base_url: "http://localhost:1234/v1".into(),
                model: "qwen".into(),
            });
            f.llm_dir = Some("/m/Qwen3-4B".into());
            f.llm = Loadable::Loading;
            let row = f.minutes();
            assert_eq!(row.value, "API key · OpenAI-compatible · qwen");
            assert_eq!(
                row.details,
                vec!["Local model (backup): Qwen3-4B (loading)".to_string()]
            );

            f.byok = None;
            f.llm = Loadable::Idle;
            f.llm_dir = None;
            f.backend = MinutesBackend::Antigravity;
            f.agy_found = true;
            let row = f.minutes();
            assert_eq!(row.value, "Antigravity");
            assert_eq!(row.details[0], "Falls back to: No way to write minutes");
        });
    }

    #[test]
    fn model_names_are_short() {
        assert_eq!(
            model_name("mlx-community/Qwen3-ASR-1.7B-4bit"),
            "Qwen3-ASR-1.7B-4bit"
        );
        assert_eq!(
            model_name("/hf/hub/models--google--embeddinggemma-2/snapshots/abc123"),
            "embeddinggemma-2"
        );
        assert_eq!(model_name("/models/gemma"), "gemma");
        assert_eq!(model_name("plain"), "plain");
    }
}
