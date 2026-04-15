//! Automation card rendering using `DashboardCard`.
//!
//! Provides `render_automation_card` that builds an automation card with
//! `DashboardCard` (ListItem-based) — replacing the old `render_card_shell`
//! manual layout for automation entries.

use gpui::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, StatefulInteractiveElement, Styled,
};
use ui::{
    Color, CommonAnimationExt, Disclosure, DynamicSpacing, Icon, IconButton, IconName, IconSize,
    Label, LabelSize, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::OpenOptions;

use crate::AutomationRunStatus;
use crate::card::{CardIcon, CardRenderContext, DashboardCard};
use crate::config::icon_for_automation;
use crate::paths::automations_dir_for;

/// Build an automation card using `DashboardCard`.
///
/// Replaces the old `render_card_shell` path for automations. All action
/// buttons (run, schedule toggle, gear, delete, disclosure, edit TOML) are
/// built here using the entity handle for callbacks. Pre-built elements
/// (param fields, schedule controls, context rows) are passed in because
/// they require `&mut Dashboard` context to render.
pub fn render_automation_card(
    ctx: &CardRenderContext<'_>,
    icon_color: Color,
    badge_label: SharedString,
    badge_color: Color,
    param_fields: Vec<AnyElement>,
    schedule_controls: Option<AnyElement>,
    context_rows: Vec<AnyElement>,
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
    let entry_prompt = entry.prompt.clone();
    let has_params = !entry.params.is_empty();
    let group_name = SharedString::from(format!("automation-{}", entry_id));
    let icon_tint_bg = cx.theme().colors().element_background;

    // --- Clone entity handles for each button's closure ----------------------

    let click_entity = entity.clone();
    let click_id = entry_id.clone();

    let run_entity = entity.clone();
    let run_id = entry_id.clone();
    let run_label = entry_label.clone();
    let run_prompt = entry_prompt;

    let sched_entity = entity.clone();
    let sched_id = entry_id.clone();

    let gear_entity = entity.clone();
    let gear_id = entry_id.clone();

    let delete_entity = entity.clone();
    let delete_id = entry_id.clone();

    let disc_entity = entity.clone();
    let disc_id = entry_id.clone();

    let prompt_entity = entity.clone();
    let prompt_id = entry_id.clone();
    let prompt_file = entry.prompt_file.clone();
    let prompt_automation_id = entry_id.clone();

    let edit_entity = entity;
    let edit_id = entry_id.clone();

    // --- Action buttons (end slot) -------------------------------------------

    let action_buttons = h_flex()
        .gap(DynamicSpacing::Base08.rems(cx))
        .items_center()
        .child(
            Label::new(badge_label)
                .color(badge_color)
                .size(LabelSize::XSmall),
        )
        .child(
            h_flex()
                .gap(DynamicSpacing::Base04.rems(cx))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .child(
                    if matches!(ctx.run_status, Some(AutomationRunStatus::GatheringContext)) {
                        h_flex()
                            .id(SharedString::from(format!("run-spinner-wrap-{}", run_id)))
                            .size(IconSize::Small.rems())
                            .items_center()
                            .justify_center()
                            .tooltip(Tooltip::text("Gathering context…"))
                            .child(
                                Icon::new(IconName::LoadCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Muted)
                                    .with_keyed_rotate_animation(
                                        ElementId::Name(
                                            format!("automation-spinner-{}", run_id).into(),
                                        ),
                                        2,
                                    ),
                            )
                            .into_any_element()
                    } else {
                        IconButton::new(format!("run-automation-{}", run_id), IconName::PlayFilled)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Run automation"))
                            .on_click(move |_, window, cx| {
                                run_entity
                                    .update(cx, |this, cx| {
                                        this.run_automation(
                                            &run_id,
                                            &run_label,
                                            &run_prompt,
                                            window,
                                            cx,
                                        );
                                    })
                                    .log_err();
                            })
                            .into_any_element()
                    },
                )
                .child(
                    IconButton::new(
                        format!("sched-toggle-{}", sched_id),
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
                    IconButton::new(format!("gear-automation-{}", gear_id), IconName::Settings)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Settings"))
                        .on_click(move |_, _, cx| {
                            gear_entity
                                .update(cx, |this, cx| {
                                    this.expanded_automations.insert(gear_id.clone());
                                    this.toggle_context_edit_mode(&gear_id, cx);
                                })
                                .log_err();
                        }),
                )
                .when(prompt_file.is_some(), |this| {
                    this.child(
                        IconButton::new(format!("edit-prompt-{}", prompt_id), IconName::Pencil)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Edit Prompt"))
                            .on_click(move |_, window, cx| {
                                if prompt_file.is_some() {
                                    let auto_id = prompt_automation_id.clone();
                                    prompt_entity
                                        .update(cx, |this, cx| {
                                            this.open_postprod_rules_scoped(&auto_id, window, cx);
                                        })
                                        .log_err();
                                }
                            }),
                    )
                })
                .child(
                    IconButton::new(format!("delete-automation-{}", delete_id), IconName::Trash)
                        .icon_size(IconSize::Small)
                        .icon_color(if is_pending_delete {
                            Color::Error
                        } else {
                            Color::Muted
                        })
                        .tooltip(Tooltip::text(if is_pending_delete {
                            "Click again to confirm delete"
                        } else {
                            "Delete automation"
                        }))
                        .on_click(move |_, _, cx| {
                            delete_entity
                                .update(cx, |this, cx| {
                                    if this.automations_pending_delete.contains(&delete_id) {
                                        this.automations_pending_delete.remove(&delete_id);
                                        this.delete_automation_toml(&delete_id, cx);
                                    } else {
                                        this.automations_pending_delete.insert(delete_id.clone());
                                        cx.notify();
                                    }
                                })
                                .log_err();
                        }),
                )
                .child(
                    Disclosure::new(
                        SharedString::from(format!("disc-auto-{}", disc_id)),
                        is_expanded,
                    )
                    .on_click(move |_, _, cx| {
                        disc_entity
                            .update(cx, |this, cx| {
                                this.toggle_automation_expanded(&disc_id, cx);
                            })
                            .log_err();
                    }),
                )
                .child(
                    IconButton::new(format!("edit-automation-{}", edit_id), IconName::FileToml)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Edit TOML"))
                        .on_click(move |_, window, cx| {
                            edit_entity
                                .update(cx, |this, cx| {
                                    let path = this
                                        .automations
                                        .iter()
                                        .find(|a| a.id == edit_id)
                                        .and_then(|a| a.source_path.clone())
                                        .unwrap_or_else(|| {
                                            automations_dir_for(&this.config_root)
                                                .join(format!("{}.toml", edit_id))
                                        });
                                    let workspace = this.workspace.clone();
                                    cx.spawn_in(window, async move |_this, cx| {
                                        workspace
                                            .update_in(cx, |workspace, window, cx| {
                                                workspace
                                                    .open_abs_path(
                                                        path,
                                                        OpenOptions::default(),
                                                        window,
                                                        cx,
                                                    )
                                                    .detach();
                                            })
                                            .log_err();
                                    })
                                    .detach();
                                })
                                .log_err();
                        }),
                ),
        );

    // --- Expanded content (below header) -------------------------------------

    let param_pl = DynamicSpacing::Base48.rems(cx);
    let has_expanded_content = has_params || is_scheduled || is_expanded;

    let expanded_content = if has_expanded_content {
        Some(
            div()
                .when(has_params, |el| {
                    el.child(
                        h_flex()
                            .w_full()
                            .pl(param_pl)
                            .pr(DynamicSpacing::Base08.rems(cx))
                            .pb(DynamicSpacing::Base04.rems(cx))
                            .gap(DynamicSpacing::Base08.rems(cx))
                            .flex_wrap()
                            .children(param_fields),
                    )
                })
                .when_some(schedule_controls, |el, ctrl| el.child(ctrl))
                .when(is_expanded, |el| {
                    el.child(
                        v_flex()
                            .w_full()
                            .px(DynamicSpacing::Base12.rems(cx))
                            .pb(DynamicSpacing::Base08.rems(cx))
                            .gap(DynamicSpacing::Base04.rems(cx))
                            .children(context_rows),
                    )
                }),
        )
    } else {
        None
    };

    // --- Build DashboardCard -------------------------------------------------

    let mut card = DashboardCard::new(
        SharedString::from(format!("automation-card-{}-{}", entry_id, idx)),
        CardIcon::new(icon).color(icon_color).bg(icon_tint_bg),
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
