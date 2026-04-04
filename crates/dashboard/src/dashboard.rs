mod automation_picker;
mod config;
mod hotkeys;
mod paths;
pub(crate) mod persistence;
mod scheduler_ui;

use config::{
    AgentEntry, AutomationEntry, BackendEntry, FolderTarget, ParamEntry, ParamType, PipelineStep,
    ScheduleConfig, ToolEntry, ToolSource, ToolTier, icon_for_automation, icon_for_tool,
    load_agents_config, load_automations_registry, load_tools_registry,
};
use paths::{
    DeliveryStatus, automations_dir_for, ensure_config_extracted, ensure_workspace_dirs,
    folder_has_dashboard_config, local_tools_dir_for, resolve_agent_tools_path, resolve_bin,
    resolve_runtime_path, scan_delivery_folder, state_dir_for, suite_root, tools_config_dir_for,
};
use persistence::{
    group_by_section, read_active_folder, read_background_tools, read_collapsed_sections,
    read_destination_folder, read_param_values, read_recent_destinations, read_recent_folders,
    read_section_order, write_active_folder, write_background_tools, write_collapsed_sections,
    write_destination_folder, write_param_values,
};

pub use hotkeys::init_global_hotkeys;
use hotkeys::GlobalShortcutModal;

use agent_ui::AgentPanel;
use menu;
use editor::{Editor, EditorEvent};
use gpui::{
    Action, AnyWindowHandle, App, AsyncApp, ClipboardItem, Context, Corner, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, IntoElement, MouseButton,
    ParentElement, PathPromptOptions, Render, ScrollHandle, SharedString, Styled, Subscription,
    WeakEntity, Window, actions,
};
use schemars::JsonSchema;
use serde::Deserialize;
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use ui::{
    ButtonLike, ButtonStyle, Callout, ContextMenu, Disclosure, Divider, DividerColor,
    DropdownMenu, DropdownStyle, Headline, HeadlineSize, Icon, IconButton, IconName, IconSize,
    Indicator, Label, LabelSize, PopoverMenu,
    ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip, WithScrollbar as _,
    prelude::*,
};
use project::WorktreeId;
use workspace::{
    DraggedSelection, OpenOptions, Pane, ProToolsSessionName, Toast,
    Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
    with_active_or_new_workspace,
};

use postprod_scheduler::{
    ChainOnlyEntry, CompletionReport, RunResult, Scheduler, SchedulerEvent, SyncEntry,
    completion_marker_path,
};
use util::ResultExt as _;

use futures::channel::oneshot;

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

actions!(
    dashboard,
    [
        /// Show the PostProd Tools Dashboard.
        ShowDashboard
    ]
);

/// Run a dashboard tool by its `tool_id`. Users can bind keyboard shortcuts
/// to specific tools by adding entries like:
/// ```json
/// { "context": "Dashboard", "bindings": {
///     "cmd-shift-b": ["dashboard::RunDashboardTool", { "tool_id": "bounceAll" }]
/// }}
/// ```
#[derive(Clone, PartialEq, Deserialize, Default, JsonSchema, Action)]
pub struct RunDashboardTool {
    pub tool_id: String,
}

#[derive(Clone, PartialEq, Deserialize, Default, JsonSchema, Action)]
pub struct RunDashboardAutomation {
    pub automation_id: String,
}

/// Marker type for the context-launcher failure toast notification ID.
struct ContextLauncherToast;

/// Marker type for the auto-disable toast notification ID.
struct AutoDisableToast;

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

/// Reject directory-only drops on the pane overlay when Dashboard is active,
/// so dashboard folder-cards can handle them instead.
fn set_pane_drop_predicate(pane: &Entity<Pane>, workspace: &Workspace, cx: &mut App) {
    let weak_pane = pane.downgrade();
    let weak_project = workspace.project().downgrade();

    pane.update(cx, |pane, _cx| {
        pane.set_can_drop(Some(Arc::new(move |dragged: &dyn Any, _window, cx| {
            let dashboard_active = weak_pane
                .read_with(cx, |pane, _cx| {
                    pane.active_item()
                        .and_then(|item| item.downcast::<Dashboard>())
                        .is_some()
                })
                .unwrap_or(false);

            if !dashboard_active {
                return true;
            }

            if let Some(paths) = dragged.downcast_ref::<ExternalPaths>() {
                return paths.paths().iter().any(|p| !p.is_dir());
            }

            if let Some(selection) = dragged.downcast_ref::<DraggedSelection>() {
                let is_dir_only = weak_project
                    .read_with(cx, |project, cx| {
                        let worktree_store = project.worktree_store().read(cx);
                        selection.items().all(|entry| {
                            worktree_store
                                .entry_for_id(entry.entry_id, cx)
                                .is_some_and(|e| e.is_dir())
                        })
                    })
                    .unwrap_or(false);
                return !is_dir_only;
            }

            true
        })));
    });
}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &ShowDashboard, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            workspace
                .with_local_workspace(window, cx, |workspace, window, cx| {
                    // Find existing Dashboard in any pane
                    let existing = workspace.panes().iter().find_map(|pane| {
                        pane.read(cx)
                            .items()
                            .find_map(|item| item.downcast::<Dashboard>())
                    });

                    if let Some(existing) = existing {
                        workspace.activate_item(&existing, true, true, window, cx);
                    } else {
                        let dashboard = Dashboard::new(workspace, suite_root(), cx);
                        workspace.add_item_to_active_pane(
                            Box::new(dashboard),
                            Some(0),
                            true,
                            window,
                            cx,
                        );
                        workspace.active_pane().update(cx, |pane, _cx| {
                            pane.set_pinned_count(pane.pinned_count() + 1);
                        });
                        set_pane_drop_predicate(workspace.active_pane(), workspace, cx);
                    }
                })
                .detach();
        });
    });

    cx.observe_new::<Workspace>(|workspace, _, _cx| {
        workspace.register_action(
            |workspace, _: &automation_picker::RunAutomationPicker, window, cx| {
                let entries = automation_picker::build_picker_entries(workspace, cx);
                let weak_workspace = workspace.weak_handle();
                workspace.toggle_modal(window, cx, move |window, cx| {
                    automation_picker::AutomationModal::new(entries, weak_workspace, window, cx)
                });
            },
        );
    })
    .detach();
}

/// Ensure a Dashboard tab exists in the workspace, and switch config_root
/// if the active workspace folder has its own dashboard config.
pub fn ensure_dashboard(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // If a dashboard already exists in any pane, nothing to do.
    // Folder switching is handled by the Dashboard's workspace observation.
    let has_dashboard = workspace
        .panes()
        .iter()
        .flat_map(|pane| pane.read(cx).items())
        .any(|item| item.downcast::<Dashboard>().is_some());

    if has_dashboard {
        return;
    }

    // Initial creation: use the first root folder that has dashboard config
    let config_root = workspace
        .root_paths(cx)
        .into_iter()
        .find(|path| folder_has_dashboard_config(path))
        .map(|arc_path| arc_path.to_path_buf())
        .unwrap_or_else(suite_root);

    let dashboard = Dashboard::new(workspace, config_root, cx);
    workspace.add_item_to_center(Box::new(dashboard), window, cx);
    workspace.active_pane().update(cx, |pane, _cx| {
        pane.set_pinned_count(pane.pinned_count() + 1);
    });
    set_pane_drop_predicate(workspace.active_pane(), workspace, cx);
}


// ---------------------------------------------------------------------------
// Agent backend selector
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Default)]
enum AgentBackend {
    #[default]
    Claude,
    Gemini,
    CopyOnly,
    Native,
}

impl AgentBackend {
    fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Gemini => 1,
            Self::CopyOnly => 2,
            Self::Native => 3,
        }
    }

    fn backend_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::CopyOnly => "",
            Self::Native => "native",
        }
    }

    fn badge_label(self, backends: &[BackendEntry]) -> SharedString {
        match self {
            Self::CopyOnly => "(copy prompt)".into(),
            Self::Native => "(native agent)".into(),
            _ => backends
                .iter()
                .find(|b| b.id == self.backend_id())
                .map(|b| SharedString::from(b.label.clone()))
                .unwrap_or_else(|| "run".into()),
        }
    }

    fn badge_color(self) -> Color {
        match self {
            Self::Claude | Self::Gemini => Color::Accent,
            Self::CopyOnly => Color::Muted,
            Self::Native => Color::Success,
        }
    }

    /// Returns (accent, accent_background) for card styling per backend.
    fn card_accent(self, cx: &App) -> (gpui::Hsla, gpui::Hsla) {
        let status = cx.theme().status();
        match self {
            Self::Claude => (status.info, status.info_background),
            Self::Gemini => (status.modified, status.modified_background),
            Self::CopyOnly => (status.hint, status.hint_background),
            Self::Native => (status.success, status.success_background),
        }
    }
}

const MAX_PIPELINE_DEPTH: u32 = 10;

/// Groups pipeline steps for execution. Ungrouped steps each become their own
/// single-element group (sequential). Steps sharing a `group` number are
/// collected into one Vec (parallel). Order follows first occurrence.
fn collect_step_groups(steps: &[PipelineStep]) -> Vec<Vec<PipelineStep>> {
    let mut groups: Vec<Vec<PipelineStep>> = Vec::new();
    let mut group_indices: HashMap<u32, usize> = HashMap::new();
    let mut next_implicit = u32::MAX;

    for step in steps {
        let group_key = match step.group {
            Some(g) => g,
            None => {
                next_implicit = next_implicit.wrapping_sub(1);
                next_implicit
            }
        };

        if let Some(&idx) = group_indices.get(&group_key) {
            groups[idx].push(step.clone());
        } else {
            let idx = groups.len();
            group_indices.insert(group_key, idx);
            groups.push(vec![step.clone()]);
        }
    }
    groups
}

// ---------------------------------------------------------------------------
// Dashboard struct
// ---------------------------------------------------------------------------

pub struct Dashboard {
    workspace: WeakEntity<Workspace>,
    last_worktree_override: Option<WorktreeId>,
    _workspace_observation: Option<Subscription>,
    focus_handle: FocusHandle,
    pub(crate) config_root: PathBuf,
    runtime_path: PathBuf,
    agent_tools_path: PathBuf,
    // TOML-driven tool registry
    pub(crate) tools: Vec<ToolEntry>,
    // Session polling
    session_path: Option<String>,
    session_name: Option<String>,
    _session_poll_task: gpui::Task<()>,
    // Active folder
    active_folder: Option<PathBuf>,
    recent_folders: Vec<PathBuf>,
    // Destination folder
    destination_folder: Option<PathBuf>,
    recent_destinations: Vec<PathBuf>,
    // Delivery status
    delivery_status: DeliveryStatus,
    _delivery_scan_task: gpui::Task<()>,
    // Automations (loaded from TOML)
    pub(crate) automations: Vec<AutomationEntry>,
    // Default context entries (from config/default-context/)
    default_contexts: Vec<config::ContextEntry>,
    agent_backend: AgentBackend,
    backends: Vec<BackendEntry>,
    agent_launchers: Vec<AgentEntry>,
    _automations_reload_task: gpui::Task<()>,
    _tools_reload_task: gpui::Task<()>,
    _agents_reload_task: gpui::Task<()>,
    // Background execution mode per tool
    background_tools: HashSet<String>,
    // Collapsed section state (persisted)
    collapsed_sections: HashSet<String>,
    // Section display order (optional, from config/.state/section_order)
    section_order: Vec<String>,
    // Expanded automation prompt previews (ephemeral)
    expanded_automations: HashSet<String>,
    // Automations showing the full context CRUD editor (gear toggle)
    automations_in_context_edit: HashSet<String>,
    // Config parse errors
    tools_error: Option<String>,
    automations_error: Option<String>,
    // Scroll
    scroll_handle: ScrollHandle,
    // Param values (inline parameter fields on cards)
    param_values: HashMap<String, HashMap<String, String>>,
    param_editors: HashMap<(String, String), Entity<Editor>>,
    _param_editor_subscriptions: Vec<Subscription>,
    _param_write_task: Option<gpui::Task<()>>,
    // Scheduler
    scheduler: Entity<Scheduler>,
    window_handle: Option<AnyWindowHandle>,
    _scheduler_subscription: Subscription,
    // Pipelines
    active_pipelines: HashSet<String>,
    pipelines_in_edit_mode: HashSet<String>,
    pipeline_cancel_flags: HashMap<String, Arc<AtomicBool>>,
    pipelines_pending_delete: HashSet<String>,
    // Pipeline creation
    pending_new_pipeline: Option<Entity<Editor>>,
    _pending_pipeline_subscription: Option<Subscription>,
    // "Add Automation" ghost card state
    pending_new_automation: Option<Entity<Editor>>,
    _pending_automation_subscription: Option<Subscription>,
}

pub(crate) fn resolve_tool_command(
    tool: &ToolEntry,
    runtime_path: &Path,
    agent_tools_path: &Path,
    config_root: &Path,
    session_path: &Option<String>,
    active_folder: &Option<PathBuf>,
    tool_param_values: &HashMap<String, String>,
) -> (String, Vec<String>, PathBuf, HashMap<String, String>) {
    let (command, cwd) = match tool.source {
        ToolSource::Agent => {
            let cmd = agent_tools_path
                .join(&tool.binary)
                .to_string_lossy()
                .to_string();
            let work_dir = agent_tools_path.to_path_buf();
            (cmd, work_dir)
        }
        ToolSource::Local => {
            let local_tools = local_tools_dir_for(config_root);
            let tool_dir = if tool.cwd.is_empty() {
                local_tools
            } else {
                local_tools.join(&tool.cwd)
            };
            let cmd = tool_dir.join(&tool.binary).to_string_lossy().to_string();
            let work_dir = tool_dir;
            (cmd, work_dir)
        }
        ToolSource::Runtime => {
            let cmd = runtime_path
                .join(&tool.cwd)
                .join(&tool.binary)
                .to_string_lossy()
                .to_string();
            let work_dir = if tool.tier == ToolTier::Standard {
                if let Some(pa) = active_folder {
                    pa.clone()
                } else {
                    runtime_path.join(&tool.cwd)
                }
            } else {
                runtime_path.join(&tool.cwd)
            };
            (cmd, work_dir)
        }
    };

    let mut args = match tool.source {
        ToolSource::Agent => vec!["--output-json".to_string()],
        ToolSource::Runtime | ToolSource::Local => vec![],
    };

    if tool.needs_session {
        if let Some(session) = session_path {
            args.insert(0, session.clone());
            args.insert(0, "--session".to_string());
        }
    }

    args.extend(tool.extra_args.iter().cloned());

    for param in &tool.params {
        if let Some(value) = tool_param_values.get(&param.key) {
            if !value.is_empty() {
                args.push(format!("--{}", param.key));
                args.push(value.clone());
            }
        }
    }

    let mut env = HashMap::new();
    let ffmpeg_candidate = runtime_path.join("tools/ffmpeg");
    if ffmpeg_candidate.exists() {
        env.insert(
            "FFMPEG_PATH".to_string(),
            ffmpeg_candidate.to_string_lossy().to_string(),
        );
    }

    (command, args, cwd, env)
}

