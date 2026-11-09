//! Meeting-note rendering and safe, partial rewriting.

use std::path::Path;

use chrono::{DateTime, Local, Utc};

use mm_i18n::{tr, trf};

use crate::VaultError;

/// Data needed to create the initial note for a meeting.
#[derive(Debug, Clone)]
pub struct MeetingNote {
    pub meeting_id: String,
    pub title: String,
    pub starts_at: DateTime<Utc>,
    /// Display names; rendered as `[[wikilinks]]` so Obsidian builds a people graph.
    pub attendees: Vec<String>,
    pub tags: Vec<String>,
    /// Absolute path of the recording, linked from the note (media stays outside the vault).
    pub recording_path: Option<String>,
}

impl MeetingNote {
    /// Vault-relative path, e.g. `Meetings/2026/2026-11-05 1400 Q3 Review.md`.
    ///
    /// Returns: a path with characters that are illegal in Obsidian links replaced.
    pub fn relative_path(&self) -> String {
        let local = self.starts_at.with_timezone(&Local);
        let safe: String = self
            .title
            .chars()
            .map(|c| {
                if matches!(c, '/' | '\\' | ':' | '#' | '^' | '|' | '[' | ']') {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        format!(
            "Meetings/{}/{} {}.md",
            local.format("%Y"),
            local.format("%Y-%m-%d %H%M"),
            safe.trim()
        )
    }

    /// Renders the initial Markdown with frontmatter, a user-owned notes area and empty managed blocks.
    ///
    /// Headings and placeholders follow the current UI language. Notes made earlier keep
    /// their headings (Chinese, English or the old bilingual "## 笔记 Notes") because only
    /// managed blocks are ever rewritten, so readers of notes must accept all of them.
    ///
    /// Returns: the full note text.
    pub fn render(&self) -> String {
        // JSON string literals are valid YAML scalars, which gives us correct quoting for free.
        let q = |s: &str| serde_json::to_string(s).expect("string serialization cannot fail");
        let attendees: Vec<String> = self
            .attendees
            .iter()
            .map(|a| q(&format!("[[{a}]]")))
            .collect();
        let tags: Vec<String> = self.tags.iter().map(|t| q(t)).collect();
        let recording = self
            .recording_path
            .as_deref()
            .map(|p| {
                trf!(
                    "\n录音：[打开]({})\n",
                    "\nRecording: [Open]({})\n",
                    p.replace(' ', "%20")
                )
            })
            .unwrap_or_default();
        let (notes, minutes, actions) = (
            tr!("笔记", "Notes"),
            tr!("纪要", "Minutes"),
            tr!("待办", "Action items"),
        );
        format!(
            "---\ntype: meeting\nmeeting_id: {id}\ntitle: {title}\ndate: {date}\nattendees: [{att}]\ntags: [{tags}]\nstatus: scheduled\n---\n\
# {heading}\n{recording}\n## {notes}\n\n\n## {minutes}\n{minutes_block}\n## {actions}\n{actions_block}",
            id = q(&self.meeting_id),
            title = q(&self.title),
            date = self.starts_at.to_rfc3339(),
            att = attendees.join(", "),
            tags = tags.join(", "),
            heading = self.title,
            minutes_block = managed_block(
                "minutes",
                tr!("_会后自动生成_", "_Generated after the meeting_")
            ),
            actions_block = managed_block("actions", ""),
        )
    }
}

/// Renders an empty managed block.
fn managed_block(name: &str, body: &str) -> String {
    format!("<!-- mm:{name}:start -->\n{body}\n<!-- mm:{name}:end -->\n")
}

/// Replaces the body of managed block `name`, leaving everything else byte-identical.
///
/// Params: `doc` – current note text (possibly edited by the user); `name` – block name; `body` – new content.
/// Returns: the updated note; if the block was deleted by the user it is appended at the end.
pub fn replace_managed_block(doc: &str, name: &str, body: &str) -> Result<String, VaultError> {
    let start = format!("<!-- mm:{name}:start -->");
    let end = format!("<!-- mm:{name}:end -->");
    match (doc.find(&start), doc.find(&end)) {
        (Some(s), Some(e)) if s < e => {
            let head = &doc[..s + start.len()];
            let tail = &doc[e..];
            Ok(format!("{head}\n{body}\n{tail}"))
        }
        (None, None) => {
            let sep = if doc.ends_with('\n') { "" } else { "\n" };
            Ok(format!("{doc}{sep}\n{}", managed_block(name, body)))
        }
        _ => Err(VaultError::UnbalancedBlock(name.to_string())),
    }
}

/// Writes `contents` via a temp file + rename so Obsidian never sees a half-written note.
///
/// Params: `path` – destination; `contents` – full file text.
/// Returns: `Ok(())` once the rename succeeded.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), VaultError> {
    let err = |source| VaultError::Write {
        path: path.display().to_string(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let tmp = path.with_extension("md.mm-tmp");
    std::fs::write(&tmp, contents).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn note() -> MeetingNote {
        MeetingNote {
            meeting_id: "0190".into(),
            title: "Q3 预算评审: final".into(),
            starts_at: Utc.with_ymd_and_hms(2026, 11, 5, 6, 0, 0).unwrap(),
            attendees: vec!["Alice".into(), "王伟".into()],
            tags: vec!["budget".into()],
            recording_path: None,
        }
    }

    #[test]
    fn renders_frontmatter_links_and_blocks() {
        let md = note().render();
        assert!(md.starts_with("---\ntype: meeting\n"));
        assert!(md.contains(r#"attendees: ["[[Alice]]", "[[王伟]]"]"#));
        assert!(md.contains("<!-- mm:minutes:start -->"));
        assert!(note().relative_path().ends_with("Q3 预算评审- final.md"));
    }

    #[test]
    fn regenerating_minutes_preserves_user_edits() {
        let mut md = note().render();
        md = md.replace("## 笔记\n\n", "## 笔记\n\n我的私人笔记\n");
        let updated = replace_managed_block(&md, "minutes", "- 决定：预算上调 5%").unwrap();
        assert!(updated.contains("我的私人笔记"));
        assert!(
            updated.contains(
                "<!-- mm:minutes:start -->\n- 决定：预算上调 5%\n<!-- mm:minutes:end -->"
            )
        );
        assert!(!updated.contains("会后自动生成"));
        assert_eq!(
            replace_managed_block(&updated, "minutes", "x")
                .unwrap()
                .matches("mm:minutes:start")
                .count(),
            1
        );
    }

    #[test]
    fn headings_follow_the_ui_language() {
        use mm_i18n::{Lang, with_lang};
        let mut n = note();
        n.recording_path = Some("/r/a b.m4a".into());
        let zh = with_lang(Lang::Zh, || n.render());
        assert!(zh.contains("\n录音：[打开](/r/a%20b.m4a)\n"));
        assert!(zh.contains("\n## 笔记\n\n\n## 纪要\n<!-- mm:minutes:start -->\n_会后自动生成_\n"));
        assert!(zh.contains("\n## 待办\n<!-- mm:actions:start -->"));
        let en = with_lang(Lang::En, || n.render());
        assert!(en.contains("\nRecording: [Open](/r/a%20b.m4a)\n"));
        assert!(en.contains(
            "\n## Notes\n\n\n## Minutes\n<!-- mm:minutes:start -->\n_Generated after the meeting_\n"
        ));
        assert!(en.contains("\n## Action items\n<!-- mm:actions:start -->"));
        assert!(!en.contains("笔记"));
    }

    #[test]
    fn deleted_block_is_appended_and_broken_block_is_rejected() {
        let appended = replace_managed_block("# t\n", "minutes", "m").unwrap();
        assert!(appended.ends_with("<!-- mm:minutes:start -->\nm\n<!-- mm:minutes:end -->\n"));
        assert!(replace_managed_block("<!-- mm:minutes:start -->", "minutes", "m").is_err());
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Meetings/2026/n.md");
        write_atomic(&p, "hi").unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "hi");
    }
}
