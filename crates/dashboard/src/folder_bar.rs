//! Folder bar — active folder, session status, and destination selectors.
//!
//! Extracted from `dashboard.rs` to keep the panel rendering modular.
//! All functions take data by reference and return `AnyElement`, so the
//! parent `Dashboard` stays in control of state mutations.
//!
//! Uses Zed's `ListItem` component for row layouts (session status, folder
//! selectors) to get native hover, spacing, and slot layout for free.

use std::path::PathBuf;

use gpui::{
    App, Corner, ExternalPaths, IntoElement, ParentElement, SharedString, Styled, Window,
};
use ui::{
    ButtonLike, ButtonStyle, ContextMenu, DynamicSpacing, Icon, IconName, IconSize, Label,
    LabelSize, ListItem, ListItemSpacing, PopoverMenu, prelude::*,
};
use workspace::DraggedSelection;


/// Render the session status row using `ListItem`.
///
/// Shows a check icon with the session name when a Pro Tools session is
/// detected, or an empty element otherwise.
pub(crate) fn render_session_status(
    session_name: &Option<String>,
    session_path: &Option<String>,
    cx: &App,
) -> AnyElement {
    match session_name {
        Some(name) => {
            let mut label_content = v_flex()
                .flex_1()
                .overflow_hidden()
                .min_w(px(60.))
                .child(Label::new(format!("Session: {}", name)).truncate());
            if let Some(path) = session_path {
                label_content = label_content.child(
                    Label::new(path.clone())
                        .color(Color::Muted)
                        .size(LabelSize::Small)
                        .truncate(),
                );
            }

            ListItem::new("session-status")
                .spacing(ListItemSpacing::Dense)
                .inset(true)
                .start_slot(
                    Icon::new(IconName::Check)
                        .color(Color::Success)
                        .size(IconSize::Small),
                )
                .child(label_content)
                .into_any_element()
        }
        None => div()
            .h(DynamicSpacing::Base08.rems(cx))
            .into_any_element(),
    }
}

