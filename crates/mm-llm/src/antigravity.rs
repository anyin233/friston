//! Post-meeting speech recognition and minutes through the Antigravity CLI (`agy`).
//!
//! Gemini 3.8 Flash hears audio directly: the agent opens the recording with its own file
//! viewer, so the audio never passes through a shell. Codex was evaluated for the same job
//! (2026-11-08) and rejected: `gpt-6.1-terra` and `gpt-6.1-sol` only advertise text and image
//! input and answer "cannot read the audio" to a `localAudio` item.
//!
//! Safety: `agy -p` runs headless, where every tool that needs a permission is denied
//! automatically. We never pass `--dangerously-skip-permissions`; the only allow-rule is
//! `read_file(<workdir>/**)`, so the agent can read the prepared audio and nothing else.
//!
//! Output: `--output-format json` prints one object with `status`, `response` and, when
//! `--json-schema` is given, `structured_output`. A run can exit 0 with `status: "ERROR"`
//! (backend 500s end as "The stream was interrupted"), so the status is always checked.
//! On recordings that are mostly keyboard noise, Flash sometimes keeps generating after the
//! JSON (17k output tokens for three sentences) until the stream is cut; the structured
//! output it already produced was complete and correct in every case we saw, so it is kept
//! as a last resort and flagged as [`Heard::interrupted`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mm_core::AudioOrigin;
use mm_i18n::tr;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{LlmError, Minutes, MinutesInput, ModelOption, instructions, minutes_json_schema};

/// Model used when the user has not picked one.
///
/// `-low` is deliberately not offered: on a recording whose system-audio track was digital
/// silence it invented a whole meeting with names, while `-medium` reported the silence.
pub const DEFAULT_MODEL: &str = "gemini-3.8-flash-medium";

/// `agy` model ids offered in settings; labels come from [`models`].
const MODEL_IDS: [&str; 2] = ["gemini-3.8-flash-medium", "gemini-3.8-flash-high"];

/// Models offered in settings, labelled in the current UI language.
///
/// Returns: `(agy model id, label)`, default model first.
pub fn models() -> [(&'static str, &'static str); 2] {
    [
        (MODEL_IDS[0], "Gemini 3.8 Flash"),
        (
            MODEL_IDS[1],
            tr!(
                "Gemini 3.8 Flash（深度思考）",
                "Gemini 3.8 Flash (deep thinking)"
            ),
        ),
    ]
}

/// Human label of a model id, for progress messages.
///
/// Params: `model` – agy model id.
/// Returns: the settings label in the current UI language, or the id itself for custom models.
pub fn model_label(model: &str) -> &str {
    models()
        .into_iter()
        .find(|(id, _)| *id == model)
        .map(|(_, label)| label)
        .unwrap_or(model)
}

/// Whether a model can do the post-meeting refine: it must hear audio (Gemini) and not be a
/// `-low` variant (see [`DEFAULT_MODEL`]).
///
/// Params: `id` – agy model id.
/// Returns: `true` when settings may offer it for refine.
pub fn refine_capable(id: &str) -> bool {
    id.starts_with("gemini-") && !id.ends_with("-low")
}

/// Parses `agy models`: one `id<TAB>label` line per model, after a "Fetching…" line.
///
/// Params: `stdout` – everything the command printed.
/// Returns: the models in agy's order; [`DEFAULT_MODEL`] is marked as the default.
pub fn parse_models(stdout: &str) -> Vec<ModelOption> {
    stdout
        .lines()
        .filter_map(|l| {
            let (id, label) = l.split_once('\t')?;
            let id = id.trim();
            (!id.is_empty() && !id.contains(' ')).then(|| ModelOption {
                id: id.to_string(),
                label: label.trim().to_string(),
                is_default: id == DEFAULT_MODEL,
            })
        })
        .collect()
}

/// Lists the models the signed-in Antigravity account offers (`agy models`).
///
/// Params: `agy` – executable path.
/// Returns: the models, or an error when agy printed none.
pub fn list_models(agy: &Path) -> Result<Vec<ModelOption>, LlmError> {
    let out = run_once(
        agy,
        &["models".to_string()],
        &std::env::temp_dir(),
        Duration::from_secs(60),
    )?;
    let models = parse_models(&out);
    if models.is_empty() {
        let head: String = out.trim().chars().take(300).collect();
        return Err(LlmError::Antigravity(format!(
            "agy models listed nothing: {head}"
        )));
    }
    Ok(models)
}

