use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    actions, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, Subscription, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{
    Color, HighlightedLabel, Icon, IconName, IconSize, KeyBinding, Label, LabelSize, ListItem,
    ListItemSpacing, ToggleButtonGroup, ToggleButtonSimple, prelude::*,
};
use workspace::{ModalView, Workspace, pane};

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
// Picker mode
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub(crate) enum PickerMode {
    #[default]
    Run,
    AddPipelineStep {
        pipeline_source_path: PathBuf,
        group: Option<u32>,
    },
    AddContextScript {
        #[allow(dead_code)]
        automation_id: String,
        source_path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Tab enum
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum PickerTab {
    Tools,
    Automations,
}

// ---------------------------------------------------------------------------
// Entry types for the picker
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)]
enum PickerEntryKind {
    Tool(ToolEntry),
    Automation(AutomationEntry),
    GlobalShortcutOnly(ToolEntry),
    ContextScript(PathBuf),
}

#[derive(Clone)]
pub(crate) struct PickerEntry {
    kind: PickerEntryKind,
    id: String,
    label: String,
    global_hotkey: Option<String>,
    config_root: Option<PathBuf>,
}

impl PickerEntry {
    pub(crate) fn new_tool(tool: crate::config::ToolEntry, config_root: Option<PathBuf>) -> Self {
        Self {
            id: tool.id.clone(),
            label: tool.label.clone(),
            kind: PickerEntryKind::Tool(tool),
            global_hotkey: None,
            config_root,
        }
    }

    pub(crate) fn new_automation(
        automation: crate::config::AutomationEntry,
        config_root: Option<PathBuf>,
    ) -> Self {
        Self {
            id: automation.id.clone(),
            label: automation.label.clone(),
            kind: PickerEntryKind::Automation(automation),
            global_hotkey: None,
            config_root,
        }
    }

    pub(crate) fn new_context_script(script_path: PathBuf, config_root: Option<PathBuf>) -> Self {
        let filename = script_path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let label = std::path::Path::new(&filename)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| filename.clone());
        Self {
            id: filename,
            label,
            kind: PickerEntryKind::ContextScript(script_path),
            global_hotkey: None,
            config_root,
        }
    }
}

// ---------------------------------------------------------------------------
// Build entry list from workspace state (called before toggle_modal)
// ---------------------------------------------------------------------------

pub(crate) fn build_picker_entries(workspace: &Workspace, cx: &App) -> Vec<PickerEntry> {
    let mut entries: Vec<PickerEntry> = Vec::new();

    let dashboard_data = workspace
        .panel::<Dashboard>(cx)
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

    // GlobalShortcutOnly entries first (cross-project tools with hotkeys)
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
// AutomationModal — wraps picker with tab bar and footer
// ---------------------------------------------------------------------------

pub(crate) struct AutomationModal {
    picker: Entity<Picker<AutomationPickerDelegate>>,
    active_tab: PickerTab,
    mode: PickerMode,
    _subscription: Subscription,
}

impl AutomationModal {
    pub(crate) fn new(
        entries: Vec<PickerEntry>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_mode(entries, PickerMode::Run, None, workspace, window, cx)
    }

    pub(crate) fn new_with_mode(
        entries: Vec<PickerEntry>,
        mode: PickerMode,
        exclude_id: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_tab = match &mode {
            PickerMode::Run => PickerTab::Tools,
            PickerMode::AddPipelineStep { .. } => PickerTab::Automations,
            PickerMode::AddContextScript { .. } => PickerTab::Automations,
        };
        let delegate_mode = mode.clone();

        let picker = cx.new(|cx| {
            let delegate = AutomationPickerDelegate {
                workspace,
                all_entries: entries,
                matches: Vec::new(),
                selected_index: 0,
                active_tab: initial_tab,
                mode: delegate_mode,
                exclude_id,
            };
            let picker = Picker::uniform_list(delegate, window, cx);
            picker.set_query("", window, cx);
            picker
        });

        let subscription = cx.subscribe(&picker, |_this, _picker, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });

        Self {
            picker,
            active_tab: initial_tab,
            mode,
            _subscription: subscription,
        }
    }

    fn set_tab(&mut self, tab: PickerTab, window: &mut Window, cx: &mut Context<Self>) {
        self.active_tab = tab;
        self.picker.update(cx, |picker, cx| {
            picker.delegate.active_tab = tab;
            let query = picker.query(cx);
            picker.update_matches(query, window, cx);
        });
        cx.notify();
    }

    fn switch_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = match self.active_tab {
            PickerTab::Tools => PickerTab::Automations,
            PickerTab::Automations => PickerTab::Tools,
        };
        self.set_tab(next, window, cx);
    }
}

impl Focusable for AutomationModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for AutomationModal {}
impl ModalView for AutomationModal {}

