//! Tool card rendering using `DashboardCard`.
//!
//! Provides functions that build tool cards for all three tiers (Featured,
//! Standard, Compact) using the shared `DashboardCard` component from
//! `card.rs`. Adapted for panel-width (~350px) layout — all tiers render
//! full-width instead of multi-column grids.

use gpui::{
    AnyElement, App, ClickEvent, ExternalPaths, IntoElement, ParentElement, SharedString, Styled,
    Window,
};
use ui::{Color, DynamicSpacing, ListItemSpacing, Tooltip, prelude::*};

use crate::card::{CardIcon, DashboardCard};
use crate::config::{ToolEntry, icon_for_tool};

/// Build a Featured tool card: full-width, accent border + left strip, full
/// icon, description, params below header, hover-reveal action buttons.
pub fn render_featured_tool(
    tool: &ToolEntry,
    action_buttons: impl IntoElement,
    param_fields: Vec<AnyElement>,
    click_handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    drop_handler: Option<impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static>,
    cx: &App,
) -> AnyElement {
    let accent_color = cx.theme().colors().text_accent;
    let icon_tint_bg = cx.theme().colors().element_background;
    let group_name = SharedString::from(format!("tool-{}", tool.id));
    let tool_icon = icon_for_tool(&tool.icon);
    let has_params = !param_fields.is_empty();

    let expanded = if has_params {
        Some(
            h_flex()
                .px(DynamicSpacing::Base08.rems(cx))
                .pb(DynamicSpacing::Base08.rems(cx))
                .gap(DynamicSpacing::Base08.rems(cx))
                .flex_wrap()
                .children(param_fields),
        )
    } else {
        None
    };

    let mut card = DashboardCard::new(
        SharedString::from(format!("featured-{}", tool.id)),
        CardIcon::new(tool_icon)
            .color(Color::Accent)
            .bg(icon_tint_bg),
        tool.label.clone(),
    )
    .description(tool.description.clone())
    .accent(accent_color)
    .end_slot(action_buttons)
    .group_name(group_name.clone())
    .on_click(click_handler);

    if let Some(exp) = expanded {
        card = card.expanded_content(exp);
    }

    let mut el = div().group(group_name).w_full().child(card.render(cx));

    if let Some(handler) = drop_handler {
        el = el
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(move |paths: &ExternalPaths, window, cx| {
                handler(paths, window, cx);
            });
    }

    el.into_any_element()
}

/// Build a Standard tool card: neutral style, compact icon, description,
/// params below header. Full-width for panel layout (no multi-column).
pub fn render_standard_tool(
    tool: &ToolEntry,
    action_buttons: impl IntoElement,
    param_fields: Vec<AnyElement>,
    click_handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    drop_handler: Option<impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static>,
    cx: &App,
) -> AnyElement {
    let group_name = SharedString::from(format!("tool-{}", tool.id));
    let tool_icon = icon_for_tool(&tool.icon);
    let icon_bg = cx.theme().colors().element_background;
    let has_params = !param_fields.is_empty();

    let expanded = if has_params {
        Some(
            h_flex()
                .px(DynamicSpacing::Base08.rems(cx))
                .pb(DynamicSpacing::Base08.rems(cx))
                .gap(DynamicSpacing::Base08.rems(cx))
                .flex_wrap()
                .children(param_fields),
        )
    } else {
        None
    };

    let mut card = DashboardCard::new(
        SharedString::from(format!("standard-{}", tool.id)),
        CardIcon::new(tool_icon)
            .color(Color::Muted)
            .bg(icon_bg)
            .compact(),
        tool.label.clone(),
    )
    .description(tool.description.clone())
    .end_slot(action_buttons)
    .group_name(group_name.clone())
    .spacing(ListItemSpacing::Dense)
    .on_click(click_handler);

    if let Some(exp) = expanded {
        card = card.expanded_content(exp);
    }

    let mut el = div().group(group_name).w_full().child(card.render(cx));

    if let Some(handler) = drop_handler {
        el = el
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(move |paths: &ExternalPaths, window, cx| {
                handler(paths, window, cx);
            });
    }

    el.into_any_element()
}

/// Build a Compact tool card: minimal icon + label row, no description,
/// tooltip for details. Full-width for panel layout.
pub fn render_compact_tool(
    tool: &ToolEntry,
    action_buttons: impl IntoElement,
    click_handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let group_name = SharedString::from(format!("tool-{}", tool.id));
    let tool_icon = icon_for_tool(&tool.icon);
    let tool_description = tool.description.clone();

    let card = DashboardCard::new(
        SharedString::from(format!("compact-{}", tool.id)),
        CardIcon::new(tool_icon).color(Color::Muted).compact(),
        tool.label.clone(),
    )
    .end_slot(action_buttons)
    .group_name(group_name.clone())
    .spacing(ListItemSpacing::ExtraDense)
    .on_click(click_handler);

    div()
        .id(SharedString::from(format!("compact-wrap-{}", tool.id)))
        .group(group_name)
        .w_full()
        .child(card.render(cx))
        .tooltip(Tooltip::text(tool_description))
        .into_any_element()
}
