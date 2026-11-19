//! Small shared building blocks for pages: headings, cards, tags, speaker colours.

use gpui_kit::base::TestTextExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::MeetingStatus;
use mm_i18n::tr;

use crate::logic;
use crate::state::LoadError;

/// Produces a placeholder in the current language (a non-capturing `|| tr!(…)`).
type PlaceholderText = fn() -> &'static str;

/// Input placeholders that follow the interface language.
///
/// Input states keep their placeholder text, so a page built before a language switch would
/// keep showing the old wording. Pages register each input with a function that produces
/// its text, and call [`Placeholders::sync`] at the end of `new` and at the top of `render`.
#[derive(Default)]
pub struct Placeholders {
    /// Language the placeholders were last written in; `None` until the first sync.
    lang: Option<mm_i18n::Lang>,
    inputs: Vec<(Entity<InputState>, PlaceholderText)>,
    textareas: Vec<(Entity<TextareaState>, PlaceholderText)>,
}

impl Placeholders {
    /// Registers a single-line input.
    ///
    /// Params: `input`; `text` – returns the placeholder in the current language.
    pub fn input(&mut self, input: &Entity<InputState>, text: fn() -> &'static str) {
        self.inputs.push((input.clone(), text));
    }

    /// Registers a multi-line text area.
    ///
    /// Params: `area`; `text` – returns the placeholder in the current language.
    pub fn textarea(&mut self, area: &Entity<TextareaState>, text: fn() -> &'static str) {
        self.textareas.push((area.clone(), text));
    }

    /// Rewrites every registered placeholder when the language changed since the last call;
    /// a no-op otherwise, so calling it on every render is cheap.
    ///
    /// Params: `window`; `cx`.
    pub fn sync(&mut self, window: &mut Window, cx: &mut App) {
        let now = mm_i18n::current();
        if self.lang == Some(now) {
            return;
        }
        self.lang = Some(now);
        for (input, text) in &self.inputs {
            input.update(cx, |s, cx| s.set_placeholder(text(), window, cx));
        }
        for (area, text) in &self.textareas {
            area.update(cx, |s, cx| s.set_placeholder(text(), window, cx));
        }
    }
}

/// Colour of a speaker hue (identity colour: one person, one hue on every surface).
///
/// Params: `hue` – degrees, `None` for unknown speakers.
/// Returns: the colour.
pub fn hue_color(hue: Option<u16>) -> Hsla {
    match hue {
        Some(h) => hsla(f32::from(h % 360) / 360.0, 0.62, 0.52, 1.0),
        None => hsla(0.65, 0.12, 0.62, 1.0),
    }
}

/// Caption text whose volatile tail (what the engine may still rewrite) is drawn in `tail`.
///
/// New characters appear after the stable part instead of the whole line flickering, the
/// way Foresight shows its interim results.
///
/// Params: `text`; `stable_bytes` – byte offset where the volatile tail starts
/// (`CaptionLine::stable_byte_len`); `tail` – colour of the volatile part.
/// Returns: the styled text (inherits the surrounding text colour for the stable part).
pub fn caption_text(text: &str, stable_bytes: usize, tail: Hsla) -> StyledText {
    let cut = stable_bytes.min(text.len());
    let highlights = (cut < text.len()).then(|| {
        (
            cut..text.len(),
            HighlightStyle {
                color: Some(tail),
                ..Default::default()
            },
        )
    });
    StyledText::new(SharedString::from(text.to_string())).with_highlights(highlights)
}

/// Page title plus a one-line description.
///
/// Params: `title`; `sub`; `cx`.
/// Returns: the header element.
pub fn page_head(title: impl Into<SharedString>, sub: impl Into<SharedString>, cx: &App) -> Div {
    let title = title.into();
    let sub = sub.into();
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.clone().test_text(title)),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(sub.clone().test_text(sub)),
        )
}

/// Section title.
///
/// Params: `title`.
/// Returns: the element.
pub fn section_title(title: impl Into<SharedString>) -> Div {
    let title = title.into();
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(title.clone().test_text(title))
}

