//! Stable, heading-aware Markdown chunking.

use serde::{Deserialize, Serialize};

/// Soft upper bound for a text chunk, in characters (≈ 300–600 tokens for CJK/Latin mixes).
pub const TARGET_CHARS: usize = 1200;

/// One embeddable span of a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextChunk {
    /// Position within the note (for display ordering only, not identity).
    pub ordinal: usize,
    /// Heading breadcrumbs, e.g. `["Decisions", "Budget"]`.
    pub heading_path: Vec<String>,
    /// Note title, passed to the embedder as the document title (EmbeddingGemma 2
    /// applies its own task prompt, so the text must not be pre-formatted).
    pub title: String,
    /// Breadcrumbs + body: the text handed to the embedder.
    pub text: String,
    /// blake3 over title and text; identity for change detection.
    pub hash: String,
}

/// Splits a Markdown note into chunks whose boundaries survive unrelated edits.
///
/// Boundaries are heading sections; inside a section, blank-line separated blocks
/// are packed greedily from the top, so appending text (the common case while
/// taking notes live) only changes the last chunk of that section.
///
/// Params: `title` – note title used in the EmbeddingGemma document prompt; `markdown` – note body.
/// Returns: chunks in document order.
pub fn chunk_markdown(title: &str, markdown: &str) -> Vec<TextChunk> {
    let mut chunks = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;

    let body = strip_frontmatter(markdown);
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if !in_fence {
            if let Some((level, text)) = parse_heading(trimmed) {
                flush_block(&mut current, &mut blocks);
                pack_section(title, &headings, std::mem::take(&mut blocks), &mut chunks);
                headings.truncate(level - 1);
                while headings.len() < level - 1 {
                    headings.push(String::new());
                }
                headings.push(text.to_string());
                continue;
            }
            if trimmed.is_empty() {
                flush_block(&mut current, &mut blocks);
                continue;
            }
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    flush_block(&mut current, &mut blocks);
    pack_section(title, &headings, blocks, &mut chunks);
    chunks
}

/// Removes a leading YAML frontmatter block (metadata is indexed separately).
fn strip_frontmatter(md: &str) -> &str {
    if let Some(rest) = md.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let after = &rest[end + 4..];
        return after.strip_prefix('\n').unwrap_or(after);
    }
    md
}

/// Parses an ATX heading (`#`..`######`).
fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&level) && line[level..].starts_with(' ') {
        Some((level, line[level..].trim()))
    } else {
        None
    }
}

/// Moves the block being accumulated into the section's block list.
fn flush_block(current: &mut String, blocks: &mut Vec<String>) {
    if !current.trim().is_empty() {
        blocks.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

/// Greedily packs one section's blocks into chunks of at most [`TARGET_CHARS`].
fn pack_section(title: &str, headings: &[String], blocks: Vec<String>, out: &mut Vec<TextChunk>) {
    let path: Vec<String> = headings.iter().filter(|h| !h.is_empty()).cloned().collect();
    let mut body = String::new();
    for block in blocks {
        if !body.is_empty() && body.chars().count() + block.chars().count() > TARGET_CHARS {
            push_chunk(title, &path, std::mem::take(&mut body), out);
        }
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&block);
    }
    if !body.is_empty() {
        push_chunk(title, &path, body, out);
    }
}

/// Builds the embedder input (breadcrumbs + body) and hashes it together with the title.
fn push_chunk(title: &str, path: &[String], body: String, out: &mut Vec<TextChunk>) {
    let crumbs = if path.is_empty() {
        String::new()
    } else {
        format!("{}\n", path.join(" > "))
    };
    let text = format!("{crumbs}{body}");
    let mut hasher = blake3::Hasher::new();
    // Unit separator keeps ("ab","c") and ("a","bc") from colliding.
    hasher
        .update(title.as_bytes())
        .update(b"\x1f")
        .update(text.as_bytes());
    let hash = hasher.finalize().to_hex().to_string();
    out.push(TextChunk {
        ordinal: out.len(),
        heading_path: path.to_vec(),
        title: title.to_string(),
        text,
        hash,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ntype: meeting\n---\n# Q3 预算评审\n\n开场。\n\n## 决策\n\n- 预算上调 5%\n\n```\n# not a heading\n```\n\n## 待办\n\n- [ ] Alice 跟进\n";

    #[test]
    fn splits_by_heading_and_ignores_frontmatter_and_fences() {
        let chunks = chunk_markdown("Q3 review", NOTE);
        let paths: Vec<_> = chunks.iter().map(|c| c.heading_path.join(">")).collect();
        assert_eq!(
            paths,
            ["Q3 预算评审", "Q3 预算评审>决策", "Q3 预算评审>待办"]
        );
        assert!(chunks[1].text.contains("# not a heading"));
        assert!(!chunks[0].text.contains("type: meeting"));
        assert!(
            chunks[0].text.starts_with("Q3 预算评审\n"),
            "breadcrumbs lead the text"
        );
    }

    #[test]
    fn appending_only_changes_the_last_chunk() {
        let before = chunk_markdown("t", NOTE);
        let after = chunk_markdown("t", &format!("{NOTE}- [ ] Bob 补充数据\n"));
        assert_eq!(before.len(), after.len());
        assert_eq!(before[..2], after[..2]);
        assert_ne!(before[2].hash, after[2].hash);
    }

    #[test]
    fn long_sections_are_packed_under_the_target() {
        let para = "字".repeat(700);
        let md = format!("# A\n\n{para}\n\n{para}\n\n{para}\n");
        let chunks = chunk_markdown("t", &md);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.heading_path == ["A"]));
    }
}
