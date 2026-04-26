mod ai_agents_section;
mod automation_card;
mod automation_picker;
pub(crate) mod card;
mod config;
mod context_editor;
mod event_inbox;
mod folder_bar;
mod hotkeys;
mod popup_inbox;
mod dashboard_paths;
pub(crate) mod persistence;
mod pipeline_card;
mod rules_integration;
mod watcher_card;
mod runtime;
mod scheduler_ui;
mod section;
mod tool_card;

use config::FolderTarget;
use dashboard_paths::{
    DeliveryStatus, ensure_config_extracted, ensure_workspace_dirs, folder_has_dashboard_config,
    local_tools_dir_for, resolve_agent_tools_path, resolve_runtime_path, scan_delivery_folder,
    suite_root,
};
use postprod_dashboard_config as dcfg;
use postprod_dashboard_config::{
    AgentEntry, AutomationEntry, BackendEntry, ParamEntry, ParamType, PipelineStep, ScheduleConfig,
    ToolEntry, ToolSource, ToolTier, automations_dir_for, load_agents_config,
    load_automations_registry, load_tools_registry, state_dir_for, tools_config_dir_for,
};
use persistence::{
    group_by_section, read_active_folder, read_background_tools, read_collapsed_sections,
    read_destination_folder, read_param_values, read_recent_destinations, read_recent_folders,
    read_section_order, write_active_folder, write_background_tools, write_collapsed_sections,
    write_destination_folder, write_param_values,
};

use card::CardRenderContext;
use hotkeys::GlobalShortcutModal;
pub use hotkeys::init_global_hotkeys;

use agent_ui::{AgentPanel, InlineAssistant};
use editor::{Editor, EditorEvent};
use gpui::{
    Action, AnyWindowHandle, App, AsyncApp, Context, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, IntoElement, ParentElement, PathPromptOptions, Render, ScrollHandle,
    SharedString, Styled, Subscription, UpdateGlobal, WeakEntity, Window, WindowHandle, actions,
};
use menu;
use postprod_rules::note_store::NoteStore;
use project::WorktreeId;
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{RegisterSetting, Settings, update_settings_file};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use ui::{
    ButtonLike, ButtonStyle, ContextMenu, Disclosure, Divider, DividerColor, DropdownMenu,
    DropdownStyle, Headline, HeadlineSize, Icon, IconButton, IconName, IconSize, Indicator, Label,
    LabelSize, ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip,
    WithScrollbar as _, prelude::*,
};
use workspace::{
    DraggedSelection, OpenOptions, ProToolsSessionName, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use postprod_scheduler::{ChainOnlyEntry, Scheduler, SchedulerEvent, SyncEntry};
use util::ResultExt as _;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

actions!(
    dashboard,
    [
        /// Toggle the PostProd Tools Dashboard panel.
        ToggleFocus
    ]
);

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, RegisterSetting)]
struct DashboardSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub starts_open: bool,
}

