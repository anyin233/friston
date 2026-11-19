//! "这是谁？" dialog: names an anonymous speaker cluster of a meeting.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;
use mm_core::MeetingId;
use mm_i18n::{tr, trf};

use crate::state::{AppState, PersonChoice};
use crate::ui;

/// Opens the dialog for `cluster` of `meeting`.
///
/// Params: `app`; `meeting`; `cluster`; `input` – persistent input for a new name (cleared
/// here); `window`; `cx`.
pub fn open(
    app: Entity<AppState>,
    meeting: MeetingId,
    cluster: u32,
    input: Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) {
    input.update(cx, |s, cx| s.set_value("", window, cx));
    window.open_dialog(cx, move |dialog, _window, cx| {
        let people: Vec<_> = app
            .read(cx)
            .people
            .ready()
            .map(|p| {
                p.iter()
                    .map(|r| (r.person.id, r.person.display_name.clone(), r.hue))
                    .collect()
            })
            .unwrap_or_default();
        let existing = h_flex()
            .flex_wrap()
            .gap_2()
            .children(people.into_iter().enumerate().map(|(i, (id, name, hue))| {
                let app = app.clone();
                Button::new(("assign-person", i))
                    .small()
                    .outline()
                    .child(ui::speaker_name(name, Some(hue)))
                    .on_click(move |_, window, cx| {
                        app.update(cx, |a, cx| {
                            a.assign_speaker(meeting, cluster, PersonChoice::Existing(id), cx)
                        });
                        window.close_dialog(cx);
                    })
            }));
        let app_new = app.clone();
        let input_new = input.clone();
        dialog
            .title(trf!("说话人 {cluster} 是谁？", "Who is speaker {cluster}?"))
            .w(px(460.))
            .child(
                v_flex()
                    .gap_3()
                    .child(ui::muted(
                        tr!(
                            "命名后，本场之前的发言也会一起改名。",
                            "Earlier lines in this meeting are renamed too.",
                        ),
                        cx,
                    ))
                    .child(existing)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&input).id("assign-name")))
                            .child(
                                Button::new("assign-new")
                                    .primary()
                                    .label(tr!("新建人物", "New person"))
                                    .on_click(move |_, window, cx| {
                                        let name = input_new.read(cx).value().trim().to_string();
                                        if name.is_empty() {
                                            return;
                                        }
                                        app_new.update(cx, |a, cx| {
                                            a.assign_speaker(
                                                meeting,
                                                cluster,
                                                PersonChoice::New(name),
                                                cx,
                                            )
                                        });
                                        window.close_dialog(cx);
                                    }),
                            ),
                    ),
            )
    });
}
