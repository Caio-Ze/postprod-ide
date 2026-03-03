use agent_ui::AgentPanel;
use editor::{Editor, EditorEvent};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager as NativeHotKeyManager,
    hotkey::{Code, HotKey, Modifiers as GHModifiers},
};
use gpui::{
    Action, App, AsyncApp, ClipboardItem, Context, Corner, DismissEvent, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, IntoElement, Keystroke, KeystrokeEvent, MouseButton,
    ParentElement, PathPromptOptions, Render, ScrollHandle, SharedString, Styled, Subscription,
    WeakEntity, Window, actions,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use ui::{
    Button, ButtonLike, ButtonStyle, Callout, ContextMenu, Disclosure, Divider, DividerColor,
    DropdownMenu, DropdownStyle, Headline, HeadlineSize, Icon, IconButton, IconName, IconSize,
    Indicator, Label, LabelSize, Modal, ModalFooter, ModalHeader, PopoverMenu, Section,
    ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip, WithScrollbar as _,
    prelude::*,
};
use workspace::{
    DraggedSelection, ModalView, MultiWorkspace, OpenOptions, Pane, ProToolsSessionName, Toast,
    Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
    with_active_or_new_workspace,
};

use util::ResultExt as _;

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// Marker type for the context-launcher failure toast notification ID.
struct ContextLauncherToast;

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
                        let dashboard = Dashboard::new(workspace, cx);
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
}

/// Ensure a Dashboard tab exists in the workspace. Idempotent — scans all
/// panes before creating a new one.
pub fn ensure_dashboard(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Check all panes for an existing Dashboard
    for pane in workspace.panes() {
        let found = pane
            .read(cx)
            .items()
            .any(|item| item.downcast::<Dashboard>().is_some());
        if found {
            return;
        }
    }

    let dashboard = Dashboard::new(workspace, cx);
    workspace.add_item_to_center(Box::new(dashboard), window, cx);
    // Pin the dashboard so it stays as the first tab
    workspace.active_pane().update(cx, |pane, _cx| {
        pane.set_pinned_count(pane.pinned_count() + 1);
    });
    set_pane_drop_predicate(workspace.active_pane(), workspace, cx);
}

// ---------------------------------------------------------------------------
// TOML-driven tool registry
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ToolTier {
    Featured,
    Standard,
    Compact,
}

impl ToolTier {
    fn label(self) -> &'static str {
        match self {
            Self::Featured => "FEATURED TOOLS",
            Self::Standard => "TOOLS",
            Self::Compact => "AGENT TOOLS",
        }
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ToolSource {
    Runtime,
    Agent,
}

#[derive(Clone, Copy)]
enum FolderTarget {
    Active,
    Destination,
}

#[derive(Deserialize, Serialize, Clone)]
struct ParamEntry {
    key: String,
    label: String,
    #[serde(default)]
    placeholder: String,
    #[serde(default)]
    default: String,
    #[serde(default = "default_param_type")]
    param_type: ParamType,
    #[serde(default)]
    options: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ParamType {
    Text,
    Path,
    Select,
}

fn default_param_type() -> ParamType {
    ParamType::Text
}

#[derive(Deserialize, Clone)]
struct ToolEntry {
    id: String,
    label: String,
    description: String,
    icon: String,
    binary: String,
    #[serde(default)]
    cwd: String,
    source: ToolSource,
    tier: ToolTier,
    #[serde(default)]
    needs_session: bool,
    #[serde(default)]
    extra_args: Vec<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default, rename = "param")]
    params: Vec<ParamEntry>,
}

#[derive(Deserialize)]
struct SingleToolFile {
    tool: ToolEntry,
}

fn icon_for_tool(name: &str) -> IconName {
    match name {
        "audio_on" => IconName::AudioOn,
        "audio_off" => IconName::AudioOff,
        "play_filled" => IconName::PlayFilled,
        "sparkle" => IconName::Sparkle,
        "mic" => IconName::Mic,
        "check" => IconName::Check,
        "forward_arrow" => IconName::ForwardArrow,
        "list_tree" => IconName::ListTree,
        "tool_terminal" => IconName::ToolTerminal,
        "replace" => IconName::Replace,
        "trash" => IconName::Trash,
        "file_doc" => IconName::FileDoc,
        "file_rust" => IconName::FileRust,
        "select_all" => IconName::SelectAll,
        "arrow_up_right" => IconName::ArrowUpRight,
        "minimize" => IconName::Minimize,
        "folder" => IconName::Folder,
        "folder_open" => IconName::FolderOpen,
        "maximize" => IconName::Maximize,
        _ => IconName::Sparkle,
    }
}

fn load_single_tool(path: &Path) -> Result<ToolEntry, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: SingleToolFile = toml::from_str(&content).map_err(|e| {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        format!("{filename}: {e}")
    })?;
    Ok(file.tool)
}

fn load_tools_registry() -> (Vec<ToolEntry>, Option<String>) {
    let dir = tools_config_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (Vec::new(), None);
    };

    let mut tools = Vec::new();
    let mut errors = Vec::new();

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    for path in paths {
        match load_single_tool(&path) {
            Ok(entry) => tools.push(entry),
            Err(e) => {
                log::error!("config: {e}");
                errors.push(e);
            }
        }
    }

    let error = if errors.is_empty() {
        None
    } else {
        Some(errors.join("; "))
    };
    (tools, error)
}

// ---------------------------------------------------------------------------
// Automations — each automation is a separate .toml in config/automations/
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
struct AutomationEntry {
    id: String,
    label: String,
    description: String,
    icon: String,
    prompt: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default, rename = "param")]
    params: Vec<ParamEntry>,
}

fn automations_dir() -> PathBuf {
    config_dir().join("automations")
}

fn load_single_automation(path: &Path) -> Result<AutomationEntry, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str::<AutomationEntry>(&content).map_err(|e| {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        format!("{filename}: {e}")
    })
}

fn load_automations_registry() -> (Vec<AutomationEntry>, Option<String>) {
    let dir = automations_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (Vec::new(), Some(format!("cannot read {}", dir.display())));
    };

    let mut automations = Vec::new();
    let mut errors = Vec::new();

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    for path in paths {
        match load_single_automation(&path) {
            Ok(entry) => automations.push(entry),
            Err(e) => {
                log::error!("config: {e}");
                errors.push(e);
            }
        }
    }

    let error = if errors.is_empty() {
        None
    } else {
        Some(errors.join("; "))
    };
    (automations, error)
}

fn icon_for_automation(name: &str) -> IconName {
    match name {
        "play" => IconName::PlayFilled,
        "zap" => IconName::PlayFilled,
        "mic" => IconName::Mic,
        "folder" => IconName::Folder,
        "audio" => IconName::AudioOn,
        "sparkle" => IconName::Sparkle,
        "replace" => IconName::Replace,
        "arrow_up_right" => IconName::ArrowUpRight,
        _ => IconName::Sparkle,
    }
}

// ---------------------------------------------------------------------------
// Agent backends — loaded from TOML at runtime
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
struct BackendEntry {
    id: String,
    label: String,
    command: String,
    #[serde(default)]
    flags: String,
    #[serde(default)]
    prompt_flag: String,
}