impl Dashboard {
    pub fn new(workspace: &Workspace, config_root: PathBuf, cx: &mut App) -> Entity<Self> {
        let runtime_path = resolve_runtime_path();

        let agent_tools_path = resolve_agent_tools_path();

        ensure_workspace_dirs();
        ensure_config_extracted(cx);

        let active_folder = read_active_folder(&config_root);
        let recent_folders = read_recent_folders(&config_root);
        let destination_folder = read_destination_folder(&config_root);
        let recent_destinations = read_recent_destinations(&config_root);
        let (automations, automations_error) = load_automations_registry(&config_root);
        let (tools, tools_error) = load_tools_registry(&config_root);
        let (backends, agent_launchers, _agents_error) = load_agents_config(&config_root);
        let default_contexts = config::load_default_contexts(&config_root);
        let background_tools = read_background_tools(&config_root);
        let collapsed_sections = read_collapsed_sections(&config_root);
        let section_order = read_section_order(&config_root);
        let mut param_values = read_param_values(&config_root);

        // Seed defaults for any params not yet persisted
        for entry in &automations {
            for param in &entry.params {
                param_values
                    .entry(entry.id.clone())
                    .or_default()
                    .entry(param.key.clone())
                    .or_insert_with(|| param.default.clone());
            }
        }
        for entry in &tools {
            for param in &entry.params {
                param_values
                    .entry(entry.id.clone())
                    .or_default()
                    .entry(param.key.clone())
                    .or_insert_with(|| param.default.clone());
            }
        }

        cx.new(|cx| {
            // Spawn session polling task (every 5 seconds)
            let poll_binary = runtime_path.join("tools/get_session_path");
            let session_poll_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    let binary = poll_binary.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            smol::process::Command::new(&binary)
                                .output()
                                .await
                                .ok()
                                .filter(|o| o.status.success())
                                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                                .filter(|s| !s.is_empty())
                        })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                            let name = result.as_ref().map(|p| {
                                Path::new(p)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string()
                            });
                            let session_changed = dashboard.session_path != result;
                            dashboard.session_path = result;
                            dashboard.session_name = name;

                            // Register session directory as a hidden worktree so the
                            // native agent's terminal tool can cd into it.
                            if session_changed {
                                if let Some(session) = &dashboard.session_path {
                                    if let Some(session_dir) = Path::new(session).parent() {
                                        let session_dir = session_dir.to_path_buf();
                                        let workspace = dashboard.workspace.clone();
                                        workspace.update(cx, |workspace, cx| {
                                            let project = workspace.project().clone();
                                            project
                                                .update(cx, |project, cx| {
                                                    project.find_or_create_worktree(
                                                        &session_dir,
                                                        false,
                                                        cx,
                                                    )
                                                })
                                                .detach();
                                        }).log_err();
                                    }
                                }
                            }

                            // Update the global for window title
                            let global_name = dashboard.session_name.clone().unwrap_or_default();
                            cx.set_global(ProToolsSessionName(global_name));

                            cx.notify();
                        },
                    ).log_err();

                    cx.background_executor().timer(Duration::from_secs(5)).await;
                }
            });

            // Spawn delivery scan task (every 15 seconds)
            let delivery_scan_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    let status = cx
                        .background_executor()
                        .spawn(async { scan_delivery_folder() })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                            dashboard.delivery_status = status;
                            cx.notify();
                        },
                    ).log_err();

                    cx.background_executor()
                        .timer(Duration::from_secs(15))
                        .await;
                }
            });

            // Spawn automations reload task (every 10 seconds)
            let automations_reload_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(10))
                        .await;

                    let config_root = this.update(cx, |dashboard, _| {
                        dashboard.config_root.clone()
                    });
                    let Ok(config_root) = config_root else { break };
                    let loaded_from = config_root.clone();

                    let (merged, error) = cx
                        .background_executor()
                        .spawn(async move { load_automations_registry(&config_root) })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                            if dashboard.config_root != loaded_from { return; }
                            dashboard.automations = merged;
                            dashboard.automations_error = error;
                            dashboard.section_order = read_section_order(&dashboard.config_root);
                            // Seed defaults for new params and clean stale editors
                            for entry in &dashboard.automations {
                                for param in &entry.params {
                                    dashboard
                                        .param_values
                                        .entry(entry.id.clone())
                                        .or_default()
                                        .entry(param.key.clone())
                                        .or_insert_with(|| param.default.clone());
                                }
                            }
                            let valid_keys: HashSet<(String, String)> = dashboard
                                .automations
                                .iter()
                                .flat_map(|a| {
                                    a.params.iter().map(|p| (a.id.clone(), p.key.clone()))
                                })
                                .chain(dashboard.tools.iter().flat_map(|t| {
                                    t.params.iter().map(|p| (t.id.clone(), p.key.clone()))
                                }))
                                .collect();
                            dashboard
                                .param_editors
                                .retain(|k, _| valid_keys.contains(k));

                            // Sync schedule entries to scheduler
                            let default_folder = dashboard.active_folder.clone()
                                .unwrap_or_else(|| dashboard.config_root.clone());
                            let sync_entries = Self::build_sync_entries(&dashboard.automations);
                            let unscheduled = Self::build_unscheduled_entries(&dashboard.automations, &default_folder);
                            dashboard.scheduler.update(cx, |scheduler, _cx| {
                                scheduler.sync_entries(sync_entries, unscheduled, default_folder);
                            });

                            cx.notify();
                        },
                    ).log_err();
                }
            });

            // Spawn tools reload task (every 10 seconds)
            let tools_reload_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(10))
                        .await;

                    let config_root = this.update(cx, |dashboard, _| {
                        dashboard.config_root.clone()
                    });
                    let Ok(config_root) = config_root else { break };
                    let loaded_from = config_root.clone();

                    let (merged, error) = cx
                        .background_executor()
                        .spawn(async move { load_tools_registry(&config_root) })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                            if dashboard.config_root != loaded_from { return; }
                            dashboard.tools = merged;
                            dashboard.tools_error = error;
                            // Seed defaults for new tool params
                            for entry in &dashboard.tools {
                                for param in &entry.params {
                                    dashboard
                                        .param_values
                                        .entry(entry.id.clone())
                                        .or_default()
                                        .entry(param.key.clone())
                                        .or_insert_with(|| param.default.clone());
                                }
                            }
                            cx.notify();
                        },
                    ).log_err();
                }
            });

            // Spawn agents config reload task (every 10 seconds)
            let agents_reload_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(10))
                        .await;

                    let config_root = this.update(cx, |dashboard, _| {
                        dashboard.config_root.clone()
                    });
                    let Ok(config_root) = config_root else { break };
                    let loaded_from = config_root.clone();

                    let (backends, agent_launchers, _err) = cx
                        .background_executor()
                        .spawn(async move { load_agents_config(&config_root) })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                            if dashboard.config_root != loaded_from { return; }
                            dashboard.backends = backends;
                            dashboard.agent_launchers = agent_launchers;
                            cx.notify();
                        },
                    ).log_err();
                }
            });

            // Create scheduler entity
            let state_dir = state_dir_for(&config_root);
            let scheduler = cx.new(|cx| {
                Scheduler::new(state_dir.join("scheduler_status.json"), cx)
            });

            // Initial schedule sync
            {
                let default_folder = active_folder.clone()
                    .unwrap_or_else(|| config_root.clone());
                let entries = Self::build_sync_entries(&automations);
                let unscheduled = Self::build_unscheduled_entries(&automations, &default_folder);

                scheduler.update(cx, |scheduler, _cx| {
                    scheduler.sync_entries(entries, unscheduled, default_folder);
                });
            }

            let _scheduler_subscription = cx.subscribe(&scheduler, |dashboard, _scheduler, event, cx| {
                match event {
                    SchedulerEvent::Fire { automation_id, active_folder, chain_depth } => {
                        let is_pipeline = dashboard.automations.iter()
                            .find(|a| &a.id == automation_id)
                            .is_some_and(|a| a.is_pipeline());
                        if is_pipeline {
                            if let Some(entry) = dashboard.automations.iter().find(|a| &a.id == automation_id).cloned() {
                                dashboard.run_pipeline(&entry, active_folder, *chain_depth, RevealStrategy::Never, cx);
                            }
                        } else {
                            dashboard.run_scheduled_automation(automation_id, active_folder, *chain_depth, cx);
                        }
                    }
                    SchedulerEvent::Skipped { automation_id, reason } => {
                        log::info!("Scheduler: skipped {automation_id}: {reason}");
                    }
                    SchedulerEvent::MissedJob { automation_id, policy } => {
                        log::info!("Scheduler: missed job {automation_id} (policy: {policy:?})");
                    }
                    SchedulerEvent::AutoDisabled { automation_id, consecutive_failures } => {
                        log::warn!("Scheduler: {automation_id} auto-disabled after {consecutive_failures} failures — notifying user");
                        let label = dashboard
                            .automations
                            .iter()
                            .find(|a| &a.id == automation_id)
                            .map(|a| a.label.clone())
                            .unwrap_or_else(|| automation_id.clone());
                        if let Some(workspace) = dashboard.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                workspace.show_toast(
                                    Toast::new(
                                        NotificationId::unique::<AutoDisableToast>(),
                                        format!(
                                            "\"{}\" auto-disabled after {} consecutive failures",
                                            label, consecutive_failures
                                        ),
                                    ),
                                    cx,
                                );
                            });
                        }
                        cx.notify();
                    }
                }
            });

            // Observe the workspace for active worktree override changes
            // (fires when user switches folders via the title bar dropdown)
            let workspace_observation = workspace.weak_handle().upgrade().map(|ws_entity| {
                cx.observe(&ws_entity, |dashboard: &mut Dashboard, workspace_entity, cx| {
                    let workspace = workspace_entity.read(cx);
                    let current = workspace.active_worktree_override();
                    if current == dashboard.last_worktree_override {
                        return;
                    }
                    dashboard.last_worktree_override = current;
                    if let Some(worktree_id) = current {
                        let folder = {
                            let project = workspace.project().read(cx);
                            project
                                .visible_worktrees(cx)
                                .find(|wt| wt.read(cx).id() == worktree_id)
                                .map(|wt| wt.read(cx).abs_path().to_path_buf())
                        };
                        if let Some(folder) = folder {
                            if folder_has_dashboard_config(&folder)
                                && folder != dashboard.config_root
                            {
                                dashboard.switch_config_root(folder, cx);
                            }
                        }
                    }
                })
            });

            Self {
                workspace: workspace.weak_handle(),
                last_worktree_override: None,
                _workspace_observation: workspace_observation,
                focus_handle: cx.focus_handle(),
                config_root,
                runtime_path,
                agent_tools_path,
                tools,
                session_path: None,
                session_name: None,
                _session_poll_task: session_poll_task,
                active_folder,
                recent_folders,
                destination_folder,
                recent_destinations,
                delivery_status: DeliveryStatus::default(),
                _delivery_scan_task: delivery_scan_task,
                automations,
                default_contexts,
                agent_backend: AgentBackend::Claude,
                backends,
                agent_launchers,
                _automations_reload_task: automations_reload_task,
                _tools_reload_task: tools_reload_task,
                _agents_reload_task: agents_reload_task,
                background_tools,
                collapsed_sections,
                section_order,
                expanded_automations: HashSet::new(),
                automations_in_context_edit: HashSet::new(),
                tools_error,
                automations_error,
                scroll_handle: ScrollHandle::new(),
                param_values,
                param_editors: HashMap::new(),
                _param_editor_subscriptions: Vec::new(),
                _param_write_task: None,
                scheduler,
                window_handle: None,
                _scheduler_subscription,
                active_pipelines: HashSet::new(),
                pipelines_in_edit_mode: HashSet::new(),
                pipeline_cancel_flags: HashMap::new(),
                pipelines_pending_delete: HashSet::new(),
                pending_new_pipeline: None,
                _pending_pipeline_subscription: None,
                pending_new_automation: None,
                _pending_automation_subscription: None,
            }
        })
    }

    pub(crate) fn spawn_tool_entry(
        tool: &ToolEntry,
        runtime_path: &Path,
        agent_tools_path: &Path,
        config_root: &Path,
        session_path: &Option<String>,
        active_folder: &Option<PathBuf>,
        tool_param_values: &HashMap<String, String>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let (command, args, cwd, env) = resolve_tool_command(
            tool,
            runtime_path,
            agent_tools_path,
            config_root,
            session_path,
            active_folder,
            tool_param_values,
        );

        let mut spawn = SpawnInTerminal {
            id: TaskId(format!("dashboard-{}", tool.id)),
            label: tool.label.clone(),
            full_label: tool.label.clone(),
            command: Some(command),
            args,
            command_label: tool.label.clone(),
            cwd: Some(cwd),
            use_new_terminal: true,
            allow_concurrent_runs: false,
            reveal: RevealStrategy::Always,
            show_command: true,
            show_rerun: true,
            ..Default::default()
        };

        for (key, value) in env {
            spawn.env.insert(key, value);
        }

        workspace.spawn_in_terminal(spawn, window, cx).detach();
    }

    pub(crate) fn spawn_tool_background(
        tool: &ToolEntry,
        runtime_path: &Path,
        agent_tools_path: &Path,
        config_root: &Path,
        session_path: &Option<String>,
        active_folder: &Option<PathBuf>,
        tool_param_values: &HashMap<String, String>,
        cx: &mut Context<Workspace>,
    ) {
        let (command, args, cwd, env) = resolve_tool_command(
            tool,
            runtime_path,
            agent_tools_path,
            config_root,
            session_path,
            active_folder,
            tool_param_values,
        );
        let tool_label = tool.label.clone();

        cx.spawn(
            async move |_this: WeakEntity<Workspace>, cx: &mut AsyncApp| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let mut cmd = smol::process::Command::new(&command);
                        cmd.args(&args).current_dir(&cwd);
                        for (key, value) in &env {
                            cmd.env(key, value);
                        }
                        cmd.output().await
                    })
                    .await;

                match result {
                    Ok(output) if output.status.success() => {
                        log::info!("background tool '{}': success", tool_label);
                    }
                    Ok(output) => {
                        log::warn!(
                            "background tool '{}': exit {}: {}",
                            tool_label,
                            output.status,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    Err(e) => {
                        log::error!("background tool '{}': {}", tool_label, e);
                    }
                }
            },
        )
        .detach();
    }

    /// Best working directory for AI agents. Priority:
    /// 1. Active folder (user-selected working folder — defines the agent's
    ///    workspace and file-system permissions)
    /// 2. Open session's parent folder (contextual hint)
    /// 3. suite_root (~/PostProd_IDE)
    fn agent_cwd(&self) -> PathBuf {
        if let Some(pa) = &self.active_folder {
            return pa.clone();
        }
        if let Some(session) = &self.session_path {
            let session_path = Path::new(session);
            if let Some(parent) = session_path.parent() {
                if parent.is_dir() {
                    return parent.to_path_buf();
                }
            }
        }
        self.config_root.clone()
    }

    fn switch_config_root(&mut self, new_root: PathBuf, cx: &mut Context<Self>) {
        log::info!(
            "dashboard: switching config_root from {} to {}",
            self.config_root.display(),
            new_root.display()
        );
        self.config_root = new_root;

        // Ensure state dir exists for the new config_root
        let state = state_dir_for(&self.config_root);
        if !state.exists() {
            std::fs::create_dir_all(&state).log_err();
        }

        // Immediate reload of all config
        let (tools, tools_error) = load_tools_registry(&self.config_root);
        let (automations, automations_error) = load_automations_registry(&self.config_root);
        let (backends, agent_launchers, _) = load_agents_config(&self.config_root);

        self.tools = tools;
        self.tools_error = tools_error;
        self.automations = automations;
        self.automations_error = automations_error;
        self.default_contexts = config::load_default_contexts(&self.config_root);
        self.backends = backends;
        self.agent_launchers = agent_launchers;

        // Reload per-folder state
        self.active_folder = read_active_folder(&self.config_root);
        self.recent_folders = read_recent_folders(&self.config_root);
        self.destination_folder = read_destination_folder(&self.config_root);
        self.recent_destinations = read_recent_destinations(&self.config_root);
        self.background_tools = read_background_tools(&self.config_root);
        self.collapsed_sections = read_collapsed_sections(&self.config_root);
        self.section_order = read_section_order(&self.config_root);
        self.param_values = read_param_values(&self.config_root);

        // Seed defaults for params not yet persisted
        for entry in &self.automations {
            for param in &entry.params {
                self.param_values
                    .entry(entry.id.clone())
                    .or_default()
                    .entry(param.key.clone())
                    .or_insert_with(|| param.default.clone());
            }
        }
        for entry in &self.tools {
            for param in &entry.params {
                self.param_values
                    .entry(entry.id.clone())
                    .or_default()
                    .entry(param.key.clone())
                    .or_insert_with(|| param.default.clone());
            }
        }

        // Clear cached param editors (they hold old state)
        self.param_editors.clear();
        self._param_editor_subscriptions.clear();
        self._param_write_task = None;
        self.expanded_automations.clear();
        self.pending_new_pipeline = None;
        self._pending_pipeline_subscription = None;

        cx.notify();
    }



    pub(crate) fn run_automation(
        &self,
        entry_id: &str,
        entry_label: &str,
        prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback_prompt = self.resolve_variables(prompt, entry_id);

        // Check if this automation has chain config (for terminal backends only)
        let has_chain = self.automations.iter()
            .find(|a| a.id == entry_id)
            .and_then(|a| a.chain.as_ref())
            .is_some_and(|c| !c.triggers.is_empty());

        // Prepare chain tracking for terminal backends
        let chain_marker = if has_chain {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let state_dir = state_dir_for(&self.config_root);
            let marker_path = completion_marker_path(&state_dir, entry_id, timestamp);
            if let Some(parent) = marker_path.parent() {
                std::fs::create_dir_all(parent).log_err();
            }
            Some(marker_path)
        } else {
            None
        };

        // Capture values needed by the async block
        let workspace = self.workspace.clone();
        let agent_backend = self.agent_backend;
        let backends = self.backends.clone();
        let agent_cwd = self.agent_cwd();
        let entry_id = entry_id.to_string();
        let entry_label = entry_label.to_string();

        // Collect context entries (default + per-automation)
        let contexts = {
            let entry = self.automations.iter().find(|a| a.id == entry_id);
            let mut all_contexts = if entry.is_some_and(|e| !e.skip_default_context) {
                self.default_contexts.clone()
            } else {
                Vec::new()
            };
            if let Some(e) = entry {
                all_contexts.extend(e.contexts.clone());
            }
            all_contexts
        };
        let config_root = self.config_root.clone();
        let session_path_for_env = self.session_path.clone().unwrap_or_default();
        let active_folder_for_env = self.active_folder.as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let destination_for_env = self.destination_folder.as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let param_values_for_env = self.param_values.get(&entry_id).cloned().unwrap_or_default();

        // If chain is configured but backend can't support it, warn
        if has_chain && matches!(agent_backend, AgentBackend::Native | AgentBackend::CopyOnly) {
            log::warn!(
                "dashboard: automation '{}' has chain config but uses non-terminal backend — chains require a terminal backend",
                entry_id
            );
        }

        // Spawn the completion poller before the async block (needs &self + Context)
        if let Some(marker_path) = &chain_marker {
            if !matches!(agent_backend, AgentBackend::Native | AgentBackend::CopyOnly) {
                let timeout_secs = self.automations.iter()
                    .find(|a| a.id == entry_id)
                    .and_then(|a| a.schedule.as_ref())
                    .map(|s| s.timeout)
                    .unwrap_or(3600);
                self.spawn_completion_poller(
                    entry_id.clone(),
                    marker_path.clone(),
                    timeout_secs,
                    0,
                    cx,
                );
            }
        }

        cx.spawn_in(window, async move |_this, cx| {
            // Phase 1: Gather context on background thread.
            let context_result: Result<String, String> = if contexts.is_empty() {
                Ok(String::new())
            } else {
                let contexts = contexts.clone();
                let config_root = config_root.clone();
                let session = session_path_for_env.clone();
                let folder = active_folder_for_env.clone();
                let dest = destination_for_env.clone();
                let params = param_values_for_env.clone();
                cx.background_executor().spawn(async move {
                    gather_context_blocking(
                        &contexts, &config_root, &session, &folder, &dest, &params,
                    )
                }).await
            };

            let enriched_prompt = match context_result {
                Ok(ctx) if ctx.is_empty() => fallback_prompt.clone(),
                Ok(ctx) => format!("{ctx}\n=== End of pre-loaded context ===\n\n{fallback_prompt}"),
                Err(reason) => {
                    log::warn!("context gathering failed for '{}': {reason}", entry_id);
                    let ws_for_toast = workspace.clone();
                    let fallback = fallback_prompt.clone();
                    let backends_for_toast = backends.clone();
                    let cwd_for_toast = agent_cwd.clone();
                    let id_for_toast = entry_id.clone();
                    let label_for_toast = entry_label.clone();

                    workspace.update_in(cx, |workspace, _window, cx| {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<ContextLauncherToast>(),
                                format!("Context gathering failed for '{}': {}", entry_label, reason),
                            )
                            .on_click(
                                "Run without context",
                                move |window, cx| {
                                    if agent_backend == AgentBackend::CopyOnly {
                                        cx.write_to_clipboard(ClipboardItem::new_string(fallback.clone()));
                                        return;
                                    }
                                    if agent_backend == AgentBackend::Native {
                                        let prompt = fallback.clone();
                                        ws_for_toast.update(cx, |workspace, cx| {
                                            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                                panel.update(cx, |panel, cx| {
                                                    panel.new_external_thread_with_auto_submit(prompt, window, cx);
                                                });
                                                workspace.focus_panel::<AgentPanel>(window, cx);
                                            }
                                        }).log_err();
                                        return;
                                    }
                                    // Terminal fallback
                                    if let Some(spawn) = build_temp_file_terminal_command(
                                        &fallback, &id_for_toast, &label_for_toast,
                                        agent_backend, &backends_for_toast, &cwd_for_toast, &None,
                                    ) {
                                        ws_for_toast.update(cx, |workspace, cx| {
                                            workspace.spawn_in_terminal(spawn, window, cx).detach();
                                        }).log_err();
                                    }
                                },
                            ),
                            cx,
                        );
                    }).log_err();
                    return;
                }
            };

            // Phase 2: Route to backend.
            if agent_backend == AgentBackend::Native {
                let prompt = enriched_prompt;
                workspace.update_in(cx, |workspace, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.new_external_thread_with_auto_submit(prompt, window, cx);
                        });
                        workspace.focus_panel::<AgentPanel>(window, cx);
                    }
                }).log_err();
                return;
            }

            if agent_backend == AgentBackend::CopyOnly {
                cx.update(|_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(enriched_prompt));
                }).log_err();
                return;
            }

            // Terminal backend: write temp file and spawn
            if let Some(spawn) = build_temp_file_terminal_command(
                &enriched_prompt, &entry_id, &entry_label,
                agent_backend, &backends, &agent_cwd, &chain_marker,
            ) {
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.spawn_in_terminal(spawn, window, cx).detach();
                }).log_err();
            }
        })
        .detach();
    }

    fn resolve_variables(&self, prompt: &str, entry_id: &str) -> String {
        let mut resolved = if let Some(session) = &self.session_path {
            prompt.replace("{session_path}", session)
        } else {
            prompt.replace("{session_path}", "<no session open>")
        };

        resolved = if let Some(pa) = &self.active_folder {
            resolved.replace("{active_folder}", &pa.to_string_lossy())
        } else {
            resolved.replace("{active_folder}", "<no active folder selected>")
        };

        resolved = if let Some(pd) = &self.destination_folder {
            resolved.replace("{destination_folder}", &pd.to_string_lossy())
        } else {
            resolved.replace("{destination_folder}", "<no destination folder selected>")
        };

        if let Some(values) = self.param_values.get(entry_id) {
            for (key, value) in values {
                resolved = resolved.replace(&format!("{{{key}}}"), value);
            }
        }

        resolved
    }

    /// Resolves prompt, appends completion instruction, spawns terminal.
    /// Returns (marker_path, terminal_task) for the caller to poll/race.
    /// Both `run_scheduled_automation()` and `run_pipeline()` use this —
    /// they differ only in how they handle completion.
    fn spawn_automation_in_terminal(
        &self,
        automation_id: &str,
        active_folder: &Path,
        reveal: RevealStrategy,
        label_prefix: &str,
        cx: &mut Context<Self>,
    ) -> Option<(PathBuf, gpui::Task<()>)> {
        let entry = match self.automations.iter().find(|a| a.id == automation_id) {
            Some(e) => e.clone(),
            None => {
                log::warn!("spawn_automation_in_terminal: automation {automation_id} not found");
                return None;
            }
        };

        let backend_config = match self.backends.iter().find(|b| !b.command.is_empty()) {
            Some(b) => b.clone(),
            None => {
                log::warn!("spawn_automation_in_terminal: no CLI backend configured");
                return None;
            }
        };

        // Build completion marker path
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let state_dir = state_dir_for(&self.config_root);
        let marker_path = completion_marker_path(&state_dir, automation_id, timestamp);
        if let Some(parent) = marker_path.parent() {
            std::fs::create_dir_all(parent).log_err();
        }

        let resolved_prompt = self.resolve_variables(&entry.prompt, &entry.id);

        // Collect context entries (default + per-automation)
        let all_contexts = {
            let mut contexts = if !entry.skip_default_context {
                self.default_contexts.clone()
            } else {
                Vec::new()
            };
            contexts.extend(entry.contexts.clone());
            contexts
        };
        let config_root = self.config_root.clone();
        let session_for_env = self.session_path.clone().unwrap_or_default();
        let folder_for_env = active_folder.to_string_lossy().to_string();
        let dest_for_env = self.destination_folder.as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let params_for_env = self.param_values.get(&entry.id).cloned().unwrap_or_default();

        let command = resolve_bin(&backend_config.command);
        let flags = backend_config.flags;
        let agent_cwd = self.agent_cwd();
        let entry_id = entry.id;
        let entry_label = entry.label;

        let display_label = if label_prefix.is_empty() {
            entry_label
        } else {
            format!("[{label_prefix}] {}", entry_label)
        };

        let completion_instruction = Self::completion_report_instruction(&marker_path);

        let workspace = self.workspace.clone();
        let window_handle = self.window_handle;

        let (tx, rx) = oneshot::channel();

        cx.spawn(async move |_this, cx: &mut AsyncApp| -> anyhow::Result<()> {
            // Gather context on background thread
            let context_text = if all_contexts.is_empty() {
                String::new()
            } else {
                match cx.background_executor().spawn(async move {
                    gather_context_blocking(
                        &all_contexts, &config_root, &session_for_env,
                        &folder_for_env, &dest_for_env, &params_for_env,
                    )
                }).await {
                    Ok(text) => text,
                    Err(reason) => {
                        log::warn!("context gathering failed for pipeline step '{entry_id}': {reason}");
                        String::new()
                    }
                }
            };

            // Build enriched prompt with context + completion instruction
            let enriched_prompt = if context_text.is_empty() {
                format!("{resolved_prompt}{completion_instruction}")
            } else {
                format!("{context_text}\n=== End of pre-loaded context ===\n\n{resolved_prompt}{completion_instruction}")
            };

            // Write prompt to temp file
            let temp_path = std::env::temp_dir().join(format!("postprod_prompt_{entry_id}.md"));
            if let Err(e) = std::fs::write(&temp_path, &enriched_prompt) {
                log::error!("failed to write temp prompt file: {e}");
                return Ok(());
            }

            let temp_escaped = temp_path.display().to_string().replace('\'', "'\\''");
            let full_command = format!("cat '{temp_escaped}' | {command} {flags}");

            let spawn = SpawnInTerminal {
                id: TaskId(format!("automation-{entry_id}")),
                label: display_label.clone(),
                full_label: display_label.clone(),
                command: Some(full_command),
                args: vec![],
                command_label: display_label,
                cwd: Some(agent_cwd),
                use_new_terminal: true,
                allow_concurrent_runs: false,
                reveal,
                show_command: false,
                show_rerun: true,
                ..Default::default()
            };

            let Some(window_handle) = window_handle else {
                log::warn!("spawn_automation_in_terminal: window handle not yet available");
                return Ok(());
            };
            let Some(workspace) = workspace.upgrade() else {
                log::warn!("spawn_automation_in_terminal: workspace released");
                return Ok(());
            };
            if let Ok(terminal_task) = window_handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.spawn_in_terminal(spawn, window, cx)
                })
            }) {
                tx.send(terminal_task).ok();
            }
            Ok(())
        })
        .detach();

        let terminal_task = cx.background_spawn(async move {
            if let Ok(inner_task) = rx.await {
                let _ = inner_task.await;
            }
        });

        Some((marker_path, terminal_task))
    }

    fn run_scheduled_automation(
        &self,
        automation_id: &str,
        active_folder: &Path,
        chain_depth: u32,
        cx: &mut Context<Self>,
    ) {
        let Some((marker_path, _terminal_task)) = self.spawn_automation_in_terminal(
            automation_id,
            active_folder,
            RevealStrategy::Never,
            "Scheduled",
            cx,
        ) else {
            return;
        };

        let automation_id_owned = automation_id.to_string();
        let timeout_secs = self.scheduler.read(cx)
            .entries()
            .get(&automation_id_owned)
            .map(|e| e.timeout_secs)
            .unwrap_or(3600);

        self.spawn_completion_poller(automation_id_owned, marker_path, timeout_secs, chain_depth, cx);
    }

    fn run_pipeline(
        &mut self,
        pipeline_entry: &AutomationEntry,
        active_folder: &Path,
        depth: u32,
        reveal: RevealStrategy,
        cx: &mut Context<Self>,
    ) {
        let pipeline_id = pipeline_entry.id.clone();

        if self.active_pipelines.contains(&pipeline_id) {
            log::warn!("Pipeline '{pipeline_id}' is already running — skipping");
            return;
        }
        if depth >= MAX_PIPELINE_DEPTH {
            log::warn!("Pipeline depth limit ({MAX_PIPELINE_DEPTH}) reached at '{pipeline_id}'");
            return;
        }
        if pipeline_entry.steps.is_empty() {
            log::warn!("Pipeline '{pipeline_id}' has no steps — nothing to run");
            return;
        }

        self.active_pipelines.insert(pipeline_id.clone());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.pipeline_cancel_flags.insert(pipeline_id.clone(), cancel_flag.clone());
        cx.notify();

        let steps = pipeline_entry.steps.clone();
        let active_folder = active_folder.to_path_buf();
        let config_root = self.config_root.clone();
        let automations = self.automations.clone();
        let tools = self.tools.clone();
        let runtime_path = self.runtime_path.clone();
        let agent_tools_path = self.agent_tools_path.clone();
        let session_path = self.session_path.clone();
        let param_values = self.param_values.clone();

        let label_prefix = if reveal == RevealStrategy::Never {
            "Scheduled"
        } else {
            "Pipeline"
        };

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let groups = collect_step_groups(&steps);
            let mut pipeline_failed = false;
            let mut failure_message = String::new();

            'outer: for group in &groups {
                if pipeline_failed {
                    break;
                }
                if cancel_flag.load(Ordering::Relaxed) {
                    pipeline_failed = true;
                    failure_message = "Pipeline cancelled by user".to_string();
                    break;
                }

                let mut step_futures = Vec::new();
                let mut term_watchers: Vec<gpui::Task<()>> = Vec::new();

                for step in group {
                    let Some(target_id) = step.target_id() else {
                        continue;
                    };

                    if step.is_tool() {
                        // Tool step: resolve command and spawn
                        let tool = tools.iter().find(|t| t.id == target_id).cloned();
                        let Some(tool) = tool else {
                            log::warn!("Pipeline '{pipeline_id}': tool '{target_id}' not found — skipping step");
                            continue;
                        };
                        let tool_params = param_values.get(&tool.id).cloned().unwrap_or_default();
                        let runtime_path = runtime_path.clone();
                        let agent_tools_path = agent_tools_path.clone();
                        let config_root = config_root.clone();
                        let session_path = session_path.clone();
                        let active_folder = active_folder.clone();
                        let future = cx.background_executor().spawn(async move {
                            let (command, args, cwd, env) = resolve_tool_command(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &config_root,
                                &session_path,
                                &Some(active_folder),
                                &tool_params,
                            );
                            let mut cmd = smol::process::Command::new(&command);
                            cmd.args(&args).current_dir(&cwd);
                            for (key, value) in &env {
                                cmd.env(key, value);
                            }
                            match cmd.status().await {
                                Ok(status) => status.success(),
                                Err(e) => {
                                    log::error!("Pipeline tool '{}' failed to start: {e}", tool.id);
                                    false
                                }
                            }
                        });
                        step_futures.push(future);
                    } else {
                        // Automation step: check if target is itself a pipeline
                        let target_entry = automations.iter().find(|a| a.id == target_id).cloned();
                        let Some(target_entry) = target_entry else {
                            log::warn!("Pipeline '{pipeline_id}': automation '{target_id}' not found — skipping step");
                            continue;
                        };

                        if target_entry.is_pipeline() {
                            // Nested pipeline: spawn recursively via this.update
                            let target = target_entry.clone();
                            let active_folder = active_folder.clone();
                            let this = this.clone();
                            let new_depth = depth + 1;

                            // For nested pipelines, spawn a marker-based poll
                            let state_dir = state_dir_for(&config_root);
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            let nested_marker = completion_marker_path(&state_dir, target_id, timestamp);

                            // Fire the nested pipeline
                            this.update(cx, |dashboard, cx| {
                                dashboard.run_pipeline(
                                    &target,
                                    &active_folder,
                                    new_depth,
                                    reveal,
                                    cx,
                                );
                            }).log_err();

                            // Poll for the nested pipeline's completion marker
                            let marker = nested_marker;
                            let executor = cx.background_executor().clone();
                            let future = executor.spawn(async move {
                                // Nested pipelines write their own marker when done.
                                // However, since the nested pipeline manages its own marker
                                // lifecycle, we poll for the active_pipelines set instead.
                                // For now, we consider nested pipeline steps as fire-and-forget
                                // and always succeed (the nested pipeline tracks its own status).
                                // TODO: poll nested pipeline completion properly
                                let _ = marker;
                                true
                            });
                            step_futures.push(future);
                        } else {
                            // Regular automation: spawn in terminal and poll marker
                            let target_id = target_id.to_string();
                            let active_folder = active_folder.clone();
                            let label_prefix = label_prefix.to_string();
                            let this = this.clone();

                            let spawn_result = this.update(cx, |dashboard, cx| {
                                dashboard.spawn_automation_in_terminal(
                                    &target_id,
                                    &active_folder,
                                    reveal,
                                    &label_prefix,
                                    cx,
                                )
                            }).ok().flatten();

                            let Some((marker_path, terminal_task)) = spawn_result else {
                                log::warn!("Pipeline '{pipeline_id}': failed to spawn automation '{target_id}'");
                                continue;
                            };

                            let (term_tx, mut term_rx) = oneshot::channel::<()>();
                            let term_watcher = cx.background_executor().spawn(async move {
                                let _ = terminal_task.await;
                                term_tx.send(()).ok();
                            });
                            term_watchers.push(term_watcher);

                            let cancel = cancel_flag.clone();
                            let spawner = cx.background_executor().clone();
                            let timer_executor = spawner.clone();
                            let future = spawner.spawn({
                                let marker_path = marker_path.clone();
                                async move {
                                    let poll_interval = Duration::from_secs(10);
                                    let timeout = Duration::from_secs(3600);
                                    let start = std::time::Instant::now();
                                    loop {
                                        timer_executor.timer(poll_interval).await;

                                        if cancel.load(Ordering::Relaxed) {
                                            log::info!("Pipeline step '{target_id}' cancelled by user");
                                            return false;
                                        }

                                        if marker_path.exists() {
                                            let result = CompletionReport::from_marker(&marker_path);
                                            std::fs::remove_file(&marker_path).log_err();
                                            return match result {
                                                Some((_report, RunResult::Success)) => true,
                                                Some((report, _)) => {
                                                    log::warn!("Pipeline step '{target_id}' failed: {}", report.summary);
                                                    false
                                                }
                                                None => true,
                                            };
                                        }

                                        if matches!(term_rx.try_recv(), Ok(Some(()))) {
                                            log::warn!("Pipeline step '{target_id}': terminal exited without completion marker");
                                            return false;
                                        }

                                        if start.elapsed() > timeout {
                                            log::warn!("Pipeline step '{target_id}' timed out");
                                            return false;
                                        }
                                    }
                                }
                            });
                            step_futures.push(future);
                        }
                    }
                }

                // Wait for all steps in this group to complete
                let results = futures::future::join_all(step_futures).await;
                drop(term_watchers);
                for (i, success) in results.iter().enumerate() {
                    if !success {
                        pipeline_failed = true;
                        if let Some(step) = group.get(i) {
                            failure_message = format!(
                                "Step '{}' failed",
                                step.target_id().unwrap_or("unknown"),
                            );
                        }
                        break 'outer;
                    }
                }
            }

            // Write pipeline's own completion marker
            let state_dir = state_dir_for(&config_root);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let pipeline_marker = completion_marker_path(&state_dir, &pipeline_id, timestamp);
            if let Some(parent) = pipeline_marker.parent() {
                std::fs::create_dir_all(parent).log_err();
            }
            let status = if pipeline_failed { format!("failed: {failure_message}") } else { "success".to_string() };
            let report = serde_json::json!({
                "status": status,
                "summary": if pipeline_failed { &failure_message } else { "All steps completed" },
                "outputs": [],
                "skip_chain": false,
                "message": ""
            });
            std::fs::write(&pipeline_marker, report.to_string()).log_err();

            // Remove from active set and clean up cancel flag
            this.update(cx, |dashboard, cx| {
                dashboard.active_pipelines.remove(&pipeline_id);
                dashboard.pipeline_cancel_flags.remove(&pipeline_id);
                cx.notify();
            }).log_err();
        })
        .detach();
    }

    fn spawn_completion_poller(
        &self,
        automation_id: String,
        marker_path: PathBuf,
        timeout_secs: u64,
        chain_depth: u32,
        cx: &mut Context<Self>,
    ) {
        let scheduler = self.scheduler.downgrade();

        cx.spawn(async move |_this, cx: &mut AsyncApp| -> anyhow::Result<()> {
            // Poll for the completion marker file instead of awaiting the terminal process.
            // The agent writes this JSON file as its final action.
            let poll_interval = Duration::from_secs(10);
            let marker = marker_path.clone();
            let executor = cx.background_executor().clone();
            let completion = async {
                loop {
                    executor.timer(poll_interval).await;
                    if marker.exists() {
                        return CompletionReport::from_marker(&marker);
                    }
                }
            };
            let timeout = async {
                executor
                    .timer(Duration::from_secs(timeout_secs))
                    .await;
                None::<(CompletionReport, RunResult)>
            };

            let outcome = smol::future::or(completion, timeout).await;

            let (report, result) = match outcome {
                Some((report, result)) => {
                    // Clean up the marker file
                    std::fs::remove_file(&marker_path).log_err();
                    (Some(report), result)
                }
                None => (None, RunResult::Timeout),
            };

            // Report result back to scheduler
            scheduler.update(cx, |scheduler, cx| {
                scheduler.report_completion(
                    &automation_id,
                    &result,
                    report.as_ref(),
                    chain_depth,
                    cx,
                );
            }).log_err();

            Ok(())
        })
        .detach();
    }

    fn set_folder(&mut self, target: FolderTarget, path: PathBuf, cx: &mut Context<Self>) {
        match target {
            FolderTarget::Active => {
                write_active_folder(&self.config_root, &path);
                self.active_folder = Some(path);
                self.recent_folders = read_recent_folders(&self.config_root);
            }
            FolderTarget::Destination => {
                write_destination_folder(&self.config_root, &path);
                self.destination_folder = Some(path);
                self.recent_destinations = read_recent_destinations(&self.config_root);
            }
        }
        cx.notify();
    }

    fn resolve_dragged_directory(&self, selection: &DraggedSelection, cx: &App) -> Option<PathBuf> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        for entry in selection.items() {
            if let Some(project_path) = project.path_for_entry(entry.entry_id, cx) {
                if let Some(abs) = project.absolute_path(&project_path, cx) {
                    if abs.is_dir() {
                        return Some(abs);
                    }
                }
            }
        }
        None
    }

    fn pick_active_folder(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });

        let config_root = self.config_root.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    write_active_folder(&config_root, &path);
                    this.update(cx, |this, cx| {
                        this.active_folder = Some(path);
                        this.recent_folders = read_recent_folders(&this.config_root);
                        cx.notify();
                    }).log_err();
                }
            }
        })
        .detach();
    }

    fn pick_destination_folder(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });

        let config_root = self.config_root.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    write_destination_folder(&config_root, &path);
                    this.update(cx, |this, cx| {
                        this.destination_folder = Some(path);
                        this.recent_destinations = read_recent_destinations(&this.config_root);
                        cx.notify();
                    }).log_err();
                }
            }
        })
        .detach();
    }

    fn open_global_shortcut_modal(
        &self,
        tool_id: String,
        tool_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        let config_root = self.config_root.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, {
                let tool_id = tool_id.clone();
                let tool_label = tool_label.clone();
                move |window, cx| GlobalShortcutModal::new(tool_id, tool_label, config_root, window, cx)
            });
        }).log_err();
    }

    // -- Param editor helpers --

    fn ensure_param_editor(
        &mut self,
        entry_id: &str,
        param: &ParamEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        let key = (entry_id.to_string(), param.key.clone());
        if let Some(editor) = self.param_editors.get(&key) {
            return editor.clone();
        }

        let initial_value = self
            .param_values
            .get(entry_id)
            .and_then(|m| m.get(&param.key))
            .cloned()
            .unwrap_or_default();

        let editor = cx.new(|cx| {
            let mut ed = Editor::single_line(window, cx);
            if !param.placeholder.is_empty() {
                ed.set_placeholder_text(&param.placeholder, window, cx);
            }
            if !initial_value.is_empty() {
                ed.set_text(initial_value, window, cx);
            }
            ed
        });

        let entry_id_for_sub = entry_id.to_string();
        let param_key_for_sub = param.key.clone();
        let subscription = cx.subscribe(&editor, move |this: &mut Dashboard, editor, event, cx| {
            if let EditorEvent::BufferEdited = event {
                let text = editor.read(cx).text(cx);
                this.param_values
                    .entry(entry_id_for_sub.clone())
                    .or_default()
                    .insert(param_key_for_sub.clone(), text);
                this.schedule_param_write(cx);
            }
        });

        self._param_editor_subscriptions.push(subscription);
        self.param_editors.insert(key, editor.clone());
        editor
    }

    fn completion_report_instruction(marker_path: &Path) -> String {
        let marker_display = marker_path.display();
        format!(r#"

FINAL STEP — COMPLETION REPORT (mandatory):
When ALL tasks above are complete, create this JSON file using the Bash tool:

cat > {marker_display} << 'COMPLETION_EOF'
{{
  "status": "success",
  "summary": "1-2 sentence summary of what you did and found",
  "outputs": ["list/of/files/you/created/or/modified.md"],
  "skip_chain": false,
  "message": ""
}}
COMPLETION_EOF

Rules for the completion report:
- Set "status" to "success" if you completed the core task, or "failed: reason" if you could not.
- Set "skip_chain" to true ONLY if your work produced no meaningful output for downstream automations (e.g., no changes since last scan).
- "outputs" should list the files you created or modified during this run.
- "message" is for anything the user should see (warnings, suggestions, etc.). Leave empty if none.
- This is your FINAL action. Do not do anything after writing this file.
"#)
    }

    fn schedule_param_write(&mut self, cx: &mut Context<Self>) {
        self._param_write_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            this.update(cx, |this, _cx| {
                write_param_values(&this.config_root, &this.param_values);
            }).log_err();
        }));
    }

    // -- Rendering helpers --

    fn toggle_section(&mut self, section_id: &str, cx: &mut Context<Self>) {
        if self.collapsed_sections.contains(section_id) {
            self.collapsed_sections.remove(section_id);
        } else {
            self.collapsed_sections.insert(section_id.to_string());
        }
        write_collapsed_sections(&self.config_root, &self.collapsed_sections);
        cx.notify();
    }

    fn toggle_automation_expanded(&mut self, automation_id: &str, cx: &mut Context<Self>) {
        if self.expanded_automations.contains(automation_id) {
            self.expanded_automations.remove(automation_id);
        } else {
            self.expanded_automations.insert(automation_id.to_string());
        }
        cx.notify();
    }

    fn toggle_schedule(&mut self, automation_id: &str, cx: &mut Context<Self>) {
        let entry = match self.automations.iter_mut().find(|a| a.id == automation_id) {
            Some(e) => e,
            None => return,
        };

        let schedule = entry.schedule.get_or_insert_with(ScheduleConfig::default);
        schedule.enabled = !schedule.enabled;

        // If enabling with no cron expression, set a sensible default (daily at 3 AM)
        if schedule.enabled && schedule.cron.is_empty() {
            schedule.cron = "0 3 * * *".to_string();
        }

        self.write_schedule_field(automation_id, cx);
        cx.notify();
    }

    fn update_schedule_cron(
        &mut self,
        automation_id: &str,
        interval: &str,
        hour: u32,
        cx: &mut Context<Self>,
    ) {
        let entry = match self.automations.iter_mut().find(|a| a.id == automation_id) {
            Some(e) => e,
            None => return,
        };

        let cron = scheduler_ui::cron_from_interval_and_hour(interval, hour);

        let schedule = entry.schedule.get_or_insert_with(ScheduleConfig::default);
        schedule.cron = cron;

        self.write_schedule_field(automation_id, cx);
        cx.notify();
    }

    fn build_sync_entries(automations: &[AutomationEntry]) -> Vec<SyncEntry> {
        automations
            .iter()
            .filter_map(|a| {
                a.schedule.as_ref().map(|s| SyncEntry {
                    automation_id: a.id.clone(),
                    cron_expr: s.cron.clone(),
                    enabled: s.enabled,
                    catch_up: s.catch_up.clone(),
                    timeout_secs: s.timeout,
                    auto_disable_after: s.auto_disable_after,
                    chain: a.chain.as_ref().map(|c| {
                        postprod_scheduler::ChainConfig {
                            triggers: c.triggers.clone(),
                        }
                    }),
                })
            })
            .collect()
    }

    fn build_unscheduled_entries(
        automations: &[AutomationEntry],
        default_folder: &Path,
    ) -> Vec<ChainOnlyEntry> {
        automations
            .iter()
            .filter(|a| a.schedule.is_none())
            .map(|a| ChainOnlyEntry {
                automation_id: a.id.clone(),
                active_folder: default_folder.to_path_buf(),
                chain: a.chain.as_ref().map(|c| postprod_scheduler::ChainConfig {
                    triggers: c.triggers.clone(),
                }),
            })
            .collect()
    }

    fn write_schedule_field(&self, automation_id: &str, cx: &mut Context<Self>) {
        let entry = match self.automations.iter().find(|a| a.id == automation_id) {
            Some(e) => e,
            None => return,
        };

        let Some(source_path) = &entry.source_path else { return };
        let Some(schedule) = &entry.schedule else { return };

        // Capture values for the background task
        let source_path = source_path.clone();
        let enabled = schedule.enabled;
        let cron = schedule.cron.clone();

        cx.background_spawn(async move {
            let content = match std::fs::read_to_string(&source_path) {
                Ok(c) => c,
                Err(error) => {
                    log::warn!("Failed to read {}: {error}", source_path.display());
                    return;
                }
            };
            let mut doc = match content.parse::<toml_edit::DocumentMut>() {
                Ok(d) => d,
                Err(error) => {
                    log::warn!("Failed to parse {}: {error}", source_path.display());
                    return;
                }
            };

            let table = doc
                .entry("schedule")
                .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));

            if let Some(table) = table.as_table_mut() {
                table.insert("enabled", toml_edit::value(enabled));
                if !cron.is_empty() {
                    table.insert("cron", toml_edit::value(&cron));
                }
            }

            if let Err(error) = std::fs::write(&source_path, doc.to_string()) {
                log::warn!("Failed to write schedule to {}: {error}", source_path.display());
            }
        })
        .detach();

        // Sync updated schedule to scheduler (immediate — scheduler state is in-memory)
        let default_folder = self.active_folder.clone()
            .unwrap_or_else(|| self.config_root.clone());
        let sync_entries = Self::build_sync_entries(&self.automations);
        let unscheduled = Self::build_unscheduled_entries(&self.automations, &default_folder);
        self.scheduler.update(cx, |scheduler, _cx| {
            scheduler.sync_entries(sync_entries, unscheduled, default_folder);
        });
    }

    fn reload_automations(&mut self, cx: &mut Context<Self>) {
        let (automations, automations_error) = load_automations_registry(&self.config_root);
        self.automations = automations;
        self.automations_error = automations_error;
        self.default_contexts = config::load_default_contexts(&self.config_root);
        cx.notify();
    }

    fn write_pipeline_steps(&self, pipeline_id: &str, steps: &[PipelineStep], cx: &mut Context<Self>) {
        let entry = match self.automations.iter().find(|a| a.id == pipeline_id) {
            Some(e) => e,
            None => return,
        };
        let Some(source_path) = &entry.source_path else { return };
        let source_path = source_path.clone();
        let steps = steps.to_vec();

        cx.background_spawn(async move {
            let content = match std::fs::read_to_string(&source_path) {
                Ok(c) => c,
                Err(error) => {
                    log::warn!("Failed to read {}: {error}", source_path.display());
                    return;
                }
            };
            let mut doc = match content.parse::<toml_edit::DocumentMut>() {
                Ok(d) => d,
                Err(error) => {
                    log::warn!("Failed to parse {}: {error}", source_path.display());
                    return;
                }
            };

            doc.remove("step");
            let mut array = toml_edit::ArrayOfTables::new();
            for step in &steps {
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
            if !steps.is_empty() {
                doc.insert("step", toml_edit::Item::ArrayOfTables(array));
            }

            if let Err(error) = std::fs::write(&source_path, doc.to_string()) {
                log::warn!("Failed to write pipeline steps to {}: {error}", source_path.display());
            }
        })
        .detach();
    }

    fn create_pipeline_toml(&mut self, name: &str, cx: &mut Context<Self>) -> String {
        let mut id = format!(
            "pipeline-{}",
            name.to_lowercase()
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect::<String>()
        );
        let dir = automations_dir_for(&self.config_root).join("pipelines");
        std::fs::create_dir_all(&dir).log_err();
        let mut path = dir.join(format!("{id}.toml"));
        let base_id = id.clone();
        let mut counter = 2u32;
        while path.exists() {
            id = format!("{base_id}-{counter}");
            path = dir.join(format!("{id}.toml"));
            counter += 1;
        }
        let content = format!(
            "id = \"{id}\"\nlabel = \"{name}\"\ndescription = \"\"\nicon = \"zap\"\ntype = \"pipeline\"\n"
        );
        std::fs::write(&path, content).log_err();
        self.reload_automations(cx);
        id
    }

    fn start_new_pipeline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut ed = Editor::single_line(window, cx);
            ed.set_placeholder_text("Pipeline name", window, cx);
            ed
        });

        let subscription = cx.subscribe(&editor, |this: &mut Dashboard, editor, event, cx| {
            if let EditorEvent::Blurred = event {
                if this.pending_new_pipeline.is_some() {
                    let text = editor.read(cx).text(cx).trim().to_string();
                    this.finish_new_pipeline(text, cx);
                }
            }
        });

        self.pending_new_pipeline = Some(editor.clone());
        self._pending_pipeline_subscription = Some(subscription);

        editor.update(cx, |ed, cx| {
            ed.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    fn finish_new_pipeline(&mut self, name: String, cx: &mut Context<Self>) {
        self.pending_new_pipeline = None;
        self._pending_pipeline_subscription = None;

        if name.is_empty() {
            cx.notify();
            return;
        }

        let id = self.create_pipeline_toml(&name, cx);
        self.pipelines_in_edit_mode.insert(id.clone());
        self.expanded_automations.insert(id);
    }

    fn cancel_new_pipeline(&mut self, cx: &mut Context<Self>) {
        self.pending_new_pipeline = None;
        self._pending_pipeline_subscription = None;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Context entry CRUD (follows pipeline step pattern)
    // ------------------------------------------------------------------

    fn remove_context_entry(&mut self, automation_id: &str, index: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == automation_id) {
            if index < entry.contexts.len() {
                entry.contexts.remove(index);
                let contexts = entry.contexts.clone();
                self.write_context_entries(automation_id, &contexts, cx);
                cx.notify();
            }
        }
    }

    fn reorder_context_entry(&mut self, automation_id: &str, from: usize, direction: i32, cx: &mut Context<Self>) {
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == automation_id) {
            let to_signed = from as i32 + direction;
            if to_signed < 0 { return; }
            let to = to_signed as usize;
            if from < entry.contexts.len() && to < entry.contexts.len() {
                entry.contexts.swap(from, to);
                let contexts = entry.contexts.clone();
                self.write_context_entries(automation_id, &contexts, cx);
                cx.notify();
            }
        }
    }

    fn add_context_path_entry(&mut self, automation_id: &str, path: PathBuf, cx: &mut Context<Self>) {
        let label = path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let new_entry = config::ContextEntry {
            source_type: "path".to_string(),
            label,
            path: Some(path.to_string_lossy().to_string()),
            script: None,
            required: true,
        };
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == automation_id) {
            entry.contexts.push(new_entry);
            let contexts = entry.contexts.clone();
            self.write_context_entries(automation_id, &contexts, cx);
            cx.notify();
        }
    }

    #[allow(dead_code)] // Used by automation_picker's AddContextScript mode; will be used in context edit mode
    fn add_context_script_entry(&mut self, automation_id: &str, script_name: String, cx: &mut Context<Self>) {
        let label = std::path::Path::new(&script_name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| script_name.clone());
        let new_entry = config::ContextEntry {
            source_type: "script".to_string(),
            label,
            path: None,
            script: Some(script_name),
            required: false,
        };
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == automation_id) {
            entry.contexts.push(new_entry);
            let contexts = entry.contexts.clone();
            self.write_context_entries(automation_id, &contexts, cx);
            cx.notify();
        }
    }

    fn toggle_skip_default_context(&mut self, automation_id: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.automations.iter_mut().find(|a| a.id == automation_id) else { return };
        entry.skip_default_context = !entry.skip_default_context;
        let new_value = entry.skip_default_context;
        let Some(source_path) = entry.source_path.clone() else { return };

        cx.background_spawn(async move {
            let content = match std::fs::read_to_string(&source_path) {
                Ok(c) => c,
                Err(e) => { log::warn!("Failed to read {}: {e}", source_path.display()); return; }
            };
            let mut doc = match content.parse::<toml_edit::DocumentMut>() {
                Ok(d) => d,
                Err(e) => { log::warn!("Failed to parse {}: {e}", source_path.display()); return; }
            };
            if new_value {
                doc.insert("skip_default_context", toml_edit::value(true));
            } else {
                doc.remove("skip_default_context");
            }
            if let Err(e) = std::fs::write(&source_path, doc.to_string()) {
                log::warn!("Failed to write {}: {e}", source_path.display());
            }
        }).detach();
        cx.notify();
    }

    fn toggle_context_edit_mode(&mut self, automation_id: &str, cx: &mut Context<Self>) {
        if !self.automations_in_context_edit.remove(automation_id) {
            self.automations_in_context_edit.insert(automation_id.to_string());
        }
        cx.notify();
    }

    fn write_context_entries(&self, automation_id: &str, contexts: &[config::ContextEntry], cx: &mut Context<Self>) {
        let entry = match self.automations.iter().find(|a| a.id == automation_id) {
            Some(e) => e,
            None => return,
        };
        let Some(source_path) = &entry.source_path else { return };
        let source_path = source_path.clone();
        let contexts = contexts.to_vec();

        cx.background_spawn(async move {
            let content = match std::fs::read_to_string(&source_path) {
                Ok(c) => c,
                Err(e) => { log::warn!("Failed to read {}: {e}", source_path.display()); return; }
            };
            let mut doc = match content.parse::<toml_edit::DocumentMut>() {
                Ok(d) => d,
                Err(e) => { log::warn!("Failed to parse {}: {e}", source_path.display()); return; }
            };

            doc.remove("context");
            let mut array = toml_edit::ArrayOfTables::new();
            for ctx in &contexts {
                let mut table = toml_edit::Table::new();
                table.insert("source", toml_edit::value(&ctx.source_type));
                table.insert("label", toml_edit::value(&ctx.label));
                if let Some(path) = &ctx.path {
                    table.insert("path", toml_edit::value(path.as_str()));
                }
                if let Some(script) = &ctx.script {
                    table.insert("script", toml_edit::value(script.as_str()));
                }
                if !ctx.required {
                    table.insert("required", toml_edit::value(false));
                }
                array.push(table);
            }
            if !contexts.is_empty() {
                doc.insert("context", toml_edit::Item::ArrayOfTables(array));
            }

            if let Err(e) = std::fs::write(&source_path, doc.to_string()) {
                log::warn!("Failed to write context entries to {}: {e}", source_path.display());
            }
        }).detach();
    }

    // ------------------------------------------------------------------
    // "Add Automation" flow (follows pipeline ghost card pattern)
    // ------------------------------------------------------------------

    fn create_automation_toml(&mut self, name: &str, cx: &mut Context<Self>) -> String {
        let mut id = name.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>();
        let dir = automations_dir_for(&self.config_root);
        std::fs::create_dir_all(&dir).log_err();
        let mut path = dir.join(format!("{id}.toml"));
        let base_id = id.clone();
        let mut counter = 2u32;
        while path.exists() {
            id = format!("{base_id}-{counter}");
            path = dir.join(format!("{id}.toml"));
            counter += 1;
        }
        let content = format!(
            "id = \"{id}\"\nlabel = \"{name}\"\ndescription = \"\"\nicon = \"zap\"\nprompt_file = \"\"\n"
        );
        std::fs::write(&path, content).log_err();
        self.reload_automations(cx);
        id
    }

    fn start_new_automation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut ed = Editor::single_line(window, cx);
            ed.set_placeholder_text("Automation name", window, cx);
            ed
        });

        let subscription = cx.subscribe(&editor, |this: &mut Dashboard, editor, event, cx| {
            if let EditorEvent::Blurred = event {
                if this.pending_new_automation.is_some() {
                    let text = editor.read(cx).text(cx).trim().to_string();
                    this.finish_new_automation(text, cx);
                }
            }
        });

        self.pending_new_automation = Some(editor.clone());
        self._pending_automation_subscription = Some(subscription);

        editor.update(cx, |ed, cx| {
            ed.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    fn finish_new_automation(&mut self, name: String, cx: &mut Context<Self>) {
        self.pending_new_automation = None;
        self._pending_automation_subscription = None;

        if name.is_empty() {
            cx.notify();
            return;
        }

        let id = self.create_automation_toml(&name, cx);
        self.expanded_automations.insert(id);
    }

    fn cancel_new_automation(&mut self, cx: &mut Context<Self>) {
        self.pending_new_automation = None;
        self._pending_automation_subscription = None;
        cx.notify();
    }

    fn delete_pipeline_toml(&mut self, pipeline_id: &str, cx: &mut Context<Self>) {
        if let Some(path) = self.automations.iter()
            .find(|a| a.id == pipeline_id)
            .and_then(|a| a.source_path.clone())
        {
            std::fs::remove_file(&path).log_err();
            self.pipelines_in_edit_mode.remove(pipeline_id);
            self.reload_automations(cx);
        }
    }

    fn remove_pipeline_step(&mut self, pipeline_id: &str, step_index: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == pipeline_id) {
            if step_index < entry.steps.len() {
                entry.steps.remove(step_index);
                let steps = entry.steps.clone();
                self.write_pipeline_steps(pipeline_id, &steps, cx);
                cx.notify();
            }
        }
    }

    fn reorder_pipeline_step(&mut self, pipeline_id: &str, from: usize, direction: i32, cx: &mut Context<Self>) {
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == pipeline_id) {
            let to_signed = from as i32 + direction;
            if to_signed < 0 { return; }
            let to = to_signed as usize;
            if from < entry.steps.len() && to < entry.steps.len() {
                entry.steps.swap(from, to);
                let steps = entry.steps.clone();
                self.write_pipeline_steps(pipeline_id, &steps, cx);
                cx.notify();
            }
        }
    }

    fn render_session_status(&self, _cx: &App) -> AnyElement {
        match &self.session_name {
            Some(name) => {
                let callout = Callout::new()
                    .severity(Severity::Success)
                    .icon(IconName::Check)
                    .title(format!("Session: {}", name));
                if let Some(path) = &self.session_path {
                    callout.description(path.clone()).into_any_element()
                } else {
                    callout.into_any_element()
                }
            }
            None => div().into_any_element(),
        }
    }

    fn render_folder_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();

        let active_current = self.active_folder.clone();
        let active_recent = self.recent_folders.clone();
        let dest_current = self.destination_folder.clone();
        let dest_recent = self.recent_destinations.clone();
        let config_root = self.config_root.clone();

        let active_dropdown = Self::build_folder_dropdown(
            "active-folder",
            "Active Folder",
            FolderTarget::Active,
            &active_current,
            &active_recent,
            Color::Accent,
            {
                let entity = entity.clone();
                let config_root = config_root.clone();
                move |path, _window, cx: &mut App| {
                    write_active_folder(&config_root, &path);
                    entity.update(cx, |this, cx| {
                        this.active_folder = Some(path);
                        this.recent_folders = read_recent_folders(&this.config_root);
                        cx.notify();
                    });
                }
            },
            {
                let entity = entity.clone();
                move |_window, cx: &mut App| {
                    entity.update(cx, |this, cx| {
                        this.pick_active_folder(cx);
                    });
                }
            },
            window,
            cx,
        );

        let dest_dropdown = Self::build_folder_dropdown(
            "destination",
            "Destination",
            FolderTarget::Destination,
            &dest_current,
            &dest_recent,
            Color::Success,
            {
                let entity = entity.clone();
                move |path, _window, cx: &mut App| {
                    write_destination_folder(&config_root, &path);
                    entity.update(cx, |this, cx| {
                        this.destination_folder = Some(path);
                        this.recent_destinations = read_recent_destinations(&this.config_root);
                        cx.notify();
                    });
                }
            },
            {
                move |_window, cx: &mut App| {
                    entity.update(cx, |this, cx| {
                        this.pick_destination_folder(cx);
                    });
                }
            },
            window,
            cx,
        );

        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(div().flex_1().child(active_dropdown))
            .child(
                Label::new("\u{2192}")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(div().flex_1().child(dest_dropdown))
            .into_any_element()
    }

    fn build_folder_dropdown(
        id: &str,
        tag: &str,
        target: FolderTarget,
        current: &Option<PathBuf>,
        recent: &[PathBuf],
        icon_color: Color,
        on_select: impl Fn(PathBuf, &mut Window, &mut App) + 'static + Clone,
        on_browse: impl Fn(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
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
        let card_bg = cx.theme().colors().elevated_surface_background;

        let label_el = h_flex()
            .gap_2()
            .items_center()
            .child(
                Icon::new(IconName::Folder)
                    .color(icon_color)
                    .size(IconSize::Small),
            )
            .child(
                v_flex()
                    .items_start()
                    .child(
                        Label::new(SharedString::from(tag.to_string()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(display_name)
                            .color(name_color)
                            .size(LabelSize::Small),
                    ),
            );

        let trigger = ButtonLike::new(SharedString::from(format!("{}-trigger", id)))
            .child(label_el)
            .child(
                Icon::new(IconName::ChevronUpDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .style(ButtonStyle::Transparent)
            .full_width()
            .height(px(56.).into());

        let drop_external = cx.listener(move |this, paths: &ExternalPaths, _window, cx| {
            if let Some(dir) = paths.paths().iter().find(|p| p.is_dir()) {
                this.set_folder(target, dir.clone(), cx);
            }
        });
        let drop_selection = cx.listener(move |this, selection: &DraggedSelection, _window, cx| {
            if let Some(dir) = this.resolve_dragged_directory(selection, cx) {
                this.set_folder(target, dir, cx);
            }
        });

        div()
            .id(SharedString::from(format!("{}-drop", id)))
            .w_full()
            .rounded_lg()
            .border_1()
            .border_l_3()
            .border_color(border_hsla)
            .bg(card_bg)
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .drag_over::<DraggedSelection>(|style, _, _, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(drop_external)
            .on_drop(drop_selection)
            .child(
                PopoverMenu::new(SharedString::from(format!("{}-popover", id)))
                    .full_width(true)
                    .menu(move |_window, _cx| Some(menu.clone()))
                    .trigger(trigger)
                    .attach(Corner::BottomLeft),
            )
            .into_any_element()
    }

    fn render_delivery_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains("delivery");
        let status = &self.delivery_status;
        let has_any = status.tv_count > 0
            || status.net_count > 0
            || status.spot_count > 0
            || status.mp3_count > 0;

        v_flex()
            .w_full()
            .gap_1()
            .child(self.section_header("DELIVERY", "delivery", cx))
            .when(is_open && !has_any, |el| {
                el.child(
                    h_flex().px_2().child(
                        Label::new("No files in deliveries/")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
                )
            })
            .when(is_open && has_any, |el| {
                el.child(
                    v_flex()
                        .id("delivery-content-anim")
                        .w_full()
                        .gap_1()
                        .child(
                            h_flex()
                                .px_2()
                                .gap_4()
                                .child(Self::delivery_badge(
                                    "TV",
                                    status.tv_count,
                                    status.tv_count > 0,
                                ))
                                .child(Self::delivery_badge(
                                    "NET",
                                    status.net_count,
                                    status.net_count > 0,
                                ))
                                .child(Self::delivery_badge(
                                    "SPOT",
                                    status.spot_count,
                                    status.spot_count > 0,
                                ))
                                .child(Self::delivery_badge(
                                    "MP3",
                                    status.mp3_count,
                                    status.mp3_count > 0,
                                )),
                        )
                        .children(status.warnings.iter().map(|w| {
                            h_flex().px_2().child(
                                Label::new(format!("  {}", w))
                                    .color(Color::Warning)
                                    .size(LabelSize::XSmall),
                            )
                        })),
                )
            })
    }

    fn delivery_badge(label: &str, count: usize, ok: bool) -> impl IntoElement {
        let dot_color = if ok { Color::Created } else { Color::Muted };
        h_flex()
            .gap_1p5()
            .items_center()
            .child(Indicator::dot().color(dot_color))
            .child(
                Label::new(format!("{}: {}", label, count))
                    .size(LabelSize::Small)
                    .color(if ok { Color::Default } else { Color::Muted }),
            )
    }

    fn section_header(
        &self,
        title: &str,
        section_id: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains(section_id);
        let entity = cx.entity().downgrade();
        let id_for_toggle = section_id.to_string();

        h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .items_center()
            .child(
                Disclosure::new(SharedString::from(format!("disc-{}", section_id)), is_open)
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.toggle_section(&id_for_toggle, cx);
                        }).log_err();
                    }),
            )
            .child(
                Label::new(title.to_string())
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
    }

    fn sub_section_header(
        &self,
        title: &str,
        section_id: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains(section_id);
        let entity = cx.entity().downgrade();
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
                        entity.update(cx, |this, cx| {
                            this.toggle_section(&id_for_toggle, cx);
                        }).log_err();
                    }),
            )
            .child(
                Label::new(title.to_string())
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
    }

    /// Build a click handler closure for running a tool (background or terminal).
    /// Pre-clones all needed state so the closure is `'static`.
    fn tool_click_handler(
        &self,
        tool: &ToolEntry,
        _cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let is_background = self.background_tools.contains(&tool.id);
        let runtime_path = self.runtime_path.clone();
        let agent_tools_path = self.agent_tools_path.clone();
        let config_root = self.config_root.clone();
        let workspace = self.workspace.clone();
        let session_path = self.session_path.clone();
        let active_folder = self.active_folder.clone();
        let tool = tool.clone();
        let tool_param_values = self.param_values.get(&tool.id).cloned().unwrap_or_default();

        move |_, window, cx| {
            let runtime_path = runtime_path.clone();
            let agent_tools_path = agent_tools_path.clone();
            let config_root = config_root.clone();
            let active_folder = active_folder.clone();
            let session_path = session_path.clone();
            let tool_param_values = tool_param_values.clone();
            if is_background {
                workspace.update(cx, |_workspace, cx| {
                    Self::spawn_tool_background(
                        &tool,
                        &runtime_path,
                        &agent_tools_path,
                        &config_root,
                        &session_path,
                        &active_folder,
                        &tool_param_values,
                        cx,
                    );
                }).log_err();
            } else {
                workspace.update(cx, |workspace, cx| {
                    Self::spawn_tool_entry(
                        &tool,
                        &runtime_path,
                        &agent_tools_path,
                        &config_root,
                        &session_path,
                        &active_folder,
                        &tool_param_values,
                        workspace,
                        window,
                        cx,
                    );
                }).log_err();
            }
        }
    }

    /// Build hover-reveal action buttons (terminal toggle + global shortcut)
    /// for a tool card. Buttons are invisible by default and appear on hover
    /// via the `.visible_on_hover(group_name)` pattern.
    fn tool_action_buttons(
        &self,
        tool_id: &str,
        tool_label: &str,
        group_name: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let is_background = self.background_tools.contains(tool_id);

        let toggle_tool_id = tool_id.to_string();
        let toggle_entity = entity.clone();

        let globe_tool_id = tool_id.to_string();
        let globe_tool_label = tool_label.to_string();
        let globe_entity = entity;

        h_flex()
            .gap_1()
            .child(
                IconButton::new(
                    SharedString::from(format!("bg-toggle-{}", toggle_tool_id)),
                    IconName::ToolTerminal,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(if is_background {
                    Color::Muted
                } else {
                    Color::Accent
                })
                .tooltip(Tooltip::text(if is_background {
                    "Background mode (click to switch to terminal)"
                } else {
                    "Terminal mode (click to switch to background)"
                }))
                .on_click(move |_, _, cx| {
                    let tool_id = toggle_tool_id.clone();
                    toggle_entity.update(cx, |this, cx| {
                        if this.background_tools.contains(&tool_id) {
                            this.background_tools.remove(&tool_id);
                        } else {
                            this.background_tools.insert(tool_id);
                        }
                        write_background_tools(&this.config_root, &this.background_tools);
                        cx.notify();
                    }).log_err();
                })
                .visible_on_hover(group_name.clone()),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("globe-{}", globe_tool_id)),
                    IconName::Keyboard,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Create global shortcut"))
                .on_click(move |_, window, cx| {
                    let tool_id = globe_tool_id.clone();
                    let tool_label = globe_tool_label.clone();
                    globe_entity.update(cx, |this, cx| {
                        this.open_global_shortcut_modal(tool_id, tool_label, window, cx);
                    }).log_err();
                })
                .visible_on_hover(group_name),
            )
    }

    /// Build Featured tool cards: full-width, accent border + left strip,
    /// 40px tinted icon, hover-reveal actions.
    fn build_featured_cards(
        &mut self,
        tools: &[ToolEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let accent_color = cx.theme().colors().text_accent;
        let icon_tint_bg = cx.theme().status().info_background.opacity(0.15);

        let tools_owned: Vec<ToolEntry> = tools.to_vec();
        tools_owned
            .into_iter()
            .map(|tool| {
                let group_name = SharedString::from(format!("tool-{}", tool.id));
                let click_handler = self.tool_click_handler(&tool, cx);
                let tool_icon = icon_for_tool(&tool.icon);
                let tool_label: SharedString = tool.label.clone().into();
                let tool_description: SharedString = tool.description.clone().into();

                let has_params = !tool.params.is_empty();

                // Render params first (returns owned Vec, releases &mut borrows)
                let param_fields = if has_params {
                    self.render_entry_params(&tool.id, &tool.params, window, cx)
                } else {
                    Vec::new()
                };

                let featured_drop = cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                    if let Some(dir) = paths.paths().iter().find(|p| p.is_dir()) {
                        this.set_folder(FolderTarget::Destination, dir.clone(), cx);
                    }
                });

                // Action buttons last (impl IntoElement captures cx lifetime)
                let action_buttons =
                    self.tool_action_buttons(&tool.id, &tool.label, group_name.clone(), cx);

                let extra_content = div()
                    .when(has_params, |el| {
                        el.child(
                            h_flex()
                                .px_2()
                                .pb_2()
                                .pl(px(50.))
                                .gap_2()
                                .flex_wrap()
                                .children(param_fields),
                        )
                    });

                Self::render_card_shell(
                    format!("featured-{}", tool.id),
                    accent_color,
                    tool_icon,
                    Color::Accent,
                    icon_tint_bg,
                    v_flex()
                        .flex_1()
                        .child(Label::new(tool_label))
                        .child(
                            Label::new(tool_description)
                                .color(Color::Muted)
                                .size(LabelSize::XSmall),
                        ),
                    h_flex().child(action_buttons),
                    extra_content,
                    cx,
                )
                .group(group_name)
                .drag_over::<ExternalPaths>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(featured_drop)
                .on_click(click_handler)
                .into_any_element()
            })
            .collect()
    }

    /// Build Standard tool cards: neutral border, 28px icon, params inline below.
    fn build_standard_cards(
        &mut self,
        tools: &[ToolEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let hover_border = cx.theme().colors().text_accent;
        let card_bg = cx.theme().colors().elevated_surface_background;
        let border_color = cx.theme().colors().border_variant;
        let icon_bg = cx.theme().colors().element_background;

        let tools_owned: Vec<ToolEntry> = tools.to_vec();
        tools_owned
            .into_iter()
            .map(|tool| {
                let group_name = SharedString::from(format!("tool-{}", tool.id));
                let click_handler = self.tool_click_handler(&tool, cx);
                let tool_icon = icon_for_tool(&tool.icon);
                let tool_label: SharedString = tool.label.clone().into();
                let tool_description: SharedString = tool.description.clone().into();
                let has_params = !tool.params.is_empty();

                // Render params first (returns owned Vec, releases &mut borrows)
                let param_fields = if has_params {
                    self.render_entry_params(&tool.id, &tool.params, window, cx)
                } else {
                    Vec::new()
                };

                let path_drop_handler = tool
                    .params
                    .iter()
                    .find(|p| p.param_type == ParamType::Path)
                    .map(|p| {
                        let entry_id = tool.id.clone();
                        let param_key = p.key.clone();
                        cx.listener(
                            move |this: &mut Dashboard, paths: &ExternalPaths, _window, cx| {
                                if let Some(path) = paths.paths().first() {
                                    this.param_values
                                        .entry(entry_id.clone())
                                        .or_default()
                                        .insert(
                                            param_key.clone(),
                                            path.to_string_lossy().to_string(),
                                        );
                                    write_param_values(&this.config_root, &this.param_values);
                                    cx.notify();
                                }
                            },
                        )
                    });

                // Action buttons last (impl IntoElement captures cx lifetime)
                let action_buttons =
                    self.tool_action_buttons(&tool.id, &tool.label, group_name.clone(), cx);

                div()
                    .id(SharedString::from(format!("standard-{}", tool.id)))
                    .group(group_name)
                    .flex_basis(relative(0.48))
                    .flex_grow()
                    .rounded_md()
                    .border_1()
                    .border_color(border_color)
                    .bg(card_bg)
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(move |style| style.border_color(hover_border))
                    .when(path_drop_handler.is_some(), |el| {
                        el.drag_over::<ExternalPaths>(|style, _, _, cx| {
                            style.bg(cx.theme().colors().drop_target_background)
                        })
                    })
                    .when_some(path_drop_handler, |el, handler| el.on_drop(handler))
                    .child(
                        h_flex()
                            .w_full()
                            .p_2()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .size(px(28.))
                                    .rounded(px(6.))
                                    .bg(icon_bg)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(tool_icon)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(Label::new(tool_label).size(LabelSize::Small))
                                    .child(
                                        Label::new(tool_description)
                                            .color(Color::Muted)
                                            .size(LabelSize::XSmall),
                                    ),
                            )
                            .child(action_buttons),
                    )
                    .when(has_params, |el| {
                        el.child(
                            h_flex()
                                .px_2()
                                .pb_2()
                                .pl(px(44.))
                                .gap_2()
                                .flex_wrap()
                                .children(param_fields),
                        )
                    })
                    .on_click(click_handler)
                    .into_any_element()
            })
            .collect()
    }

    /// Build Compact tool cards: minimal icon + label, 3-column grid items.
    fn build_compact_cards(&self, tools: &[ToolEntry], cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let hover_border = cx.theme().colors().text_accent;
        let border_color = cx.theme().colors().border_variant;

        let tools_owned: Vec<ToolEntry> = tools.to_vec();
        tools_owned
            .into_iter()
            .map(|tool| {
                let group_name = SharedString::from(format!("tool-{}", tool.id));
                let click_handler = self.tool_click_handler(&tool, cx);
                let action_buttons =
                    self.tool_action_buttons(&tool.id, &tool.label, group_name.clone(), cx);
                let tool_icon = icon_for_tool(&tool.icon);
                let tool_label: SharedString = tool.label.clone().into();
                let tool_description = tool.description.clone();

                div()
                    .id(SharedString::from(format!("compact-{}", tool.id)))
                    .group(group_name)
                    .flex_basis(relative(0.31))
                    .flex_grow()
                    .rounded_sm()
                    .border_1()
                    .border_color(border_color)
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(move |style| style.border_color(hover_border))
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::new(tool_icon)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(tool_label).size(LabelSize::XSmall))
                            .child(div().flex_grow())
                            .child(action_buttons),
                    )
                    .tooltip(Tooltip::text(tool_description))
                    .on_click(click_handler)
                    .into_any_element()
            })
            .collect()
    }

    fn render_tools_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains("tools");
        let entity = cx.entity().downgrade();
        let id_for_toggle = "tools".to_string();

        let edit_btn = ButtonLike::new("edit-tools-btn")
            .size(ButtonSize::Compact)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::FileToml)
                            .color(Color::Muted)
                            .size(IconSize::XSmall),
                    )
                    .child(
                        Label::new("Edit TOML")
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    ),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                let path = tools_config_dir_for(&this.config_root);
                let workspace = this.workspace.clone();
                cx.spawn_in(window, async move |_this, cx| {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace
                                .open_abs_path(path, OpenOptions::default(), window, cx)
                                .detach();
                        })
                        .log_err();
                })
                .detach();
            }));

        let header = h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .items_center()
            .child(
                Disclosure::new(SharedString::from("disc-tools"), is_open).on_click(
                    move |_, _, cx| {
                        entity
                            .update(cx, |this, cx| {
                                this.toggle_section(&id_for_toggle, cx);
                            })
                            .log_err();
                    },
                ),
            )
            .child(
                Label::new("TOOLS")
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
            .child(edit_btn);

        if !is_open {
            return v_flex().w_full().gap_1().child(header);
        }

        let all_tools: Vec<ToolEntry> = self
            .tools
            .iter()
            .filter(|t| !t.hidden)
            .cloned()
            .collect();

        let grouped = group_by_section(
            &all_tools,
            |t| t.section.as_deref(),
            |t| t.order,
            |t| &t.label,
            &self.section_order,
        );

        let tools_error = self.tools_error.clone();
        let is_empty = all_tools.is_empty();

        let mut section_elements: Vec<gpui::AnyElement> = Vec::new();

        for (section_name, section_tools) in &grouped {
            let collapse_key = format!("section-tools-{}", section_name);
            let sub_header = self
                .sub_section_header(section_name, &collapse_key, cx)
                .into_any_element();
            section_elements.push(sub_header);

            if !self.collapsed_sections.contains(&collapse_key) {
                let featured: Vec<ToolEntry> = section_tools
                    .iter()
                    .filter(|t| t.tier == ToolTier::Featured)
                    .cloned()
                    .collect();
                let standard: Vec<ToolEntry> = section_tools
                    .iter()
                    .filter(|t| t.tier == ToolTier::Standard)
                    .cloned()
                    .collect();
                let compact: Vec<ToolEntry> = section_tools
                    .iter()
                    .filter(|t| t.tier == ToolTier::Compact)
                    .cloned()
                    .collect();

                if !featured.is_empty() {
                    let cards = self.build_featured_cards(&featured, window, cx);
                    section_elements.push(
                        v_flex().w_full().gap_2().children(cards).into_any_element(),
                    );
                }
                if !standard.is_empty() {
                    let cards = self.build_standard_cards(&standard, window, cx);
                    section_elements.push(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap(px(8.))
                            .children(cards)
                            .into_any_element(),
                    );
                }
                if !compact.is_empty() {
                    let cards = self.build_compact_cards(&compact, cx);
                    section_elements.push(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap(px(8.))
                            .children(cards)
                            .into_any_element(),
                    );
                }
            }
        }

        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .when_some(tools_error, |el, err| {
                el.child(
                    Label::new(format!("Parse error: {}", err))
                        .color(Color::Error)
                        .size(LabelSize::XSmall),
                )
            })
            .when(is_empty, |el| {
                el.child(
                    h_flex().px_2().child(
                        Label::new("No tools found (config/tools/)")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
                )
            })
            .when(!is_empty, |el| {
                el.child(
                    v_flex()
                        .id("tools-content-anim")
                        .w_full()
                        .gap_1()
                        .children(section_elements),
                )
            })
    }

    /// Render param fields (Text/Path/Select) for a tool or automation entry.
    /// Returns a vec of elements, one per param.
    fn render_entry_params(
        &mut self,
        entry_id: &str,
        params: &[ParamEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let entity = cx.entity().downgrade();
        params
            .iter()
            .map(|param| {
                let label_el = Label::new(format!("{}:", param.label))
                    .color(Color::Muted)
                    .size(LabelSize::XSmall);

                match param.param_type {
                    ParamType::Text => {
                        let editor = self.ensure_param_editor(entry_id, param, window, cx);
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(label_el)
                            .child(
                                div()
                                    .w(px(120.))
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .rounded_sm()
                                    .px_1()
                                    .py(px(-2.))
                                    .child(editor),
                            )
                            .into_any_element()
                    }
                    ParamType::Path => {
                        let current_value = self
                            .param_values
                            .get(entry_id)
                            .and_then(|m| m.get(&param.key))
                            .cloned()
                            .unwrap_or_default();
                        let display_text: SharedString = if current_value.is_empty() {
                            param.placeholder.clone().into()
                        } else {
                            std::path::Path::new(&current_value)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| current_value.clone())
                                .into()
                        };
                        let full_path_tooltip = if current_value.is_empty() {
                            param.placeholder.clone()
                        } else {
                            current_value
                        };
                        let path_entity = entity.clone();
                        let path_entry_id = entry_id.to_string();
                        let path_param_key = param.key.clone();
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(label_el)
                            .child(
                                Label::new(display_text)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "param-path-{}-{}",
                                        entry_id, param.key
                                    )),
                                    IconName::Folder,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text(full_path_tooltip))
                                .on_click(move |_, _window, cx| {
                                    let entity = path_entity.clone();
                                    let entry_id = path_entry_id.clone();
                                    let param_key = path_param_key.clone();
                                    entity.update(cx, |_this, cx| {
                                        let receiver = cx.prompt_for_paths(PathPromptOptions {
                                            files: false,
                                            directories: true,
                                            multiple: false,
                                            prompt: None,
                                        });
                                        let entity = cx.entity().downgrade();
                                        cx.spawn(async move |_this, cx| {
                                            if let Ok(Ok(Some(paths))) = receiver.await {
                                                if let Some(path) = paths.into_iter().next() {
                                                    let path_str =
                                                        path.to_string_lossy().to_string();
                                                    entity.update(
                                                        cx,
                                                        |this: &mut Dashboard, cx| {
                                                            this.param_values
                                                                .entry(entry_id.clone())
                                                                .or_default()
                                                                .insert(
                                                                    param_key.clone(),
                                                                    path_str,
                                                                );
                                                            write_param_values(&this.config_root, &this.param_values);
                                                            cx.notify();
                                                        },
                                                    ).log_err();
                                                }
                                            }
                                        })
                                        .detach();
                                    }).log_err();
                                }),
                            )
                            .into_any_element()
                    }
                    ParamType::Select => {
                        let current_value = self
                            .param_values
                            .get(entry_id)
                            .and_then(|m| m.get(&param.key))
                            .cloned()
                            .unwrap_or_else(|| param.default.clone());
                        let display_label: SharedString = if current_value.is_empty() {
                            "Select...".into()
                        } else {
                            current_value.into()
                        };
                        let select_entity = entity.clone();
                        let select_entry_id = entry_id.to_string();
                        let select_param_key = param.key.clone();
                        let menu = ContextMenu::build(window, cx, {
                            let entry_id = select_entry_id;
                            let param_key = select_param_key;
                            let entity = select_entity;
                            let options = param.options.clone();
                            move |mut menu: ContextMenu,
                                  _window: &mut Window,
                                  _cx: &mut Context<ContextMenu>| {
                                for option in &options {
                                    let value = option.clone();
                                    let entity = entity.clone();
                                    let entry_id = entry_id.clone();
                                    let param_key = param_key.clone();
                                    menu = menu.entry(
                                        option.clone(),
                                        None,
                                        move |_window, cx: &mut App| {
                                            entity.update(cx, |this: &mut Dashboard, cx| {
                                                this.param_values
                                                    .entry(entry_id.clone())
                                                    .or_default()
                                                    .insert(param_key.clone(), value.clone());
                                                write_param_values(&this.config_root, &this.param_values);
                                                cx.notify();
                                            }).log_err();
                                        },
                                    );
                                }
                                menu
                            }
                        });
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(label_el)
                            .child(
                                DropdownMenu::new(
                                    SharedString::from(format!(
                                        "param-select-{}-{}",
                                        entry_id, param.key
                                    )),
                                    display_label,
                                    menu,
                                )
                                .trigger_size(ButtonSize::None)
                                .style(DropdownStyle::Outlined),
                            )
                            .into_any_element()
                    }
                }
            })
            .collect()
    }

    /// Shared card layout used by automation, pipeline, and tool cards.
    ///
    /// Builds the outer container (background, border, hover, rounded corners),
    /// the left accent strip, icon container, and places `content` and
    /// `right_side` in the header row.  Any additional content (params,
    /// schedule, expansion) is appended from `extra_content`.
    ///
    /// Background and hover colors are looked up from the theme — callers
    /// don't need to pass them.  Chain `.on_click()` / `.group()` on the
    /// returned `Div`.
    fn render_card_shell(
        element_id: impl Into<SharedString>,
        accent: gpui::Hsla,
        icon: IconName,
        icon_color: Color,
        icon_tint_bg: gpui::Hsla,
        content: impl IntoElement,
        right_side: Div,
        extra_content: Div,
        cx: &App,
    ) -> gpui::Stateful<Div> {
        let hover_bg = cx.theme().colors().ghost_element_hover;

        div()
            .id(element_id.into())
            .w_full()
            .elevation_2(cx)
            .border_color(accent.opacity(0.5))
            .overflow_hidden()
            .cursor_pointer()
            .hover(move |style| style.bg(hover_bg))
            .child(
                h_flex()
                    .w_full()
                    .child(div().w(px(3.)).h_full().flex_shrink_0().bg(accent))
                    .child(
                        v_flex()
                            .flex_1()
                            .child(
                                h_flex()
                                    .flex_1()
                                    .p_2()
                                    .gap_3()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .size(px(36.))
                                            .rounded(px(8.))
                                            .bg(icon_tint_bg)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                Icon::new(icon)
                                                    .size(IconSize::Medium)
                                                    .color(icon_color),
                                            ),
                                    )
                                    .child(content)
                                    .child(right_side),
                            )
                            .child(extra_content),
                    ),
            )
    }

    fn render_automation_card(
        &mut self,
        entry: &AutomationEntry,
        idx: usize,
        icon_color: Color,
        badge_label: &SharedString,
        badge_color: Color,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let icon = icon_for_automation(&entry.icon);
        let entry_id = entry.id.clone();
        let entry_label: SharedString = entry.label.clone().into();
        let entry_description: SharedString = entry.description.clone().into();
        let entry_prompt = entry.prompt.clone();
        let badge_label = badge_label.clone();
        let has_params = !entry.params.is_empty();
        let is_expanded = self.expanded_automations.contains(&entry.id);

        let (accent, accent_bg) = self.agent_backend.card_accent(cx);
        let icon_tint_bg = accent_bg.opacity(0.15);
        let editor_bg = cx.theme().colors().editor_background;

        let is_scheduled = entry.schedule.as_ref().is_some_and(|s| s.enabled);
        let schedule_cron = entry.schedule.as_ref()
            .map(|s| s.cron.clone())
            .unwrap_or_default();

        let entity = cx.entity().downgrade();

        let click_entity = entity.clone();
        let click_id = entry_id.clone();

        let run_entity = entity.clone();
        let run_id = entry_id.clone();
        let run_label = entry_label.clone();
        let run_prompt = entry_prompt.clone();

        let edit_entity = entity.clone();
        let edit_id = entry_id.clone();

        let sched_entity = entity.clone();
        let sched_id = entry_id.clone();

        let disc_entity = entity;
        let disc_id = entry_id.clone();

        let param_fields = self.render_entry_params(&entry.id, &entry.params, window, cx);
        let context_rows = if is_expanded {
            if self.automations_in_context_edit.contains(&entry.id) {
                self.render_context_editor(&entry.id, &entry.contexts, entry.skip_default_context, cx)
            } else {
                self.render_context_summary(&entry.id, &entry.contexts, entry.skip_default_context, cx)
            }
        } else {
            Vec::new()
        };
        let group_name = SharedString::from(format!("automation-{}", entry_id));

        // Right-side: badge + action buttons with mouse-down propagation prevention
        let right_side = h_flex()
            .gap_2()
            .items_center()
            .child(
                Label::new(badge_label)
                    .color(badge_color)
                    .size(LabelSize::XSmall),
            )
            .child(
                h_flex()
                    .gap_1()
                    .on_mouse_down(
                        MouseButton::Left,
                        |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        },
                    )
                    .child(
                        IconButton::new(
                            format!("run-automation-{}", run_id),
                            IconName::PlayFilled,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Run automation"))
                        .on_click(
                            move |_, window, cx| {
                                run_entity
                                    .update(cx, |this, cx| {
                                        this.run_automation(&run_id, &run_label, &run_prompt, window, cx);
                                    })
                                    .log_err();
                            },
                        ),
                    )
                    .child(
                        IconButton::new(
                            format!("sched-toggle-{}", sched_id),
                            IconName::CountdownTimer,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(if is_scheduled { Color::Accent } else { Color::Muted })
                        .tooltip(Tooltip::text(if is_scheduled { "Disable schedule" } else { "Enable schedule" }))
                        .on_click(
                            move |_, _window, cx| {
                                sched_entity
                                    .update(cx, |this, cx| {
                                        this.toggle_schedule(&sched_id, cx);
                                    })
                                    .log_err();
                            },
                        ),
                    )
                    .child(
                        Disclosure::new(
                            SharedString::from(format!("disc-auto-{}", disc_id)),
                            is_expanded,
                        )
                        .on_click(
                            move |_, _, cx| {
                                disc_entity
                                    .update(cx, |this, cx| {
                                        this.toggle_automation_expanded(&disc_id, cx);
                                    })
                                    .log_err();
                            },
                        ),
                    )
                    .child(
                        IconButton::new(
                            format!("edit-automation-{}", edit_id),
                            IconName::FileToml,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Edit"))
                        .on_click(
                            move |_, window, cx| {
                                edit_entity
                                    .update(cx, |this, cx| {
                                        let path = this.automations.iter()
                                            .find(|a| a.id == edit_id)
                                            .and_then(|a| a.source_path.clone())
                                            .unwrap_or_else(|| automations_dir_for(&this.config_root)
                                                .join(format!("{}.toml", edit_id)));
                                        let workspace = this.workspace.clone();
                                        cx.spawn_in(
                                            window,
                                            async move |_this, cx| {
                                                workspace
                                                    .update_in(
                                                        cx,
                                                        |workspace, window, cx| {
                                                            workspace
                                                                .open_abs_path(
                                                                    path,
                                                                    OpenOptions::default(),
                                                                    window,
                                                                    cx,
                                                                )
                                                                .detach();
                                                        },
                                                    )
                                                    .log_err();
                                            },
                                        )
                                        .detach();
                                    })
                                    .log_err();
                            },
                        ),
                    ),
            );

        // Below-header: params, schedule controls, expansion content
        let sched_controls = self.render_schedule_controls(
            &entry_id, &schedule_cron, window, cx,
        );
        let extra_content = div()
            .when(has_params, |el| {
                el.child(
                    h_flex()
                        .w_full()
                        .pl(px(52.))
                        .pr_2()
                        .pb_1()
                        .gap_2()
                        .flex_wrap()
                        .children(param_fields),
                )
            })
            .when(is_scheduled, move |el| el.child(sched_controls))
            .when(is_expanded, |el| {
                el.child(
                    v_flex()
                        .w_full()
                        .px_3()
                        .pb_2()
                        .gap_1()
                        .children(context_rows)
                        .child(
                            div().w_full().p_2().rounded_md().bg(editor_bg).child(
                                Label::new(entry_prompt)
                                    .color(Color::Muted)
                                    .size(LabelSize::XSmall),
                            ),
                        ),
                )
            });

        Self::render_card_shell(
            format!("automation-card-{}-{}", entry_id, idx),
            accent,
            icon,
            icon_color,
            icon_tint_bg,
            v_flex()
                .flex_1()
                .child(Label::new(entry_label))
                .child(
                    Label::new(entry_description)
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                ),
            right_side,
            extra_content,
            cx,
        )
        .group(group_name)
        .on_click(move |_, _window, cx| {
            click_entity.update(cx, |this, cx| {
                this.toggle_automation_expanded(&click_id, cx);
            }).log_err();
        })
    }

    fn render_context_summary(
        &self,
        automation_id: &str,
        contexts: &[config::ContextEntry],
        skip_default: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut elements = Vec::new();

        if contexts.is_empty() && !skip_default && self.default_contexts.is_empty() {
            return elements;
        }

        let entity = cx.entity().downgrade();
        let gear_id = automation_id.to_string();

        let mut row = h_flex()
            .gap_1()
            .pl_4()
            .items_center()
            .flex_wrap();

        if !skip_default && !self.default_contexts.is_empty() {
            row = row.child(
                div()
                    .px_1p5()
                    .py_0p5()
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
                    .px_1p5()
                    .py_0p5()
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

    fn render_context_editor(
        &self,
        automation_id: &str,
        contexts: &[config::ContextEntry],
        skip_default: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut elements = Vec::new();
        let entity = cx.entity().downgrade();
        let context_count = contexts.len();

        // "Use default context" toggle
        let toggle_entity = entity.clone();
        let toggle_id = automation_id.to_string();
        let default_label: SharedString = if skip_default { "Default context: off".into() } else { "Default context: on".into() };
        elements.push(
            h_flex()
                .gap_1()
                .pl_4()
                .items_center()
                .child(
                    ButtonLike::new(format!("toggle-default-ctx-{}", toggle_id))
                        .style(ButtonStyle::Subtle)
                        .child(
                            h_flex()
                                .gap_1()
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
        for (i, ctx) in contexts.iter().enumerate() {
            let badge = if ctx.source_type == "script" { "script" } else { "path" };
            let label_text = format!("{}  [{}]", ctx.label, badge);

            let up_entity = entity.clone();
            let down_entity = entity.clone();
            let remove_entity = entity.clone();
            let auto_id = automation_id.to_string();
            let auto_id2 = automation_id.to_string();
            let auto_id3 = automation_id.to_string();

            let row = h_flex()
                .gap_1()
                .pl_4()
                .items_center()
                .child(
                    Label::new(label_text)
                        .size(LabelSize::XSmall)
                        .color(if ctx.required { Color::Default } else { Color::Muted }),
                )
                .child(div().flex_1())
                .child(
                    IconButton::new(format!("ctx-up-{}-{}", automation_id, i), IconName::ArrowUp)
                        .size(ui::ButtonSize::Compact)
                        .disabled(i == 0)
                        .on_click(move |_, _, cx| {
                            up_entity.update(cx, |this, cx| {
                                this.reorder_context_entry(&auto_id, i, -1, cx);
                            }).log_err();
                        }),
                )
                .child(
                    IconButton::new(format!("ctx-down-{}-{}", automation_id, i), IconName::ArrowDown)
                        .size(ui::ButtonSize::Compact)
                        .disabled(i >= context_count - 1)
                        .on_click(move |_, _, cx| {
                            down_entity.update(cx, |this, cx| {
                                this.reorder_context_entry(&auto_id2, i, 1, cx);
                            }).log_err();
                        }),
                )
                .child(
                    IconButton::new(format!("ctx-remove-{}-{}", automation_id, i), IconName::Close)
                        .size(ui::ButtonSize::Compact)
                        .on_click(move |_, _, cx| {
                            remove_entity.update(cx, |this, cx| {
                                this.remove_context_entry(&auto_id3, i, cx);
                            }).log_err();
                        }),
                );

            elements.push(row.into_any_element());
        }

        // "Add Context" buttons
        let add_path_entity = entity;
        let add_path_id = automation_id.to_string();
        let add_script_id = automation_id.to_string();
        let automation_source_path = self.automations.iter()
            .find(|a| a.id == automation_id)
            .and_then(|a| a.source_path.clone());
        let workspace_for_script = self.workspace.clone();
        let scripts = config::collect_context_scripts(&self.config_root);
        let config_root_for_picker = self.config_root.clone();

        elements.push(
            h_flex()
                .pl_4()
                .pt_1()
                .gap_2()
                .child(
                    ButtonLike::new(format!("add-ctx-path-{}", add_path_id))
                        .style(ButtonStyle::Subtle)
                        .child(
                            h_flex()
                                .gap_1()
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
                    let ws = workspace_for_script;
                    move |el| {
                        el.child(
                            ButtonLike::new(format!("add-ctx-script-{}", auto_id))
                                .style(ButtonStyle::Subtle)
                                .child(
                                    h_flex()
                                        .gap_1()
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

        // Gear icon to collapse back to summary
        let done_entity = cx.entity().downgrade();
        let done_id = automation_id.to_string();
        elements.push(
            h_flex()
                .pl_4()
                .pt_0p5()
                .child(
                    ButtonLike::new(format!("ctx-done-gear-{}", done_id))
                        .style(ButtonStyle::Subtle)
                        .child(
                            h_flex()
                                .gap_1()
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

    fn render_pipeline_step_tree(
        &self,
        steps: &[PipelineStep],
        _cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let groups = collect_step_groups(steps);
        let mut elements = Vec::new();
        let mut display_num: u32 = 0;

        for group in &groups {
            display_num += 1;
            let is_parallel = group.len() > 1;

            for (sub_idx, step) in group.iter().enumerate() {
                let target_id = step.target_id().unwrap_or("unknown");

                // Resolve display name from automations/tools registry
                let (display_name, is_tool_step) = if step.is_tool() {
                    let name = self.tools.iter()
                        .find(|t| t.id == target_id)
                        .map(|t| t.label.clone())
                        .unwrap_or_else(|| format!("missing: {target_id}"));
                    (name, true)
                } else {
                    let name = self.automations.iter()
                        .find(|a| a.id == target_id)
                        .map(|a| a.label.clone())
                        .unwrap_or_else(|| format!("missing: {target_id}"));
                    (name, false)
                };

                let is_broken = if step.is_tool() {
                    !self.tools.iter().any(|t| t.id == target_id)
                } else {
                    !self.automations.iter().any(|a| a.id == target_id)
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

                let text_color = if is_broken { Color::Error } else { Color::Muted };

                let mut row = h_flex()
                    .gap_2()
                    .pl_4()
                    .child(
                        Label::new(label_text)
                            .size(LabelSize::XSmall)
                            .color(text_color),
                    );

                if is_parallel && sub_idx == 0 {
                    row = row.child(
                        Label::new("┐")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
                } else if is_parallel && sub_idx == group.len() - 1 {
                    row = row.child(
                        Label::new("┘")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
                } else if is_parallel {
                    row = row.child(
                        Label::new("│")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
                }

                elements.push(row.into_any_element());
            }
        }
        elements
    }

    fn render_pipeline_edit_steps(
        &self,
        entry: &AutomationEntry,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut elements = Vec::new();
        let entity = cx.entity().downgrade();
        let step_count = entry.steps.len();

        for (i, step) in entry.steps.iter().enumerate() {
            let target_id = step.target_id().unwrap_or("unknown");
            let display_name = if step.is_tool() {
                self.tools.iter()
                    .find(|t| t.id == target_id)
                    .map(|t| t.label.clone())
                    .unwrap_or_else(|| format!("missing: {target_id}"))
            } else {
                self.automations.iter()
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
                .gap_1()
                .pl_4()
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
                            up_entity.update(cx, |this, cx| {
                                this.reorder_pipeline_step(&pipeline_id, i, -1, cx);
                            }).log_err();
                        }),
                )
                .child(
                    IconButton::new(format!("step-down-{}-{}", entry.id, i), IconName::ArrowDown)
                        .size(ui::ButtonSize::Compact)
                        .disabled(i >= step_count - 1)
                        .on_click(move |_, _, cx| {
                            down_entity.update(cx, |this, cx| {
                                this.reorder_pipeline_step(&pipeline_id2, i, 1, cx);
                            }).log_err();
                        }),
                )
                .child(
                    IconButton::new(format!("step-remove-{}-{}", entry.id, i), IconName::Close)
                        .size(ui::ButtonSize::Compact)
                        .on_click(move |_, _, cx| {
                            remove_entity.update(cx, |this, cx| {
                                this.remove_pipeline_step(&pipeline_id3, i, cx);
                            }).log_err();
                        }),
                );

            elements.push(row.into_any_element());
        }

        // [+ Add Step] button — capture everything outside the closure to avoid
        // re-entering Dashboard while it's being updated (build_picker_entries
        // calls dashboard.read() which would deadlock).
        let source_path = entry.source_path.clone();
        let add_pipeline_id = entry.id.clone();
        let workspace_handle = self.workspace.clone();
        let picker_tools = self.tools.clone();
        let picker_automations = self.automations.clone();
        let picker_config_root = self.config_root.clone();
        elements.push(
            h_flex()
                .pl_4()
                .pt_1()
                .child(
                    ButtonLike::new(format!("add-step-{}", add_pipeline_id))
                        .style(ButtonStyle::Subtle)
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::Plus).size(IconSize::XSmall).color(Color::Muted))
                                .child(Label::new("Add Step").size(LabelSize::XSmall).color(Color::Muted)),
                        )
                        .on_click(move |_, window, cx| {
                            let Some(source_path) = source_path.clone() else { return };
                            let Some(workspace) = workspace_handle.upgrade() else { return };

                            // Build picker entries from pre-captured data (not from dashboard.read())
                            let mut entries: Vec<crate::automation_picker::PickerEntry> = Vec::new();
                            for tool in &picker_tools {
                                if tool.hidden { continue; }
                                entries.push(crate::automation_picker::PickerEntry::new_tool(
                                    tool.clone(),
                                    Some(picker_config_root.clone()),
                                ));
                            }
                            for auto in &picker_automations {
                                if auto.hidden { continue; }
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

    fn render_pipeline_card(
        &mut self,
        entry: &AutomationEntry,
        idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let entry_id = entry.id.clone();
        let entry_label: SharedString = entry.label.clone().into();
        let entry_description: SharedString = entry.description.clone().into();
        let icon = icon_for_automation(&entry.icon);
        let is_running = self.active_pipelines.contains(&entry.id);
        let has_steps = !entry.steps.is_empty();
        let is_expanded = self.expanded_automations.contains(&entry.id);

        let accent = cx.theme().colors().text_accent;
        let accent_bg = accent.opacity(0.15);

        let entity = cx.entity().downgrade();

        let is_editing = self.pipelines_in_edit_mode.contains(&entry.id);
        let is_pending_delete = self.pipelines_pending_delete.contains(&entry.id);

        // Run/Stop button
        let run_entity = entity.clone();
        let stop_entity = entity.clone();
        let run_id = entry_id.clone();
        let stop_id = entry_id.clone();
        let run_entry = entry.clone();

        // Edit toggle
        let edit_entity = entity.clone();
        let edit_id = entry_id.clone();

        // Delete
        let delete_entity = entity.clone();
        let delete_id = entry_id.clone();

        // Expand/collapse
        let disc_entity = entity;
        let disc_id = entry_id.clone();

        // Step tree (built before the element tree to avoid borrow issues)
        let step_tree = if is_editing {
            self.render_pipeline_edit_steps(entry, cx)
        } else {
            self.render_pipeline_step_tree(&entry.steps, cx)
        };

        let active_folder = self.active_folder.clone()
            .unwrap_or_else(|| self.config_root.clone());

        // Right-side: status badge + action buttons
        let right_side = h_flex()
            .gap_2()
            .items_center()
            .child(
                Label::new(if is_running {
                    SharedString::from("running")
                } else {
                    SharedString::from(format!("{} steps", entry.steps.len()))
                })
                .color(if is_running { Color::Accent } else { Color::Muted })
                .size(LabelSize::XSmall),
            )
            .child(
                h_flex()
                    .gap_1()
                    .on_mouse_down(
                        MouseButton::Left,
                        |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        },
                    )
                    .child(
                        Disclosure::new(
                            SharedString::from(format!("disc-pipeline-{}", disc_id)),
                            is_expanded,
                        )
                        .on_click(move |_, _, cx| {
                            disc_entity.update(cx, |this, cx| {
                                if this.expanded_automations.contains(&disc_id) {
                                    this.expanded_automations.remove(&disc_id);
                                } else {
                                    this.expanded_automations.insert(disc_id.clone());
                                }
                                cx.notify();
                            }).log_err();
                        }),
                    )
                    .child(if is_running {
                        IconButton::new(
                            format!("stop-pipeline-{}", stop_id),
                            IconName::Stop,
                        )
                        .tooltip(Tooltip::text("Stop pipeline"))
                        .on_click(move |_, _, cx| {
                            stop_entity.update(cx, |this, cx| {
                                if let Some(flag) = this.pipeline_cancel_flags.get(&stop_id) {
                                    flag.store(true, Ordering::Relaxed);
                                }
                                cx.notify();
                            }).log_err();
                        })
                    } else {
                        IconButton::new(
                            format!("run-pipeline-{}", run_id),
                            IconName::PlayFilled,
                        )
                        .disabled(!has_steps)
                        .tooltip(Tooltip::text(if !has_steps {
                            "No steps to run"
                        } else {
                            "Run pipeline"
                        }))
                        .on_click(move |_, _, cx| {
                            run_entity.update(cx, |this, cx| {
                                this.run_pipeline(
                                    &run_entry,
                                    &active_folder,
                                    0,
                                    RevealStrategy::Always,
                                    cx,
                                );
                            }).log_err();
                        })
                    })
                    .child(
                        IconButton::new(
                            format!("edit-pipeline-{}", edit_id),
                            if is_editing { IconName::Check } else { IconName::Settings },
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
                            edit_entity.update(cx, |this, cx| {
                                if this.pipelines_in_edit_mode.contains(&edit_id) {
                                    this.pipelines_in_edit_mode.remove(&edit_id);
                                    this.pipelines_pending_delete.remove(&edit_id);
                                } else {
                                    this.pipelines_in_edit_mode.insert(edit_id.clone());
                                    this.expanded_automations.insert(edit_id.clone());
                                }
                                cx.notify();
                            }).log_err();
                        }),
                    )
                    .when(is_editing, |el| {
                        el.child(
                            IconButton::new(
                                format!("delete-pipeline-{}", delete_id),
                                IconName::Trash,
                            )
                            .icon_color(if is_pending_delete { Color::Error } else { Color::Default })
                            .tooltip(Tooltip::text(if is_pending_delete {
                                "Click again to confirm delete"
                            } else {
                                "Delete pipeline"
                            }))
                            .on_click(move |_, _, cx| {
                                delete_entity.update(cx, |this, cx| {
                                    if this.pipelines_pending_delete.contains(&delete_id) {
                                        this.pipelines_pending_delete.remove(&delete_id);
                                        this.delete_pipeline_toml(&delete_id, cx);
                                    } else {
                                        this.pipelines_pending_delete.insert(delete_id.clone());
                                        cx.notify();
                                    }
                                }).log_err();
                            }),
                        )
                    }),
            );

        // Below-header: step tree when expanded
        let extra_content = div()
            .when(is_expanded && !step_tree.is_empty(), |el| {
                el.child(
                    v_flex()
                        .px_2()
                        .pb_2()
                        .gap_px()
                        .children(step_tree),
                )
            });

        Self::render_card_shell(
            format!("pipeline-card-{}-{}", entry_id, idx),
            accent,
            icon,
            Color::Accent,
            accent_bg,
            v_flex()
                .flex_1()
                .child(Label::new(entry_label))
                .child(
                    Label::new(entry_description)
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                ),
            right_side,
            extra_content,
            cx,
        )
        .into_any_element()
    }

    fn render_pipelines_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pipelines: Vec<AutomationEntry> = self
            .automations
            .iter()
            .filter(|a| a.is_pipeline() && !a.hidden)
            .cloned()
            .collect();

        // Hide the section entirely if no pipelines exist and no automations dir
        if pipelines.is_empty() && !automations_dir_for(&self.config_root).join("pipelines").exists() {
            return v_flex().w_full();
        }

        let is_open = !self.collapsed_sections.contains("pipelines");

        let disc_entity = cx.entity().downgrade();
        let disclosure = Disclosure::new(SharedString::from("disc-pipelines"), is_open).on_click(
            move |_, _, cx| {
                disc_entity.update(cx, |this, cx| {
                    this.toggle_section("pipelines", cx);
                }).log_err();
            },
        );

        let header = h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .items_center()
            .child(disclosure)
            .child(
                Label::new("PIPELINES")
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant));

        if !is_open {
            return v_flex().w_full().gap_1().child(header);
        }

        let cards: Vec<gpui::AnyElement> = pipelines
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                self.render_pipeline_card(entry, idx, window, cx)
            })
            .collect();

        // Ghost card for pending new pipeline
        let ghost_card = self.pending_new_pipeline.clone().map(|editor| {
            let accent = cx.theme().colors().text_accent;
            let accent_bg = accent.opacity(0.15);
            let border_color = cx.theme().colors().border;
            let confirm_entity = cx.entity().downgrade();
            let cancel_entity = cx.entity().downgrade();

            Self::render_card_shell(
                "new-pipeline-ghost",
                accent,
                IconName::PlayFilled,
                Color::Accent,
                accent_bg,
                div()
                    .flex_1()
                    .border_1()
                    .border_color(border_color)
                    .rounded_sm()
                    .px_1()
                    .child(editor),
                div(),
                div(),
                cx,
            )
            .on_action(move |_: &menu::Confirm, _, cx| {
                confirm_entity.update(cx, |this, cx| {
                    if let Some(editor) = &this.pending_new_pipeline {
                        let text = editor.read(cx).text(cx).trim().to_string();
                        this.finish_new_pipeline(text, cx);
                    }
                }).log_err();
            })
            .on_action(move |_: &menu::Cancel, _, cx| {
                cancel_entity.update(cx, |this, cx| {
                    this.cancel_new_pipeline(cx);
                }).log_err();
            })
        });

        // [+ New Pipeline] button
        let has_pending = self.pending_new_pipeline.is_some();
        let new_pipeline_button = ButtonLike::new("new-pipeline-btn")
            .style(ButtonStyle::Subtle)
            .disabled(has_pending)
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::Plus).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new("New Pipeline").size(LabelSize::XSmall).color(Color::Muted)),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.start_new_pipeline(window, cx);
            }));

        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .child(v_flex().w_full().gap_2().children(cards))
            .when_some(ghost_card, |el, card| el.child(card))
            .child(new_pipeline_button)
    }

    fn render_automations_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains("automations");
        let backend = self.agent_backend;
        let automations_error = self.automations_error.clone();
        let entity = cx.entity().downgrade();

        // Build the disclosure chevron
        let disc_entity = cx.entity().downgrade();
        let disclosure = Disclosure::new(SharedString::from("disc-automations"), is_open).on_click(
            move |_, _, cx| {
                disc_entity.update(cx, |this, cx| {
                    this.toggle_section("automations", cx);
                }).log_err();
            },
        );

        // Build the custom header with disclosure + label + divider + backend toggle
        let header = h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .items_center()
            .child(disclosure)
            .child(
                Label::new("AUTOMATIONS")
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
            .child({
                let entity_claude = entity.clone();
                let entity_gemini = entity.clone();
                let entity_native = entity.clone();
                let entity_copy = entity;

                ToggleButtonGroup::single_row(
                    "agent-backend-toggle",
                    [
                        ToggleButtonSimple::new("Claude", move |_, _, cx| {
                            entity_claude.update(cx, |this, cx| {
                                this.agent_backend = AgentBackend::Claude;
                                cx.notify();
                            }).log_err();
                        }),
                        ToggleButtonSimple::new("Gemini", move |_, _, cx| {
                            entity_gemini.update(cx, |this, cx| {
                                this.agent_backend = AgentBackend::Gemini;
                                cx.notify();
                            }).log_err();
                        }),
                        ToggleButtonSimple::new("Copy", move |_, _, cx| {
                            entity_copy.update(cx, |this, cx| {
                                this.agent_backend = AgentBackend::CopyOnly;
                                cx.notify();
                            }).log_err();
                        }),
                        ToggleButtonSimple::new("Native", move |_, _, cx| {
                            entity_native.update(cx, |this, cx| {
                                this.agent_backend = AgentBackend::Native;
                                cx.notify();
                            }).log_err();
                        }),
                    ],
                )
                .selected_index(backend.index())
                .style(ToggleButtonGroupStyle::Outlined)
                .auto_width()
            });

        if !is_open {
            return v_flex().w_full().gap_1().child(header);
        }

        let all: Vec<AutomationEntry> = self
            .automations
            .iter()
            .filter(|a| !a.hidden && !a.is_pipeline())
            .cloned()
            .collect();
        let meta: Vec<_> = all
            .iter()
            .filter(|a| a.id.starts_with('_'))
            .cloned()
            .collect();
        let regular: Vec<_> = all
            .iter()
            .filter(|a| !a.id.starts_with('_'))
            .cloned()
            .collect();
        let badge_label = backend.badge_label(&self.backends);
        let badge_color = backend.badge_color();
        let has_both = !meta.is_empty() && !regular.is_empty();
        let is_empty = all.is_empty();

        let meta_cards: Vec<_> = meta
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                self.render_automation_card(
                    entry,
                    idx,
                    Color::Muted,
                    &badge_label,
                    badge_color,
                    window,
                    cx,
                )
                .into_any_element()
            })
            .collect();

        let grouped = group_by_section(
            &regular,
            |a| a.section.as_deref(),
            |a| a.order,
            |a| &a.label,
            &self.section_order,
        );

        let mut regular_elements: Vec<gpui::AnyElement> = Vec::new();
        let mut card_idx = meta.len();
        for (section_name, section_automations) in &grouped {
            let collapse_key = format!("section-auto-{}", section_name);
            regular_elements.push(
                self.sub_section_header(section_name, &collapse_key, cx)
                    .into_any_element(),
            );

            if !self.collapsed_sections.contains(&collapse_key) {
                for entry in section_automations {
                    regular_elements.push(
                        self.render_automation_card(
                            entry,
                            card_idx,
                            Color::Accent,
                            &badge_label,
                            badge_color,
                            window,
                            cx,
                        )
                        .into_any_element(),
                    );
                    card_idx += 1;
                }
            } else {
                card_idx += section_automations.len();
            }
        }

        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .when_some(automations_error, |el, err| {
                el.child(
                    Label::new(format!("Parse error: {}", err))
                        .color(Color::Error)
                        .size(LabelSize::XSmall),
                )
            })
            .when(is_empty, |el| {
                el.child(
                    h_flex().px_2().child(
                        Label::new("No automations found (config/automations/)")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
                )
            })
            .when(!is_empty, |el| {
                el.child(
                    v_flex()
                        .id("automations-content-anim")
                        .w_full()
                        .gap_1()
                        .children(meta_cards)
                        .when(has_both, |el| {
                            el.child(
                                div().py_1().child(
                                    Divider::horizontal().color(DividerColor::BorderVariant),
                                ),
                            )
                        })
                        .children(regular_elements),
                )
            })
            .when_some(self.render_new_automation_ghost(cx), |el, card| el.child(card))
            .child(self.render_new_automation_button(cx))
    }

    fn render_new_automation_ghost(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let editor = self.pending_new_automation.clone()?;
        let accent = cx.theme().colors().text_accent;
        let accent_bg = accent.opacity(0.15);
        let border_color = cx.theme().colors().border;
        let confirm_entity = cx.entity().downgrade();
        let cancel_entity = cx.entity().downgrade();

        Some(
            Self::render_card_shell(
                "new-automation-ghost",
                accent,
                IconName::Sparkle,
                Color::Accent,
                accent_bg,
                div()
                    .flex_1()
                    .border_1()
                    .border_color(border_color)
                    .rounded_sm()
                    .px_1()
                    .child(editor),
                div(),
                div(),
                cx,
            )
            .on_action(move |_: &menu::Confirm, _, cx| {
                confirm_entity.update(cx, |this, cx| {
                    if let Some(editor) = &this.pending_new_automation {
                        let text = editor.read(cx).text(cx).trim().to_string();
                        this.finish_new_automation(text, cx);
                    }
                }).log_err();
            })
            .on_action(move |_: &menu::Cancel, _, cx| {
                cancel_entity.update(cx, |this, cx| {
                    this.cancel_new_automation(cx);
                }).log_err();
            })
            .into_any_element(),
        )
    }

    fn render_new_automation_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_pending = self.pending_new_automation.is_some();
        ButtonLike::new("new-automation-btn")
            .style(ButtonStyle::Subtle)
            .disabled(has_pending)
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::Plus).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new("New Automation").size(LabelSize::XSmall).color(Color::Muted)),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.start_new_automation(window, cx);
            }))
    }

    fn render_ai_agents_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains("ai-agents");

        if !is_open {
            return v_flex().w_full().gap_1().child(self.section_header(
                "AI AGENTS",
                "ai-agents",
                cx,
            ));
        }

        let workspace = self.workspace.clone();
        let cwd = self.agent_cwd();

        // Resolve actual binary paths so we can run them directly without a
        // shell wrapper (avoids `.zshrc` errors from `-i` flag).
        let agents: Vec<_> = self
            .agent_launchers
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
                                .gap_2()
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
                            workspace.update(cx, |workspace, cx| {
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
                            }).log_err();
                        })
                        .into_any_element()
                }
            })
            .collect();

        v_flex()
            .w_full()
            .gap_1()
            .child(self.section_header("AI AGENTS", "ai-agents", cx))
            .child(
                v_flex()
                    .id("ai-agents-content-anim")
                    .w_full()
                    .gap_1()
                    .children(agent_buttons),
            )
    }
}