impl Render for AutomationModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AutomationModal")
            .w(rems(34.))
            .elevation_3(cx)
            .overflow_hidden()
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_action(cx.listener(|this, _: &pane::ActivateNextItem, window, cx| {
                this.switch_tab(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &pane::ActivatePreviousItem, window, cx| {
                    this.switch_tab(window, cx);
                }),
            )
            // Tab bar (hidden in AddContextScript mode since only scripts are shown)
            .when(!matches!(self.mode, PickerMode::AddContextScript { .. }), |this| {
                this.child(
                    h_flex().p_2().pb_0p5().w_full().child(
                        ToggleButtonGroup::<ToggleButtonSimple, 2>::single_row(
                            "automation-tabs",
                            [
                                ToggleButtonSimple::new(
                                    "Tools",
                                    cx.listener(|this, _, window, cx| {
                                        this.set_tab(PickerTab::Tools, window, cx);
                                    }),
                                ),
                                ToggleButtonSimple::new(
                                    "Automations",
                                    cx.listener(|this, _, window, cx| {
                                        this.set_tab(PickerTab::Automations, window, cx);
                                    }),
                                ),
                            ],
                        )
                        .style(ui::ToggleButtonGroupStyle::Outlined)
                        .label_size(LabelSize::Default)
                        .auto_width()
                        .selected_index(match self.active_tab {
                            PickerTab::Tools => 0,
                            PickerTab::Automations => 1,
                        }),
                    ),
                )
            })
            // Picker content
            .child(v_flex().child(self.picker.clone()))
            // Footer
            .child(
                h_flex()
                    .w_full()
                    .p_1p5()
                    .gap_2()
                    .justify_end()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new(match &self.mode {
                            PickerMode::AddPipelineStep { .. } => "Add Step",
                            PickerMode::AddContextScript { .. } => "Add Script",
                            PickerMode::Run => "Spawn",
                        })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(KeyBinding::for_action(&menu::Confirm, cx)),
            )
    }
}

// ---------------------------------------------------------------------------
// Delegate
// ---------------------------------------------------------------------------

pub(crate) struct AutomationPickerDelegate {
    workspace: WeakEntity<Workspace>,
    all_entries: Vec<PickerEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    active_tab: PickerTab,
    mode: PickerMode,
    exclude_id: Option<String>,
}

impl AutomationPickerDelegate {
    fn entry_for_match(&self, ix: usize) -> Option<&PickerEntry> {
        self.matches
            .get(ix)
            .and_then(|m| self.all_entries.get(m.candidate_id))
    }
}