impl Settings for DashboardSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.dashboard_panel.as_ref();
        Self {
            button: panel.and_then(|p| p.button).unwrap_or(true),
            dock: panel
                .and_then(|p| p.dock)
                .map(Into::into)
                .unwrap_or(DockPosition::Right),
            default_width: panel
                .and_then(|p| p.default_width.map(px))
                .unwrap_or(px(360.)),
            starts_open: panel.and_then(|p| p.starts_open).unwrap_or(true),
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutomationRunStatus {
    GatheringContext,
    Failed(String),
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

pub fn init(cx: &mut App) {
    cx.observe_new::<Workspace>(|workspace, _, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<Dashboard>(window, cx);
        });
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

const DASHBOARD_PANEL_KEY: &str = "Dashboard";

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
    default_contexts: Vec<dcfg::ContextEntry>,
    agent_backend: AgentBackend,
    backends: Vec<BackendEntry>,
    agent_launchers: Vec<AgentEntry>,
    _automations_reload_task: gpui::Task<()>,
    _tools_reload_task: gpui::Task<()>,
    _agents_reload_task: gpui::Task<()>,
    // Event-bus reader for `kind = "notification"` — drains pending files
    // on construction (offline-accumulation) and on every fs-watch batch.
    // The inbox owns its own `_watch_task`; this field's lifetime keeps
    // the watch subscription alive (drop = unsubscribe).
    _notification_inbox: event_inbox::DashboardNotificationInbox,
    // Event-bus reader for `kind = "notification.popup"`. Peer of
    // `_notification_inbox` — same delivery mechanism, OS-level popup window
    // rendering. `pub(crate)` because `popup_inbox.rs` re-enters via
    // `dashboard.popup_inbox.dispatch_event(...)` from a fs-watch task.
    pub(crate) popup_inbox: popup_inbox::DashboardPopupInbox,
    // Folder watchers (`postprod_watchers`). `watcher_runtime` owns the
    // running per-watcher tasks; reconciled by `_watchers_reload_task`
    // every 10s (no-op when the config hash is unchanged, per D19).
    // `watcher_status_task` keeps `watcher_statuses` synced with the
    // runtime's `smol::channel` updates.
    watcher_runtime: postprod_watchers::WatcherRuntime,
    pub(crate) watcher_configs:
        Vec<Result<dcfg::watcher_config::WatcherConfig, dcfg::watcher_config::LoadError>>,
    pub(crate) watcher_statuses:
        HashMap<postprod_watchers::WatcherId, postprod_watchers::WatcherStatus>,
    _watchers_reload_task: gpui::Task<()>,
    _watcher_status_task: gpui::Task<()>,
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
    automation_status: HashMap<String, AutomationRunStatus>,
    pipelines_in_edit_mode: HashSet<String>,
    pipeline_cancel_flags: HashMap<String, Arc<AtomicBool>>,
    pipelines_pending_delete: HashSet<String>,
    automations_pending_delete: HashSet<String>,
    // Pipeline creation
    pending_new_pipeline: Option<Entity<Editor>>,
    _pending_pipeline_subscription: Option<Subscription>,
    // "Add Automation" ghost card state
    pending_new_automation: Option<Entity<Editor>>,
    _pending_automation_subscription: Option<Subscription>,
    // PostProd Rules (notes LMDB store + window handle)
    note_store: Option<Entity<NoteStore>>,
    postprod_rules_window: Option<WindowHandle<postprod_rules::PostProdRules>>,
    _note_store_init: Option<gpui::Task<()>>,
}

/// Resolve the event-bus root that watchers and the notification inbox
/// share. Honors `POSTPROD_EVENTS_INBOX` (used by integration tests) and
/// otherwise falls back to the workspace `paths` crate's `data_dir()`.
fn resolve_event_bus_root() -> PathBuf {
    if let Some(over) = std::env::var_os(postprod_events::bus::INBOX_ENV_VAR) {
        return PathBuf::from(over);
    }
    paths::data_dir().join("events")
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

/// 2-tier resolver mirroring upstream's `TitleBar::effective_active_worktree`
/// (PR #53645): worktree containing the active repository wins; otherwise
/// fall back to the first visible worktree. Pure function so the GPUI-free
/// behavior can be unit-tested directly.
pub(crate) fn effective_active_worktree_id<'a>(
    active_repo_path: Option<&Path>,
    visible: impl IntoIterator<Item = (WorktreeId, &'a Path)>,
) -> Option<WorktreeId> {
    let mut first: Option<WorktreeId> = None;
    let mut matched: Option<WorktreeId> = None;
    for (id, wt_path) in visible {
        if first.is_none() {
            first = Some(id);
        }
        if matched.is_none() {
            if let Some(repo_path) = active_repo_path {
                if wt_path == repo_path || wt_path.starts_with(repo_path) {
                    matched = Some(id);
                }
            }
        }
    }
    matched.or(first)
}

impl Dashboard {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let config_root = workspace
                .root_paths(cx)
                .into_iter()
                .find(|path| folder_has_dashboard_config(path))
                .map(|arc_path| arc_path.to_path_buf())
                .unwrap_or_else(suite_root);

            Dashboard::new(workspace, config_root, cx)
        })
    }

    fn render_watchers_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        watcher_card::render_watchers_section(
            &self.collapsed_sections,
            &self.config_root,
            &self.watcher_configs,
            &self.watcher_statuses,
            &self.workspace,
            cx.entity().downgrade(),
            cx,
        )
    }

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
        let default_contexts = dcfg::load_default_contexts(&config_root);
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
                            dashboard.prune_stale_automation_status();
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

            // Observe the workspace for effective-worktree changes (fires
            // when user picks a folder via the recent-project menu). Upstream
            // PR #53645 removed `active_worktree_override`; pickers now set
            // `Project::active_repository`, and the worktree containing that
            // repo is the new "effective active worktree." 2-tier resolution
            // delegated to `effective_active_worktree_id` so it stays
            // unit-testable.
            let workspace_observation = workspace.weak_handle().upgrade().map(|ws_entity| {
                cx.observe(&ws_entity, |dashboard: &mut Dashboard, workspace_entity, cx| {
                    let workspace = workspace_entity.read(cx);
                    let project = workspace.project().read(cx);
                    let active_repo_path = project
                        .active_repository(cx)
                        .map(|repo| repo.read(cx).work_directory_abs_path.clone());
                    let visible: Vec<(WorktreeId, Arc<Path>)> = project
                        .visible_worktrees(cx)
                        .map(|wt| {
                            let wt_ref = wt.read(cx);
                            (wt_ref.id(), wt_ref.abs_path())
                        })
                        .collect();
                    let current = effective_active_worktree_id(
                        active_repo_path.as_deref(),
                        visible.iter().map(|(id, p)| (*id, p.as_ref())),
                    );
                    if current == dashboard.last_worktree_override {
                        return;
                    }
                    dashboard.last_worktree_override = current;
                    if let Some(worktree_id) = current {
                        let folder = project
                            .visible_worktrees(cx)
                            .find(|wt| wt.read(cx).id() == worktree_id)
                            .map(|wt| wt.read(cx).abs_path().to_path_buf());
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

            // Create NoteStore init task before building the struct
            let db_path = state_dir_for(&config_root).join("notes.mdb");
            let note_store_task = NoteStore::new(db_path, cx);
            let note_store_init = cx.spawn(async move |dashboard, cx: &mut AsyncApp| {
                match note_store_task.await {
                    Ok(store) => {
                        dashboard.update(cx, |dashboard, cx| {
                            dashboard.note_store = Some(cx.new(|_| store));
                            dashboard._note_store_init = None;
                            cx.notify();
                        }).log_err();
                    }
                    Err(err) => {
                        log::error!("Failed to initialize NoteStore: {:?}", err);
                    }
                }
            });

            // Event-bus notification reader. Drains pending files on
            // construction (catches anything that accumulated while the
            // dashboard was closed) and re-drains on each fs-watch batch.
            // Uses the workspace's project-wide `fs::Fs` so test impls
            // observe the same state as the watch subscription.
            let fs = workspace.project().read(cx).fs().clone();
            let notification_inbox = event_inbox::DashboardNotificationInbox::new(
                fs.clone(),
                workspace.weak_handle(),
                cx,
            );

            // Peer inbox for `kind = "notification.popup"`. Same delivery
            // mechanism as `notification_inbox`; renders an OS-level popup
            // window instead of an in-app toast.
            let popup_inbox = popup_inbox::DashboardPopupInbox::new(fs.clone(), cx);

            // Folder watchers — runtime + status listener + 10s reload task.
            // Reload mirrors `_automations_reload_task` (uncondtional 10s
            // tick); reconcile inside is hash-gated per D19 so the kernel
            // does NOT re-acquire fs-watch subscriptions every tick.
            let watcher_configs = dcfg::watcher_config::load_watchers(&config_root);
            let mut watcher_runtime = postprod_watchers::WatcherRuntime::new();
            let watcher_status_rx = watcher_runtime.status_receiver();
            // Initial reconcile: start any enabled+valid watchers immediately
            // so the first cards render as `idle` instead of `starting…`.
            {
                let initial_configs: Vec<dcfg::watcher_config::WatcherConfig> = watcher_configs
                    .iter()
                    .filter_map(|r| r.as_ref().ok().cloned())
                    .collect();
                let bus_root = resolve_event_bus_root();
                watcher_runtime.reconcile(initial_configs, fs.clone(), bus_root, cx);
            }
            // Status listener loop — drains the smol::channel and updates
            // `watcher_statuses` + cx.notify() on each message. Outer cx is
            // Context<Dashboard>; spawn provides (this, AsyncApp).
            let watcher_status_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                while let Ok((id, status)) = watcher_status_rx.recv().await {
                    if this
                        .update(cx, |dashboard, cx| {
                            dashboard.watcher_statuses.insert(id.clone(), status.clone());
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            // 10s reload + reconcile task. Mirrors `_automations_reload_task`.
            let watchers_reload_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(10))
                        .await;

                    let config_root = match this.update(cx, |d, _| d.config_root.clone()) {
                        Ok(c) => c,
                        Err(_) => break,
                    };
                    let loaded_from = config_root.clone();
                    let results = cx
                        .background_executor()
                        .spawn(async move { dcfg::watcher_config::load_watchers(&config_root) })
                        .await;

                    this.update(cx, |dashboard, cx| {
                        if dashboard.config_root != loaded_from {
                            return;
                        }
                        let configs: Vec<dcfg::watcher_config::WatcherConfig> = results
                            .iter()
                            .filter_map(|r| r.as_ref().ok().cloned())
                            .collect();
                        dashboard.watcher_configs = results;
                        let Some(workspace) = dashboard.workspace.upgrade() else { return };
                        let fs = workspace.read(cx).project().read(cx).fs().clone();
                        let bus_root = resolve_event_bus_root();
                        dashboard.watcher_runtime.reconcile(configs, fs, bus_root, cx);
                        cx.notify();
                    })
                    .log_err();
                }
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
                _notification_inbox: notification_inbox,
                popup_inbox,
                watcher_runtime,
                watcher_configs,
                watcher_statuses: HashMap::new(),
                _watchers_reload_task: watchers_reload_task,
                _watcher_status_task: watcher_status_task,
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
                automation_status: HashMap::new(),
                pipelines_in_edit_mode: HashSet::new(),
                pipeline_cancel_flags: HashMap::new(),
                pipelines_pending_delete: HashSet::new(),
                automations_pending_delete: HashSet::new(),
                pending_new_pipeline: None,
                _pending_pipeline_subscription: None,
                pending_new_automation: None,
                _pending_automation_subscription: None,
                note_store: None,
                postprod_rules_window: None,
                _note_store_init: Some(note_store_init),
            }
        })
    }

    // PostProd Rules bridge methods (open_postprod_rules,
    // open_postprod_rules_scoped, build_automation_info,
    // build_all_automation_infos, build_context_callbacks,
    // reinit_rules_for_new_root, refresh_scoped_rules_window) live in
    // `crates/dashboard/src/rules_integration.rs`.

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
        self.default_contexts = dcfg::load_default_contexts(&self.config_root);
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
        self.automation_status.clear();
        self.pending_new_pipeline = None;
        self._pending_pipeline_subscription = None;

        // Close the rules window (tied to the old NoteStore) and spin up a
        // fresh NoteStore for the new config_root.
        self.reinit_rules_for_new_root(cx);

        cx.notify();
    }

    /// Gather all notes (default + assigned) for a given automation.
    /// Returns a formatted string for injection into the prompt.
    /// Only used for standalone runs, not pipeline/scheduler paths.
    fn gather_notes_for_automation(&self, automation_id: &str, cx: &App) -> String {
        let Some(store) = &self.note_store else {
            return String::new();
        };
        let store_ref = store.read(cx);
        let matching_notes = store_ref.notes_for_automation(automation_id);

        if matching_notes.is_empty() {
            return String::new();
        }

        let mut parts = vec!["=== Notes ===".to_string()];
        for note in &matching_notes {
            let title = note
                .title
                .as_ref()
                .map(|t| t.as_ref())
                .unwrap_or("Untitled");
            match store_ref.load_body_sync(note.id) {
                Ok(body) => {
                    parts.push(format!("{}:\n{}", title, body));
                }
                Err(err) => {
                    log::warn!("Failed to load note body for '{}': {:?}", title, err);
                }
            }
        }
        parts.push("=== End of notes ===".to_string());
        parts.join("\n\n")
    }

    // Backend runtime methods (run_automation, spawn_automation_in_terminal,
    // run_pipeline, spawn_completion_poller) and their free-function
    // helpers (apply_agent_profile_to_thread, gather_context_blocking,
    // build_temp_file_terminal_command) live in
    // `crates/dashboard/src/runtime.rs`.


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
        let timeout_secs = self
            .scheduler
            .read(cx)
            .entries()
            .get(&automation_id_owned)
            .map(|e| e.timeout_secs)
            .unwrap_or(3600);

        self.spawn_completion_poller(
            automation_id_owned,
            marker_path,
            timeout_secs,
            chain_depth,
            cx,
        );
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
                    })
                    .log_err();
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
                    })
                    .log_err();
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
        workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, {
                    let tool_id = tool_id.clone();
                    let tool_label = tool_label.clone();
                    move |window, cx| {
                        GlobalShortcutModal::new(tool_id, tool_label, config_root, window, cx)
                    }
                });
            })
            .log_err();
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
        format!(
            r#"

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
"#
        )
    }

    fn schedule_param_write(&mut self, cx: &mut Context<Self>) {
        self._param_write_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            this.update(cx, |this, _cx| {
                write_param_values(&this.config_root, &this.param_values);
            })
            .log_err();
        }));
    }

    // -- Rendering helpers --

    pub(crate) fn toggle_section(&mut self, section_id: &str, cx: &mut Context<Self>) {
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
        day: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let entry = match self.automations.iter_mut().find(|a| a.id == automation_id) {
            Some(e) => e,
            None => return,
        };

        let cron = scheduler_ui::cron_from_interval_and_hour(interval, hour, day);

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
                    chain: a.chain.as_ref().map(|c| postprod_scheduler::ChainConfig {
                        triggers: c.triggers.clone(),
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

        let Some(source_path) = &entry.source_path else {
            return;
        };
        let Some(schedule) = &entry.schedule else {
            return;
        };

        let source_path = source_path.clone();
        let schedule = schedule.clone();

        cx.background_spawn(async move {
            if let Err(error) = dcfg::edit::write_schedule(&source_path, &schedule) {
                log::warn!(
                    "Failed to write schedule to {}: {error}",
                    source_path.display()
                );
            }
        })
        .detach();

        // Sync updated schedule to scheduler (immediate — scheduler state is in-memory)
        let default_folder = self
            .active_folder
            .clone()
            .unwrap_or_else(|| self.config_root.clone());
        let sync_entries = Self::build_sync_entries(&self.automations);
        let unscheduled = Self::build_unscheduled_entries(&self.automations, &default_folder);
        self.scheduler.update(cx, |scheduler, _cx| {
            scheduler.sync_entries(sync_entries, unscheduled, default_folder);
        });
    }

    fn prune_stale_automation_status(&mut self) {
        let live_ids: HashSet<&str> = self.automations.iter().map(|a| a.id.as_str()).collect();
        self.automation_status
            .retain(|id, _| live_ids.contains(id.as_str()));
    }

    fn reload_automations(&mut self, cx: &mut Context<Self>) {
        let (automations, automations_error) = load_automations_registry(&self.config_root);
        self.automations = automations;
        self.automations_error = automations_error;
        self.default_contexts = dcfg::load_default_contexts(&self.config_root);
        self.prune_stale_automation_status();

        self.refresh_scoped_rules_window(cx);

        cx.notify();
    }

    fn write_pipeline_steps(
        &self,
        pipeline_id: &str,
        steps: &[PipelineStep],
        cx: &mut Context<Self>,
    ) {
        let entry = match self.automations.iter().find(|a| a.id == pipeline_id) {
            Some(e) => e,
            None => return,
        };
        let Some(source_path) = &entry.source_path else {
            return;
        };
        let source_path = source_path.clone();
        let steps = steps.to_vec();

        cx.background_spawn(async move {
            if let Err(error) = dcfg::edit::write_pipeline_steps(&source_path, &steps) {
                log::warn!(
                    "Failed to write pipeline steps to {}: {error}",
                    source_path.display()
                );
            }
        })
        .detach();
    }

    fn create_pipeline_toml(&mut self, name: &str, cx: &mut Context<Self>) -> String {
        let automations_dir = automations_dir_for(&self.config_root);
        let created = match dcfg::edit::create_pipeline_stub(&automations_dir, name) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to create pipeline stub: {e}");
                return String::new();
            }
        };
        self.reload_automations(cx);
        created.id
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

    fn reorder_context_entry(
        &mut self,
        automation_id: &str,
        from: usize,
        direction: i32,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == automation_id) {
            let to_signed = from as i32 + direction;
            if to_signed < 0 {
                return;
            }
            let to = to_signed as usize;
            if from < entry.contexts.len() && to < entry.contexts.len() {
                entry.contexts.swap(from, to);
                let contexts = entry.contexts.clone();
                self.write_context_entries(automation_id, &contexts, cx);
                cx.notify();
            }
        }
    }

    fn add_context_path_entry(
        &mut self,
        automation_id: &str,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let new_entry = dcfg::ContextEntry {
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
    fn add_context_script_entry(
        &mut self,
        automation_id: &str,
        script_name: String,
        cx: &mut Context<Self>,
    ) {
        let label = std::path::Path::new(&script_name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| script_name.clone());
        let new_entry = dcfg::ContextEntry {
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
        let Some(entry) = self.automations.iter_mut().find(|a| a.id == automation_id) else {
            return;
        };
        entry.skip_default_context = !entry.skip_default_context;
        let new_value = entry.skip_default_context;
        let Some(source_path) = entry.source_path.clone() else {
            return;
        };

        cx.background_spawn(async move {
            if let Err(e) = dcfg::edit::set_skip_default_context(&source_path, new_value) {
                log::warn!("Failed to write {}: {e}", source_path.display());
            }
        })
        .detach();
        cx.notify();
    }

    fn toggle_context_edit_mode(&mut self, automation_id: &str, cx: &mut Context<Self>) {
        if !self.automations_in_context_edit.remove(automation_id) {
            self.automations_in_context_edit
                .insert(automation_id.to_string());
        }
        cx.notify();
    }

    fn write_context_entries(
        &self,
        automation_id: &str,
        contexts: &[dcfg::ContextEntry],
        cx: &mut Context<Self>,
    ) {
        let entry = match self.automations.iter().find(|a| a.id == automation_id) {
            Some(e) => e,
            None => return,
        };
        let Some(source_path) = &entry.source_path else {
            return;
        };
        let source_path = source_path.clone();
        let contexts = contexts.to_vec();

        cx.background_spawn(async move {
            if let Err(e) = dcfg::edit::write_context_entries(&source_path, &contexts) {
                log::warn!(
                    "Failed to write context entries to {}: {e}",
                    source_path.display()
                );
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // "Add Automation" flow (follows pipeline ghost card pattern)
    // ------------------------------------------------------------------

    fn create_automation_toml(&mut self, name: &str, cx: &mut Context<Self>) -> String {
        let automations_dir = automations_dir_for(&self.config_root);
        let created = match dcfg::edit::create_automation_stub(&automations_dir, name) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to create automation stub: {e}");
                return String::new();
            }
        };
        self.reload_automations(cx);
        created.id
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

    fn delete_automation_toml(&mut self, automation_id: &str, cx: &mut Context<Self>) {
        if let Some(path) = self
            .automations
            .iter()
            .find(|a| a.id == automation_id)
            .and_then(|a| a.source_path.clone())
        {
            std::fs::remove_file(&path).log_err();
            self.automations_pending_delete.remove(automation_id);
            self.reload_automations(cx);
        }
    }

    fn delete_pipeline_toml(&mut self, pipeline_id: &str, cx: &mut Context<Self>) {
        if let Some(path) = self
            .automations
            .iter()
            .find(|a| a.id == pipeline_id)
            .and_then(|a| a.source_path.clone())
        {
            std::fs::remove_file(&path).log_err();
            self.pipelines_in_edit_mode.remove(pipeline_id);
            self.reload_automations(cx);
        }
    }

    fn remove_pipeline_step(
        &mut self,
        pipeline_id: &str,
        step_index: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == pipeline_id) {
            if step_index < entry.steps.len() {
                entry.steps.remove(step_index);
                let steps = entry.steps.clone();
                self.write_pipeline_steps(pipeline_id, &steps, cx);
                cx.notify();
            }
        }
    }

    fn reorder_pipeline_step(
        &mut self,
        pipeline_id: &str,
        from: usize,
        direction: i32,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.automations.iter_mut().find(|a| a.id == pipeline_id) {
            let to_signed = from as i32 + direction;
            if to_signed < 0 {
                return;
            }
            let to = to_signed as usize;
            if from < entry.steps.len() && to < entry.steps.len() {
                entry.steps.swap(from, to);
                let steps = entry.steps.clone();
                self.write_pipeline_steps(pipeline_id, &steps, cx);
                cx.notify();
            }
        }
    }

    fn render_session_status(&self, cx: &App) -> AnyElement {
        folder_bar::render_session_status(&self.session_name, &self.session_path, cx)
    }

    fn render_folder_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let config_root = self.config_root.clone();

        let active_drop_ext = cx.listener(|this, paths: &ExternalPaths, _window, cx| {
            if let Some(dir) = paths.paths().iter().find(|p| p.is_dir()) {
                this.set_folder(FolderTarget::Active, dir.clone(), cx);
            }
        });
        let active_drop_sel = cx.listener(|this, sel: &DraggedSelection, _window, cx| {
            if let Some(dir) = this.resolve_dragged_directory(sel, cx) {
                this.set_folder(FolderTarget::Active, dir, cx);
            }
        });
        let dest_drop_ext = cx.listener(|this, paths: &ExternalPaths, _window, cx| {
            if let Some(dir) = paths.paths().iter().find(|p| p.is_dir()) {
                this.set_folder(FolderTarget::Destination, dir.clone(), cx);
            }
        });
        let dest_drop_sel = cx.listener(|this, sel: &DraggedSelection, _window, cx| {
            if let Some(dir) = this.resolve_dragged_directory(sel, cx) {
                this.set_folder(FolderTarget::Destination, dir, cx);
            }
        });

        folder_bar::render_folder_row(
            &self.active_folder,
            &self.recent_folders,
            &self.destination_folder,
            &self.recent_destinations,
            // on_active_select
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
            // on_active_browse
            {
                let entity = entity.clone();
                move |_window, cx: &mut App| {
                    entity.update(cx, |this, cx| {
                        this.pick_active_folder(cx);
                    });
                }
            },
            // on_dest_select
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
            // on_dest_browse
            {
                let entity = entity;
                move |_window, cx: &mut App| {
                    entity.update(cx, |this, cx| {
                        this.pick_destination_folder(cx);
                    });
                }
            },
            // drag-drop handlers (created via cx.listener above)
            move |paths, window, cx| active_drop_ext(paths, window, cx),
            move |sel, window, cx| active_drop_sel(sel, window, cx),
            move |paths, window, cx| dest_drop_ext(paths, window, cx),
            move |sel, window, cx| dest_drop_sel(sel, window, cx),
            window,
            cx,
        )
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
        section::section_header(
            title,
            section_id,
            &self.collapsed_sections,
            cx.entity().downgrade(),
            cx,
        )
    }

    fn sub_section_header(
        &self,
        title: &str,
        section_id: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        section::sub_section_header(
            title,
            section_id,
            &self.collapsed_sections,
            cx.entity().downgrade(),
            cx,
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
                workspace
                    .update(cx, |_workspace, cx| {
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
                    })
                    .log_err();
            } else {
                workspace
                    .update(cx, |workspace, cx| {
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
                    })
                    .log_err();
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
                    toggle_entity
                        .update(cx, |this, cx| {
                            if this.background_tools.contains(&tool_id) {
                                this.background_tools.remove(&tool_id);
                            } else {
                                this.background_tools.insert(tool_id);
                            }
                            write_background_tools(&this.config_root, &this.background_tools);
                            cx.notify();
                        })
                        .log_err();
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
                    globe_entity
                        .update(cx, |this, cx| {
                            this.open_global_shortcut_modal(tool_id, tool_label, window, cx);
                        })
                        .log_err();
                })
                .visible_on_hover(group_name),
            )
    }

    /// Build Featured tool cards using the shared `DashboardCard` component.
    fn build_featured_cards(
        &mut self,
        tools: &[ToolEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let tools_owned: Vec<ToolEntry> = tools.to_vec();
        tools_owned
            .into_iter()
            .map(|tool| {
                let group_name = SharedString::from(format!("tool-{}", tool.id));
                let click_handler = self.tool_click_handler(&tool, cx);
                let param_fields = if !tool.params.is_empty() {
                    self.render_entry_params(&tool.id, &tool.params, window, cx)
                } else {
                    Vec::new()
                };
                let drop_handler = cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                    if let Some(dir) = paths.paths().iter().find(|p| p.is_dir()) {
                        this.set_folder(FolderTarget::Destination, dir.clone(), cx);
                    }
                });
                let tool_id = tool.id.clone();
                let tool_label = tool.label.clone();
                let action_buttons = self
                    .tool_action_buttons(&tool_id, &tool_label, group_name, cx)
                    .into_any_element();

                tool_card::render_featured_tool(
                    &tool,
                    action_buttons,
                    param_fields,
                    click_handler,
                    Some(drop_handler),
                    cx,
                )
            })
            .collect()
    }

    /// Build Standard tool cards using the shared `DashboardCard` component.
    fn build_standard_cards(
        &mut self,
        tools: &[ToolEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let tools_owned: Vec<ToolEntry> = tools.to_vec();
        tools_owned
            .into_iter()
            .map(|tool| {
                let group_name = SharedString::from(format!("tool-{}", tool.id));
                let click_handler = self.tool_click_handler(&tool, cx);
                let param_fields = if !tool.params.is_empty() {
                    self.render_entry_params(&tool.id, &tool.params, window, cx)
                } else {
                    Vec::new()
                };

                let drop_handler = tool
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

                let tool_id = tool.id.clone();
                let tool_label = tool.label.clone();
                let action_buttons = self
                    .tool_action_buttons(&tool_id, &tool_label, group_name, cx)
                    .into_any_element();

                tool_card::render_standard_tool(
                    &tool,
                    action_buttons,
                    param_fields,
                    click_handler,
                    drop_handler,
                    cx,
                )
            })
            .collect()
    }

    /// Build Compact tool cards using the shared `DashboardCard` component.
    fn build_compact_cards(
        &self,
        tools: &[ToolEntry],
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let tools_owned: Vec<ToolEntry> = tools.to_vec();
        tools_owned
            .into_iter()
            .map(|tool| {
                let group_name = SharedString::from(format!("tool-{}", tool.id));
                let click_handler = self.tool_click_handler(&tool, cx);
                let tool_id = tool.id.clone();
                let tool_label = tool.label.clone();
                let action_buttons = self
                    .tool_action_buttons(&tool_id, &tool_label, group_name, cx)
                    .into_any_element();

                tool_card::render_compact_tool(&tool, action_buttons, click_handler, cx)
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
                    .size(LabelSize::Small),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
            .child(edit_btn);

        if !is_open {
            return v_flex().w_full().gap_1().child(header);
        }

        let all_tools: Vec<ToolEntry> = self.tools.iter().filter(|t| !t.hidden).cloned().collect();

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
                        v_flex()
                            .w_full()
                            .gap(DynamicSpacing::Base06.rems(cx))
                            .children(cards)
                            .into_any_element(),
                    );
                }
                if !standard.is_empty() {
                    let cards = self.build_standard_cards(&standard, window, cx);
                    section_elements.push(
                        v_flex()
                            .w_full()
                            .gap(DynamicSpacing::Base06.rems(cx))
                            .children(cards)
                            .into_any_element(),
                    );
                }
                if !compact.is_empty() {
                    let cards = self.build_compact_cards(&compact, cx);
                    section_elements.push(
                        v_flex()
                            .w_full()
                            .gap(DynamicSpacing::Base04.rems(cx))
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
                    .size(LabelSize::Small);

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
                    ParamType::Path | ParamType::Cwd => {
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
                                    .size(LabelSize::Small)
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
                                    entity
                                        .update(cx, |_this, cx| {
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
                                                        entity
                                                            .update(
                                                                cx,
                                                                |this: &mut Dashboard, cx| {
                                                                    this.param_values
                                                                        .entry(entry_id.clone())
                                                                        .or_default()
                                                                        .insert(
                                                                            param_key.clone(),
                                                                            path_str,
                                                                        );
                                                                    write_param_values(
                                                                        &this.config_root,
                                                                        &this.param_values,
                                                                    );
                                                                    cx.notify();
                                                                },
                                                            )
                                                            .log_err();
                                                    }
                                                }
                                            })
                                            .detach();
                                        })
                                        .log_err();
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
                                            entity
                                                .update(cx, |this: &mut Dashboard, cx| {
                                                    this.param_values
                                                        .entry(entry_id.clone())
                                                        .or_default()
                                                        .insert(param_key.clone(), value.clone());
                                                    write_param_values(
                                                        &this.config_root,
                                                        &this.param_values,
                                                    );
                                                    cx.notify();
                                                })
                                                .log_err();
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

    fn render_automation_card(
        &mut self,
        entry: &AutomationEntry,
        idx: usize,
        icon_color: Color,
        badge_label: &SharedString,
        badge_color: Color,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let is_expanded = self.expanded_automations.contains(&entry.id);
        let is_scheduled = entry.schedule.as_ref().is_some_and(|s| s.enabled);
        let is_pending_delete = self.automations_pending_delete.contains(&entry.id);
        let (accent, _) = self.agent_backend.card_accent(cx);

        let schedule_cron = entry
            .schedule
            .as_ref()
            .map(|s| s.cron.clone())
            .unwrap_or_default();

        let param_fields = self.render_entry_params(&entry.id, &entry.params, window, cx);
        let context_rows = if is_expanded {
            let entity = cx.entity().downgrade();
            if self.automations_in_context_edit.contains(&entry.id) {
                let automation_source_path = self
                    .automations
                    .iter()
                    .find(|a| a.id == entry.id)
                    .and_then(|a| a.source_path.clone());
                let scripts = dcfg::collect_context_scripts(&self.config_root);
                context_editor::render_context_editor(
                    &entry.id,
                    &entry.contexts,
                    entry.skip_default_context,
                    automation_source_path,
                    self.workspace.clone(),
                    scripts,
                    self.config_root.clone(),
                    entity,
                    cx,
                )
            } else {
                context_editor::render_context_summary(
                    &entry.id,
                    &entry.contexts,
                    entry.skip_default_context,
                    &self.default_contexts,
                    entity,
                    cx,
                )
            }
        } else {
            Vec::new()
        };
        let schedule_controls = if is_scheduled {
            Some(
                self.render_schedule_controls(&entry.id, &schedule_cron, window, cx)
                    .into_any_element(),
            )
        } else {
            None
        };

        let run_status = self.automation_status.get(&entry.id);
        let ctx = CardRenderContext {
            entry,
            idx,
            accent,
            is_expanded,
            is_scheduled,
            is_pending_delete,
            entity: cx.entity().downgrade(),
            run_status,
        };

        automation_card::render_automation_card(
            &ctx,
            icon_color,
            badge_label.clone(),
            badge_color,
            param_fields,
            schedule_controls,
            context_rows,
            cx,
        )
    }

    fn render_pipeline_card(
        &mut self,
        entry: &AutomationEntry,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let is_running = self.active_pipelines.contains(&entry.id);
        let is_expanded = self.expanded_automations.contains(&entry.id);
        let is_editing = self.pipelines_in_edit_mode.contains(&entry.id);
        let is_pending_delete = self.pipelines_pending_delete.contains(&entry.id);
        let is_scheduled = entry.schedule.as_ref().is_some_and(|s| s.enabled);
        let schedule_cron = entry
            .schedule
            .as_ref()
            .map(|s| s.cron.clone())
            .unwrap_or_default();
        let accent = cx.theme().colors().text_accent;
        let entity = cx.entity().downgrade();
        let active_folder = self
            .active_folder
            .clone()
            .unwrap_or_else(|| self.config_root.clone());

        let step_tree = if is_editing {
            pipeline_card::render_pipeline_edit_steps(
                entry,
                &self.tools,
                &self.automations,
                entity.clone(),
                self.workspace.clone(),
                self.config_root.clone(),
                cx,
            )
        } else {
            pipeline_card::render_pipeline_step_tree(
                &entry.steps,
                &self.tools,
                &self.automations,
                cx,
            )
        };

        let sched_controls = self.render_schedule_controls(&entry.id, &schedule_cron, window, cx);

        let run_status = self.automation_status.get(&entry.id);
        let ctx = CardRenderContext {
            entry,
            idx,
            accent,
            is_expanded,
            is_scheduled,
            is_pending_delete,
            entity,
            run_status,
        };

        pipeline_card::render_pipeline_card(
            &ctx,
            is_running,
            is_editing,
            step_tree,
            sched_controls.into_any_element(),
            active_folder,
            cx,
        )
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
        if pipelines.is_empty()
            && !automations_dir_for(&self.config_root)
                .join("pipelines")
                .exists()
        {
            return v_flex().w_full();
        }

        let is_open = !self.collapsed_sections.contains("pipelines");

        let disc_entity = cx.entity().downgrade();
        let disclosure = Disclosure::new(SharedString::from("disc-pipelines"), is_open).on_click(
            move |_, _, cx| {
                disc_entity
                    .update(cx, |this, cx| {
                        this.toggle_section("pipelines", cx);
                    })
                    .log_err();
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
                    .size(LabelSize::Small),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant));

        if !is_open {
            return v_flex().w_full().gap_1().child(header);
        }

        let grouped = group_by_section(
            &pipelines,
            |a| a.section.as_deref(),
            |a| a.order,
            |a| &a.label,
            &self.section_order,
        );

        let mut cards: Vec<gpui::AnyElement> = Vec::new();
        let mut card_idx = 0;
        for (section_name, section_pipelines) in &grouped {
            let collapse_key = format!("section-pipe-{}", section_name);
            cards.push(
                self.sub_section_header(section_name, &collapse_key, cx)
                    .into_any_element(),
            );

            if !self.collapsed_sections.contains(&collapse_key) {
                for entry in section_pipelines {
                    cards.push(self.render_pipeline_card(entry, card_idx, window, cx));
                    card_idx += 1;
                }
            } else {
                card_idx += section_pipelines.len();
            }
        }

        // Ghost card for pending new pipeline
        let ghost_card = self.pending_new_pipeline.clone().map(|editor| {
            let accent = cx.theme().colors().text_accent;
            let border_color = cx.theme().colors().border;
            let confirm_entity = cx.entity().downgrade();
            let cancel_entity = cx.entity().downgrade();

            div()
                .id("new-pipeline-ghost")
                .child(
                    card::DashboardCard::new(
                        "new-pipeline-ghost-inner",
                        card::CardIcon::new(IconName::PlayFilled).color(Color::Accent),
                        "",
                    )
                    .accent(accent)
                    .custom_child(
                        div()
                            .flex_1()
                            .border_1()
                            .border_color(border_color)
                            .rounded_sm()
                            .px_1()
                            .child(editor),
                    )
                    .render(cx),
                )
                .on_action(move |_: &menu::Confirm, _, cx| {
                    confirm_entity
                        .update(cx, |this, cx| {
                            if let Some(editor) = &this.pending_new_pipeline {
                                let text = editor.read(cx).text(cx).trim().to_string();
                                this.finish_new_pipeline(text, cx);
                            }
                        })
                        .log_err();
                })
                .on_action(move |_: &menu::Cancel, _, cx| {
                    cancel_entity
                        .update(cx, |this, cx| {
                            this.cancel_new_pipeline(cx);
                        })
                        .log_err();
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
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("New Pipeline")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.start_new_pipeline(window, cx);
            }));

        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .child(
                v_flex()
                    .w_full()
                    .gap(DynamicSpacing::Base06.rems(cx))
                    .children(cards),
            )
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
                disc_entity
                    .update(cx, |this, cx| {
                        this.toggle_section("automations", cx);
                    })
                    .log_err();
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
                    .size(LabelSize::Small),
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
                            entity_claude
                                .update(cx, |this, cx| {
                                    this.agent_backend = AgentBackend::Claude;
                                    cx.notify();
                                })
                                .log_err();
                        }),
                        ToggleButtonSimple::new("Gemini", move |_, _, cx| {
                            entity_gemini
                                .update(cx, |this, cx| {
                                    this.agent_backend = AgentBackend::Gemini;
                                    cx.notify();
                                })
                                .log_err();
                        }),
                        ToggleButtonSimple::new("Copy", move |_, _, cx| {
                            entity_copy
                                .update(cx, |this, cx| {
                                    this.agent_backend = AgentBackend::CopyOnly;
                                    cx.notify();
                                })
                                .log_err();
                        }),
                        ToggleButtonSimple::new("Native", move |_, _, cx| {
                            entity_native
                                .update(cx, |this, cx| {
                                    this.agent_backend = AgentBackend::Native;
                                    cx.notify();
                                })
                                .log_err();
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
                        .gap(DynamicSpacing::Base06.rems(cx))
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
            .when_some(self.render_new_automation_ghost(cx), |el, card| {
                el.child(card)
            })
            .child(self.render_new_automation_button(cx))
    }

    fn render_new_automation_ghost(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let editor = self.pending_new_automation.clone()?;
        let accent = cx.theme().colors().text_accent;
        let border_color = cx.theme().colors().border;
        let confirm_entity = cx.entity().downgrade();
        let cancel_entity = cx.entity().downgrade();

        Some(
            div()
                .id("new-automation-ghost")
                .child(
                    card::DashboardCard::new(
                        "new-automation-ghost-inner",
                        card::CardIcon::new(IconName::Sparkle).color(Color::Accent),
                        "",
                    )
                    .accent(accent)
                    .custom_child(
                        div()
                            .flex_1()
                            .border_1()
                            .border_color(border_color)
                            .rounded_sm()
                            .px_1()
                            .child(editor),
                    )
                    .render(cx),
                )
                .on_action(move |_: &menu::Confirm, _, cx| {
                    confirm_entity
                        .update(cx, |this, cx| {
                            if let Some(editor) = &this.pending_new_automation {
                                let text = editor.read(cx).text(cx).trim().to_string();
                                this.finish_new_automation(text, cx);
                            }
                        })
                        .log_err();
                })
                .on_action(move |_: &menu::Cancel, _, cx| {
                    cancel_entity
                        .update(cx, |this, cx| {
                            this.cancel_new_automation(cx);
                        })
                        .log_err();
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
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("New Automation")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.start_new_automation(window, cx);
            }))
    }

    fn render_ai_agents_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ai_agents_section::render_ai_agents_section(
            &self.collapsed_sections,
            &self.agent_launchers,
            &self.workspace,
            self.agent_cwd(),
            cx.entity().downgrade(),
            cx,
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
                        this.workspace
                            .update(cx, |_workspace, cx| {
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
                            })
                            .log_err();
                    } else {
                        this.workspace
                            .update(cx, |workspace, cx| {
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
                            })
                            .log_err();
                    }
                }
            }))
            .on_action(
                cx.listener(|this, action: &RunDashboardAutomation, window, cx| {
                    if let Some(entry) = this
                        .automations
                        .iter()
                        .find(|a| a.id == action.automation_id)
                    {
                        let id = entry.id.clone();
                        let label = entry.label.clone();
                        let prompt = entry.prompt.clone();
                        this.run_automation(&id, &label, &prompt, window, cx);
                    }
                }),
            )
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .relative()
            .child(
                v_flex()
                    .id("dashboard-scroll")
                    .size_full()
                    .min_w_0()
                    .px(DynamicSpacing::Base08.rems(cx))
                    .pt(DynamicSpacing::Base16.rems(cx))
                    .pb(DynamicSpacing::Base16.rems(cx))
                    .gap(DynamicSpacing::Base16.rems(cx))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    // Header
                    .child(
                        h_flex()
                            .w_full()
                            .mb(DynamicSpacing::Base08.rems(cx))
                            .gap(DynamicSpacing::Base08.rems(cx))
                            .child(
                                Icon::new(IconName::AudioOn)
                                    .size(IconSize::Medium)
                                    .color(Color::Accent),
                            )
                            .child(Headline::new("PostProd Tools").size(HeadlineSize::Small))
                            .child(div().flex_grow())
                            .child(
                                IconButton::new("open-postprod-rules", IconName::Notepad)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Prompts & Notes"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_postprod_rules(None, window, cx);
                                    })),
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
                    // Folder watchers
                    .child(self.render_watchers_section(cx))
                    // Delivery status
                    .child(self.render_delivery_status(cx)),
            )
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
    }
}