#[derive(Deserialize, Clone)]
struct AgentEntry {
    id: String,
    label: String,
    command: String,
    #[serde(default)]
    flags: String,
}

fn builtin_agent_launchers() -> Vec<AgentEntry> {
    vec![
        AgentEntry {
            id: "claude".to_string(),
            label: "Open Claude".to_string(),
            command: "claude".to_string(),
            flags: String::new(),
        },
        AgentEntry {
            id: "gemini".to_string(),
            label: "Open Gemini".to_string(),
            command: "gemini".to_string(),
            flags: String::new(),
        },
    ]
}

#[derive(Deserialize)]
struct AgentsFile {
    #[serde(default)]
    backend: Vec<BackendEntry>,
    #[serde(default)]
    agent: Vec<AgentEntry>,
}

fn load_toml_agents(path: &Path) -> (Vec<BackendEntry>, Vec<AgentEntry>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (Vec::new(), builtin_agent_launchers(), None);
    };
    match toml::from_str::<AgentsFile>(&content) {
        Ok(file) => {
            let agents = if file.agent.is_empty() {
                builtin_agent_launchers()
            } else {
                file.agent
            };
            (file.backend, agents, None)
        }
        Err(e) => {
            let filename = path.file_name().unwrap_or_default().to_string_lossy();
            let err = format!("{filename}: {e}");
            log::error!("config: {err}");
            (Vec::new(), builtin_agent_launchers(), Some(err))
        }
    }
}

fn load_agents_config() -> (Vec<BackendEntry>, Vec<AgentEntry>, Option<String>) {
    load_toml_agents(&agents_toml_path())
}

fn ensure_config_extracted(_cx: &App) {
    ensure_workspace_dirs();
}

// ---------------------------------------------------------------------------
// Global Hotkeys — system-wide shortcuts via CGEventTap
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
struct GlobalHotkeyEntry {
    keystroke: String,
    tool_id: String,
}

#[derive(Deserialize)]
struct GlobalHotkeysFile {
    #[serde(default)]
    hotkey: Vec<GlobalHotkeyEntry>,
}

fn global_hotkeys_toml_path() -> PathBuf {
    paths::config_dir().join("global-hotkeys.toml")
}

fn ensure_global_hotkeys_config() {
    let path = global_hotkeys_toml_path();
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).log_err();
    }
    let header = "\
# PostProd Tools — Global Hotkeys
# These shortcuts work even when PostProd Tools is not focused.
# Requires \"Input Monitoring\" permission in System Settings.
#
# [[hotkey]]
# keystroke = \"ctrl-alt-0\"
# tool_id = \"bounceAll\"
";
    std::fs::write(&path, header).log_err();
}

fn load_global_hotkeys_config() -> Vec<GlobalHotkeyEntry> {
    let path = global_hotkeys_toml_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match toml::from_str::<GlobalHotkeysFile>(&content) {
        Ok(file) => file.hotkey,
        Err(e) => {
            log::warn!("global-hotkeys.toml: parse error: {e}");
            Vec::new()
        }
    }
}

fn gpui_key_to_code(key: &str) -> Option<Code> {
    Some(match key {
        "a" => Code::KeyA,
        "b" => Code::KeyB,
        "c" => Code::KeyC,
        "d" => Code::KeyD,
        "e" => Code::KeyE,
        "f" => Code::KeyF,
        "g" => Code::KeyG,
        "h" => Code::KeyH,
        "i" => Code::KeyI,
        "j" => Code::KeyJ,
        "k" => Code::KeyK,
        "l" => Code::KeyL,
        "m" => Code::KeyM,
        "n" => Code::KeyN,
        "o" => Code::KeyO,
        "p" => Code::KeyP,
        "q" => Code::KeyQ,
        "r" => Code::KeyR,
        "s" => Code::KeyS,
        "t" => Code::KeyT,
        "u" => Code::KeyU,
        "v" => Code::KeyV,
        "w" => Code::KeyW,
        "x" => Code::KeyX,
        "y" => Code::KeyY,
        "z" => Code::KeyZ,
        "0" => Code::Digit0,
        "1" => Code::Digit1,
        "2" => Code::Digit2,
        "3" => Code::Digit3,
        "4" => Code::Digit4,
        "5" => Code::Digit5,
        "6" => Code::Digit6,
        "7" => Code::Digit7,
        "8" => Code::Digit8,
        "9" => Code::Digit9,
        "f1" => Code::F1,
        "f2" => Code::F2,
        "f3" => Code::F3,
        "f4" => Code::F4,
        "f5" => Code::F5,
        "f6" => Code::F6,
        "f7" => Code::F7,
        "f8" => Code::F8,
        "f9" => Code::F9,
        "f10" => Code::F10,
        "f11" => Code::F11,
        "f12" => Code::F12,
        "space" => Code::Space,
        "enter" => Code::Enter,
        "tab" => Code::Tab,
        "escape" => Code::Escape,
        "backspace" => Code::Backspace,
        "delete" => Code::Delete,
        "up" => Code::ArrowUp,
        "down" => Code::ArrowDown,
        "left" => Code::ArrowLeft,
        "right" => Code::ArrowRight,
        "home" => Code::Home,
        "end" => Code::End,
        "pageup" => Code::PageUp,
        "pagedown" => Code::PageDown,
        "-" => Code::Minus,
        "=" => Code::Equal,
        "[" => Code::BracketLeft,
        "]" => Code::BracketRight,
        "\\" => Code::Backslash,
        ";" => Code::Semicolon,
        "'" => Code::Quote,
        "," => Code::Comma,
        "." => Code::Period,
        "/" => Code::Slash,
        "`" => Code::Backquote,
        _ => return None,
    })
}

fn parse_global_hotkey(keystroke_str: &str) -> Option<HotKey> {
    let keystroke = Keystroke::parse(keystroke_str).ok()?;
    let mut modifiers = GHModifiers::empty();
    if keystroke.modifiers.control {
        modifiers |= GHModifiers::CONTROL;
    }
    if keystroke.modifiers.alt {
        modifiers |= GHModifiers::ALT;
    }
    if keystroke.modifiers.shift {
        modifiers |= GHModifiers::SHIFT;
    }
    if keystroke.modifiers.platform {
        modifiers |= GHModifiers::SUPER;
    }
    let code = gpui_key_to_code(&keystroke.key)?;
    Some(HotKey::new(
        if modifiers.is_empty() {
            None
        } else {
            Some(modifiers)
        },
        code,
    ))
}

fn keystroke_to_display(keystroke_str: &str) -> String {
    let Ok(keystroke) = Keystroke::parse(keystroke_str) else {
        return keystroke_str.to_string();
    };
    let mut parts = Vec::new();
    if keystroke.modifiers.control {
        parts.push("\u{2303}"); // ⌃
    }
    if keystroke.modifiers.alt {
        parts.push("\u{2325}"); // ⌥
    }
    if keystroke.modifiers.shift {
        parts.push("\u{21E7}"); // ⇧
    }
    if keystroke.modifiers.platform {
        parts.push("\u{2318}"); // ⌘
    }
    parts.push(&keystroke.key);
    parts.join(" ")
}