// ---------------------------------------------------------------------------
// Render — Three-tier layout
// ---------------------------------------------------------------------------

impl Render for Dashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Capture window handle on first render (needed by scheduler for spawn_in_terminal)
        if self.window_handle.is_none() {
            self.window_handle = Some(window.window_handle());
        }

        h_flex()
            .key_context("Dashboard")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, action: &RunDashboardTool, window, cx| {
                if let Some(tool) = this.tools.iter().find(|t| t.id == action.tool_id) {
                    let tool = tool.clone();
                    let is_background = this.background_tools.contains(&action.tool_id);
                    let runtime_path = this.runtime_path.clone();
                    let agent_tools_path = this.agent_tools_path.clone();
                    let config_root = this.config_root.clone();
                    let session_path = this.session_path.clone();
                    let active_folder = this.active_folder.clone();
                    let tool_param_values = this
                        .param_values
                        .get(&action.tool_id)
                        .cloned()
                        .unwrap_or_default();
                    if is_background {
                        this.workspace.update(cx, |_workspace, cx| {
                            Self::spawn_tool_background(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &config_root,
                                &session_path,
                                &active_folder,
                                &tool_param_values,
                                cx,
                            );
                        }).log_err();
                    } else {
                        this.workspace.update(cx, |workspace, cx| {
                            Self::spawn_tool_entry(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &config_root,
                                &session_path,
                                &active_folder,
                                &tool_param_values,
                                workspace,
                                window,
                                cx,
                            );
                        }).log_err();
                    }
                }
            }))
            .on_action(cx.listener(|this, action: &RunDashboardAutomation, window, cx| {
                if let Some(entry) = this.automations.iter().find(|a| a.id == action.automation_id) {
                    let id = entry.id.clone();
                    let label = entry.label.clone();
                    let prompt = entry.prompt.clone();
                    this.run_automation(&id, &label, &prompt, window, cx);
                }
            }))
            .size_full()
            .justify_center()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .relative()
                    .size_full()
                    .px_6()
                    .max_w(px(1100.))
                    .child(
                        v_flex()
                            .id("dashboard-scroll")
                            .size_full()
                            .min_w_0()
                            .pt_8()
                            .pb_8()
                            .max_w_full()
                            .gap_6()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            // Header
                            .child(
                                h_flex()
                                    .w_full()
                                    .mb_4()
                                    .gap_3()
                                    .child(
                                        Icon::new(IconName::AudioOn)
                                            .size(IconSize::Medium)
                                            .color(Color::Accent),
                                    )
                                    .child(
                                        Headline::new("PostProd Tools").size(HeadlineSize::Small),
                                    ),
                            )
                            // Session status bar
                            .child(self.render_session_status(cx))
                            // Folder selectors
                            .child(self.render_folder_row(window, cx))
                            // Three-tier tool layout
                            .child(self.render_tools_section(window, cx))
                            // AI Agents
                            .child(self.render_ai_agents_section(cx))
                            // Scheduled automations (only shown when at least one is scheduled)
                            .child(self.render_scheduled_section(cx))
                            // Pipelines
                            .child(self.render_pipelines_section(window, cx))
                            // Automations
                            .child(self.render_automations_section(window, cx))
                            // Delivery status
                            .child(self.render_delivery_status(cx)),
                    )
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
    }
}