/// Finds the `agy` executable (installed by Antigravity into `~/.local/bin`).
///
/// Returns: the path, if found.
pub fn find_agy() -> Option<PathBuf> {
    crate::codex::find_executable("agy")
}

/// One recording handed to the model, relative to the run's working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    pub origin: AudioOrigin,
    /// File name inside the working directory, e.g. `microphone.m4a`.
    pub file_name: String,
}

/// A sentence the model heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeardLine {
    /// Which track the sentence came from.
    pub source: AudioOrigin,
    /// Offsets from the start of the audio file, in milliseconds.
    pub start_ms: u64,
    pub end_ms: u64,
    /// The model's own label ("说话人1"); only consistent within one run.
    pub speaker: String,
    pub text: String,
}

/// Schema for one transcription run: `{"transcript": [HeardLine…]}`.
///
/// Returns: a JSON Schema whose root is an object, as `--json-schema` requires.
pub fn transcript_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["transcript"],
        "properties": {
            "transcript": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["source", "start_ms", "end_ms", "speaker", "text"],
                    "properties": {
                        "source": {"type": "string", "enum": ["microphone", "system"]},
                        "start_ms": {"type": "integer"},
                        "end_ms": {"type": "integer"},
                        "speaker": {"type": "string"},
                        "text": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// Name of a language code for the prompt.
fn language_name(language: Option<&str>) -> &'static str {
    match language {
        Some("en") => "英文",
        Some("ja") => "日文",
        Some("zh") => "简体中文",
        _ => "录音里说的语言（中文用简体）",
    }
}

/// Builds the transcription prompt.
///
/// Params: `tracks` – files in the working directory; `context` – names and terms that are
/// likely to be spoken (the same hint the local refine pass gets); `language` – caption language.
/// Returns: the prompt text.
pub fn transcribe_prompt(
    tracks: &[AudioTrack],
    context: Option<&str>,
    language: Option<&str>,
) -> String {
    let mut out = String::from("用你的文件查看工具直接打开并听下面的会议录音：\n");
    for t in tracks {
        let what = match t.origin {
            AudioOrigin::Microphone => "本地麦克风，会议室里的人",
            AudioOrigin::System => "系统声音，远程参会者",
        };
        let source = match t.origin {
            AudioOrigin::Microphone => "microphone",
            AudioOrigin::System => "system",
        };
        out.push_str(&format!(
            "- ./{}（{what}，source 填 {source}）\n",
            t.file_name
        ));
    }
    out.push_str(&format!(
        "所有录音都从同一时刻开始计时。不要运行任何命令，也不要打开其他文件。\n\
         逐句转写，用{}。每句给出 source、start_ms 和 end_ms（毫秒，相对录音开头）、speaker、text。\n\
         speaker：同一段录音里能分辨不同的人就用 说话人1、说话人2……，同一个人始终用同一个标签。\n\
         只转写真正听到的话。没有人声的部分不要输出任何句子，听不清的地方宁可省略，绝对不要编造内容。\n",
        language_name(language)
    ));
    if let Some(c) = context.map(str::trim).filter(|c| !c.is_empty()) {
        out.push_str(&format!(
            "下面是会议标题、参会人和笔记里的术语，录音里出现时按这里的写法：\n{c}\n"
        ));
    }
    out
}

/// Builds the minutes prompt for a text-only run (the transcript is in the prompt).
///
/// Params: `input` – meeting material, with the transcript just produced.
/// Returns: the prompt text.
pub fn minutes_prompt(input: &MinutesInput) -> String {
    format!(
        "{}\n不要打开任何文件，也不要运行命令；只根据下面的材料输出纪要。\n\n{}",
        instructions(input.language.as_deref()),
        input.render()
    )
}

/// Schema for a run that transcribes and writes minutes at once:
/// `{"transcript": [HeardLine…], "minutes": Minutes}`.
///
/// Returns: a JSON Schema whose root is an object.
pub fn transcript_and_minutes_schema() -> Value {
    let transcript = transcript_schema()["properties"]["transcript"].clone();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["transcript", "minutes"],
        "properties": {
            "transcript": transcript,
            "minutes": minutes_json_schema()
        }
    })
}