/// Render the folder row — active folder and destination stacked vertically
/// for panel-width layout (~350 px).
///
/// Each selector is a popover dropdown with recent folders and a browse button.
pub(crate) fn render_folder_row(
    active_current: &Option<PathBuf>,
    active_recent: &[PathBuf],
    dest_current: &Option<PathBuf>,
    dest_recent: &[PathBuf],
    on_active_select: impl Fn(PathBuf, &mut Window, &mut App) + 'static + Clone,
    on_active_browse: impl Fn(&mut Window, &mut App) + 'static,
    on_dest_select: impl Fn(PathBuf, &mut Window, &mut App) + 'static + Clone,
    on_dest_browse: impl Fn(&mut Window, &mut App) + 'static,
    on_active_drop_external: impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static,
    on_active_drop_selection: impl Fn(&DraggedSelection, &mut Window, &mut App) + 'static,
    on_dest_drop_external: impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static,
    on_dest_drop_selection: impl Fn(&DraggedSelection, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let active_dropdown = build_folder_dropdown(
        "active-folder",
        "Active Folder",
        active_current,
        active_recent,
        Color::Accent,
        on_active_select,
        on_active_browse,
        on_active_drop_external,
        on_active_drop_selection,
        window,
        cx,
    );

    let dest_dropdown = build_folder_dropdown(
        "destination",
        "Destination",
        dest_current,
        dest_recent,
        Color::Success,
        on_dest_select,
        on_dest_browse,
        on_dest_drop_external,
        on_dest_drop_selection,
        window,
        cx,
    );

    // Vertical stack for narrow panel width — each dropdown gets full width.
    v_flex()
        .w_full()
        .gap(DynamicSpacing::Base04.rems(cx))
        .child(active_dropdown)
        .child(dest_dropdown)
        .into_any_element()
}

/// Build a single folder dropdown selector with popover menu, drag-drop, and
/// accent-colored left border.
fn build_folder_dropdown(
    id: &str,
    tag: &str,
    current: &Option<PathBuf>,
    recent: &[PathBuf],
    icon_color: Color,
    on_select: impl Fn(PathBuf, &mut Window, &mut App) + 'static + Clone,
    on_browse: impl Fn(&mut Window, &mut App) + 'static,
    on_drop_external: impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static,
    on_drop_selection: impl Fn(&DraggedSelection, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let display_name: SharedString = match current {
        Some(p) => p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
            .into(),
        None => "(none)".into(),
    };
    let name_color = if current.is_some() {
        Color::Default
    } else {
        Color::Muted
    };

    let menu = ContextMenu::build(window, cx, {
        let current = current.clone();
        let recent = recent.to_vec();
        move |mut menu, _window, _cx| {
            menu = menu.header("Recent");
            if recent.is_empty() {
                menu = menu.entry(
                    "No recent folders",
                    None,
                    |_window: &mut Window, _cx: &mut App| {},
                );
            } else {
                for folder in &recent {
                    let is_current = current.as_deref() == Some(folder.as_path());
                    let components: Vec<_> = folder.components().collect();
                    let short_path: SharedString = if components.len() <= 5 {
                        folder.to_string_lossy().to_string()
                    } else {
                        let tail: PathBuf = components[components.len() - 5..].iter().collect();
                        format!("\u{2026}/{}", tail.to_string_lossy())
                    }
                    .into();
                    let path = folder.clone();
                    let handler = on_select.clone();
                    menu = menu.toggleable_entry(
                        short_path,
                        is_current,
                        IconPosition::Start,
                        None,
                        move |window: &mut Window, cx: &mut App| {
                            handler(path.clone(), window, cx);
                        },
                    );
                }
            }
            menu = menu.separator();
            let browse_handler = on_browse;
            menu = menu.entry(
                "Browse\u{2026}",
                None,
                move |window: &mut Window, cx: &mut App| {
                    browse_handler(window, cx);
                },
            );
            menu
        }
    });

    let border_hsla = icon_color.color(cx);

    let label_content = v_flex()
        .flex_1()
        .overflow_hidden()
        .min_w(px(60.))
        .child(
            Label::new(SharedString::from(format!("{}:", tag)))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .child(Label::new(display_name).color(name_color).size(LabelSize::Small).truncate());

    let folder_row = ListItem::new(SharedString::from(format!("{}-row", id)))
        .spacing(ListItemSpacing::Dense)
        .inset(true)
        .start_slot(
            Icon::new(IconName::Folder)
                .color(icon_color)
                .size(IconSize::XSmall),
        )
        .child(label_content)
        .end_slot(
            Icon::new(IconName::ChevronUpDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        );

    let trigger = ButtonLike::new(SharedString::from(format!("{}-trigger", id)))
        .child(folder_row)
        .style(ButtonStyle::Transparent)
        .full_width();

    div()
        .id(SharedString::from(format!("{}-drop", id)))
        .w_full()
        .overflow_hidden()
        .rounded_md()
        .border_l_2()
        .border_color(border_hsla)
        .drag_over::<ExternalPaths>(|style, _, _, cx| {
            style.bg(cx.theme().colors().drop_target_background)
        })
        .drag_over::<DraggedSelection>(|style, _, _, cx| {
            style.bg(cx.theme().colors().drop_target_background)
        })
        .on_drop(move |paths: &ExternalPaths, window: &mut Window, cx: &mut App| {
            on_drop_external(paths, window, cx);
        })
        .on_drop(
            move |selection: &DraggedSelection, window: &mut Window, cx: &mut App| {
                on_drop_selection(selection, window, cx);
            },
        )
        .child(
            PopoverMenu::new(SharedString::from(format!("{}-popover", id)))
                .full_width(true)
                .menu(move |_window, _cx| Some(menu.clone()))
                .trigger(trigger)
                .attach(Corner::BottomLeft),
        )
        .into_any_element()
}
