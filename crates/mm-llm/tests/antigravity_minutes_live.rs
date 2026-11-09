//! Live check of minutes through the Antigravity CLI. Ignored by default because it uses the
//! signed-in Antigravity account; run with
//! `cargo test -p mm-llm --test antigravity_minutes_live -- --ignored --nocapture`
//! (`MM_AGY_MINUTES_MODEL=<agy model id>` picks the model; default: the first Claude model).

use std::time::Duration;

use mm_llm::{MinutesInput, TranscriptLine, antigravity};

/// Lists models, then writes minutes from a two-line transcript with the chosen model.
#[test]
#[ignore = "uses the signed-in Antigravity account"]
fn antigravity_lists_models_and_writes_minutes() {
    let agy = antigravity::find_agy().expect("agy not found");
    let models = antigravity::list_models(&agy).expect("agy models");
    println!("{models:#?}");
    let model = std::env::var("MM_AGY_MINUTES_MODEL").unwrap_or_else(|_| {
        models
            .iter()
            .find(|m| m.id.starts_with("claude-"))
            .unwrap_or(&models[0])
            .id
            .clone()
    });
    let input = MinutesInput {
        title: "冒烟测试".into(),
        when: "2026-11-09 10:00".into(),
        attendees: vec!["赵磊".into(), "Alice".into()],
        transcript: vec![
            TranscriptLine {
                start_ms: 5_000,
                speaker: "赵磊".into(),
                text: "下季度预算上调百分之五。".into(),
            },
            TranscriptLine {
                start_ms: 12_000,
                speaker: "Alice".into(),
                text: "我周五前更新预算表。".into(),
            },
        ],
        ..Default::default()
    };
    let minutes =
        antigravity::generate_minutes_in_scratch(&agy, &input, &model, Duration::from_secs(600))
            .expect("agy minutes");
    println!("{model}: {minutes:#?}");
    assert!(!minutes.summary.is_empty());
    assert!(minutes.action_items.iter().any(|a| a.task.contains("预算")));
}