// ---------------------------------------------------------------------------
// Trait impls for docked Panel
// ---------------------------------------------------------------------------

impl EventEmitter<PanelEvent> for Dashboard {}

impl Focusable for Dashboard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for Dashboard {
    fn persistent_name() -> &'static str {
        "Dashboard"
    }

    fn panel_key() -> &'static str {
        DASHBOARD_PANEL_KEY
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        DashboardSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Left | DockPosition::Right | DockPosition::Bottom
        )
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fs) = self
            .workspace
            .upgrade()
            .map(|w| w.read(cx).app_state().fs.clone())
        else {
            return;
        };
        update_settings_file(fs, cx, move |settings, _| {
            settings.dashboard_panel.get_or_insert_default().dock = Some(position.into());
        });
    }

    fn default_size(&self, _window: &Window, cx: &App) -> Pixels {
        DashboardSettings::get_global(cx).default_width
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        DashboardSettings::get_global(cx)
            .button
            .then_some(IconName::AudioOn)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Dashboard")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        DashboardSettings::get_global(cx).starts_open
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_automation_run_status_equality_and_clone() {
        let gathering = AutomationRunStatus::GatheringContext;
        let gathering_copy = gathering.clone();
        let failed = AutomationRunStatus::Failed("boom".into());
        let failed_copy = failed.clone();
        assert_eq!(gathering, gathering_copy);
        assert_eq!(failed, failed_copy);
        assert_ne!(gathering, failed);
    }

    #[test]
    fn test_reentry_guard_pattern_only_matches_gathering() {
        let gathering = Some(AutomationRunStatus::GatheringContext);
        let failed = Some(AutomationRunStatus::Failed("x".into()));
        let idle: Option<AutomationRunStatus> = None;

        assert!(matches!(
            gathering.as_ref(),
            Some(AutomationRunStatus::GatheringContext)
        ));
        assert!(!matches!(
            failed.as_ref(),
            Some(AutomationRunStatus::GatheringContext)
        ));
        assert!(!matches!(
            idle.as_ref(),
            Some(AutomationRunStatus::GatheringContext)
        ));
    }

    #[test]
    fn test_automation_status_retain_drops_stale_ids() {
        let mut status: HashMap<String, AutomationRunStatus> = HashMap::new();
        status.insert("live-1".to_string(), AutomationRunStatus::GatheringContext);
        status.insert(
            "stale".to_string(),
            AutomationRunStatus::Failed("err".into()),
        );
        status.insert("live-2".to_string(), AutomationRunStatus::GatheringContext);

        let live_ids: HashSet<&str> = ["live-1", "live-2"].into_iter().collect();
        status.retain(|id, _| live_ids.contains(id.as_str()));

        assert_eq!(status.len(), 2);
        assert!(status.contains_key("live-1"));
        assert!(status.contains_key("live-2"));
        assert!(!status.contains_key("stale"));
    }

    #[test]
    fn test_schedule_config_deserialize_default() {
        let toml_str = r#"
id = "test"
label = "Test"
description = "Test automation"
icon = "sparkle"
prompt = "Do something"
"#;
        let entry: dcfg::AutomationEntry = toml::from_str(toml_str).unwrap();
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
        let entry: dcfg::AutomationEntry = toml::from_str(toml_str).unwrap();
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
        let entry: dcfg::AutomationEntry = toml::from_str(toml_str).unwrap();
        let chain = entry.chain.unwrap();
        assert_eq!(chain.triggers, vec!["review", "deploy"]);
    }

    #[test]
    fn test_collect_step_groups_sequential() {
        let steps = vec![
            dcfg::PipelineStep {
                automation: Some("a".into()),
                tool: None,
                group: None,
            },
            dcfg::PipelineStep {
                automation: Some("b".into()),
                tool: None,
                group: None,
            },
            dcfg::PipelineStep {
                automation: Some("c".into()),
                tool: None,
                group: None,
            },
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
            dcfg::PipelineStep {
                automation: Some("a".into()),
                tool: None,
                group: Some(1),
            },
            dcfg::PipelineStep {
                automation: Some("b".into()),
                tool: None,
                group: Some(1),
            },
            dcfg::PipelineStep {
                automation: Some("c".into()),
                tool: None,
                group: Some(1),
            },
        ];
        let groups = collect_step_groups(&steps);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn test_collect_step_groups_mixed() {
        let steps = vec![
            dcfg::PipelineStep {
                automation: Some("first".into()),
                tool: None,
                group: None,
            },
            dcfg::PipelineStep {
                automation: Some("second".into()),
                tool: None,
                group: None,
            },
            dcfg::PipelineStep {
                automation: Some("par-a".into()),
                tool: None,
                group: Some(3),
            },
            dcfg::PipelineStep {
                automation: Some("par-b".into()),
                tool: None,
                group: Some(3),
            },
            dcfg::PipelineStep {
                tool: Some("launcher".into()),
                automation: None,
                group: None,
            },
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
            dcfg::PipelineStep {
                automation: Some("a".into()),
                tool: None,
                group: Some(1),
            },
            dcfg::PipelineStep {
                automation: Some("middle".into()),
                tool: None,
                group: None,
            },
            dcfg::PipelineStep {
                automation: Some("b".into()),
                tool: None,
                group: Some(1),
            },
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
        let entry = dcfg::load_single_automation(&path, tmp.path())?;
        assert_eq!(entry.steps.len(), 2);

        // Modify steps via TOML writer
        let new_steps = vec![
            dcfg::PipelineStep {
                automation: Some("new-first".into()),
                tool: None,
                group: None,
            },
            dcfg::PipelineStep {
                tool: Some("my-tool".into()),
                automation: None,
                group: Some(2),
            },
            dcfg::PipelineStep {
                automation: Some("par-auto".into()),
                tool: None,
                group: Some(2),
            },
        ];

        dcfg::edit::write_pipeline_steps(&path, &new_steps)?;

        // Re-parse and verify
        let entry = dcfg::load_single_automation(&path, tmp.path())?;
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

    #[test]
    fn test_effective_active_worktree_id_returns_match_on_starts_with() {
        let a = Path::new("/proj/A");
        let b = Path::new("/proj/B");
        let repo = Path::new("/proj/A/sub/repo");
        let id_a = WorktreeId::from_usize(1);
        let id_b = WorktreeId::from_usize(2);

        let result = effective_active_worktree_id(
            Some(repo),
            [(id_a, a), (id_b, b)],
        );
        assert_eq!(result, Some(id_a));
    }

    #[test]
    fn test_effective_active_worktree_id_returns_match_on_exact_equal() {
        let a = Path::new("/proj/A");
        let b = Path::new("/proj/B");
        let id_a = WorktreeId::from_usize(1);
        let id_b = WorktreeId::from_usize(2);

        let result = effective_active_worktree_id(
            Some(b),
            [(id_a, a), (id_b, b)],
        );
        assert_eq!(result, Some(id_b));
    }

    #[test]
    fn test_effective_active_worktree_id_falls_back_when_no_repo_match() {
        let a = Path::new("/proj/A");
        let b = Path::new("/proj/B");
        let unrelated = Path::new("/elsewhere/repo");
        let id_a = WorktreeId::from_usize(1);
        let id_b = WorktreeId::from_usize(2);

        let result = effective_active_worktree_id(
            Some(unrelated),
            [(id_a, a), (id_b, b)],
        );
        assert_eq!(result, Some(id_a));
    }

    #[test]
    fn test_effective_active_worktree_id_falls_back_when_active_repo_is_none() {
        let a = Path::new("/proj/A");
        let b = Path::new("/proj/B");
        let id_a = WorktreeId::from_usize(1);
        let id_b = WorktreeId::from_usize(2);

        let result = effective_active_worktree_id(
            None,
            [(id_a, a), (id_b, b)],
        );
        assert_eq!(result, Some(id_a));
    }

    #[test]
    fn test_effective_active_worktree_id_returns_none_when_no_visible() {
        let visible: [(WorktreeId, &Path); 0] = [];
        let result = effective_active_worktree_id(Some(Path::new("/proj/A")), visible);
        assert_eq!(result, None);

        let visible2: [(WorktreeId, &Path); 0] = [];
        let result_no_repo = effective_active_worktree_id(None, visible2);
        assert_eq!(result_no_repo, None);
    }
}

// ---------------------------------------------------------------------------
// PostProd Rules — InlineAssistDelegate implementation
// ---------------------------------------------------------------------------

struct PostProdInlineAssist {
    workspace: WeakEntity<Workspace>,
}

impl postprod_rules::InlineAssistDelegate for PostProdInlineAssist {
    fn assist(
        &self,
        prompt_editor: &Entity<Editor>,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<postprod_rules::PostProdRules>,
    ) {
        InlineAssistant::update_global(cx, |assistant, cx| {
            let Some(workspace) = self.workspace.upgrade() else {
                return;
            };
            let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
                return;
            };
            let project = workspace.read(cx).project().downgrade();
            let panel = panel.read(cx);
            let thread_store = panel.thread_store().clone();
            assistant.assist(
                prompt_editor,
                self.workspace.clone(),
                project,
                thread_store,
                None,
                initial_prompt,
                window,
                cx,
            );
        })
    }

    fn focus_agent_panel(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        workspace.focus_panel::<AgentPanel>(window, cx).is_some()
    }
}
