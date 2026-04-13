//! Context entry rendering — badges (summary) and CRUD editor.
//!
//! Provides two public functions extracted from `Dashboard`:
//! - `render_context_summary` — read-only badge row with gear toggle
//! - `render_context_editor` — default toggle, entry rows with reorder/remove,
//!   add-path button, add-script picker, done button

use std::path::PathBuf;

use gpui::{AnyElement, App, IntoElement, ParentElement, PathPromptOptions, SharedString, Styled, WeakEntity};
use ui::{
    ButtonLike, ButtonStyle, Color, DynamicSpacing, Icon, IconName, IconSize, Label,
    LabelSize, prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;

use crate::Dashboard;
use crate::config;

// ---------------------------------------------------------------------------
// render_context_summary — read-only badge row
// ---------------------------------------------------------------------------

/// Renders context entries as compact badges with a gear icon to switch to
/// edit mode. Returns an empty vec when there's nothing to show.
pub(crate) fn render_context_summary(
    automation_id: &str,
    contexts: &[config::ContextEntry],
    skip_default: bool,
    default_contexts: &[config::ContextEntry],
    entity: WeakEntity<Dashboard>,
    cx: &App,
) -> Vec<AnyElement> {
    let mut elements = Vec::new();

    if contexts.is_empty() && !skip_default && default_contexts.is_empty() {
        return elements;
    }

    let gear_id = automation_id.to_string();

    let mut row = h_flex()
        .gap(DynamicSpacing::Base04.rems(cx))
        .pl(DynamicSpacing::Base16.rems(cx))
        .items_center()
        .flex_wrap();

    if !skip_default && !default_contexts.is_empty() {
        row = row.child(
            div()
                .px(DynamicSpacing::Base06.rems(cx))
                .py(DynamicSpacing::Base02.rems(cx))
                .rounded_sm()
                .bg(cx.theme().colors().element_background)
                .child(Label::new("defaults").size(LabelSize::XSmall).color(Color::Muted)),
        );
    }

    for ctx in contexts {
        let badge = if ctx.source_type == "script" { "script" } else { "path" };
        let label_text = format!("{}  [{}]", ctx.label, badge);
        row = row.child(
            div()
                .px(DynamicSpacing::Base06.rems(cx))
                .py(DynamicSpacing::Base02.rems(cx))
                .rounded_sm()
                .bg(cx.theme().colors().element_background)
                .child(Label::new(label_text).size(LabelSize::XSmall).color(Color::Muted)),
        );
    }

    // Gear icon to switch to edit mode
    row = row.child(
        ButtonLike::new(format!("ctx-edit-gear-{gear_id}"))
            .style(ButtonStyle::Subtle)
            .child(Icon::new(IconName::Settings).size(IconSize::XSmall).color(Color::Muted))
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| {
                    this.toggle_context_edit_mode(&gear_id, cx);
                }).log_err();
            }),
    );

    elements.push(row.into_any_element());
    elements
}

// ---------------------------------------------------------------------------
// render_context_editor — full CRUD editor
// ---------------------------------------------------------------------------

