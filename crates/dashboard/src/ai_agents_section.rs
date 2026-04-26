//! AI Agents section rendering for the dashboard panel.
//!
//! Renders launcher buttons for configured AI agents (Claude, Gemini, etc.)
//! that spawn in terminal tabs. Uses the shared `section_header` from
//! `section.rs` for the collapsible header.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{IntoElement, ParentElement, SharedString, Styled, WeakEntity};
use ui::{
    ButtonLike, ButtonSize, Color, DynamicSpacing, Icon, IconName, IconSize, Label, prelude::*,
};
use util::ResultExt as _;

use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use workspace::Workspace;

use postprod_dashboard_config::AgentEntry;

use crate::DashboardItem;
use crate::dashboard_paths::resolve_bin;
use crate::section;

/// Render the AI Agents section with launcher buttons.
///
/// When collapsed, shows only the disclosure header. When open, shows a
/// vertically stacked list of agent launcher buttons that spawn in a terminal.
pub fn render_ai_agents_section(
    collapsed_sections: &HashSet<String>,
    agent_launchers: &[AgentEntry],
    workspace: &WeakEntity<Workspace>,
    cwd: PathBuf,
    entity: WeakEntity<DashboardItem>,
    cx: &App,
) -> impl IntoElement {
    let is_open = !collapsed_sections.contains("ai-agents");

    if !is_open {
        return v_flex()
            .w_full()
            .gap(DynamicSpacing::Base04.rems(cx))
            .child(section::section_header(
                "AI AGENTS",
                "ai-agents",
                collapsed_sections,
                entity,
                cx,
            ));
    }

    let workspace = workspace.clone();

    let agents: Vec<_> = agent_launchers
        .iter()
        .map(|entry| {
            let id = entry.id.clone();
            let label = entry.label.clone();
            let program = resolve_bin(&entry.command);
            let args: Vec<String> = entry.flags.split_whitespace().map(String::from).collect();
            (id, label, program, args)
        })
        .collect();

    let agent_buttons: Vec<_> = agents
        .into_iter()
        .map({
            move |(id, label, program, args)| {
                let workspace = workspace.clone();
                let cwd = cwd.clone();

                ButtonLike::new(SharedString::from(id.clone()))
                    .full_width()
                    .size(ButtonSize::Medium)
                    .child(
                        h_flex()
                            .w_full()
                            .gap(DynamicSpacing::Base08.rems(cx))
                            .child(
                                Icon::new(IconName::Sparkle)
                                    .color(Color::Accent)
                                    .size(IconSize::Small),
                            )
                            .child(Label::new(label.clone())),
                    )
                    .on_click(move |_, window, cx| {
                        let workspace = workspace.clone();
                        let args = args.clone();
                        let program = program.clone();
                        let cwd = cwd.clone();
                        let label = label.clone();
                        let id = id.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                let spawn = SpawnInTerminal {
                                    id: TaskId(format!("ai-agent-{}", id)),
                                    label: label.clone(),
                                    full_label: label.clone(),
                                    command_label: label.clone(),
                                    cwd: Some(cwd),
                                    shell: Shell::WithArguments {
                                        program,
                                        args,
                                        title_override: Some(label),
                                    },
                                    use_new_terminal: true,
                                    allow_concurrent_runs: false,
                                    reveal: RevealStrategy::Always,
                                    ..Default::default()
                                };
                                workspace.spawn_in_terminal(spawn, window, cx).detach();
                            })
                            .log_err();
                    })
                    .into_any_element()
            }
        })
        .collect();

    v_flex()
        .w_full()
        .gap(DynamicSpacing::Base04.rems(cx))
        .child(section::section_header(
            "AI AGENTS",
            "ai-agents",
            collapsed_sections,
            entity,
            cx,
        ))
        .child(
            v_flex()
                .id("ai-agents-content-anim")
                .w_full()
                .gap(DynamicSpacing::Base04.rems(cx))
                .children(agent_buttons),
        )
}
