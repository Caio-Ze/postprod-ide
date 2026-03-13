use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    actions, App, AnyElement, Context, DismissEvent,
    IntoElement, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{
    HighlightedLabel, KeyBinding, Label, LabelSize, ListItem, ListItemSpacing,
    prelude::*,
};
use workspace::Workspace;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{AutomationEntry, ToolEntry};
use crate::hotkeys::{GlobalHotkeyManagerHandle, ResolvedHotkeyEntry};
use crate::paths::{resolve_agent_tools_path, resolve_runtime_path, state_dir_for};
use crate::persistence::read_background_tools;
use crate::{Dashboard, RunDashboardTool, resolve_tool_command};

use task::{RevealStrategy, SpawnInTerminal, TaskId};
use util::ResultExt as _;

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

actions!(dashboard, [RunAutomationPicker]);

// ---------------------------------------------------------------------------
// Entry types for the picker
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)]
enum PickerEntryKind {
    Tool(ToolEntry),
    Automation(AutomationEntry),
    GlobalShortcutOnly(ToolEntry),
}

#[derive(Clone)]
pub(crate) struct PickerEntry {
    kind: PickerEntryKind,
    id: String,
    label: String,
    global_hotkey: Option<String>,
    config_root: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Build entry list from workspace state (called before toggle_modal)
// ---------------------------------------------------------------------------

pub(crate) fn build_picker_entries(workspace: &Workspace, cx: &App) -> Vec<PickerEntry> {
    let mut entries: Vec<PickerEntry> = Vec::new();

    let dashboard_data = workspace
        .panes()
        .iter()
        .flat_map(|pane| pane.read(cx).items())
        .find_map(|item| item.downcast::<Dashboard>())
        .map(|d| {
            let d = d.read(cx);
            (d.tools.clone(), d.automations.clone(), d.config_root.clone())
        });

    let (current_tools, current_automations, current_config_root) =
        dashboard_data.unwrap_or_else(|| (Vec::new(), Vec::new(), crate::paths::suite_root()));

    let hotkey_entries: Vec<ResolvedHotkeyEntry> = cx
        .try_global::<GlobalHotkeyManagerHandle>()
        .map(|handle| handle.0.read(cx).all_hotkey_entries().to_vec())
        .unwrap_or_default();

    let mut hotkey_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut global_only_entries: Vec<ResolvedHotkeyEntry> = Vec::new();

    for hotkey_entry in &hotkey_entries {
        let in_current_project = current_tools.iter().any(|t| t.id == hotkey_entry.tool_id);
        if in_current_project {
            hotkey_map.insert(
                hotkey_entry.tool_id.clone(),
                hotkey_entry.keystroke_display.clone(),
            );
        } else if hotkey_entry.tool.is_some() {
            global_only_entries.push(hotkey_entry.clone());
        }
    }

    // GlobalShortcutOnly entries first
    for hotkey_entry in &global_only_entries {
        if let Some(tool) = &hotkey_entry.tool {
            entries.push(PickerEntry {
                kind: PickerEntryKind::GlobalShortcutOnly(tool.clone()),
                id: tool.id.clone(),
                label: tool.label.clone(),
                global_hotkey: Some(hotkey_entry.keystroke_display.clone()),
                config_root: Some(hotkey_entry.config_root.clone()),
            });
        }
    }

    // Current project tools
    for tool in &current_tools {
        if tool.hidden {
            continue;
        }
        entries.push(PickerEntry {
            kind: PickerEntryKind::Tool(tool.clone()),
            id: tool.id.clone(),
            label: tool.label.clone(),
            global_hotkey: hotkey_map.get(&tool.id).cloned(),
            config_root: Some(current_config_root.clone()),
        });
    }

    // Current project automations
    for automation in &current_automations {
        if automation.hidden {
            continue;
        }
        entries.push(PickerEntry {
            kind: PickerEntryKind::Automation(automation.clone()),
            id: automation.id.clone(),
            label: automation.label.clone(),
            global_hotkey: None,
            config_root: Some(current_config_root.clone()),
        });
    }

    entries
}

// ---------------------------------------------------------------------------
// Build picker from entries (called inside toggle_modal closure)
// ---------------------------------------------------------------------------

pub(crate) fn build_picker(
    entries: Vec<PickerEntry>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<Picker<AutomationPickerDelegate>>,
) -> Picker<AutomationPickerDelegate> {
    let delegate = AutomationPickerDelegate {
        workspace,
        all_entries: entries,
        matches: Vec::new(),
        selected_index: 0,
        separator_indices: Vec::new(),
    };

    let picker = Picker::uniform_list(delegate, window, cx);
    picker.set_query("", window, cx);
    picker
}

// ---------------------------------------------------------------------------
// Delegate
// ---------------------------------------------------------------------------

pub(crate) struct AutomationPickerDelegate {
    workspace: WeakEntity<Workspace>,
    all_entries: Vec<PickerEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    separator_indices: Vec<usize>,
}

impl AutomationPickerDelegate {
    fn entry_for_match(&self, ix: usize) -> Option<&PickerEntry> {
        self.matches
            .get(ix)
            .and_then(|m| self.all_entries.get(m.candidate_id))
    }
}

impl PickerDelegate for AutomationPickerDelegate {
    type ListItem = AnyElement;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.matches.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Run automation or tool...".into()
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        self.separator_indices.clone()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates: Vec<StringMatchCandidate> = self
            .all_entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| StringMatchCandidate::new(ix, &entry.label))
            .collect();

        let executor = cx.background_executor().clone();

        cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .enumerate()
                    .map(|(ix, c)| StringMatch {
                        candidate_id: ix,
                        string: c.string.clone(),
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    true,
                    true,
                    1000,
                    &Default::default(),
                    executor,
                )
                .await
            };

            this.update(cx, |picker, _cx| {
                let delegate = &mut picker.delegate;
                delegate.matches = matches;
                delegate.selected_index = 0;
                delegate.recompute_separators();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.entry_for_match(self.selected_index).cloned() else {
            return;
        };

        match &entry.kind {
            PickerEntryKind::Tool(tool) | PickerEntryKind::GlobalShortcutOnly(tool) => {
                let config_root = entry
                    .config_root
                    .clone()
                    .unwrap_or_else(crate::paths::suite_root);

                spawn_tool_from_picker(tool, &config_root, &self.workspace, window, cx);
            }
            PickerEntryKind::Automation(_) => {
                window.dispatch_action(Box::new(crate::ShowDashboard), cx);
            }
        }

        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let string_match = self.matches.get(ix)?;
        let entry = self.all_entries.get(string_match.candidate_id)?;

        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);

        let label_element = HighlightedLabel::new(
            entry.label.clone(),
            string_match.positions.clone(),
        );

        let is_global_only = matches!(entry.kind, PickerEntryKind::GlobalShortcutOnly(_));
        let is_automation = matches!(entry.kind, PickerEntryKind::Automation(_));

        let mut row = h_flex().gap_2().child(label_element);

        if is_global_only {
            if let Some(config_root) = &entry.config_root {
                let project_name = config_root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                row = row.child(
                    Label::new(project_name)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
            }
        }

        if is_automation {
            row = row.child(
                Label::new("automation")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }

        let mut right = h_flex().gap_2();

        if let Some(hotkey_display) = &entry.global_hotkey {
            right = right.child(
                Label::new(hotkey_display.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }

        if let PickerEntryKind::Tool(_) = &entry.kind {
            let action = RunDashboardTool {
                tool_id: entry.id.clone(),
            };
            right = right.child(KeyBinding::for_action(&action as &dyn gpui::Action, cx));
        }

        Some(
            item.child(h_flex().w_full().justify_between().child(row).child(right))
                .into_any_element(),
        )
    }
}

impl AutomationPickerDelegate {
    fn recompute_separators(&mut self) {
        self.separator_indices.clear();
        let mut last_section = None;

        for (ix, string_match) in self.matches.iter().enumerate() {
            let Some(entry) = self.all_entries.get(string_match.candidate_id) else {
                continue;
            };
            let section = match &entry.kind {
                PickerEntryKind::GlobalShortcutOnly(_) => 0,
                PickerEntryKind::Tool(_) => 1,
                PickerEntryKind::Automation(_) => 2,
            };
            if let Some(prev) = last_section {
                if section != prev && ix > 0 {
                    self.separator_indices.push(ix - 1);
                }
            }
            last_section = Some(section);
        }
    }
}

// ---------------------------------------------------------------------------
// Spawn a tool from the picker (reused for Tool and GlobalShortcutOnly)
// ---------------------------------------------------------------------------

fn spawn_tool_from_picker(
    tool: &ToolEntry,
    config_root: &Path,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<Picker<AutomationPickerDelegate>>,
) {
    let runtime_path = resolve_runtime_path();
    let agent_tools_path = resolve_agent_tools_path();

    let state_dir = state_dir_for(config_root);
    let active_folder = crate::hotkeys::read_state_string_pub(&state_dir.join("active_folder"))
        .map(PathBuf::from);
    let session_path = if tool.needs_session {
        crate::hotkeys::read_state_string_pub(&state_dir.join("session_path"))
    } else {
        None
    };
    let param_values =
        crate::hotkeys::read_param_values_pub(&state_dir.join("param_values.toml"), &tool.id);

    let (command, args, cwd, env) = resolve_tool_command(
        tool,
        &runtime_path,
        &agent_tools_path,
        config_root,
        &session_path,
        &active_folder,
        &param_values,
    );

    let background_tools = read_background_tools(config_root);
    let is_background = background_tools.contains(&tool.id);

    if is_background {
        let tool_label = tool.label.clone();
        cx.background_executor()
            .spawn(async move {
                let mut cmd = smol::process::Command::new(&command);
                cmd.args(&args).current_dir(&cwd);
                for (key, value) in &env {
                    cmd.env(key, value);
                }
                match cmd.output().await {
                    Ok(output) if output.status.success() => {
                        log::info!("background picker tool '{}': success", tool_label);
                    }
                    Ok(output) => {
                        log::warn!(
                            "background picker tool '{}': exit {}: {}",
                            tool_label,
                            output.status,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    Err(e) => {
                        log::error!("background picker tool '{}': {}", tool_label, e);
                    }
                }
            })
            .detach();
        return;
    }

    let tool_label = tool.label.clone();
    let tool_id = tool.id.clone();

    let spawn = SpawnInTerminal {
        id: TaskId(format!("dashboard-{}", tool_id)),
        full_label: tool_label.clone(),
        command_label: tool_label.clone(),
        label: tool_label,
        command: Some(command),
        args,
        cwd: Some(cwd),
        use_new_terminal: true,
        allow_concurrent_runs: false,
        reveal: RevealStrategy::Always,
        show_command: true,
        show_rerun: true,
        env: env.into_iter().collect(),
        ..Default::default()
    };

    if let Some(workspace) = workspace.upgrade() {
        workspace.update(cx, |workspace, cx| {
            workspace.spawn_in_terminal(spawn, window, cx).detach();
        });
    }
}
