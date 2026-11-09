//! UI language for Meeting Master: Simplified Chinese (default) and English.
//!
//! Translations live next to the code that shows them, as `zh` / `en` literal pairs:
//!
//! ```
//! use mm_i18n::{tr, trf};
//! let label: &'static str = tr!("开始记录", "Start recording");
//! let n = 3;
//! let line: String = trf!("今天有 {n} 场会议", "{n} meetings today");
//! ```
//!
//! Why pairs instead of key files: the app ships exactly two languages, and pairs keep both
//! wordings visible at the call site while `format!` checks the placeholders of each
//! language at compile time, so a missing key or a mismatched argument cannot reach users.
//!
//! The active language is process-wide ([`set_current`]); tests pin a language for one
//! thread with [`with_lang`] so parallel tests do not race on the global.

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// A language the interface can be shown in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// Simplified Chinese, the original wording of the app.
    #[default]
    Zh,
    /// English.
    En,
}

impl Lang {
    /// Every supported language, in the order the settings picker lists them.
    pub const ALL: [Lang; 2] = [Lang::Zh, Lang::En];

    /// Short code stored in `config.json`.
    ///
    /// Returns: `"zh"` or `"en"`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// Parses a BCP 47 tag or POSIX locale (`zh`, `zh-Hans-CN`, `zh_TW.UTF-8`, `en-US`).
    ///
    /// Params: `tag` – language tag; only the primary subtag matters.
    /// Returns: the matching language, or `None` for anything else (e.g. `ja-JP`).
    pub fn from_tag(tag: &str) -> Option<Lang> {
        let primary = tag
            .split(['-', '_', '.', '@'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match primary.as_str() {
            "zh" => Some(Lang::Zh),
            "en" => Some(Lang::En),
            _ => None,
        }
    }

    /// Locale name understood by gpui-component's built-in strings (`rust-i18n`).
    ///
    /// Returns: `"zh-CN"` or `"en"`.
    pub fn component_locale(self) -> &'static str {
        match self {
            Lang::Zh => "zh-CN",
            Lang::En => "en",
        }
    }

    /// The language's own name, shown in the picker regardless of the current language.
    ///
    /// Returns: `"简体中文"` or `"English"`.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::Zh => "简体中文",
            Lang::En => "English",
        }
    }

    /// Decodes the value kept in [`CURRENT`].
    fn from_u8(v: u8) -> Lang {
        if v == 1 { Lang::En } else { Lang::Zh }
    }

    /// Encodes the value kept in [`CURRENT`].
    fn to_u8(self) -> u8 {
        match self {
            Lang::Zh => 0,
            Lang::En => 1,
        }
    }
}

/// Process-wide language; `Relaxed` is enough because a stale read only shows one frame
/// in the previous language and the switch triggers a repaint anyway.
static CURRENT: AtomicU8 = AtomicU8::new(0);

thread_local! {
    /// Per-thread override used by [`with_lang`]; wins over [`CURRENT`].
    static OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// The language strings should be produced in right now.
///
/// Returns: this thread's [`with_lang`] override if any, else the process-wide language.
pub fn current() -> Lang {
    OVERRIDE
        .with(Cell::get)
        .unwrap_or_else(|| Lang::from_u8(CURRENT.load(Ordering::Relaxed)))
}

/// Switches the process-wide language. Callers in the app also update gpui-component's
/// locale and refresh windows; this crate stays GUI-free so the service crates can use it.
///
/// Params: `lang` – new language.
pub fn set_current(lang: Lang) {
    CURRENT.store(lang.to_u8(), Ordering::Relaxed);
}

/// Runs `f` with `lang` as this thread's language, restoring the previous override after
/// (also on panic), so tests can check both wordings without touching the global.
///
/// Params: `lang` – language for the duration of `f`; `f` – code to run.
/// Returns: whatever `f` returns.
pub fn with_lang<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    /// Restores the previous override when dropped.
    struct Restore(Option<Lang>);
    impl Drop for Restore {
        /// Puts the saved override back.
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(self.0));
        }
    }
    let _restore = Restore(OVERRIDE.with(|o| o.replace(Some(lang))));
    f()
}

/// Picks a language from an ordered list of preferred locale tags.
///
/// Params: `tags` – the user's preferred languages, most preferred first.
/// Returns: the first supported one; English when none is supported, since a user who
/// prefers, say, Japanese is more likely to read English than Chinese.
pub fn pick_lang<'a>(tags: impl IntoIterator<Item = &'a str>) -> Lang {
    tags.into_iter()
        .find_map(Lang::from_tag)
        .unwrap_or(Lang::En)
}