/// Renders the full context editor: default toggle, entry rows with
/// reorder/remove buttons, add-path button, script picker, and done button.
pub(crate) fn render_context_editor(
    automation_id: &str,
    contexts: &[config::ContextEntry],
    skip_default: bool,
    automation_source_path: Option<PathBuf>,
    workspace: WeakEntity<Workspace>,
    scripts: Vec<PathBuf>,
    config_root: PathBuf,
    entity: WeakEntity<Dashboard>,
    cx: &App,
) -> Vec<AnyElement> {
    let mut elements = Vec::new();

    // "Use default context" toggle
    let toggle_entity = entity.clone();
    let toggle_id = automation_id.to_string();
    let default_label: SharedString = if skip_default { "Default context: off".into() } else { "Default context: on".into() };
    elements.push(
        h_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .pl(DynamicSpacing::Base16.rems(cx))
            .items_center()
            .child(
                ButtonLike::new(format!("toggle-default-ctx-{}", toggle_id))
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base04.rems(cx))
                            .child(Icon::new(if skip_default { IconName::XCircle } else { IconName::Check })
                                .size(IconSize::XSmall)
                                .color(if skip_default { Color::Muted } else { Color::Accent }))
                            .child(Label::new(default_label).size(LabelSize::XSmall)
                                .color(if skip_default { Color::Muted } else { Color::Accent })),
                    )
                    .on_click(move |_, _, cx| {
                        toggle_entity.update(cx, |this, cx| {
                            this.toggle_skip_default_context(&toggle_id, cx);
                        }).log_err();
                    }),
            )
            .into_any_element(),
    );

    // Context entry rows
    for (_i, ctx) in contexts.iter().enumerate() {
        let badge = if ctx.source_type == "script" { "script" } else { "path" };
        let label_text = format!("{}  [{}]", ctx.label, badge);

        let row = h_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .pl(DynamicSpacing::Base16.rems(cx))
            .items_center()
            .child(
                Label::new(label_text)
                    .size(LabelSize::XSmall)
                    .color(if ctx.required { Color::Default } else { Color::Muted }),
            )
            .child(div().flex_1());

        elements.push(row.into_any_element());
    }

    // "Add Context" buttons
    let add_path_entity = entity.clone();
    let add_path_id = automation_id.to_string();
    let add_script_id = automation_id.to_string();
    let config_root_for_picker = config_root;

    elements.push(
        h_flex()
            .pl(DynamicSpacing::Base16.rems(cx))
            .pt(DynamicSpacing::Base04.rems(cx))
            .gap(DynamicSpacing::Base08.rems(cx))
            .child(
                ButtonLike::new(format!("add-ctx-path-{}", add_path_id))
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base04.rems(cx))
                            .child(Icon::new(IconName::Folder).size(IconSize::XSmall).color(Color::Muted))
                            .child(Label::new("Add prompt or context file").size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .on_click(move |_, _, cx| {
                        let auto_id = add_path_id.clone();
                        add_path_entity.update(cx, |_this, cx| {
                            let receiver = cx.prompt_for_paths(PathPromptOptions {
                                files: true,
                                directories: true,
                                multiple: false,
                                prompt: None,
                            });
                            let auto_id = auto_id.clone();
                            cx.spawn(async move |this, cx| {
                                if let Ok(Ok(Some(paths))) = receiver.await {
                                    if let Some(path) = paths.into_iter().next() {
                                        this.update(cx, |this, cx| {
                                            this.add_context_path_entry(&auto_id, path, cx);
                                        }).log_err();
                                    }
                                }
                            }).detach();
                        }).log_err();
                    }),
            )
            .when(!scripts.is_empty() && automation_source_path.is_some(), {
                let scripts_entries: Vec<crate::automation_picker::PickerEntry> = scripts
                    .iter()
                    .map(|script_path| {
                        crate::automation_picker::PickerEntry::new_context_script(
                            script_path.clone(),
                            Some(config_root_for_picker.clone()),
                        )
                    })
                    .collect();
                let auto_id = add_script_id;
                let source_path = automation_source_path.expect("checked above");
                let ws = workspace;
                move |el| {
                    el.child(
                        ButtonLike::new(format!("add-ctx-script-{}", auto_id))
                            .style(ButtonStyle::Subtle)
                            .child(
                                h_flex()
                                    .gap(DynamicSpacing::Base04.rems(cx))
                                    .child(Icon::new(IconName::ToolTerminal).size(IconSize::XSmall).color(Color::Muted))
                                    .child(Label::new("Add Script").size(LabelSize::XSmall).color(Color::Muted)),
                            )
                            .on_click({
                                let scripts_entries = scripts_entries;
                                let auto_id = auto_id.clone();
                                let source_path = source_path;
                                let ws = ws;
                                move |_, window, cx| {
                                    let Some(workspace) = ws.upgrade() else { return };
                                    let mode = crate::automation_picker::PickerMode::AddContextScript {
                                        automation_id: auto_id.clone(),
                                        source_path: source_path.clone(),
                                    };
                                    workspace.update(cx, |workspace, cx| {
                                        workspace.toggle_modal(window, cx, |window, cx| {
                                            crate::automation_picker::AutomationModal::new_with_mode(
                                                scripts_entries.clone(),
                                                mode,
                                                None,
                                                ws.clone(),
                                                window,
                                                cx,
                                            )
                                        });
                                    });
                                }
                            }),
                    )
                }
            })
            .into_any_element(),
    );

    // Done button (gear icon to collapse back to summary)
    let done_entity = entity;
    let done_id = automation_id.to_string();
    elements.push(
        h_flex()
            .pl(DynamicSpacing::Base16.rems(cx))
            .pt(DynamicSpacing::Base02.rems(cx))
            .child(
                ButtonLike::new(format!("ctx-done-gear-{}", done_id))
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base04.rems(cx))
                            .child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Accent))
                            .child(Label::new("Done").size(LabelSize::XSmall).color(Color::Accent)),
                    )
                    .on_click(move |_, _, cx| {
                        done_entity.update(cx, |this, cx| {
                            this.toggle_context_edit_mode(&done_id, cx);
                        }).log_err();
                    }),
            )
            .into_any_element(),
    );

    elements
}