pub struct GlobalHotkeyManager {
    native_manager: NativeHotKeyManager,
    hotkey_map: HashMap<u32, String>,
    registered_hotkeys: Vec<HotKey>,
    last_config_content: String,
    _poll_task: gpui::Task<()>,
    _watch_task: gpui::Task<()>,
}

struct GlobalHotkeyManagerHandle(Entity<GlobalHotkeyManager>);

impl gpui::Global for GlobalHotkeyManagerHandle {}

impl GlobalHotkeyManager {
    fn register_hotkeys_from_config(&mut self) {
        // Unregister old hotkeys
        for hotkey in &self.registered_hotkeys {
            self.native_manager.unregister(*hotkey).log_err();
        }
        self.registered_hotkeys.clear();
        self.hotkey_map.clear();

        let entries = load_global_hotkeys_config();
        for entry in entries {
            let Some(hotkey) = parse_global_hotkey(&entry.keystroke) else {
                log::warn!(
                    "global hotkey: could not parse keystroke '{}'",
                    entry.keystroke
                );
                continue;
            };
            match self.native_manager.register(hotkey) {
                Ok(()) => {
                    log::info!(
                        "global hotkey: registered {} -> {}",
                        entry.keystroke,
                        entry.tool_id
                    );
                    self.hotkey_map.insert(hotkey.id(), entry.tool_id.clone());
                    self.registered_hotkeys.push(hotkey);
                }
                Err(e) => {
                    log::warn!(
                        "global hotkey: failed to register '{}': {e}",
                        entry.keystroke
                    );
                }
            }
        }
    }
}

pub fn init_global_hotkeys(cx: &mut App) {
    ensure_global_hotkeys_config();

    let native_manager = match NativeHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            log::warn!(
                "global hotkeys: could not initialize (Input Monitoring permission needed?): {e}"
            );
            return;
        }
    };

    let receiver = GlobalHotKeyEvent::receiver().clone();

    let entity: Entity<GlobalHotkeyManager> = cx.new(|cx| {
        let poll_task = cx.spawn({
            let receiver = receiver.clone();
            async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;

                    while let Ok(event) = receiver.try_recv() {
                        if event.state() == global_hotkey::HotKeyState::Released {
                            continue;
                        }
                        let hotkey_id = event.id();
                        let tool_id = this
                            .update(cx, |manager: &mut GlobalHotkeyManager, _cx| {
                                manager.hotkey_map.get(&hotkey_id).cloned()
                            })
                            .ok()
                            .flatten();

                        if let Some(tool_id) = tool_id {
                            log::info!("global hotkey: triggered tool '{tool_id}'");
                            cx.update(|cx| {
                                dispatch_global_tool(&tool_id, cx);
                            });
                        }
                    }
                }
            }
        });

        let watch_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;

                let config_path = global_hotkeys_toml_path();
                let new_content = std::fs::read_to_string(&config_path).unwrap_or_default();

                this.update(cx, |manager: &mut GlobalHotkeyManager, _cx| {
                    if new_content != manager.last_config_content {
                        log::info!("global hotkey: config changed, re-registering");
                        manager.last_config_content = new_content;
                        manager.register_hotkeys_from_config();
                    }
                }).log_err();
            }
        });

        let mut manager = GlobalHotkeyManager {
            native_manager,
            hotkey_map: HashMap::new(),
            registered_hotkeys: Vec::new(),
            last_config_content: String::new(),
            _poll_task: poll_task,
            _watch_task: watch_task,
        };
        manager.register_hotkeys_from_config();
        manager
    });

    cx.set_global(GlobalHotkeyManagerHandle(entity));
}

fn dispatch_global_tool(tool_id: &str, cx: &mut App) {
    let multi_workspace_handle = cx
        .active_window()
        .and_then(|w| w.downcast::<MultiWorkspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|w| w.downcast::<MultiWorkspace>())
        });

    if let Some(multi_workspace) = multi_workspace_handle {
        let tool_id = tool_id.to_string();
        multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                let workspace_entity = multi_workspace.workspace().clone();
                workspace_entity.update(cx, |workspace, cx| {
                    ensure_dashboard(workspace, window, cx);

                    let dashboard_data = workspace
                        .panes()
                        .iter()
                        .flat_map(|pane| pane.read(cx).items())
                        .find_map(|item| item.downcast::<Dashboard>())
                        .map(|d| {
                            let d = d.read(cx);
                            let tool_param_values =
                                d.param_values.get(&tool_id).cloned().unwrap_or_default();
                            (
                                d.tools.iter().find(|t| t.id == tool_id).cloned(),
                                d.runtime_path.clone(),
                                d.agent_tools_path.clone(),
                                d.session_path.clone(),
                                d.active_folder.clone(),
                                d.background_tools.contains(&tool_id),
                                tool_param_values,
                            )
                        });

                    if let Some((
                        Some(tool),
                        runtime_path,
                        agent_tools_path,
                        session_path,
                        active_folder,
                        is_background,
                        tool_param_values,
                    )) = dashboard_data
                    {
                        if is_background {
                            Dashboard::spawn_tool_background(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &session_path,
                                &active_folder,
                                &tool_param_values,
                                cx,
                            );
                        } else {
                            cx.activate(true);
                            Dashboard::spawn_tool_entry(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &session_path,
                                &active_folder,
                                &tool_param_values,
                                workspace,
                                window,
                                cx,
                            );
                        }
                    }
                });
            })
            .log_err();
    }
}

fn save_global_hotkey(keystroke_str: &str, tool_id: &str) {
    let path = global_hotkeys_toml_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    // Remove any existing entry for this tool_id to avoid duplicates
    let lines: Vec<&str> = existing.lines().collect();
    let mut filtered = Vec::new();
    let mut skip_next_lines = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "[[hotkey]]" {
            // Check if the next 2 lines contain this tool_id
            let next_lines: String = lines
                .get(i + 1..std::cmp::min(i + 3, lines.len()))
                .unwrap_or_default()
                .join("\n");
            if next_lines.contains(&format!("tool_id = \"{tool_id}\"")) {
                skip_next_lines = true;
                continue;
            }
        }
        if skip_next_lines {
            if line.trim().starts_with("keystroke") || line.trim().starts_with("tool_id") {
                continue;
            }
            if line.trim().is_empty() {
                skip_next_lines = false;
                continue;
            }
            skip_next_lines = false;
        }
        filtered.push(*line);
    }

    let mut content = filtered.join("\n");
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!(
        "\n[[hotkey]]\nkeystroke = \"{keystroke_str}\"\ntool_id = \"{tool_id}\"\n"
    ));

    std::fs::write(&path, content).log_err();
}

// ---------------------------------------------------------------------------
// Global Shortcut Modal — keystroke capture UI
// ---------------------------------------------------------------------------

struct GlobalShortcutModal {
    tool_id: String,
    tool_label: String,
    captured_keystroke: Option<String>,
    focus_handle: FocusHandle,
    _intercept_subscription: Option<Subscription>,
}