/// The language macOS (System Settings › Language & Region) prefers for this user.
///
/// Returns: see [`pick_lang`]; Chinese when the system reports no locale at all, matching
/// the app's original wording.
pub fn system_lang() -> Lang {
    let tags: Vec<String> = sys_locale::get_locales().collect();
    if tags.is_empty() {
        return Lang::Zh;
    }
    pick_lang(tags.iter().map(String::as_str))
}

/// Whether a literal contains a `{`, i.e. was meant for [`trf!`]. Used by [`tr!`] to reject
/// placeholders at compile time instead of showing `{name}` to users.
///
/// Params: `s` – the literal.
/// Returns: `true` if it contains `{`.
#[doc(hidden)]
pub const fn has_brace(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            return true;
        }
        i += 1;
    }
    false
}

/// Picks the Chinese or English literal for the current language.
///
/// Params: two string literals without placeholders (use [`trf!`] for those).
/// Returns: `&'static str`.
#[macro_export]
macro_rules! tr {
    ($zh:literal, $en:literal $(,)?) => {{
        const _: () = assert!(
            !$crate::has_brace($zh) && !$crate::has_brace($en),
            "tr! literals cannot contain a brace; use trf!"
        );
        match $crate::current() {
            $crate::Lang::Zh => $zh,
            $crate::Lang::En => $en,
        }
    }};
}

/// Formats the Chinese or English template for the current language.
///
/// Params: two `format!` templates, then the arguments both of them use (inline
/// `{name}` captures work too). Each template is checked by `format!` at compile time.
/// Returns: `String`.
#[macro_export]
macro_rules! trf {
    ($zh:literal, $en:literal $(,)?) => {
        match $crate::current() {
            $crate::Lang::Zh => format!($zh),
            $crate::Lang::En => format!($en),
        }
    };
    ($zh:literal, $en:literal, $($arg:tt)+) => {
        match $crate::current() {
            $crate::Lang::Zh => format!($zh, $($arg)+),
            $crate::Lang::En => format!($en, $($arg)+),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_map_to_languages() {
        assert_eq!(Lang::from_tag("zh-Hans-CN"), Some(Lang::Zh));
        assert_eq!(Lang::from_tag("zh_TW.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::from_tag("en-US"), Some(Lang::En));
        assert_eq!(Lang::from_tag("EN"), Some(Lang::En));
        assert_eq!(Lang::from_tag("ja-JP"), None);
        assert_eq!(Lang::from_tag(""), None);
    }

    #[test]
    fn first_supported_preference_wins() {
        assert_eq!(pick_lang(["ja-JP", "zh-Hans-CN", "en-US"]), Lang::Zh);
        assert_eq!(pick_lang(["en-GB", "zh-Hans"]), Lang::En);
        assert_eq!(pick_lang(["ja-JP", "ko-KR"]), Lang::En);
    }

    #[test]
    fn macros_follow_the_thread_language() {
        let n = 3;
        with_lang(Lang::Zh, || {
            assert_eq!(tr!("开始", "Start"), "开始");
            assert_eq!(trf!("有 {n} 场", "{n} meetings"), "有 3 场");
            assert_eq!(trf!("{} / {}", "{} of {}", 1, 2), "1 / 2");
        });
        with_lang(Lang::En, || {
            assert_eq!(tr!("开始", "Start"), "Start");
            assert_eq!(trf!("有 {n} 场", "{n} meetings"), "3 meetings");
            assert_eq!(trf!("{} / {}", "{} of {}", 1, 2), "1 of 2");
        });
    }

    #[test]
    fn with_lang_nests_and_restores() {
        with_lang(Lang::En, || {
            with_lang(Lang::Zh, || assert_eq!(current(), Lang::Zh));
            assert_eq!(current(), Lang::En);
        });
    }

    #[test]
    fn serde_uses_short_codes() {
        assert_eq!(serde_json::to_string(&Lang::En).unwrap(), "\"en\"");
        assert_eq!(serde_json::from_str::<Lang>("\"zh\"").unwrap(), Lang::Zh);
        assert_eq!(Lang::En.code(), "en");
        assert_eq!(Lang::Zh.component_locale(), "zh-CN");
    }

    #[test]
    fn brace_detection() {
        assert!(has_brace("a {x}"));
        assert!(!has_brace("plain"));
    }
}