/// Bordered card container.
///
/// Params: `cx`.
/// Returns: a vertical flex box.
pub fn card(cx: &App) -> Div {
    v_flex()
        .gap_2()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
}

/// Muted helper text.
///
/// Params: `text`; `cx`.
/// Returns: the element.
pub fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    let text = text.into();
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.clone().test_text(text))
}

/// Status tag of a meeting.
///
/// Params: `s`.
/// Returns: a coloured tag.
pub fn status_tag(s: MeetingStatus) -> Tag {
    let label: SharedString = logic::status_label(s).into();
    let tag = match s {
        MeetingStatus::Done => Tag::success(),
        MeetingStatus::Live => Tag::danger(),
        MeetingStatus::Processing => Tag::warning(),
        _ => Tag::secondary(),
    };
    tag.small().child(label.clone().test_text(label))
}

/// A row showing a load error; permission errors get a button to System Settings.
///
/// Params: `id` – element id; `err`; `cx`.
/// Returns: the element.
pub fn load_error(id: impl Into<ElementId>, err: &LoadError, cx: &App) -> Div {
    let url = logic::privacy_settings_url(&err.message);
    let msg: SharedString = err.message.clone().into();
    h_flex()
        .gap_2()
        .items_center()
        .text_sm()
        .text_color(cx.theme().danger)
        .child(Icon::new(IconName::TriangleAlert).small())
        .child(div().flex_1().child(msg.clone().test_text(msg)))
        .when(err.permission, |el| {
            el.child(
                Button::new(id)
                    .small()
                    .outline()
                    .label(tr!("打开系统设置", "Open System Settings"))
                    .on_click(move |_, _, cx| cx.open_url(url)),
            )
        })
}

/// Loading placeholder text.
///
/// Params: `text`; `cx`.
/// Returns: the element.
pub fn loading(text: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(gpui_kit::component::spinner::Spinner::new().small())
        .child(muted(text, cx))
}

/// Coloured dot + name of a speaker.
///
/// Params: `name`; `hue`.
/// Returns: the element.
pub fn speaker_name(name: impl Into<SharedString>, hue: Option<u16>) -> Div {
    let color = hue_color(hue);
    let name = name.into();
    h_flex()
        .gap_1()
        .items_center()
        .child(div().size(px(8.)).rounded_full().bg(color))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(name.clone().test_text(name)),
        )
}

/// A horizontal level meter (one bar per channel).
///
/// Params: `fractions` – 0..1 per channel; `color`; `cx`.
/// Returns: the element.
pub fn meter(fractions: &[f32], color: Hsla, cx: &App) -> Div {
    let track = cx.theme().muted;
    v_flex()
        .gap(px(2.))
        .w(px(56.))
        .children(fractions.iter().map(|f| {
            div().h(px(4.)).w_full().rounded_sm().bg(track).child(
                div()
                    .h_full()
                    .rounded_sm()
                    .bg(color)
                    .w(relative(f.clamp(0.0, 1.0))),
            )
        }))
}

/// An option in a [`Select`](gpui_kit::component::select::Select): a stored value and its label.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub value: String,
    pub title: SharedString,
}

impl Choice {
    /// Builds a choice.
    ///
    /// Params: `value`; `title`.
    /// Returns: the choice.
    pub fn new(value: impl Into<String>, title: impl Into<SharedString>) -> Self {
        Self {
            value: value.into(),
            title: title.into(),
        }
    }
}

impl gpui_kit::component::select::SelectItem for Choice {
    type Value = String;

    /// Label shown in the trigger and menu.
    fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// Stored value.
    fn value(&self) -> &String {
        &self.value
    }
}

#[cfg(test)]
mod tests {
    use super::hue_color;

    #[test]
    fn hues_map_to_distinct_colours() {
        assert_ne!(hue_color(Some(10)), hue_color(Some(200)));
        assert_eq!(hue_color(Some(370)), hue_color(Some(10)), "hue wraps");
        assert!(hue_color(None).s < 0.2, "unknown speakers are neutral");
    }
}