impl PickerDelegate for AutomationPickerDelegate {
    type ListItem = ListItem;

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
        match &self.mode {
            PickerMode::AddPipelineStep { .. } => "Add a step...".into(),
            PickerMode::AddContextScript { .. } => "Add a script...".into(),
            PickerMode::Run => match self.active_tab {
                PickerTab::Tools => "Find a tool...".into(),
                PickerTab::Automations => "Find an automation...".into(),
            },
        }
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let tab = self.active_tab;
        let exclude_id = self.exclude_id.clone();
        let candidates: Vec<StringMatchCandidate> = self
            .all_entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                // Filter out excluded entry (e.g., pipeline can't add itself as step)
                if let Some(ref exclude) = exclude_id {
                    if entry.id == *exclude {
                        return false;
                    }
                }
                // In AddContextScript mode, show only script entries regardless of tab
                if matches!(self.mode, PickerMode::AddContextScript { .. }) {
                    return matches!(entry.kind, PickerEntryKind::ContextScript(_));
                }
                match tab {
                    PickerTab::Tools => matches!(
                        entry.kind,
                        PickerEntryKind::Tool(_) | PickerEntryKind::GlobalShortcutOnly(_)
                    ),
                    PickerTab::Automations => matches!(entry.kind, PickerEntryKind::Automation(_)),
                }
            })
            .map(|(ix, entry)| StringMatchCandidate::new(ix, &entry.label))
            .collect();

        let executor = cx.background_executor().clone();

        cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .enumerate()
                    .map(|(_, c)| StringMatch {
                        candidate_id: c.id,
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
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.entry_for_match(self.selected_index).cloned() else {
            return;
        };

        match &self.mode {
            PickerMode::AddPipelineStep { pipeline_source_path, group } => {
                let step = match &entry.kind {
                    PickerEntryKind::Tool(tool) | PickerEntryKind::GlobalShortcutOnly(tool) => {
                        crate::config::PipelineStep {
                            automation: None,
                            tool: Some(tool.id.clone()),
                            group: *group,
                        }
                    }
                    PickerEntryKind::Automation(auto) => {
                        crate::config::PipelineStep {
                            automation: Some(auto.id.clone()),
                            tool: None,
                            group: *group,
                        }
                    }
                    PickerEntryKind::ContextScript(_) => return,
                };
                if let Err(e) = append_step_to_pipeline(pipeline_source_path, &step) {
                    log::error!("Failed to write pipeline step: {e}");
                }
                // Trigger dashboard reload
                window.dispatch_action(Box::new(crate::ToggleFocus), cx);
            }
            PickerMode::AddContextScript { source_path, .. } => {
                let script_name = match &entry.kind {
                    PickerEntryKind::ContextScript(path) => {
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default()
                    }
                    _ => return,
                };
                if let Err(e) = append_context_script_to_toml_at(source_path, &script_name) {
                    log::error!("Failed to add context script: {e}");
                }
                window.dispatch_action(Box::new(crate::ToggleFocus), cx);
            }
            PickerMode::Run => {
                match &entry.kind {
                    PickerEntryKind::Tool(tool) | PickerEntryKind::GlobalShortcutOnly(tool) => {
                        let config_root = entry
                            .config_root
                            .clone()
                            .unwrap_or_else(crate::paths::suite_root);
                        spawn_tool_from_picker(tool, &config_root, &self.workspace, window, cx);
                    }
                    PickerEntryKind::Automation(auto) => {
                        let auto_id = auto.id.clone();
                        if let Some(workspace) = self.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                if let Some(dashboard) = workspace.panel::<Dashboard>(cx) {
                                    dashboard.update(cx, |d, cx| {
                                        if let Some(entry) =
                                            d.automations.iter().find(|a| a.id == auto_id)
                                        {
                                            let id = entry.id.clone();
                                            let label = entry.label.clone();
                                            let prompt = entry.prompt.clone();
                                            d.run_automation(
                                                &id, &label, &prompt, window, cx,
                                            );
                                        }
                                    });
                                }
                            });
                        }
                    }
                    PickerEntryKind::ContextScript(_) => {}
                }
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

        let icon = match &entry.kind {
            PickerEntryKind::Tool(_) | PickerEntryKind::GlobalShortcutOnly(_) => {
                Icon::new(IconName::Settings)
                    .color(Color::Muted)
                    .size(IconSize::Small)
            }
            PickerEntryKind::Automation(_) => Icon::new(IconName::PlayOutlined)
                .color(Color::Muted)
                .size(IconSize::Small),
            PickerEntryKind::ContextScript(_) => Icon::new(IconName::ToolTerminal)
                .color(Color::Muted)
                .size(IconSize::Small),
        };

        let label = HighlightedLabel::new(entry.label.clone(), string_match.positions.clone());

        let mut end = h_flex().gap_1();

        // Cross-project indicator
        if let PickerEntryKind::GlobalShortcutOnly(_) = &entry.kind {
            if let Some(config_root) = &entry.config_root {
                let project_name = config_root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                end = end.child(
                    Label::new(project_name)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
            }
        }

        // Global hotkey badge
        if let Some(hotkey) = &entry.global_hotkey {
            end = end.child(
                Label::new(hotkey.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }

        // Zed keybinding for tools
        if let PickerEntryKind::Tool(_) = &entry.kind {
            let action = RunDashboardTool {
                tool_id: entry.id.clone(),
            };
            end = end.child(KeyBinding::for_action(&action as &dyn gpui::Action, cx));
        }

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(icon)
                .child(label)
                .end_slot(end),
        )
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

// ---------------------------------------------------------------------------
// Pipeline step TOML writer
// ---------------------------------------------------------------------------

fn append_step_to_pipeline(
    source_path: &Path,
    step: &crate::config::PipelineStep,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(source_path)?;
    let mut doc = content.parse::<toml_edit::DocumentMut>()?;

    let steps = doc
        .entry("step")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));

    if let Some(array) = steps.as_array_of_tables_mut() {
        let mut table = toml_edit::Table::new();
        if let Some(auto_id) = &step.automation {
            table.insert("automation", toml_edit::value(auto_id.as_str()));
        }
        if let Some(tool_id) = &step.tool {
            table.insert("tool", toml_edit::value(tool_id.as_str()));
        }
        if let Some(group) = step.group {
            table.insert("group", toml_edit::value(group as i64));
        }
        array.push(table);
    }

    std::fs::write(source_path, doc.to_string())?;
    Ok(())
}

fn append_context_script_to_toml_at(
    source_path: &Path,
    script_name: &str,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(source_path)?;
    let mut doc = content.parse::<toml_edit::DocumentMut>()?;

    let contexts = doc
        .entry("context")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));

    if let Some(array) = contexts.as_array_of_tables_mut() {
        let label = std::path::Path::new(script_name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| script_name.to_string());
        let mut table = toml_edit::Table::new();
        table.insert("source", toml_edit::value("script"));
        table.insert("script", toml_edit::value(script_name));
        table.insert("label", toml_edit::value(label.as_str()));
        table.insert("required", toml_edit::value(false));
        array.push(table);
    }

    std::fs::write(source_path, doc.to_string())?;
    Ok(())
}
