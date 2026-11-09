//! What every provider is asked: instructions plus the meeting material.

use serde::{Deserialize, Serialize};

/// One transcript line handed to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub start_ms: u64,
    /// Display name ("赵磊", "说话人 2", "远程 · 说话人 1").
    pub speaker: String,
    pub text: String,
}

/// Everything known about a meeting when minutes are generated.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MinutesInput {
    pub title: String,
    /// Local date/time string for the header (the model should not convert time zones).
    pub when: String,
    pub attendees: Vec<String>,
    /// The user's own notes (Markdown, may be empty).
    pub notes: String,
    pub transcript: Vec<TranscriptLine>,
    /// Absolute paths of screenshots/slides taken during the meeting.
    pub images: Vec<String>,
    /// Output language for the minutes, e.g. `zh` (default Chinese).
    pub language: Option<String>,
}

/// System/developer instructions shared by every provider.
///
/// Params: `language` – desired output language code.
/// Returns: the instruction text.
pub fn instructions(language: Option<&str>) -> String {
    let lang = match language.unwrap_or("zh") {
        "en" => "English",
        "ja" => "Japanese",
        _ => "简体中文",
    };
    format!(
        "你是会议纪要助手。根据会议转写、参会者自己的笔记和截图，写一份准确、简洁的结构化纪要。\n\
         - 用{lang}书写；人名、产品名和术语保持原文。\n\
         - 只写材料里有依据的内容，不要编造；转写可能有识别错误，结合上下文纠正明显的错字。\n\
         - summary：3–6 句，先说结论和决定，再说背景。\n\
         - decisions：明确达成的决定，每条一句话。\n\
         - action_items：谁、做什么、何时；owner 用转写里的名字，未提到则为 null；due 用原话里的时间，未提到为 null；\
           evidence_ms 填支持这条待办的转写时间戳（毫秒），找不到为 null。\n\
         - open_questions：没有结论、需要跟进的问题。topics：3–8 个主题短语。\n\
         - title：一个能概括会议的标题，可沿用原标题。\n\
         - 参会者笔记代表用户的关注点，优先保留其中的内容。"
    )
}

/// Formats milliseconds as `[mm:ss]`.
fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

impl MinutesInput {
    /// Renders title, time, attendees and notes, without the transcript. Used when the
    /// model transcribes the audio itself in the same run.
    ///
    /// Returns: the header and notes text.
    pub fn render_context(&self) -> String {
        let mut out = format!("# 会议：{}\n时间：{}\n", self.title, self.when);
        if !self.attendees.is_empty() {
            out.push_str(&format!("参会者：{}\n", self.attendees.join("、")));
        }
        out.push_str("\n## 参会者笔记\n");
        if self.notes.trim().is_empty() {
            out.push_str("（无）\n");
        } else {
            out.push_str(self.notes.trim());
            out.push('\n');
        }
        out
    }

    /// Renders the material as one text block. Timestamps carry raw milliseconds so the
    /// model can copy them into `evidence_ms`.
    ///
    /// Returns: the user message text.
    pub fn render(&self) -> String {
        let mut out = self.render_context();
        out.push_str("\n## 转写（[时间 | 毫秒] 说话人：内容）\n");
        if self.transcript.is_empty() {
            out.push_str("（无转写）\n");
        }
        for l in &self.transcript {
            out.push_str(&format!(
                "[{} | {}] {}：{}\n",
                clock(l.start_ms),
                l.start_ms,
                l.speaker,
                l.text.trim()
            ));
        }
        if !self.images.is_empty() {
            out.push_str(&format!("\n另附 {} 张会中截图。\n", self.images.len()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_notes_and_millisecond_evidence() {
        let input = MinutesInput {
            title: "周会".into(),
            when: "2026-11-05 14:00".into(),
            attendees: vec!["赵磊".into(), "Alice".into()],
            notes: "- 关注预算".into(),
            transcript: vec![TranscriptLine {
                start_ms: 61_500,
                speaker: "赵磊".into(),
                text: "预算上调 5%".into(),
            }],
            images: vec!["/a.png".into()],
            language: None,
        };
        let text = input.render();
        assert!(text.contains("参会者：赵磊、Alice"));
        assert!(text.contains("[01:01 | 61500] 赵磊：预算上调 5%"));
        assert!(text.contains("- 关注预算"));
        let context = input.render_context();
        assert!(context.contains("参会者：赵磊、Alice") && !context.contains("61500"));
        assert!(instructions(None).contains("简体中文"));
    }
}