impl GlobalShortcutModal {
    fn new(
        tool_id: String,
        tool_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let listener = cx.listener(|this, event: &KeystrokeEvent, _window, cx| {
            let keystroke = &event.keystroke;
            // Only capture if at least one modifier is pressed (ignore bare modifiers)
            if (keystroke.modifiers.control
                || keystroke.modifiers.alt
                || keystroke.modifiers.shift
                || keystroke.modifiers.platform)
                && !keystroke.key.is_empty()
                && !matches!(
                    keystroke.key.as_str(),
                    "control" | "alt" | "shift" | "cmd" | "meta"
                )
            {
                let mut parts = Vec::new();
                if keystroke.modifiers.control {
                    parts.push("ctrl");
                }
                if keystroke.modifiers.alt {
                    parts.push("alt");
                }
                if keystroke.modifiers.shift {
                    parts.push("shift");
                }
                if keystroke.modifiers.platform {
                    parts.push("cmd");
                }
                parts.push(&keystroke.key);
                this.captured_keystroke = Some(parts.join("-"));
                cx.notify();
            }
        });
        let intercept_sub = cx.intercept_keystrokes(listener);

        Self {
            tool_id,
            tool_label,
            captured_keystroke: None,
            focus_handle,
            _intercept_subscription: Some(intercept_sub),
        }
    }

    fn save_and_dismiss(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(keystroke_str) = &self.captured_keystroke {
            save_global_hotkey(keystroke_str, &self.tool_id);
            log::info!("global hotkey: saved {} -> {}", keystroke_str, self.tool_id);

            // Trigger immediate re-registration
            if let Some(handle) = cx.try_global::<GlobalHotkeyManagerHandle>() {
                let manager = handle.0.clone();
                manager.update(cx, |m, _cx| m.register_hotkeys_from_config());
            }
        }
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for GlobalShortcutModal {}

impl Focusable for GlobalShortcutModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for GlobalShortcutModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for GlobalShortcutModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let display_text = match &self.captured_keystroke {
            Some(ks) => keystroke_to_display(ks),
            None => "Waiting...".to_string(),
        };

        let has_capture = self.captured_keystroke.is_some();
        let tool_label = self.tool_label.clone();

        Modal::new("global-shortcut-modal", None)
            .header(ModalHeader::new().headline(format!("Global Shortcut: {}", tool_label)))
            .section(
                Section::new().child(
                    v_flex()
                        .gap_3()
                        .p_4()
                        .child(
                            Label::new("Press the key combination:")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .child(
                            div()
                                .p_4()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .bg(cx.theme().colors().editor_background)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(Label::new(display_text).size(LabelSize::Large).color(
                                    if has_capture {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    },
                                )),
                        )
                        .child(
                            Label::new(
                                "Use Ctrl, Option, Shift, or Cmd with a key. Escape to cancel.",
                            )
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                        ),
                ),
            )
            .footer(
                ModalFooter::new().end_slot(
                    h_flex()
                        .gap_2()
                        .child(Button::new("cancel", "Cancel").on_click(cx.listener(
                            |_this, _, _window, cx| {
                                cx.emit(DismissEvent);
                            },
                        )))
                        .child(
                            Button::new("save", "Save")
                                .style(ButtonStyle::Filled)
                                .disabled(!has_capture)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_and_dismiss(window, cx);
                                })),
                        ),
                ),
            )
    }
}

// ---------------------------------------------------------------------------
// Delivery status
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct DeliveryStatus {
    tv_count: usize,
    net_count: usize,
    spot_count: usize,
    mp3_count: usize,
    warnings: Vec<String>,
}

fn suite_root() -> PathBuf {
    if let Ok(p) = std::env::var("POSTPROD_WORKSPACE") {
        return PathBuf::from(p);
    }
    util::paths::home_dir().join("PostProd_IDE")
}

fn config_dir() -> PathBuf {
    suite_root().join("config")
}

fn state_dir() -> PathBuf {
    config_dir().join(".state")
}

fn tools_dir() -> PathBuf {
    suite_root().join("tools")
}

fn agent_tools_dir() -> PathBuf {
    tools_dir().join("agent")
}

fn runtime_tools_dir() -> PathBuf {
    tools_dir().join("runtime")
}

fn tools_config_dir() -> PathBuf {
    config_dir().join("tools")
}

fn agents_toml_path() -> PathBuf {
    config_dir().join("AGENTS.toml")
}

fn ensure_workspace_dirs() {
    for dir in [
        config_dir().join("tools"),
        automations_dir(),
        agent_tools_dir(),
        runtime_tools_dir(),
        suite_root().join("deliveries"),
        state_dir(),
    ] {
        if !dir.exists() {
            std::fs::create_dir_all(&dir).log_err();
        }
    }

    // One-time migration: move state files from old locations into config/.state/
    let migrations = [
        (suite_root().join(".active_folder"), state_dir().join("active_folder")),
        (suite_root().join(".recent_folders"), state_dir().join("recent_folders")),
        (suite_root().join(".destination_folder"), state_dir().join("destination_folder")),
        (suite_root().join(".recent_destinations"), state_dir().join("recent_destinations")),
        (config_dir().join(".background_tools"), state_dir().join("background_tools")),
        (config_dir().join(".collapsed_sections"), state_dir().join("collapsed_sections")),
        (config_dir().join(".param_values.toml"), state_dir().join("param_values.toml")),
    ];
    for (old, new) in &migrations {
        if old.exists() && !new.exists() {
            std::fs::rename(old, new).log_err();
        }
    }
}

fn scan_delivery_folder() -> DeliveryStatus {
    let dir = suite_root().join("deliveries");
    let mut status = DeliveryStatus::default();

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return status;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();

        if path.is_dir() {
            // Scan subdirectories named TV/, NET/, SPOT/
            let subdir_name = name.as_str();
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                let count = sub_entries.flatten().filter(|e| e.path().is_file()).count();
                match subdir_name {
                    "tv" => status.tv_count += count,
                    "net" => status.net_count += count,
                    "spot" => status.spot_count += count,
                    _ => {}
                }
            }
        } else if path.is_file() {
            // Classify by filename pattern
            if name.contains("_tv") {
                status.tv_count += 1;
            }
            if name.contains("_net") {
                status.net_count += 1;
            }
            if name.contains("_spot") {
                status.spot_count += 1;
            }
            if name.ends_with(".mp3") {
                status.mp3_count += 1;
            }
        }
    }

    // Generate warnings
    let has_any = status.tv_count > 0
        || status.net_count > 0
        || status.spot_count > 0
        || status.mp3_count > 0;

    if has_any {
        if status.tv_count == 0 {
            status.warnings.push("Missing: TV files".to_string());
        }
        if status.net_count == 0 {
            status.warnings.push("Missing: NET files".to_string());
        }
        if status.spot_count == 0 {
            status.warnings.push("Missing: SPOT files".to_string());
        }
        if status.tv_count > 0 && status.net_count > 0 && status.tv_count != status.net_count {
            status.warnings.push(format!(
                "TV ({}) != NET ({})",
                status.tv_count, status.net_count
            ));
        }
    }

    status
}

// ---------------------------------------------------------------------------
// Active folder helpers
// ---------------------------------------------------------------------------

