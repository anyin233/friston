//! Renders [`Minutes`] into the note's managed blocks.

use crate::Minutes;

/// Markdown for the `minutes` and `actions` managed blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMinutes {
    pub minutes: String,
    pub actions: String,
}

/// Formats milliseconds as `mm:ss` / `h:mm:ss`.
fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Renders minutes as Markdown. Action items become Obsidian tasks so they show up in
/// the user's task queries.
///
/// Params: `m` – minutes; `seek` – builds a link for a transcript offset (e.g. a
/// `meeting-master://` URL), or `None` to print plain timestamps.
/// Returns: the two block bodies.
pub fn render_minutes(m: &Minutes, seek: Option<&dyn Fn(u64) -> String>) -> RenderedMinutes {
    let mut minutes = format!("### 摘要\n\n{}\n", m.summary.trim());
    let list = |title: &str, items: &[String]| -> String {
        if items.is_empty() {
            return String::new();
        }
        let body: Vec<String> = items.iter().map(|i| format!("- {}", i.trim())).collect();
        format!("\n### {title}\n\n{}\n", body.join("\n"))
    };
    minutes.push_str(&list("决定", &m.decisions));
    minutes.push_str(&list("待讨论", &m.open_questions));
    if !m.topics.is_empty() {
        let tags: Vec<String> = m.topics.iter().map(|t| format!("`{}`", t.trim())).collect();
        minutes.push_str(&format!("\n主题：{}\n", tags.join(" ")));
    }

    let actions: Vec<String> = m
        .action_items
        .iter()
        .map(|a| {
            let mut line = format!("- [ ] {}", a.task.trim());
            if let Some(owner) = a.owner.as_deref().filter(|o| !o.trim().is_empty()) {
                line.push_str(&format!(" · [[{}]]", owner.trim()));
            }
            if let Some(due) = a.due.as_deref().filter(|d| !d.trim().is_empty()) {
                line.push_str(&format!(" · 📅 {}", due.trim()));
            }
            if let Some(ms) = a.evidence_ms {
                match seek {
                    Some(f) => line.push_str(&format!(" · [{}]({})", clock(ms), f(ms))),
                    None => line.push_str(&format!(" · {}", clock(ms))),
                }
            }
            line
        })
        .collect();
    RenderedMinutes {
        minutes,
        actions: if actions.is_empty() {
            "_没有待办_".into()
        } else {
            actions.join("\n")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ActionItem;

    #[test]
    fn renders_sections_tasks_and_seek_links() {
        let m = Minutes {
            title: "周会".into(),
            summary: "预算上调。".into(),
            decisions: vec!["预算上调 5%".into()],
            action_items: vec![
                ActionItem {
                    owner: Some("赵磊".into()),
                    task: "更新预算表".into(),
                    due: Some("周五".into()),
                    evidence_ms: Some(61_000),
                },
                ActionItem {
                    owner: None,
                    task: "约下次会议".into(),
                    due: None,
                    evidence_ms: None,
                },
            ],
            open_questions: vec![],
            topics: vec!["预算".into()],
        };
        let seek = |ms: u64| format!("mm://seek?t={ms}");
        let r = render_minutes(&m, Some(&seek));
        assert!(r.minutes.contains("### 决定\n\n- 预算上调 5%"));
        assert!(!r.minutes.contains("待讨论"), "empty sections are omitted");
        assert_eq!(
            r.actions,
            "- [ ] 更新预算表 · [[赵磊]] · 📅 周五 · [01:01](mm://seek?t=61000)\n- [ ] 约下次会议"
        );
    }
}
