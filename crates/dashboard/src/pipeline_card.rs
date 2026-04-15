//! Pipeline card rendering using `DashboardCard`.
//!
//! Provides `render_pipeline_card` that builds a pipeline card with
//! `DashboardCard` (ListItem-based) — replacing the old `render_card_shell`
//! manual layout for pipeline entries. Also contains step tree rendering
//! (view and edit modes).

use gpui::{
    AnyElement, App, IntoElement, MouseButton, ParentElement, SharedString, Styled, WeakEntity,
};
use ui::{
    ButtonLike, ButtonStyle, Color, Disclosure, DynamicSpacing, Icon, IconButton, IconName,
    IconSize, Label, LabelSize, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::Dashboard;
use crate::card::{CardIcon, CardRenderContext, DashboardCard};
use crate::collect_step_groups;
use crate::config::{AutomationEntry, PipelineStep, ToolEntry, icon_for_automation};

// ---------------------------------------------------------------------------
// Step tree — view mode
// ---------------------------------------------------------------------------

/// Render a pipeline's step tree in read-only (view) mode.
///
/// Each step is labeled with a sequential number. Parallel groups use
/// letter suffixes (1a, 1b). Tool steps get a "(tool)" suffix. Broken
/// references (missing target) are shown in error color.
pub fn render_pipeline_step_tree(
    steps: &[PipelineStep],
    tools: &[ToolEntry],
    automations: &[AutomationEntry],
    cx: &App,
) -> Vec<AnyElement> {
    let groups = collect_step_groups(steps);
    let mut elements = Vec::new();
    let mut display_num: u32 = 0;

    for group in &groups {
        display_num += 1;
        let is_parallel = group.len() > 1;

        for (sub_idx, step) in group.iter().enumerate() {
            let target_id = step.target_id().unwrap_or("unknown");

            let (display_name, is_tool_step) = if step.is_tool() {
                let name = tools
                    .iter()
                    .find(|t| t.id == target_id)
                    .map(|t| t.label.clone())
                    .unwrap_or_else(|| format!("missing: {target_id}"));
                (name, true)
            } else {
                let name = automations
                    .iter()
                    .find(|a| a.id == target_id)
                    .map(|a| a.label.clone())
                    .unwrap_or_else(|| format!("missing: {target_id}"));
                (name, false)
            };

            let is_broken = if step.is_tool() {
                !tools.iter().any(|t| t.id == target_id)
            } else {
                !automations.iter().any(|a| a.id == target_id)
            };

            let label_text = if is_parallel {
                let suffix = (b'a' + sub_idx as u8) as char;
                format!("{display_num}{suffix}. {display_name}")
            } else {
                format!("{display_num}. {display_name}")
            };

            let label_text = if is_tool_step {
                format!("{label_text} (tool)")
            } else {
                label_text
            };

            let text_color = if is_broken {
                Color::Error
            } else {
                Color::Muted
            };

            let mut row = h_flex()
                .gap(DynamicSpacing::Base08.rems(cx))
                .pl(DynamicSpacing::Base16.rems(cx))
                .child(
                    Label::new(label_text)
                        .size(LabelSize::XSmall)
                        .color(text_color),
                );

            if is_parallel && sub_idx == 0 {
                row = row.child(Label::new("┐").size(LabelSize::XSmall).color(Color::Muted));
            } else if is_parallel && sub_idx == group.len() - 1 {
                row = row.child(Label::new("┘").size(LabelSize::XSmall).color(Color::Muted));
            } else if is_parallel {
                row = row.child(Label::new("│").size(LabelSize::XSmall).color(Color::Muted));
            }

            elements.push(row.into_any_element());
        }
    }
    elements
}

// ---------------------------------------------------------------------------
// Step tree — edit mode
// ---------------------------------------------------------------------------

/// Render a pipeline's step tree in edit mode.
///
/// Each step gets Up/Down/Remove buttons. An "Add Step" button at the bottom
/// opens the automation picker in pipeline-step mode.
pub fn render_pipeline_edit_steps(
    entry: &AutomationEntry,
    tools: &[ToolEntry],
    automations: &[AutomationEntry],
    entity: WeakEntity<Dashboard>,
    workspace: WeakEntity<Workspace>,
    config_root: PathBuf,
    cx: &App,
) -> Vec<AnyElement> {
    let mut elements = Vec::new();
    let step_count = entry.steps.len();

    for (i, step) in entry.steps.iter().enumerate() {
        let target_id = step.target_id().unwrap_or("unknown");
        let display_name = if step.is_tool() {
            tools
                .iter()
                .find(|t| t.id == target_id)
                .map(|t| t.label.clone())
                .unwrap_or_else(|| format!("missing: {target_id}"))
        } else {
            automations
                .iter()
                .find(|a| a.id == target_id)
                .map(|a| a.label.clone())
                .unwrap_or_else(|| format!("missing: {target_id}"))
        };

        let suffix = if step.is_tool() { " (tool)" } else { "" };
        let label_text = format!("{}. {display_name}{suffix}", i + 1);

        let up_entity = entity.clone();
        let down_entity = entity.clone();
        let remove_entity = entity.clone();
        let pipeline_id = entry.id.clone();
        let pipeline_id2 = entry.id.clone();
        let pipeline_id3 = entry.id.clone();

        let row = h_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .pl(DynamicSpacing::Base16.rems(cx))
            .items_center()
            .child(
                Label::new(label_text)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new(format!("step-up-{}-{}", entry.id, i), IconName::ArrowUp)
                    .size(ui::ButtonSize::Compact)
                    .disabled(i == 0)
                    .on_click(move |_, _, cx| {
                        up_entity
                            .update(cx, |this, cx| {
                                this.reorder_pipeline_step(&pipeline_id, i, -1, cx);
                            })
                            .log_err();
                    }),
            )
            .child(
                IconButton::new(format!("step-down-{}-{}", entry.id, i), IconName::ArrowDown)
                    .size(ui::ButtonSize::Compact)
                    .disabled(i >= step_count - 1)
                    .on_click(move |_, _, cx| {
                        down_entity
                            .update(cx, |this, cx| {
                                this.reorder_pipeline_step(&pipeline_id2, i, 1, cx);
                            })
                            .log_err();
                    }),
            )
            .child(
                IconButton::new(format!("step-remove-{}-{}", entry.id, i), IconName::Close)
                    .size(ui::ButtonSize::Compact)
                    .on_click(move |_, _, cx| {
                        remove_entity
                            .update(cx, |this, cx| {
                                this.remove_pipeline_step(&pipeline_id3, i, cx);
                            })
                            .log_err();
                    }),
            );

        elements.push(row.into_any_element());
    }

    // [+ Add Step] button — data captured outside the closure to avoid
    // re-entering Dashboard while it's being updated.
    let source_path = entry.source_path.clone();
    let add_pipeline_id = entry.id.clone();
    let workspace_handle = workspace;
    let picker_tools = tools.to_vec();
    let picker_automations = automations.to_vec();
    let picker_config_root = config_root;
    elements.push(
        h_flex()
            .pl(DynamicSpacing::Base16.rems(cx))
            .pt(DynamicSpacing::Base04.rems(cx))
            .child(
                ButtonLike::new(format!("add-step-{}", add_pipeline_id))
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base04.rems(cx))
                            .child(
                                Icon::new(IconName::Plus)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("Add Step")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .on_click(move |_, window, cx| {
                        let Some(source_path) = source_path.clone() else {
                            return;
                        };
                        let Some(workspace) = workspace_handle.upgrade() else {
                            return;
                        };

                        let mut entries: Vec<crate::automation_picker::PickerEntry> = Vec::new();
                        for tool in &picker_tools {
                            if tool.hidden {
                                continue;
                            }
                            entries.push(crate::automation_picker::PickerEntry::new_tool(
                                tool.clone(),
                                Some(picker_config_root.clone()),
                            ));
                        }
                        for auto in &picker_automations {
                            if auto.hidden {
                                continue;
                            }
                            entries.push(crate::automation_picker::PickerEntry::new_automation(
                                auto.clone(),
                                Some(picker_config_root.clone()),
                            ));
                        }

                        let mode = crate::automation_picker::PickerMode::AddPipelineStep {
                            pipeline_source_path: source_path,
                            group: None,
                        };
                        let ws = workspace_handle.clone();
                        let pipeline_id = add_pipeline_id.clone();
                        workspace.update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                crate::automation_picker::AutomationModal::new_with_mode(
                                    entries,
                                    mode,
                                    Some(pipeline_id),
                                    ws,
                                    window,
                                    cx,
                                )
                            });
                        });
                    }),
            )
            .into_any_element(),
    );

    elements
}