fn active_folder_file() -> PathBuf {
    state_dir().join("active_folder")
}

fn read_active_folder() -> Option<PathBuf> {
    std::fs::read_to_string(active_folder_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| {
            let default = suite_root();
            if default.is_dir() {
                Some(default)
            } else {
                None
            }
        })
}

fn write_active_folder(path: &Path) {
    std::fs::write(active_folder_file(), path.to_string_lossy().as_bytes()).log_err();
    add_to_recent_folders(path);
}

// ---------------------------------------------------------------------------
// Recent folders helpers
// ---------------------------------------------------------------------------

fn recent_folders_file() -> PathBuf {
    state_dir().join("recent_folders")
}

fn read_recent_folders() -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(recent_folders_file()) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .take(10)
        .collect()
}

fn add_to_recent_folders(path: &Path) {
    let mut recent = read_recent_folders();
    recent.retain(|p| p != path);
    recent.insert(0, path.to_path_buf());
    recent.truncate(10);
    let content = recent
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(recent_folders_file(), content).log_err();
}

// ---------------------------------------------------------------------------
// Destination folder helpers
// ---------------------------------------------------------------------------

fn destination_folder_file() -> PathBuf {
    state_dir().join("destination_folder")
}

fn recent_destinations_file() -> PathBuf {
    state_dir().join("recent_destinations")
}

