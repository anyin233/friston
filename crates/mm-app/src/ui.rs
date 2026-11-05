//! Small shared building blocks for pages: headings, cards, tags, speaker colours.

use gpui_kit::component::button::Button;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::MeetingStatus;

use crate::logic;
use crate::state::LoadError;

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

/// Page title plus a one-line description.
///
/// Params: `title`; `sub`; `cx`.
/// Returns: the header element.
pub fn page_head(title: impl Into<SharedString>, sub: impl Into<SharedString>, cx: &App) -> Div {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(sub.into()),
        )
}

/// Section title.
///
/// Params: `title`.
/// Returns: the element.
pub fn section_title(title: impl Into<SharedString>) -> Div {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(title.into())
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
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// Status tag of a meeting.
///
/// Params: `s`.
/// Returns: a coloured tag.
pub fn status_tag(s: MeetingStatus) -> Tag {
    let tag = match s {
        MeetingStatus::Done => Tag::success(),
        MeetingStatus::Live => Tag::danger(),
        MeetingStatus::Processing => Tag::warning(),
        _ => Tag::secondary(),
    };
    tag.small().child(logic::status_label(s))
}

/// A row showing a load error; permission errors get a button to System Settings.
///
/// Params: `id` – element id; `err`; `cx`.
/// Returns: the element.
pub fn load_error(id: impl Into<ElementId>, err: &LoadError, cx: &App) -> Div {
    let url = logic::privacy_settings_url(&err.message);
    h_flex()
        .gap_2()
        .items_center()
        .text_sm()
        .text_color(cx.theme().danger)
        .child(Icon::new(IconName::TriangleAlert).small())
        .child(div().flex_1().child(err.message.clone()))
        .when(err.permission, |el| {
            el.child(
                Button::new(id)
                    .small()
                    .outline()
                    .label("打开系统设置")
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
    h_flex()
        .gap_1()
        .items_center()
        .child(div().size(px(8.)).rounded_full().bg(color))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(name.into()),
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