// ---------------------------------------------------------------------------
// Trait impls for center-pane Item
// ---------------------------------------------------------------------------

impl EventEmitter<ItemEvent> for Dashboard {}

impl Focusable for Dashboard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for Dashboard {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Dashboard".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn prevent_close(&self, _cx: &App) -> bool {
        true
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

// ---------------------------------------------------------------------------
// Context gathering (runs on background thread)
// ---------------------------------------------------------------------------

/// Gather all context entries, returning assembled text or an error if a required entry fails.
fn gather_context_blocking(
    contexts: &[config::ContextEntry],
    config_root: &Path,
    session_path: &str,
    active_folder: &str,
    destination_folder: &str,
    param_values: &HashMap<String, String>,
) -> Result<String, String> {
    use std::process::Command;

    let mut output = String::new();
    let mut total_size: usize = 0;
    let context_cap = 150 * 1024;

    for entry in contexts {
        if total_size >= context_cap {
            output.push_str("\n[... context truncated at 150KB]\n");
            break;
        }

        let content = match entry.source_type.as_str() {
            "path" => {
                let Some(path_str) = &entry.path else {
                    if entry.required {
                        return Err(format!("context '{}': source=path but no path field", entry.label));
                    }
                    continue;
                };
                let expanded = config::expand_tilde(path_str);
                let path = Path::new(&expanded);
                match config::read_path_context(path) {
                    Ok(text) => text,
                    Err(e) => {
                        if entry.required {
                            return Err(format!("context '{}': {e}", entry.label));
                        }
                        log::warn!("context '{}': {e} (skipping, not required)", entry.label);
                        continue;
                    }
                }
            }
            "script" => {
                let Some(script_name) = &entry.script else {
                    if entry.required {
                        return Err(format!("context '{}': source=script but no script field", entry.label));
                    }
                    continue;
                };
                let scripts_dir = config_root.join("config/context-scripts");
                let script_path = match config::resolve_file_path(&scripts_dir, script_name) {
                    Ok(p) => p,
                    Err(e) => {
                        if entry.required {
                            return Err(format!("context '{}': {e}", entry.label));
                        }
                        log::warn!("context '{}': {e} (skipping, not required)", entry.label);
                        continue;
                    }
                };

                // This runs on a background thread via gather_context_blocking(),
                // so blocking .output() is intentional and correct here.
                #[allow(clippy::disallowed_methods)]
                let result = Command::new("sh")
                    .arg("-c")
                    .arg(script_path.to_string_lossy().as_ref())
                    .env("POSTPROD_ACTIVE_FOLDER", active_folder)
                    .env("POSTPROD_SESSION_PATH", session_path)
                    .env("POSTPROD_DESTINATION_FOLDER", destination_folder)
                    .envs(param_values.iter().map(|(k, v)| {
                        (format!("POSTPROD_PARAM_{}", k.to_uppercase()), v.as_str())
                    }))
                    .output();

                match result {
                    Ok(out) if out.status.success() => {
                        String::from_utf8_lossy(&out.stdout).to_string()
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let msg = format!("script exited {}: {}", out.status, stderr.lines().next().unwrap_or(""));
                        if entry.required {
                            return Err(format!("context '{}': {msg}", entry.label));
                        }
                        log::warn!("context '{}': {msg} (skipping, not required)", entry.label);
                        continue;
                    }
                    Err(e) => {
                        if entry.required {
                            return Err(format!("context '{}': failed to run script: {e}", entry.label));
                        }
                        log::warn!("context '{}': {e} (skipping, not required)", entry.label);
                        continue;
                    }
                }
            }
            other => {
                log::warn!("context '{}': unknown source type '{other}', skipping", entry.label);
                continue;
            }
        };

        let section = format!("=== Context: {} ===\n{}\n\n", entry.label, content);
        total_size += section.len();
        output.push_str(&section);
    }

    Ok(output)
}

/// Build a SpawnInTerminal that reads the prompt from a temp file via stdin piping.
/// The prompt (with any gathered context prepended) is written to /tmp/postprod_prompt_{id}.md,
/// then executed as: `cat <temp_file> | <command> <flags>`.
/// Returns None if the backend config is missing or the temp file cannot be written.
fn build_temp_file_terminal_command(
    prompt: &str,
    entry_id: &str,
    entry_label: &str,
    agent_backend: AgentBackend,
    backends: &[config::BackendEntry],
    agent_cwd: &PathBuf,
    chain_marker: &Option<PathBuf>,
) -> Option<SpawnInTerminal> {
    // Append completion instruction if chained
    let final_prompt = if let Some(marker_path) = chain_marker {
        let mut p = prompt.to_string();
        p.push_str(&Dashboard::completion_report_instruction(marker_path));
        p
    } else {
        prompt.to_string()
    };

    // Write to temp file
    let temp_path = std::env::temp_dir().join(format!("postprod_prompt_{entry_id}.md"));
    if let Err(e) = std::fs::write(&temp_path, &final_prompt) {
        log::error!("failed to write temp prompt file: {e}");
        return None;
    }

    let backend_id = agent_backend.backend_id();
    let config = backends.iter().find(|b| b.id == backend_id)?;

    let command = resolve_bin(&config.command);
    let flags = &config.flags;
    let temp_escaped = temp_path.display().to_string().replace('\'', "'\\''");
    let full_command = format!("cat '{temp_escaped}' | {command} {flags}");

    Some(SpawnInTerminal {
        id: TaskId(format!("automation-{}", entry_id)),
        label: entry_label.to_string(),
        full_label: entry_label.to_string(),
        command: Some(full_command),
        args: vec![],
        command_label: entry_label.to_string(),
        cwd: Some(agent_cwd.clone()),
        use_new_terminal: true,
        allow_concurrent_runs: false,
        reveal: RevealStrategy::Always,
        show_command: true,
        show_rerun: true,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schedule_config_deserialize_default() {
        let toml_str = r#"
id = "test"
label = "Test"
description = "Test automation"
icon = "sparkle"
prompt = "Do something"
"#;
        let entry: config::AutomationEntry = toml::from_str(toml_str).unwrap();
        assert!(entry.schedule.is_none());
        assert!(entry.chain.is_none());
    }

    #[test]
    fn test_schedule_config_deserialize_with_schedule() {
        let toml_str = r#"
id = "daily-scan"
label = "Daily Scan"
description = "Scans daily"
icon = "sparkle"
prompt = "Scan {active_folder}"

[schedule]
enabled = true
cron = "0 3 * * *"
catch_up = "run_once"
timeout = 7200
"#;
        let entry: config::AutomationEntry = toml::from_str(toml_str).unwrap();
        let schedule = entry.schedule.unwrap();
        assert!(schedule.enabled);
        assert_eq!(schedule.cron, "0 3 * * *");
        assert_eq!(schedule.timeout, 7200);
    }

    #[test]
    fn test_schedule_config_deserialize_with_chain() {
        let toml_str = r#"
id = "build"
label = "Build"
description = "Build project"
icon = "sparkle"
prompt = "Build it"

[chain]
triggers = ["review", "deploy"]
"#;
        let entry: config::AutomationEntry = toml::from_str(toml_str).unwrap();
        let chain = entry.chain.unwrap();
        assert_eq!(chain.triggers, vec!["review", "deploy"]);
    }

    #[test]
    fn test_collect_step_groups_sequential() {
        let steps = vec![
            config::PipelineStep { automation: Some("a".into()), tool: None, group: None },
            config::PipelineStep { automation: Some("b".into()), tool: None, group: None },
            config::PipelineStep { automation: Some("c".into()), tool: None, group: None },
        ];
        let groups = collect_step_groups(&steps);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[2].len(), 1);
    }

    #[test]
    fn test_collect_step_groups_parallel() {
        let steps = vec![
            config::PipelineStep { automation: Some("a".into()), tool: None, group: Some(1) },
            config::PipelineStep { automation: Some("b".into()), tool: None, group: Some(1) },
            config::PipelineStep { automation: Some("c".into()), tool: None, group: Some(1) },
        ];
        let groups = collect_step_groups(&steps);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn test_collect_step_groups_mixed() {
        let steps = vec![
            config::PipelineStep { automation: Some("first".into()), tool: None, group: None },
            config::PipelineStep { automation: Some("second".into()), tool: None, group: None },
            config::PipelineStep { automation: Some("par-a".into()), tool: None, group: Some(3) },
            config::PipelineStep { automation: Some("par-b".into()), tool: None, group: Some(3) },
            config::PipelineStep { tool: Some("launcher".into()), automation: None, group: None },
        ];
        let groups = collect_step_groups(&steps);
        assert_eq!(groups.len(), 4); // first, second, group-3 (2 steps), launcher
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[0][0].automation.as_deref(), Some("first"));
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[1][0].automation.as_deref(), Some("second"));
        assert_eq!(groups[2].len(), 2);
        assert_eq!(groups[2][0].automation.as_deref(), Some("par-a"));
        assert_eq!(groups[2][1].automation.as_deref(), Some("par-b"));
        assert_eq!(groups[3].len(), 1);
        assert!(groups[3][0].is_tool());
    }

    #[test]
    fn test_collect_step_groups_non_adjacent_same_group() {
        let steps = vec![
            config::PipelineStep { automation: Some("a".into()), tool: None, group: Some(1) },
            config::PipelineStep { automation: Some("middle".into()), tool: None, group: None },
            config::PipelineStep { automation: Some("b".into()), tool: None, group: Some(1) },
        ];
        let groups = collect_step_groups(&steps);
        // group 1 collects both "a" and "b" even though "middle" is between them
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2); // group 1: a + b
        assert_eq!(groups[1].len(), 1); // middle
    }

