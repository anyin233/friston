//! `obsidian://` URI construction.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Characters left unescaped in URI query values (RFC 3986 unreserved set).
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Builds a URI that opens `file` (vault-relative, `.md` optional) in `vault`.
///
/// Params: `vault` – vault folder name; `file` – vault-relative note path.
/// Returns: e.g. `obsidian://open?vault=Work%20Notes&file=Meetings%2F2026-11-05`.
pub fn open_note_uri(vault: &str, file: &str) -> String {
    let file = file.strip_suffix(".md").unwrap_or(file);
    format!(
        "obsidian://open?vault={}&file={}",
        utf8_percent_encode(vault, QUERY),
        utf8_percent_encode(file, QUERY)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_spaces_slashes_and_cjk() {
        assert_eq!(
            open_note_uri("Work Notes", "Meetings/2026-11-05 预算.md"),
            "obsidian://open?vault=Work%20Notes&file=Meetings%2F2026-11-05%20%E9%A2%84%E7%AE%97"
        );
    }
}
