//! Section header rendering for the dashboard panel.
//!
//! Provides reusable section and sub-section header components with
//! disclosure (collapse/expand) support. State management stays in
//! `Dashboard`; this module owns only the rendering.

use std::collections::HashSet;

use gpui::{IntoElement, ParentElement, SharedString, Styled, WeakEntity};
use ui::{Color, Disclosure, Divider, DividerColor, Label, LabelSize, prelude::*};

use util::ResultExt as _;

use crate::Dashboard;

/// Render a top-level section header.
///
/// Layout: `[▸ disclosure] TITLE ——————————————`
///
/// Clicking the disclosure toggles the section via `Dashboard::toggle_section`.
pub fn section_header(
    title: &str,
    section_id: &str,
    collapsed_sections: &HashSet<String>,
    entity: WeakEntity<Dashboard>,
) -> impl IntoElement {
    let is_open = !collapsed_sections.contains(section_id);
    let id_for_toggle = section_id.to_string();

    h_flex()
        .px_1()
        .mb_2()
        .gap_2()
        .items_center()
        .child(
            Disclosure::new(SharedString::from(format!("disc-{}", section_id)), is_open)
                .on_click(move |_, _, cx| {
                    entity
                        .update(cx, |this, cx| {
                            this.toggle_section(&id_for_toggle, cx);
                        })
                        .log_err();
                }),
        )
        .child(
            Label::new(title.to_string())
                .color(Color::Muted)
                .size(LabelSize::Small),
        )
        .child(Divider::horizontal().color(DividerColor::BorderVariant))
}

/// Render a sub-section header (indented, no trailing divider).
///
/// Layout: `  [▸ disclosure] Title`
pub fn sub_section_header(
    title: &str,
    section_id: &str,
    collapsed_sections: &HashSet<String>,
    entity: WeakEntity<Dashboard>,
) -> impl IntoElement {
    let is_open = !collapsed_sections.contains(section_id);
    let id_for_toggle = section_id.to_string();

    h_flex()
        .pl_2()
        .mt_1()
        .mb_1()
        .gap_1p5()
        .items_center()
        .child(
            Disclosure::new(SharedString::from(format!("disc-{}", section_id)), is_open)
                .on_click(move |_, _, cx| {
                    entity
                        .update(cx, |this, cx| {
                            this.toggle_section(&id_for_toggle, cx);
                        })
                        .log_err();
                }),
        )
        .child(
            Label::new(title.to_string())
                .color(Color::Muted)
                .size(LabelSize::Small),
        )
}