    #[test]
    fn test_pipeline_toml_round_trip_write() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("test-pipeline.toml");
        std::fs::write(
            &path,
            r#"id = "test-pipe"
label = "Test"
description = ""
icon = "zap"
type = "pipeline"

[[step]]
automation = "scan"

[[step]]
automation = "review"
"#,
        )?;

        // Verify initial parse
        let entry = config::load_single_automation(&path, tmp.path())?;
        assert_eq!(entry.steps.len(), 2);

        // Modify steps via TOML writer
        let new_steps = vec![
            config::PipelineStep { automation: Some("new-first".into()), tool: None, group: None },
            config::PipelineStep { tool: Some("my-tool".into()), automation: None, group: Some(2) },
            config::PipelineStep { automation: Some("par-auto".into()), tool: None, group: Some(2) },
        ];

        // Write steps directly using toml_edit (same logic as write_pipeline_steps)
        let content = std::fs::read_to_string(&path)?;
        let mut doc = content.parse::<toml_edit::DocumentMut>()?;
        doc.remove("step");
        let mut array = toml_edit::ArrayOfTables::new();
        for step in &new_steps {
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
        doc.insert("step", toml_edit::Item::ArrayOfTables(array));
        std::fs::write(&path, doc.to_string())?;

        // Re-parse and verify
        let entry = config::load_single_automation(&path, tmp.path())?;
        assert_eq!(entry.steps.len(), 3);
        assert_eq!(entry.steps[0].automation.as_deref(), Some("new-first"));
        assert!(entry.steps[0].group.is_none());
        assert_eq!(entry.steps[1].tool.as_deref(), Some("my-tool"));
        assert_eq!(entry.steps[1].group, Some(2));
        assert_eq!(entry.steps[2].automation.as_deref(), Some("par-auto"));
        assert_eq!(entry.steps[2].group, Some(2));

        // Verify other fields preserved
        assert_eq!(entry.id, "test-pipe");
        assert!(entry.is_pipeline());

        Ok(())
    }
}
