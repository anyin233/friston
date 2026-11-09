//! Live check against the local `codex` CLI. Ignored by default because it uses the
//! user's ChatGPT account; run with `cargo test -p mm-llm --test codex_live -- --ignored`.

use std::time::Duration;

use mm_llm::{MinutesInput, TranscriptLine, codex};

/// Signs in check + one tiny minutes turn through `codex app-server`.
#[test]
#[ignore = "uses the signed-in Codex account"]
fn codex_generates_schema_valid_minutes() {
    let path = codex::find_codex().expect("codex CLI not found");
    let account = codex::read_account(&path).expect("account/read");
    assert!(
        account.signed_in,
        "codex is not signed in; run `codex login`"
    );
    let input = MinutesInput {
        title: "冒烟测试".into(),
        when: "2026-11-05 14:00".into(),
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
        codex::generate(&path, &input, None, Duration::from_secs(600)).expect("codex minutes");
    println!("{minutes:#?}");
    let models = codex::list_models(&path).expect("model/list");
    println!("{models:#?}");
    assert!(!models.is_empty());
    assert!(!minutes.summary.is_empty());
    assert!(minutes.action_items.iter().any(|a| a.task.contains("预算")));
}