// ---------------------------------------------------------------------------
// Pipeline card
// ---------------------------------------------------------------------------

/// Build a pipeline card using `DashboardCard`.
///
/// Replaces the old `render_card_shell` path for pipelines. All action
/// buttons (run/stop, schedule toggle, edit, delete, disclosure) are built
/// here. Pre-built elements (step_tree, schedule_controls) are passed in
/// because schedule controls require `&mut Dashboard` context to render.
pub fn render_pipeline_card(
    ctx: &CardRenderContext<'_>,
    is_running: bool,
    is_editing: bool,
    step_tree: Vec<AnyElement>,
    schedule_controls: AnyElement,
    active_folder: PathBuf,
    cx: &App,
) -> AnyElement {
    let entry = ctx.entry;
    let idx = ctx.idx;
    let accent = ctx.accent;
    let is_expanded = ctx.is_expanded;
    let is_scheduled = ctx.is_scheduled;
    let is_pending_delete = ctx.is_pending_delete;
    let entity = ctx.entity.clone();

    let icon = icon_for_automation(&entry.icon);
    let entry_id = entry.id.clone();
    let entry_label: SharedString = entry.label.clone().into();
    let entry_description: SharedString = entry.description.clone().into();
    let has_steps = !entry.steps.is_empty();
    let icon_tint_bg = cx.theme().colors().element_background;
    let group_name = SharedString::from(format!("pipeline-{}", entry_id));

    // --- Clone entity handles for each button's closure ----------------------

    let click_entity = entity.clone();
    let click_id = entry_id.clone();

    let run_entity = entity.clone();
    let stop_entity = entity.clone();
    let run_id = entry_id.clone();
    let stop_id = entry_id.clone();
    let run_entry = entry.clone();

    let edit_entity = entity.clone();
    let edit_id = entry_id.clone();

    let delete_entity = entity.clone();
    let delete_id = entry_id.clone();

    let sched_entity = entity.clone();
    let sched_id = entry_id.clone();

    let disc_entity = entity;
    let disc_id = entry_id.clone();

    // --- Action buttons (end slot) -------------------------------------------

    let action_buttons = h_flex()
        .gap_2()
        .items_center()
        .child(
            Label::new(if is_running {
                SharedString::from("running")
            } else {
                SharedString::from(format!("{} steps", entry.steps.len()))
            })
            .color(if is_running {
                Color::Accent
            } else {
                Color::Muted
            })
            .size(LabelSize::XSmall),
        )
        .child(
            h_flex()
                .gap_1()
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .child(
                    Disclosure::new(
                        SharedString::from(format!("disc-pipeline-{}", disc_id)),
                        is_expanded,
                    )
                    .on_click(move |_, _, cx| {
                        disc_entity
                            .update(cx, |this, cx| {
                                if this.expanded_automations.contains(&disc_id) {
                                    this.expanded_automations.remove(&disc_id);
                                } else {
                                    this.expanded_automations.insert(disc_id.clone());
                                }
                                cx.notify();
                            })
                            .log_err();
                    }),
                )
                .child(if is_running {
                    IconButton::new(format!("stop-pipeline-{}", stop_id), IconName::Stop)
                        .tooltip(Tooltip::text("Stop pipeline"))
                        .on_click(move |_, _, cx| {
                            stop_entity
                                .update(cx, |this, cx| {
                                    if let Some(flag) = this.pipeline_cancel_flags.get(&stop_id) {
                                        flag.store(true, Ordering::Relaxed);
                                    }
                                    cx.notify();
                                })
                                .log_err();
                        })
                } else {
                    IconButton::new(format!("run-pipeline-{}", run_id), IconName::PlayFilled)
                        .disabled(!has_steps)
                        .tooltip(Tooltip::text(if !has_steps {
                            "No steps to run"
                        } else {
                            "Run pipeline"
                        }))
                        .on_click(move |_, _, cx| {
                            run_entity
                                .update(cx, |this, cx| {
                                    this.run_pipeline(
                                        &run_entry,
                                        &active_folder,
                                        0,
                                        task::RevealStrategy::Always,
                                        cx,
                                    );
                                })
                                .log_err();
                        })
                })
                .child(
                    IconButton::new(
                        format!("sched-toggle-pipeline-{}", sched_id),
                        IconName::CountdownTimer,
                    )
                    .icon_size(IconSize::Small)
                    .icon_color(if is_scheduled {
                        Color::Accent
                    } else {
                        Color::Muted
                    })
                    .tooltip(Tooltip::text(if is_scheduled {
                        "Disable schedule"
                    } else {
                        "Enable schedule"
                    }))
                    .on_click(move |_, _window, cx| {
                        sched_entity
                            .update(cx, |this, cx| {
                                this.toggle_schedule(&sched_id, cx);
                            })
                            .log_err();
                    }),
                )
                .child(
                    IconButton::new(
                        format!("edit-pipeline-{}", edit_id),
                        if is_editing {
                            IconName::Check
                        } else {
                            IconName::Settings
                        },
                    )
                    .disabled(is_running)
                    .tooltip(Tooltip::text(if is_running {
                        "Cannot edit while running"
                    } else if is_editing {
                        "Done editing"
                    } else {
                        "Edit pipeline"
                    }))
                    .on_click(move |_, _, cx| {
                        edit_entity
                            .update(cx, |this, cx| {
                                if this.pipelines_in_edit_mode.contains(&edit_id) {
                                    this.pipelines_in_edit_mode.remove(&edit_id);
                                    this.pipelines_pending_delete.remove(&edit_id);
                                } else {
                                    this.pipelines_in_edit_mode.insert(edit_id.clone());
                                    this.expanded_automations.insert(edit_id.clone());
                                }
                                cx.notify();
                            })
                            .log_err();
                    }),
                )
                .when(is_editing, |el| {
                    el.child(
                        IconButton::new(format!("delete-pipeline-{}", delete_id), IconName::Trash)
                            .icon_color(if is_pending_delete {
                                Color::Error
                            } else {
                                Color::Default
                            })
                            .tooltip(Tooltip::text(if is_pending_delete {
                                "Click again to confirm delete"
                            } else {
                                "Delete pipeline"
                            }))
                            .on_click(move |_, _, cx| {
                                delete_entity
                                    .update(cx, |this, cx| {
                                        if this.pipelines_pending_delete.contains(&delete_id) {
                                            this.pipelines_pending_delete.remove(&delete_id);
                                            this.delete_pipeline_toml(&delete_id, cx);
                                        } else {
                                            this.pipelines_pending_delete.insert(delete_id.clone());
                                            cx.notify();
                                        }
                                    })
                                    .log_err();
                            }),
                    )
                }),
        );

    // --- Expanded content (below header) -------------------------------------

    let has_expanded_content = is_scheduled || (is_expanded && !step_tree.is_empty());
    let expanded_content = if has_expanded_content {
        Some(
            div()
                .when(is_scheduled, move |el| el.child(schedule_controls))
                .when(is_expanded && !step_tree.is_empty(), |el| {
                    el.child(
                        v_flex()
                            .px(DynamicSpacing::Base08.rems(cx))
                            .pb(DynamicSpacing::Base08.rems(cx))
                            .gap(DynamicSpacing::Base01.rems(cx))
                            .children(step_tree),
                    )
                }),
        )
    } else {
        None
    };

    // --- Build DashboardCard -------------------------------------------------

    let mut card = DashboardCard::new(
        SharedString::from(format!("pipeline-card-{}-{}", entry_id, idx)),
        CardIcon::new(icon).color(Color::Accent).bg(icon_tint_bg),
        entry_label,
    )
    .description(entry_description)
    .accent(accent)
    .end_slot(action_buttons)
    .group_name(group_name.clone())
    .on_click(move |_, _window, cx| {
        click_entity
            .update(cx, |this, cx| {
                this.toggle_automation_expanded(&click_id, cx);
            })
            .log_err();
    });

    if let Some(content) = expanded_content {
        card = card.expanded_content(content);
    }

    div()
        .group(group_name)
        .w_full()
        .child(card.render(cx))
        .into_any_element()
}