/// Builds the prompt for a run that transcribes the audio and then writes the minutes.
///
/// Params: `tracks`, `context`, `language` – as for [`transcribe_prompt`]; `input` – the
/// meeting's title, attendees, notes and minutes language (its transcript is ignored).
/// Returns: the prompt text.
pub fn transcribe_and_minutes_prompt(
    tracks: &[AudioTrack],
    context: Option<&str>,
    language: Option<&str>,
    input: &MinutesInput,
) -> String {
    format!(
        "{}\n转写完成后，根据你转写的内容和下面的材料写会议纪要，放在 minutes 字段。\n{}\n         纪要里提到的人：能从对话判断出是参会者名单里的谁，就用名单里的名字，否则用说话人标签。\n         evidence_ms 填 transcript 里对应句子的 start_ms。\n\n{}",
        transcribe_prompt(tracks, context, language),
        instructions(input.language.as_deref()),
        input.render_context()
    )
}

/// Command-line arguments for one headless run.
///
/// Params: `prompt`; `model`; `schema` – JSON Schema for `structured_output`; `workdir` –
/// the only directory the agent may read.
/// Returns: arguments for `agy`.
pub fn agy_args(prompt: &str, model: &str, schema: &Value, workdir: &Path) -> Vec<String> {
    let settings = json!({
        "permissions": {"allow": [format!("read_file({}/**)", workdir.display())]}
    });
    vec![
        "-p".into(),
        prompt.into(),
        "--model".into(),
        model.into(),
        "--output-format".into(),
        "json".into(),
        "--json-schema".into(),
        schema.to_string(),
        "--settings".into(),
        settings.to_string(),
    ]
}

/// Structured output of one run.
#[derive(Debug, Clone, PartialEq)]
pub struct AgyOutput {
    pub value: Value,
    /// agy's error when the run was cut short after producing the output.
    pub interrupted: Option<String>,
}