fn read_destination_folder() -> Option<PathBuf> {
    std::fs::read_to_string(destination_folder_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

fn read_recent_destinations() -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(recent_destinations_file()) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .take(10)
        .collect()
}

fn write_destination_folder(path: &Path) {
    std::fs::write(destination_folder_file(), path.to_string_lossy().as_bytes()).log_err();
    add_to_destination_recent(path);
}

fn add_to_destination_recent(path: &Path) {
    let mut recent = read_recent_destinations();
    recent.retain(|p| p != path);
    recent.insert(0, path.to_path_buf());
    recent.truncate(10);
    let content = recent
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(recent_destinations_file(), content).log_err();
}

// ---------------------------------------------------------------------------
// Background tools persistence
// ---------------------------------------------------------------------------

fn background_tools_file() -> PathBuf {
    state_dir().join("background_tools")
}

fn read_background_tools() -> HashSet<String> {
    let Ok(content) = std::fs::read_to_string(background_tools_file()) else {
        return HashSet::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

fn write_background_tools(set: &HashSet<String>) {
    let mut entries: Vec<_> = set.iter().cloned().collect();
    entries.sort();
    let content = entries.join("\n");
    std::fs::write(background_tools_file(), content).log_err();
}

// ---------------------------------------------------------------------------
// Collapsed sections persistence
// ---------------------------------------------------------------------------

fn collapsed_sections_file() -> PathBuf {
    state_dir().join("collapsed_sections")
}

fn read_collapsed_sections() -> HashSet<String> {
    let Ok(content) = std::fs::read_to_string(collapsed_sections_file()) else {
        return HashSet::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

fn write_collapsed_sections(set: &HashSet<String>) {
    let mut entries: Vec<_> = set.iter().cloned().collect();
    entries.sort();
    let content = entries.join("\n");
    std::fs::write(collapsed_sections_file(), content).log_err();
}

// ---------------------------------------------------------------------------
// Param values persistence
// ---------------------------------------------------------------------------

fn param_values_file() -> PathBuf {
    state_dir().join("param_values.toml")
}

fn read_param_values() -> HashMap<String, HashMap<String, String>> {
    let Ok(content) = std::fs::read_to_string(param_values_file()) else {
        return HashMap::new();
    };
    toml::from_str(&content).unwrap_or_default()
}

fn write_param_values(values: &HashMap<String, HashMap<String, String>>) {
    if let Ok(content) = toml::to_string(values) {
        std::fs::write(param_values_file(), content).log_err();
    }
}

// ---------------------------------------------------------------------------
// Binary / runtime resolution
// ---------------------------------------------------------------------------

fn dir_has_content(dir: &Path) -> bool {
    dir.is_dir()
        && std::fs::read_dir(dir)
            .ok()
            .and_then(|mut entries| entries.next())
            .is_some()
}

fn resolve_bin(name: &str) -> String {
    let candidates = [
        util::paths::home_dir().join(".local/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }
    name.to_string()
}

/// Resolve the runtime path with priority:
/// 1. `~/PostProd_IDE/tools/runtime/` (workspace — production)
/// 2. `exe_dir/runtime/` (symlinked by build.rs — development)
/// 3. `PROTOOLS_RUNTIME_PATH` env var (explicit override)
/// 4. Workspace path as expected default
fn resolve_runtime_path() -> PathBuf {
    let workspace_runtime = runtime_tools_dir();
    if dir_has_content(&workspace_runtime) {
        return workspace_runtime;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("runtime");
            if dir_has_content(&candidate) {
                return candidate;
            }
        }
    }

    if let Ok(path) = std::env::var("PROTOOLS_RUNTIME_PATH") {
        return PathBuf::from(path);
    }

    workspace_runtime
}

/// Resolve the agent tools path with priority:
/// 1. `~/PostProd_IDE/tools/agent/` (workspace — production)
/// 2. `exe_dir/agent/` (symlinked by build.rs — development)
/// 3. `PROTOOLS_AGENT_TOOLS_PATH` env var (explicit override)
/// 4. Workspace path as expected default
fn resolve_agent_tools_path() -> PathBuf {
    let workspace_agent = agent_tools_dir();
    if dir_has_content(&workspace_agent) {
        return workspace_agent;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("agent");
            if dir_has_content(&candidate) {
                return candidate;
            }
        }
    }

    if let Ok(path) = std::env::var("PROTOOLS_AGENT_TOOLS_PATH") {
        return PathBuf::from(path);
    }

    workspace_agent
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

// ---------------------------------------------------------------------------
// Dashboard struct
// ---------------------------------------------------------------------------

pub struct Dashboard {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    runtime_path: PathBuf,
    agent_tools_path: PathBuf,
    // TOML-driven tool registry
    tools: Vec<ToolEntry>,
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
    automations: Vec<AutomationEntry>,
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
    // Expanded automation prompt previews (ephemeral)
    expanded_automations: HashSet<String>,
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
}

impl Dashboard {
    pub fn new(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        let runtime_path = resolve_runtime_path();

        let agent_tools_path = resolve_agent_tools_path();

        ensure_workspace_dirs();
        ensure_config_extracted(cx);

        let active_folder = read_active_folder();
        let recent_folders = read_recent_folders();
        let destination_folder = read_destination_folder();
        let recent_destinations = read_recent_destinations();
        let (automations, automations_error) = load_automations_registry();
        let (tools, tools_error) = load_tools_registry();
        let (backends, agent_launchers, _agents_error) = load_agents_config();
        let background_tools = read_background_tools();
        let collapsed_sections = read_collapsed_sections();
        let mut param_values = read_param_values();

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

            // Spawn automations reload task (every 30 seconds)
            let automations_reload_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(30))
                        .await;

                    let (merged, error) = cx
                        .background_executor()
                        .spawn(async { load_automations_registry() })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                            dashboard.automations = merged;
                            dashboard.automations_error = error;
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
                            cx.notify();
                        },
                    ).log_err();
                }
            });

            // Spawn tools reload task (every 30 seconds)
            let tools_reload_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(30))
                        .await;

                    let (merged, error) = cx
                        .background_executor()
                        .spawn(async { load_tools_registry() })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
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

            // Spawn agents config reload task (every 30 seconds)
            let agents_reload_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(30))
                        .await;

                    let (backends, agent_launchers, _err) = cx
                        .background_executor()
                        .spawn(async { load_agents_config() })
                        .await;

                    this.update(
                        cx,
                        |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                            dashboard.backends = backends;
                            dashboard.agent_launchers = agent_launchers;
                            cx.notify();
                        },
                    ).log_err();
                }
            });

            Self {
                workspace: workspace.weak_handle(),
                focus_handle: cx.focus_handle(),
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
                agent_backend: AgentBackend::Claude,
                backends,
                agent_launchers,
                _automations_reload_task: automations_reload_task,
                _tools_reload_task: tools_reload_task,
                _agents_reload_task: agents_reload_task,
                background_tools,
                collapsed_sections,
                expanded_automations: HashSet::new(),
                tools_error,
                automations_error,
                scroll_handle: ScrollHandle::new(),
                param_values,
                param_editors: HashMap::new(),
                _param_editor_subscriptions: Vec::new(),
                _param_write_task: None,
            }
        })
    }

    fn resolve_tool_command(
        tool: &ToolEntry,
        runtime_path: &Path,
        agent_tools_path: &Path,
        session_path: &Option<String>,
        active_folder: &Option<PathBuf>,
        tool_param_values: &HashMap<String, String>,
    ) -> (String, Vec<String>, PathBuf, HashMap<String, String>) {
        let is_agent_tool = tool.source == ToolSource::Agent;

        let (command, cwd) = if is_agent_tool {
            let cmd = agent_tools_path
                .join(&tool.binary)
                .to_string_lossy()
                .to_string();
            let work_dir = agent_tools_path.to_path_buf();
            (cmd, work_dir)
        } else {
            let cmd = runtime_path
                .join(&tool.cwd)
                .join(&tool.binary)
                .to_string_lossy()
                .to_string();
            let work_dir = if tool.tier == ToolTier::Standard && tool.source == ToolSource::Runtime
            {
                if let Some(pa) = active_folder {
                    pa.clone()
                } else {
                    runtime_path.join(&tool.cwd)
                }
            } else {
                runtime_path.join(&tool.cwd)
            };
            (cmd, work_dir)
        };

        let mut args = if is_agent_tool {
            vec!["--output-json".to_string()]
        } else {
            vec![]
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

    pub(crate) fn spawn_tool_entry(
        tool: &ToolEntry,
        runtime_path: &Path,
        agent_tools_path: &Path,
        session_path: &Option<String>,
        active_folder: &Option<PathBuf>,
        tool_param_values: &HashMap<String, String>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let (command, args, cwd, env) = Self::resolve_tool_command(
            tool,
            runtime_path,
            agent_tools_path,
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
        session_path: &Option<String>,
        active_folder: &Option<PathBuf>,
        tool_param_values: &HashMap<String, String>,
        cx: &mut Context<Workspace>,
    ) {
        let (command, args, cwd, env) = Self::resolve_tool_command(
            tool,
            runtime_path,
            agent_tools_path,
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
        suite_root()
    }

    /// Find the context-launcher binary using the runtime path resolution.
    /// Returns None if the binary is not deployed (fallback to dashboard substitution).
    fn resolve_context_launcher(&self) -> Option<PathBuf> {
        let candidate = self.runtime_path.join("tools/context-launcher");
        if candidate.is_file() {
            return Some(candidate);
        }
        None
    }

    fn run_automation(
        &self,
        entry_id: &str,
        entry_label: &str,
        prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Build fallback prompt using dashboard's own variable substitution.
        // Used when context-launcher is not available or fails.
        let fallback_prompt = {
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
        };

        // Capture values needed by the async block
        let launcher_path = self.resolve_context_launcher();
        let workspace = self.workspace.clone();
        let agent_backend = self.agent_backend;
        let backends = self.backends.clone();
        let agent_cwd = self.agent_cwd();
        let entry_id = entry_id.to_string();
        let entry_label = entry_label.to_string();

        // Build context-launcher CLI args
        let mut launcher_args = vec!["--automation".to_string(), entry_id.clone()];
        if let Some(values) = self.param_values.get(&entry_id) {
            for (key, value) in values {
                if !value.is_empty() {
                    launcher_args.push("--param".to_string());
                    launcher_args.push(format!("{}={}", key, value));
                }
            }
        }

        cx.spawn_in(window, async move |_this, cx| {
            // Phase 1: Run context-launcher → Result<String, String>.
            // Ok = enriched prompt (stdout), Err = human-readable reason.
            let result: Result<String, String> = if let Some(launcher) = launcher_path {
                let args = launcher_args;
                let output_result = cx
                    .background_executor()
                    .spawn(async move {
                        smol::process::Command::new(&launcher)
                            .args(&args)
                            .output()
                            .await
                    })
                    .await;

                match output_result {
                    Ok(output) if output.status.success() => {
                        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                        if stdout.trim().is_empty() {
                            Err("context-launcher returned empty output".into())
                        } else {
                            Ok(stdout)
                        }
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let snippet = stderr.lines().next().unwrap_or("unknown error");
                        Err(format!("exit {}: {snippet}", output.status))
                    }
                    Err(e) => Err(format!("{e}")),
                }
            } else {
                Err("context-launcher not installed".into())
            };

            // Phase 2: Route enriched prompt on success, or show toast on failure.
            match result {
                Ok(enriched_prompt) => {
                    if agent_backend == AgentBackend::Native {
                        let prompt = enriched_prompt;
                        workspace.update_in(cx, |workspace, window, cx| {
                            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                panel.update(cx, |panel, cx| {
                                    panel.new_external_thread_with_auto_submit(
                                        prompt,
                                        window,
                                        cx,
                                    );
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

                    // Terminal backend: collapse multi-line prompt to single line
                    // to avoid `zsh: parse error near '\n'` when spawning
                    // `claude -p "..."`.
                    let resolved_prompt = enriched_prompt
                        .lines()
                        .map(|line| line.trim())
                        .filter(|line| !line.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");

                    let backend_id = agent_backend.backend_id();
                    let Some(config) = backends.iter().find(|b| b.id == backend_id) else {
                        log::warn!("dashboard: no backend config for '{backend_id}'");
                        return;
                    };

                    let command = resolve_bin(&config.command);
                    let escaped = resolved_prompt.replace("'", "'\\''");
                    let flags = &config.flags;
                    let prompt_flag = &config.prompt_flag;
                    let full_command = format!("{command} {flags} {prompt_flag} '{escaped}'");

                    let spawn = SpawnInTerminal {
                        id: TaskId(format!("automation-{}", entry_id)),
                        label: entry_label.clone(),
                        full_label: entry_label.clone(),
                        command: Some(full_command),
                        args: vec![],
                        command_label: entry_label,
                        cwd: Some(agent_cwd),
                        use_new_terminal: true,
                        allow_concurrent_runs: false,
                        reveal: RevealStrategy::Always,
                        show_command: true,
                        show_rerun: true,
                        ..Default::default()
                    };

                    workspace.update_in(cx, |workspace, window, cx| {
                        workspace.spawn_in_terminal(spawn, window, cx).detach();
                    }).log_err();
                }
                Err(reason) => {
                    log::warn!("context-launcher: {reason}");

                    // Capture routing state for the "Run without context" button.
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
                                format!(
                                    "Context enrichment failed for '{}': {}",
                                    entry_label, reason
                                ),
                            )
                            .on_click(
                                "Run without context",
                                move |window, cx| {
                                    if agent_backend == AgentBackend::CopyOnly {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            fallback.clone(),
                                        ));
                                        return;
                                    }

                                    if agent_backend == AgentBackend::Native {
                                        let prompt = fallback.clone();
                                        ws_for_toast.update(cx, |workspace, cx| {
                                            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                                panel.update(cx, |panel, cx| {
                                                    panel.new_external_thread_with_auto_submit(
                                                        prompt,
                                                        window,
                                                        cx,
                                                    );
                                                });
                                                workspace.focus_panel::<AgentPanel>(window, cx);
                                            }
                                        }).log_err();
                                        return;
                                    }

                                    // Terminal backend
                                    let resolved = fallback
                                        .lines()
                                        .map(|l| l.trim())
                                        .filter(|l| !l.is_empty())
                                        .collect::<Vec<_>>()
                                        .join(" ");
                                    let backend_id = agent_backend.backend_id();
                                    let Some(config) =
                                        backends_for_toast.iter().find(|b| b.id == backend_id)
                                    else {
                                        return;
                                    };
                                    let command = resolve_bin(&config.command);
                                    let escaped = resolved.replace("'", "'\\''");
                                    let full_command = format!(
                                        "{command} {} {} '{escaped}'",
                                        config.flags, config.prompt_flag
                                    );

                                    let spawn = SpawnInTerminal {
                                        id: TaskId(format!("automation-{}", id_for_toast)),
                                        label: label_for_toast.clone(),
                                        full_label: label_for_toast.clone(),
                                        command: Some(full_command),
                                        args: vec![],
                                        command_label: label_for_toast.clone(),
                                        cwd: Some(cwd_for_toast.clone()),
                                        use_new_terminal: true,
                                        allow_concurrent_runs: false,
                                        reveal: RevealStrategy::Always,
                                        show_command: true,
                                        show_rerun: true,
                                        ..Default::default()
                                    };

                                    ws_for_toast.update(cx, |workspace, cx| {
                                        workspace.spawn_in_terminal(spawn, window, cx).detach();
                                    }).log_err();
                                },
                            ),
                            cx,
                        );
                    }).log_err();
                }
            }
        })
        .detach();
    }

    fn set_folder(&mut self, target: FolderTarget, path: PathBuf, cx: &mut Context<Self>) {
        match target {
            FolderTarget::Active => {
                write_active_folder(&path);
                self.active_folder = Some(path);
                self.recent_folders = read_recent_folders();
            }
            FolderTarget::Destination => {
                write_destination_folder(&path);
                self.destination_folder = Some(path);
                self.recent_destinations = read_recent_destinations();
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

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    write_active_folder(&path);
                    this.update(cx, |this, cx| {
                        this.active_folder = Some(path);
                        this.recent_folders = read_recent_folders();
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

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    write_destination_folder(&path);
                    this.update(cx, |this, cx| {
                        this.destination_folder = Some(path);
                        this.recent_destinations = read_recent_destinations();
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
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, {
                let tool_id = tool_id.clone();
                let tool_label = tool_label.clone();
                move |window, cx| GlobalShortcutModal::new(tool_id, tool_label, window, cx)
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

    fn schedule_param_write(&mut self, cx: &mut Context<Self>) {
        self._param_write_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            this.update(cx, |this, _cx| {
                write_param_values(&this.param_values);
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
        write_collapsed_sections(&self.collapsed_sections);
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

        let active_dropdown = Self::build_folder_dropdown(
            "active-folder",
            "Active Folder",
            FolderTarget::Active,
            &active_current,
            &active_recent,
            Color::Accent,
            {
                let entity = entity.clone();
                move |path, _window, cx: &mut App| {
                    write_active_folder(&path);
                    entity.update(cx, |this, cx| {
                        this.active_folder = Some(path);
                        this.recent_folders = read_recent_folders();
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
                    write_destination_folder(&path);
                    entity.update(cx, |this, cx| {
                        this.destination_folder = Some(path);
                        this.recent_destinations = read_recent_destinations();
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
        let workspace = self.workspace.clone();
        let session_path = self.session_path.clone();
        let active_folder = self.active_folder.clone();
        let tool = tool.clone();
        let tool_param_values = self.param_values.get(&tool.id).cloned().unwrap_or_default();

        move |_, window, cx| {
            let runtime_path = runtime_path.clone();
            let agent_tools_path = agent_tools_path.clone();
            let active_folder = active_folder.clone();
            let session_path = session_path.clone();
            let tool_param_values = tool_param_values.clone();
            if is_background {
                workspace.update(cx, |_workspace, cx| {
                    Self::spawn_tool_background(
                        &tool,
                        &runtime_path,
                        &agent_tools_path,
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
                        write_background_tools(&this.background_tools);
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let tools: Vec<ToolEntry> = self
            .tools
            .iter()
            .filter(|t| t.tier == ToolTier::Featured && !t.hidden)
            .cloned()
            .collect();

        let hover_bg = cx.theme().colors().ghost_element_hover;
        let accent_color = cx.theme().colors().text_accent;
        let card_bg = cx.theme().colors().elevated_surface_background;
        let icon_tint_bg = cx.theme().status().info_background.opacity(0.15);

        tools
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

                div()
                    .id(SharedString::from(format!("featured-{}", tool.id)))
                    .group(group_name)
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(accent_color)
                    .bg(card_bg)
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover_bg))
                    .drag_over::<ExternalPaths>(|style, _, _, cx| {
                        style.bg(cx.theme().colors().drop_target_background)
                    })
                    .on_drop(featured_drop)
                    .child(
                        h_flex()
                            .w_full()
                            .child(div().w(px(3.)).h_full().flex_shrink_0().bg(accent_color))
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
                                                Icon::new(tool_icon)
                                                    .size(IconSize::Medium)
                                                    .color(Color::Accent),
                                            ),
                                    )
                                    .child(
                                        v_flex().flex_1().child(Label::new(tool_label)).child(
                                            Label::new(tool_description)
                                                .color(Color::Muted)
                                                .size(LabelSize::XSmall),
                                        ),
                                    )
                                    .child(action_buttons),
                            ),
                    )
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
                    })
                    .on_click(click_handler)
                    .into_any_element()
            })
            .collect()
    }

    /// Build Standard tool cards: neutral border, 28px icon, params inline below.
    fn build_standard_cards(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let tools: Vec<ToolEntry> = self
            .tools
            .iter()
            .filter(|t| t.tier == ToolTier::Standard && !t.hidden)
            .cloned()
            .collect();

        let hover_border = cx.theme().colors().text_accent;
        let card_bg = cx.theme().colors().elevated_surface_background;
        let border_color = cx.theme().colors().border_variant;
        let icon_bg = cx.theme().colors().element_background;

        tools
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
                                    write_param_values(&this.param_values);
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
    fn build_compact_cards(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let tools: Vec<ToolEntry> = self
            .tools
            .iter()
            .filter(|t| t.tier == ToolTier::Compact && !t.hidden)
            .cloned()
            .collect();

        let hover_border = cx.theme().colors().text_accent;
        let border_color = cx.theme().colors().border_variant;

        tools
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

    fn render_featured_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains("featured");
        let cards = if is_open {
            self.build_featured_cards(window, cx)
        } else {
            Vec::new()
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(self.section_header(ToolTier::Featured.label(), "featured", cx))
            .when(is_open, |el| {
                el.when_some(self.tools_error.as_ref(), |el, err| {
                    el.child(
                        Label::new(format!("Parse error: {}", err))
                            .color(Color::Error)
                            .size(LabelSize::XSmall),
                    )
                })
            })
            .when(is_open && !cards.is_empty(), |el| {
                el.child(
                    v_flex()
                        .id("featured-content-anim")
                        .w_full()
                        .gap_2()
                        .children(cards),
                )
            })
    }

    fn render_standard_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains("standard");
        v_flex()
            .w_full()
            .gap_2()
            .child(self.section_header(ToolTier::Standard.label(), "standard", cx))
            .when(is_open, |el| {
                let cards = self.build_standard_cards(window, cx);
                el.child(
                    h_flex()
                        .id("standard-content-anim")
                        .w_full()
                        .flex_wrap()
                        .gap(px(8.))
                        .children(cards),
                )
            })
    }

    fn render_compact_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = !self.collapsed_sections.contains("compact");
        let entity = cx.entity().downgrade();
        let id_for_toggle = "compact".to_string();

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
                let path = tools_config_dir();
                let workspace = this.workspace.clone();
                cx.spawn_in(window, async move |_this, cx| {
                    workspace.update_in(cx, |workspace, window, cx| {
                        workspace
                            .open_abs_path(path, OpenOptions::default(), window, cx)
                            .detach();
                    }).log_err();
                })
                .detach();
            }));

        let header = h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .items_center()
            .child(
                Disclosure::new(SharedString::from("disc-compact"), is_open).on_click(
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.toggle_section(&id_for_toggle, cx);
                        }).log_err();
                    },
                ),
            )
            .child(
                Label::new(ToolTier::Compact.label())
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
            .child(edit_btn);

        if !is_open {
            return v_flex().w_full().gap_2().child(header);
        }

        let cards = self.build_compact_cards(cx);

        v_flex().w_full().gap_2().child(header).child(
            h_flex()
                .id("compact-content-anim")
                .w_full()
                .flex_wrap()
                .gap(px(8.))
                .children(cards),
        )
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
                                                            write_param_values(&this.param_values);
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
                                                write_param_values(&this.param_values);
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
        let card_bg = cx.theme().colors().elevated_surface_background;
        let hover_bg = cx.theme().colors().ghost_element_hover;
        let icon_tint_bg = accent_bg.opacity(0.15);
        let editor_bg = cx.theme().colors().editor_background;

        let entity = cx.entity().downgrade();

        let click_entity = entity.clone();
        let click_id = entry_id.clone();
        let click_label = entry_label.clone();
        let click_prompt = entry_prompt.clone();

        let edit_entity = entity.clone();
        let edit_id = entry_id.clone();

        let disc_entity = entity;
        let disc_id = entry_id.clone();

        let param_fields = self.render_entry_params(&entry.id, &entry.params, window, cx);
        let group_name = SharedString::from(format!("automation-{}", entry_id));

        div()
            .id(SharedString::from(format!(
                "automation-card-{}-{}",
                entry_id, idx
            )))
            .group(group_name)
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(accent.opacity(0.5))
            .bg(card_bg)
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
                                    .child(
                                        v_flex().flex_1().child(Label::new(entry_label)).child(
                                            Label::new(entry_description)
                                                .color(Color::Muted)
                                                .size(LabelSize::XSmall),
                                        ),
                                    )
                                    .child(
                                        Label::new(badge_label)
                                            .color(badge_color)
                                            .size(LabelSize::XSmall),
                                    )
                                    // Wrapper stops mouse_down propagation to prevent the card's
                                    // window-level click tracking from interfering with child buttons.
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
                                                    SharedString::from(format!(
                                                        "disc-auto-{}",
                                                        disc_id
                                                    )),
                                                    is_expanded,
                                                )
                                                .on_click(
                                                    move |_, _, cx| {
                                                        disc_entity
                                                            .update(cx, |this, cx| {
                                                                this.toggle_automation_expanded(
                                                                    &disc_id, cx,
                                                                );
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
                                                                let path = automations_dir()
                                                                    .join(format!(
                                                                        "{}.toml",
                                                                        edit_id
                                                                    ));
                                                                let workspace =
                                                                    this.workspace.clone();
                                                                cx.spawn_in(
                                                                    window,
                                                                    async move |_this, cx| {
                                                                        workspace
                                                                            .update_in(
                                                                                cx,
                                                                                |workspace,
                                                                                 window,
                                                                                 cx| {
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
                                    ),
                            )
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
                            .when(is_expanded, |el| {
                                el.child(
                                    div().w_full().px_3().pb_2().child(
                                        div().w_full().p_2().rounded_md().bg(editor_bg).child(
                                            Label::new(entry_prompt)
                                                .color(Color::Muted)
                                                .size(LabelSize::XSmall),
                                        ),
                                    ),
                                )
                            }),
                    ),
            )
            .on_click(move |_, window, cx| {
                click_entity.update(cx, |this, cx| {
                    if this.expanded_automations.contains(click_id.as_str()) {
                        this.toggle_automation_expanded(&click_id, cx);
                    } else {
                        this.run_automation(&click_id, &click_label, &click_prompt, window, cx);
                    }
                }).log_err();
            })
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
            .filter(|a| !a.hidden)
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

        let regular_cards: Vec<_> = regular
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                self.render_automation_card(
                    entry,
                    idx + meta.len(),
                    Color::Accent,
                    &badge_label,
                    badge_color,
                    window,
                    cx,
                )
                .into_any_element()
            })
            .collect();

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
                        .children(regular_cards),
                )
            })
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
        h_flex()
            .key_context("Dashboard")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, action: &RunDashboardTool, window, cx| {
                if let Some(tool) = this.tools.iter().find(|t| t.id == action.tool_id) {
                    let tool = tool.clone();
                    let is_background = this.background_tools.contains(&action.tool_id);
                    let runtime_path = this.runtime_path.clone();
                    let agent_tools_path = this.agent_tools_path.clone();
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
                            .child(self.render_featured_section(window, cx))
                            .child(self.render_standard_section(window, cx))
                            .child(self.render_compact_section(cx))
                            // AI Agents
                            .child(self.render_ai_agents_section(cx))
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