/// Extracts `structured_output` from agy's JSON result.
///
/// Params: `stdout` – everything agy printed.
/// Returns: the structured output (flagged when the run ended in an error), or an error
/// naming agy's status and message when there is none.
pub fn parse_result(stdout: &str) -> Result<AgyOutput, LlmError> {
    // The JSON result is the last non-empty line; anything before it is diagnostics.
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| {
            let head: String = stdout.chars().take(300).collect();
            LlmError::Antigravity(format!("agy printed no JSON result: {head}"))
        })?;
    let v: Value = serde_json::from_str(line).map_err(|e| {
        let head: String = line.chars().take(300).collect();
        LlmError::Antigravity(format!("agy result is not JSON ({e}): {head}"))
    })?;
    let status = v["status"].as_str().unwrap_or("");
    let failure = format!(
        "agy status {status:?}: {}",
        v["error"].as_str().unwrap_or("no error message")
    );
    let interrupted = (status != "SUCCESS").then(|| failure.clone());
    if let Some(denied) = v["denied_actions"].as_array().filter(|d| !d.is_empty()) {
        // The agent wanted something beyond reading the audio; the result is usually empty.
        tracing::warn!("agy denied actions: {denied:?}");
    }
    let value = match &v["structured_output"] {
        Value::Null if interrupted.is_some() => return Err(LlmError::Antigravity(failure)),
        Value::Null => {
            return Err(LlmError::Antigravity(format!(
                "agy returned no structured output (denied actions: {}): {}",
                v["denied_actions"],
                v["response"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        // Some versions return the object as a JSON string.
        Value::String(s) => serde_json::from_str(s)
            .map_err(|e| LlmError::Antigravity(format!("structured_output is not JSON ({e})")))?,
        other => other.clone(),
    };
    Ok(AgyOutput { value, interrupted })
}

/// Runs `agy` once with a hard deadline.
///
/// Params: `agy`; `args`; `workdir` – current directory and agent workspace; `timeout`.
/// Returns: stdout.
fn run_once(
    agy: &Path,
    args: &[String],
    workdir: &Path,
    timeout: Duration,
) -> Result<String, LlmError> {
    let mut child = Command::new(agy)
        .args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| LlmError::Antigravity(format!("starting {}: {e}", agy.display())))?;
    // Drain both pipes on threads so a chatty agy never blocks on a full pipe.
    let mut out = child.stdout.take().expect("stdout piped");
    let mut err = child.stderr.take().expect("stderr piped");
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        out.read_to_string(&mut s).map(|_| s)
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LlmError::Antigravity(format!(
                    "agy did not finish within {} s",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(LlmError::Antigravity(format!("waiting for agy: {e}"))),
        }
    };
    let stdout = out_t
        .join()
        .map_err(|_| LlmError::Antigravity("agy stdout reader panicked".into()))?
        .map_err(|e| LlmError::Antigravity(format!("reading agy stdout: {e}")))?;
    let stderr = err_t.join().unwrap_or_default();
    if !stderr.trim().is_empty() {
        tracing::info!("agy stderr: {}", stderr.trim());
    }
    if !status.success() && !stdout.contains("\"status\"") {
        return Err(LlmError::Antigravity(format!(
            "agy exited with {status}: {}",
            stderr.trim().chars().take(300).collect::<String>()
        )));
    }
    Ok(stdout)
}

/// Runs agy and parses the structured output, retrying once: the backend occasionally
/// answers 500 and agy then reports "The stream was interrupted". A clean run wins; an
/// interrupted run's output is used only when no attempt finished cleanly.
///
/// Params: `agy`; `prompt`; `model`; `schema`; `workdir`; `timeout` – per attempt.
/// Returns: the structured output.
fn run_structured(
    agy: &Path,
    prompt: &str,
    model: &str,
    schema: &Value,
    workdir: &Path,
    timeout: Duration,
) -> Result<AgyOutput, LlmError> {
    let args = agy_args(prompt, model, schema, workdir);
    let mut partial = None;
    let mut last = None;
    for attempt in 1..=2 {
        match run_once(agy, &args, workdir, timeout).and_then(|out| parse_result(&out)) {
            Ok(o) if o.interrupted.is_none() => return Ok(o),
            Ok(o) => {
                tracing::warn!(
                    "agy attempt {attempt} interrupted with output: {:?}",
                    o.interrupted
                );
                partial = Some(o);
            }
            Err(e) => {
                tracing::warn!("agy attempt {attempt} failed: {e}");
                last = Some(e);
            }
        }
    }
    partial.ok_or_else(|| last.expect("an attempt failed without output"))
}

/// What one transcription run heard.
#[derive(Debug, Clone, PartialEq)]
pub struct Heard {
    pub lines: Vec<HeardLine>,
    /// Set when the run was cut short; the lines may miss the end of the audio.
    pub interrupted: Option<String>,
}

/// Transcribes the tracks in `workdir`.
///
/// Params: `agy`; `workdir` – holds the tracks and nothing else the agent should read;
/// `tracks`; `context` – spelling hint; `language` – caption language; `model`; `timeout` – per attempt.
/// Returns: the heard sentences, in the model's order.
pub fn transcribe(
    agy: &Path,
    workdir: &Path,
    tracks: &[AudioTrack],
    context: Option<&str>,
    language: Option<&str>,
    model: &str,
    timeout: Duration,
) -> Result<Heard, LlmError> {
    let prompt = transcribe_prompt(tracks, context, language);
    let out = run_structured(agy, &prompt, model, &transcript_schema(), workdir, timeout)?;
    heard_from(&out.value, out.interrupted)
}

/// Reads the `transcript` array of a run's structured output.
///
/// Params: `value` – structured output; `interrupted` – the run's error, if it was cut short.
/// Returns: the non-empty, well-ordered lines.
fn heard_from(value: &Value, interrupted: Option<String>) -> Result<Heard, LlmError> {
    let lines: Vec<HeardLine> = serde_json::from_value(value["transcript"].clone())
        .map_err(|e| LlmError::Antigravity(format!("transcript did not match the schema ({e})")))?;
    Ok(Heard {
        lines: lines
            .into_iter()
            .filter(|l| !l.text.trim().is_empty() && l.end_ms >= l.start_ms)
            .collect(),
        interrupted,
    })
}

/// Transcribes the tracks in `workdir` and writes the minutes in the same run, so the model
/// hears the meeting only once.
///
/// Params: as for [`transcribe`]; `input` – title, attendees, notes and minutes language.
/// Returns: what was heard, plus the minutes or why they could not be read (the transcript
/// is still usable then).
#[allow(clippy::too_many_arguments)]
pub fn transcribe_with_minutes(
    agy: &Path,
    workdir: &Path,
    tracks: &[AudioTrack],
    context: Option<&str>,
    language: Option<&str>,
    input: &MinutesInput,
    model: &str,
    timeout: Duration,
) -> Result<(Heard, Result<Minutes, LlmError>), LlmError> {
    let prompt = transcribe_and_minutes_prompt(tracks, context, language, input);
    let out = run_structured(
        agy,
        &prompt,
        model,
        &transcript_and_minutes_schema(),
        workdir,
        timeout,
    )?;
    let minutes = serde_json::from_value(out.value["minutes"].clone())
        .map_err(|e| LlmError::Antigravity(format!("minutes did not match the schema ({e})")));
    Ok((heard_from(&out.value, out.interrupted)?, minutes))
}

/// Writes minutes from a transcript (no files involved).
///
/// Params: `agy`; `workdir` – an empty directory; `input`; `model`; `timeout` – per attempt.
/// Returns: parsed minutes.
pub fn generate_minutes(
    agy: &Path,
    workdir: &Path,
    input: &MinutesInput,
    model: &str,
    timeout: Duration,
) -> Result<Minutes, LlmError> {
    let out = run_structured(
        agy,
        &minutes_prompt(input),
        model,
        &minutes_json_schema(),
        workdir,
        timeout,
    )?;
    if let Some(why) = &out.interrupted {
        tracing::warn!("using minutes from an interrupted agy run: {why}");
    }
    serde_json::from_value(out.value)
        .map_err(|e| LlmError::Antigravity(format!("minutes did not match the schema ({e})")))
}

/// Writes minutes from a transcript in a fresh empty directory, removed afterwards.
///
/// Params: `agy`; `input`; `model`; `timeout` – per attempt.
/// Returns: parsed minutes.
pub fn generate_minutes_in_scratch(
    agy: &Path,
    input: &MinutesInput,
    model: &str,
    timeout: Duration,
) -> Result<Minutes, LlmError> {
    // Unique per process and call, so two meetings writing minutes at once never share it.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("friston-agy-minutes-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| LlmError::Antigravity(format!("creating {}: {e}", dir.display())))?;
    let result = generate_minutes(agy, &dir, input, model, timeout);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        tracing::warn!("removing {}: {e}", dir.display());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_requires_success_status() {
        let ok = r#"{"status":"SUCCESS","response":"","structured_output":{"transcript":[]}}"#;
        let out = parse_result(ok).unwrap();
        assert_eq!(out.value, json!({"transcript": []}));
        assert_eq!(out.interrupted, None);

        // An interrupted run keeps its output but is flagged, so callers can prefer a retry.
        let interrupted = r#"{"status":"ERROR","error":"The stream was interrupted.","structured_output":{"transcript":[]}}"#;
        let out = parse_result(interrupted).unwrap();
        assert!(out.interrupted.unwrap().contains("interrupted"));
        let failed = r#"{"status":"ERROR","error":"quota"}"#;
        assert!(
            parse_result(failed)
                .unwrap_err()
                .to_string()
                .contains("quota")
        );

        let denied =
            r#"{"status":"SUCCESS","response":"","denied_actions":[{"action":"command"}]}"#;
        let err = parse_result(denied).unwrap_err().to_string();
        assert!(err.contains("command"), "{err}");

        let as_string =
            "log line\n{\"status\":\"SUCCESS\",\"structured_output\":\"{\\\"a\\\":1}\"}\n";
        assert_eq!(parse_result(as_string).unwrap().value, json!({"a": 1}));
        assert!(parse_result("jetski: no output").is_err());
    }

    #[test]
    fn args_only_allow_reading_the_workdir() {
        let args = agy_args("p", DEFAULT_MODEL, &transcript_schema(), Path::new("/w"));
        assert!(!args.iter().any(|a| a.contains("dangerously")));
        let settings = &args[args.iter().position(|a| a == "--settings").unwrap() + 1];
        let v: Value = serde_json::from_str(settings).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["read_file(/w/**)"]));
        let schema = &args[args.iter().position(|a| a == "--json-schema").unwrap() + 1];
        let v: Value = serde_json::from_str(schema).unwrap();
        assert_eq!(v["type"], "object");
    }

    #[test]
    fn prompt_names_each_track_and_forbids_inventing() {
        let tracks = [
            AudioTrack {
                origin: AudioOrigin::Microphone,
                file_name: "microphone.m4a".into(),
            },
            AudioTrack {
                origin: AudioOrigin::System,
                file_name: "system.m4a".into(),
            },
        ];
        let p = transcribe_prompt(&tracks, Some("会议：订单迁移\n参会人：赵磊"), Some("zh"));
        assert!(p.contains("./microphone.m4a（本地麦克风，会议室里的人，source 填 microphone）"));
        assert!(p.contains("./system.m4a（系统声音，远程参会者，source 填 system）"));
        assert!(p.contains("不要编造"));
        assert!(p.contains("赵磊"));
        assert!(!transcribe_prompt(&tracks, Some(" "), None).contains("参会人"));
    }

    #[test]
    fn models_parse_from_agy_listing() {
        let out = "Fetching available models...\ngemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)\nclaude-opus-5-5-high\tClaude Opus 5.5 (High)\n\n";
        let models = parse_models(out);
        assert_eq!(models.len(), 2);
        assert!(models[0].is_default);
        assert_eq!(models[1].id, "claude-opus-5-5-high");
        assert_eq!(models[1].label, "Claude Opus 5.5 (High)");
        assert!(refine_capable("gemini-4-argon-high"));
        assert!(!refine_capable("gemini-3.8-flash-low"));
        assert!(!refine_capable("claude-opus-5-5-high"));
    }

    #[test]
    fn combined_run_asks_for_transcript_then_minutes() {
        let schema = transcript_and_minutes_schema();
        assert_eq!(schema["required"], json!(["transcript", "minutes"]));
        assert_eq!(schema["properties"]["minutes"], minutes_json_schema());
        let tracks = [AudioTrack {
            origin: AudioOrigin::Microphone,
            file_name: "microphone.m4a".into(),
        }];
        let input = MinutesInput {
            title: "订单迁移".into(),
            attendees: vec!["赵磊".into()],
            transcript: vec![crate::TranscriptLine {
                start_ms: 1,
                speaker: "x".into(),
                text: "不应出现的实时转写".into(),
            }],
            language: Some("en".into()),
            ..Default::default()
        };
        let p = transcribe_and_minutes_prompt(&tracks, None, Some("zh"), &input);
        assert!(p.contains("./microphone.m4a"));
        assert!(p.contains("minutes 字段"));
        assert!(p.contains("参会者：赵磊"));
        assert!(p.contains("English"), "minutes language follows the input");
        assert!(!p.contains("不应出现"), "the live transcript is not sent");
    }

    #[test]
    fn heard_lines_parse_from_schema_output() {
        let v = json!({"transcript": [
            {"source": "system", "start_ms": 8000, "end_ms": 9000, "speaker": "说话人2", "text": "可以"}
        ]});
        let lines: Vec<HeardLine> = serde_json::from_value(v["transcript"].clone()).unwrap();
        assert_eq!(lines[0].source, AudioOrigin::System);
        assert_eq!(model_label(DEFAULT_MODEL), "Gemini 3.8 Flash");
        assert_eq!(model_label("x"), "x");
        assert_eq!(
            mm_i18n::with_lang(mm_i18n::Lang::En, || model_label("gemini-3.8-flash-high")),
            "Gemini 3.8 Flash (deep thinking)"
        );
        assert_eq!(
            model_label("gemini-3.8-flash-high"),
            "Gemini 3.8 Flash（深度思考）"
        );
    }
}
