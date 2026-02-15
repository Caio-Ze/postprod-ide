use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers as GHModifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager as NativeHotKeyManager,
};
use gpui::{
    actions, Action, App, AsyncApp, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, Keystroke, KeystrokeEvent, ParentElement,
    PathPromptOptions, Render, ScrollHandle, SharedString, Styled, Subscription, WeakEntity,
    Window,
};
use schemars::JsonSchema;
use serde::Deserialize;
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use ui::{
    prelude::*, Button, ButtonLike, ButtonStyle, Divider, DividerColor, Headline, HeadlineSize,
    Icon, IconButton, IconName, IconSize, Label, LabelSize, Modal, ModalFooter, ModalHeader,
    Section, ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip,
    WithScrollbar as _,
};
use workspace::{
    item::{Item, ItemEvent},
    with_active_or_new_workspace, ModalView, OpenOptions, ProToolsSessionName, Workspace,
};

use util::ResultExt as _;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

const APP_VERSION: &str = env!("PROTOOLS_VERSION");

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

actions!(
    dashboard,
    [
        /// Show the ProTools Studio Dashboard.
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

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

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
                    }
                })
                .detach();
        });
    });
}

/// Ensure a Dashboard tab exists in the workspace. Idempotent — scans all
/// panes before creating a new one.
pub fn ensure_dashboard(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    // Check all panes for an existing Dashboard
    for pane in workspace.panes() {
        let found = pane.read(cx).items().any(|item| item.downcast::<Dashboard>().is_some());
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
}

#[derive(Deserialize)]
struct ToolsFile {
    tool: Vec<ToolEntry>,
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
        _ => IconName::Sparkle,
    }
}

fn load_toml_tools(path: &Path) -> (Vec<ToolEntry>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (Vec::new(), None);
    };
    match toml::from_str::<ToolsFile>(&content) {
        Ok(file) => (file.tool, None),
        Err(e) => {
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let err = format!("{filename}: {e}");
            log::error!("config: {err}");
            (Vec::new(), Some(err))
        }
    }
}

fn merge_tools(defaults: Vec<ToolEntry>, user: Vec<ToolEntry>) -> Vec<ToolEntry> {
    let user_by_id: HashMap<String, ToolEntry> = user
        .iter()
        .map(|t| (t.id.clone(), t.clone()))
        .collect();

    let mut merged: Vec<ToolEntry> = defaults
        .into_iter()
        .map(|default| {
            match user_by_id.get(&default.id) {
                Some(user_entry) => user_entry.clone(),
                None => default,
            }
        })
        .collect();

    let default_ids: HashSet<String> = merged.iter().map(|t| t.id.clone()).collect();
    for (id, entry) in &user_by_id {
        if !default_ids.contains(id.as_str()) {
            merged.push(entry.clone());
        }
    }

    merged
}

fn load_tools_registry() -> (Vec<ToolEntry>, Option<String>) {
    let base_path = find_latest_update("TOOLS").unwrap_or_else(tools_toml_path);
    let (defaults, defaults_err) = load_toml_tools(&base_path);
    let (user, user_err) = load_toml_tools(&tools_user_toml_path());

    if !defaults.is_empty() || !user.is_empty() {
        let merged = merge_tools(defaults, user);
        let error = defaults_err.or(user_err);
        return (merged, error);
    }

    (Vec::new(), defaults_err.or(user_err))
}

// ---------------------------------------------------------------------------
// Automations — loaded from TOML at runtime
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
}

#[derive(Deserialize)]
struct AutomationsFile {
    automation: Vec<AutomationEntry>,
}

fn automations_toml_path() -> PathBuf {
    config_dir().join("AUTOMATIONS.toml")
}

fn load_toml_automations(path: &Path) -> (Vec<AutomationEntry>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (Vec::new(), None);
    };
    match toml::from_str::<AutomationsFile>(&content) {
        Ok(file) => (file.automation, None),
        Err(e) => {
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let err = format!("{filename}: {e}");
            log::error!("config: {err}");
            (Vec::new(), Some(err))
        }
    }
}

fn merge_automations(defaults: Vec<AutomationEntry>, user: Vec<AutomationEntry>) -> Vec<AutomationEntry> {
    let user_by_id: HashMap<String, AutomationEntry> = user
        .iter()
        .map(|a| (a.id.clone(), a.clone()))
        .collect();

    let mut merged: Vec<AutomationEntry> = defaults
        .into_iter()
        .map(|default| {
            match user_by_id.get(&default.id) {
                Some(user_entry) => user_entry.clone(),
                None => default,
            }
        })
        .collect();

    let default_ids: HashSet<String> = merged.iter().map(|a| a.id.clone()).collect();
    for (id, entry) in &user_by_id {
        if !default_ids.contains(id.as_str()) {
            merged.push(entry.clone());
        }
    }

    merged
}

fn load_automations_registry() -> (Vec<AutomationEntry>, Option<String>) {
    let base_path = find_latest_update("AUTOMATIONS").unwrap_or_else(automations_toml_path);
    let (defaults, defaults_err) = load_toml_automations(&base_path);
    let (user, user_err) = load_toml_automations(&automations_user_toml_path());

    if !defaults.is_empty() || !user.is_empty() {
        let merged = merge_automations(defaults, user);
        let error = defaults_err.or(user_err);
        return (merged, error);
    }

    (Vec::new(), defaults_err.or(user_err))
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

fn old_agent_skills_dir() -> PathBuf {
    suite_root()
        .join("1_Sessões")
        .join("Pro tools_EDITSESSION")
        .join("agent-skills")
}

fn ensure_config_extracted(cx: &App) {
    let dir = config_dir();
    let old_dir = old_agent_skills_dir();

    // SKILL.md — keep old behavior (never overwrite user edits)
    let skill_target = dir.join("SKILL.md");
    if !skill_target.exists() {
        let old_file = old_dir.join("SKILL.md");
        if old_file.exists() {
            if std::fs::copy(&old_file, &skill_target).is_ok() {
                log::info!("config: migrated SKILL.md from old agent-skills/ to config/");
            }
        } else if let Ok(Some(data)) = cx.asset_source().load("config/SKILL.md") {
            std::fs::write(&skill_target, data.as_ref()).log_err();
            log::info!("config: extracted default SKILL.md to config/");
        }
    }

    // TOOLS.toml + AUTOMATIONS.toml — two-layer: always overwrite defaults
    for (name, user_name, asset_path) in [
        ("TOOLS.toml", "TOOLS.user.toml", "config/TOOLS.toml"),
        (
            "AUTOMATIONS.toml",
            "AUTOMATIONS.user.toml",
            "config/AUTOMATIONS.toml",
        ),
    ] {
        let defaults_file = dir.join(name);
        let user_file = dir.join(user_name);

        // Migration: V1 → V2. User has old single-file config.
        if !user_file.exists() && defaults_file.exists() {
            std::fs::rename(&defaults_file, &user_file).log_err();
            log::info!("config: migrated {name} → {user_name} (two-layer upgrade)");
        }

        // Also check old agent-skills/ location for migration
        if !user_file.exists() {
            let old_file = old_dir.join(name);
            if old_file.exists() {
                if std::fs::copy(&old_file, &user_file).is_ok() {
                    log::info!("config: migrated {name} from old agent-skills/ → {user_name}");
                }
            }
        }

        // Always overwrite defaults from embedded asset
        if let Ok(Some(data)) = cx.asset_source().load(asset_path) {
            std::fs::write(&defaults_file, data.as_ref()).log_err();
        }
    }

    // Ensure global skill symlinks point to the new config/ location.
    // If an existing symlink points to the old agent-skills/ path, recreate it.
    let skill_target = dir.join("SKILL.md");
    if skill_target.exists() {
        let home = util::paths::home_dir();
        for skill_dir in [
            home.join(".claude/skills/ptsl-tools"),
            home.join(".agents/skills/ptsl-tools"),
        ] {
            let skill_link = skill_dir.join("SKILL.md");

            // Check if existing symlink points to old location
            let needs_update = if skill_link.is_symlink() {
                match std::fs::read_link(&skill_link) {
                    Ok(existing_target) => existing_target != skill_target,
                    Err(_) => true,
                }
            } else {
                !skill_link.exists()
            };

            if needs_update {
                std::fs::create_dir_all(&skill_dir).log_err();
                // Remove stale symlink if it exists
                if skill_link.is_symlink() || skill_link.exists() {
                    std::fs::remove_file(&skill_link).log_err();
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&skill_target, &skill_link).log_err();
            }
        }
    }

    // Version tracking: detect new deployments and purge stale updates
    check_version_and_purge(APP_VERSION, &release_version_path(), &updates_dir());
}

/// Compare the running version against the version on disk.
/// If they differ, purge the `updates_dir` and write the new version.
fn check_version_and_purge(app_version: &str, version_file: &Path, updates_dir: &Path) {
    let disk_version = std::fs::read_to_string(version_file)
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if disk_version != app_version {
        if !disk_version.is_empty() {
            log::info!(
                "config: new deployment {} → {}, cleaning updates/",
                disk_version,
                app_version
            );
            if updates_dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(updates_dir) {
                    log::warn!("config: failed to clean updates/: {e}");
                }
                std::fs::create_dir_all(updates_dir).log_err();
            }
        }
        std::fs::write(version_file, app_version).log_err();
        log::info!("config: .release_version = {}", app_version);
    }
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
# ProTools Studio — Global Hotkeys
# These shortcuts work even when ProTools Studio is not focused.
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
            log::warn!("global hotkeys: could not initialize (Input Monitoring permission needed?): {e}");
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

                let _ = this.update(cx, |manager: &mut GlobalHotkeyManager, _cx| {
                    if new_content != manager.last_config_content {
                        log::info!("global hotkey: config changed, re-registering");
                        manager.last_config_content = new_content;
                        manager.register_hotkeys_from_config();
                    }
                });
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
    let workspace_handle = cx
        .active_window()
        .and_then(|w| w.downcast::<Workspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|w| w.downcast::<Workspace>())
        });

    if let Some(workspace) = workspace_handle {
        let tool_id = tool_id.to_string();
        workspace
            .update(cx, |workspace, window, cx| {
                ensure_dashboard(workspace, window, cx);

                let dashboard_data = workspace
                    .panes()
                    .iter()
                    .flat_map(|pane| pane.read(cx).items())
                    .find_map(|item| item.downcast::<Dashboard>())
                    .map(|d| {
                        let d = d.read(cx);
                        (
                            d.tools.iter().find(|t| t.id == tool_id).cloned(),
                            d.runtime_path.clone(),
                            d.agent_tools_path.clone(),
                            d.session_path.clone(),
                            d.pasta_ativa.clone(),
                            d.background_tools.contains(&tool_id),
                        )
                    });

                if let Some((Some(tool), runtime_path, agent_tools_path, session_path, pasta_ativa, is_background)) =
                    dashboard_data
                {
                    if is_background {
                        Dashboard::spawn_tool_background(
                            &tool,
                            &runtime_path,
                            &agent_tools_path,
                            &session_path,
                            &pasta_ativa,
                            cx,
                        );
                    } else {
                        cx.activate(true);
                        Dashboard::spawn_tool_entry(
                            &tool,
                            &runtime_path,
                            &agent_tools_path,
                            &session_path,
                            &pasta_ativa,
                            workspace,
                            window,
                            cx,
                        );
                    }
                }
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
            log::info!(
                "global hotkey: saved {} -> {}",
                keystroke_str,
                self.tool_id
            );

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
            .header(
                ModalHeader::new().headline(format!("Global Shortcut: {}", tool_label)),
            )
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
                                .child(
                                    Label::new(display_text)
                                        .size(LabelSize::Large)
                                        .color(if has_capture {
                                            Color::Default
                                        } else {
                                            Color::Muted
                                        }),
                                ),
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
                        .child(
                            Button::new("cancel", "Cancel")
                                .on_click(cx.listener(|_this, _, _window, cx| {
                                    cx.emit(DismissEvent);
                                })),
                        )
                        .child(
                            Button::new("save", "Save")
                                .style(ButtonStyle::Filled)
                                .disabled(!has_capture)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.save_and_dismiss(window, cx);
                                    }),
                                ),
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
    util::paths::home_dir().join("ProTools_Suite")
}

fn config_dir() -> PathBuf {
    suite_root().join("config")
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

fn tools_toml_path() -> PathBuf {
    config_dir().join("TOOLS.toml")
}

fn tools_user_toml_path() -> PathBuf {
    config_dir().join("TOOLS.user.toml")
}

fn automations_user_toml_path() -> PathBuf {
    config_dir().join("AUTOMATIONS.user.toml")
}

fn updates_dir() -> PathBuf {
    config_dir().join("updates")
}

fn release_version_path() -> PathBuf {
    config_dir().join(".release_version")
}

/// Scan `updates/` for files matching `{prefix}.update.N.toml` and return
/// the path with the highest N. Returns `None` if no update files exist.
fn find_latest_update(prefix: &str) -> Option<PathBuf> {
    find_latest_update_in(prefix, &updates_dir())
}

fn find_latest_update_in(prefix: &str, dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;

    let pattern_prefix = format!("{prefix}.update.");

    let mut best: Option<(u32, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let Some(rest) = name.strip_prefix(&pattern_prefix) else {
            continue;
        };

        let Some(num_str) = rest.strip_suffix(".toml") else {
            continue;
        };

        let Ok(n) = num_str.parse::<u32>() else {
            continue;
        };

        if best.as_ref().map_or(true, |(current, _)| n > *current) {
            best = Some((n, path));
        }
    }

    best.map(|(_, path)| path)
}

fn ensure_workspace_dirs() {
    for dir in [config_dir(), agent_tools_dir(), runtime_tools_dir(), updates_dir()] {
        if !dir.exists() {
            std::fs::create_dir_all(&dir).log_err();
        }
    }
}

fn scan_delivery_folder() -> DeliveryStatus {
    let dir = suite_root().join("4_Finalizados");
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
                let count = sub_entries
                    .flatten()
                    .filter(|e| e.path().is_file())
                    .count();
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
            status.warnings.push("Falta: arquivos TV".to_string());
        }
        if status.net_count == 0 {
            status.warnings.push("Falta: arquivos NET".to_string());
        }
        if status.spot_count == 0 {
            status.warnings.push("Falta: arquivos SPOT".to_string());
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
// Pasta ativa helpers
// ---------------------------------------------------------------------------

fn pasta_ativa_file() -> PathBuf {
    suite_root().join(".pasta_ativa")
}

fn read_pasta_ativa() -> Option<PathBuf> {
    std::fs::read_to_string(pasta_ativa_file())
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

fn write_pasta_ativa(path: &Path) {
    let _ = std::fs::write(pasta_ativa_file(), path.to_string_lossy().as_bytes());
    add_to_recentes(path);
}

// ---------------------------------------------------------------------------
// Pastas recentes helpers
// ---------------------------------------------------------------------------

fn pastas_recentes_file() -> PathBuf {
    suite_root().join(".pastas_recentes")
}

fn read_pastas_recentes() -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(pastas_recentes_file()) else {
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

fn add_to_recentes(path: &Path) {
    let mut recentes = read_pastas_recentes();
    recentes.retain(|p| p != path);
    recentes.insert(0, path.to_path_buf());
    recentes.truncate(10);
    let content = recentes
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(pastas_recentes_file(), content).log_err();
}

// ---------------------------------------------------------------------------
// Background tools persistence
// ---------------------------------------------------------------------------

fn background_tools_file() -> PathBuf {
    config_dir().join(".background_tools")
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
/// 1. `~/ProTools_Suite/tools/runtime/` (workspace — production)
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
/// 1. `~/ProTools_Suite/tools/agent/` (workspace — production)
/// 2. `PROTOOLS_AGENT_TOOLS_PATH` env var (override for dev)
/// 3. Workspace path as expected default
fn resolve_agent_tools_path() -> PathBuf {
    let workspace_agent = agent_tools_dir();
    if dir_has_content(&workspace_agent) {
        return workspace_agent;
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
}

impl AgentBackend {
    fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Gemini => 1,
            Self::CopyOnly => 2,
        }
    }

    fn badge_label(self) -> &'static str {
        match self {
            Self::Claude => "run with Claude",
            Self::Gemini => "run with Gemini",
            Self::CopyOnly => "(copy prompt)",
        }
    }

    fn badge_color(self) -> Color {
        match self {
            Self::Claude | Self::Gemini => Color::Accent,
            Self::CopyOnly => Color::Muted,
        }
    }

    fn command(self) -> Option<String> {
        match self {
            Self::Claude => Some(resolve_bin("claude")),
            Self::Gemini => Some(resolve_bin("gemini")),
            Self::CopyOnly => None,
        }
    }

    /// Flags required for headless `-p` mode so the agent can actually
    /// execute tools (Bash, file ops, etc.) without interactive approval.
    fn headless_flags(self) -> &'static str {
        match self {
            Self::Claude => "--dangerously-skip-permissions",
            Self::Gemini => "--yolo -m gemini-3-flash-preview",
            Self::CopyOnly => "",
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
    // Pasta ativa
    pasta_ativa: Option<PathBuf>,
    pastas_recentes: Vec<PathBuf>,
    // Delivery status
    delivery_status: DeliveryStatus,
    _delivery_scan_task: gpui::Task<()>,
    // Automations (loaded from TOML)
    automations: Vec<AutomationEntry>,
    agent_backend: AgentBackend,
    _automations_reload_task: gpui::Task<()>,
    _tools_reload_task: gpui::Task<()>,
    // Background execution mode per tool
    background_tools: HashSet<String>,
    // Config parse errors
    tools_error: Option<String>,
    automations_error: Option<String>,
    // Scroll
    scroll_handle: ScrollHandle,
}

impl Dashboard {
    pub fn new(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        let runtime_path = resolve_runtime_path();

        let agent_tools_path = resolve_agent_tools_path();

        ensure_workspace_dirs();
        ensure_config_extracted(cx);

        let pasta_ativa = read_pasta_ativa();
        let pastas_recentes = read_pastas_recentes();
        let (automations, automations_error) = load_automations_registry();
        let (tools, tools_error) = load_tools_registry();
        let background_tools = read_background_tools();

        cx.new(|cx| {
            // Spawn session polling task (every 5 seconds)
            let poll_binary = runtime_path
                .join("tools/get_session_path");
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

                    let _ = this.update(cx, |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                        let name = result.as_ref().map(|p| {
                            Path::new(p)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string()
                        });
                        dashboard.session_path = result;
                        dashboard.session_name = name;

                        // Update the global for window title
                        let global_name = dashboard
                            .session_name
                            .clone()
                            .unwrap_or_default();
                        cx.set_global(ProToolsSessionName(global_name));

                        cx.notify();
                    });

                    cx.background_executor()
                        .timer(Duration::from_secs(5))
                        .await;
                }
            });

            // Spawn delivery scan task (every 15 seconds)
            let delivery_scan_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    let status = cx
                        .background_executor()
                        .spawn(async { scan_delivery_folder() })
                        .await;

                    let _ = this.update(cx, |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                        dashboard.delivery_status = status;
                        cx.notify();
                    });

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

                    let _ = this.update(cx, |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                        dashboard.automations = merged;
                        dashboard.automations_error = error;
                        cx.notify();
                    });
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

                    let _ = this.update(cx, |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                        dashboard.tools = merged;
                        dashboard.tools_error = error;
                        cx.notify();
                    });
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
                pasta_ativa,
                pastas_recentes,
                delivery_status: DeliveryStatus::default(),
                _delivery_scan_task: delivery_scan_task,
                automations,
                agent_backend: AgentBackend::Claude,
                _automations_reload_task: automations_reload_task,
                _tools_reload_task: tools_reload_task,
                background_tools,
                tools_error,
                automations_error,
                scroll_handle: ScrollHandle::new(),
            }
        })
    }

    fn resolve_tool_command(
        tool: &ToolEntry,
        runtime_path: &Path,
        agent_tools_path: &Path,
        session_path: &Option<String>,
        pasta_ativa: &Option<PathBuf>,
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
            let work_dir = if tool.tier == ToolTier::Standard && tool.source == ToolSource::Runtime {
                if let Some(pa) = pasta_ativa {
                    pa.clone()
                } else {
                    runtime_path.join(&tool.cwd)
                }
            } else {
                runtime_path.join(&tool.cwd)
            };
            (cmd, work_dir)
        };

        let mut args = vec!["--output-json".to_string()];

        if tool.needs_session {
            if let Some(session) = session_path {
                args.insert(0, session.clone());
                args.insert(0, "--session".to_string());
            }
        }

        args.extend(tool.extra_args.iter().cloned());

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
        pasta_ativa: &Option<PathBuf>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let (command, args, cwd, env) =
            Self::resolve_tool_command(tool, runtime_path, agent_tools_path, session_path, pasta_ativa);

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
        pasta_ativa: &Option<PathBuf>,
        cx: &mut Context<Workspace>,
    ) {
        let (command, args, cwd, env) =
            Self::resolve_tool_command(tool, runtime_path, agent_tools_path, session_path, pasta_ativa);
        let tool_label = tool.label.clone();

        cx.spawn(async move |_this: WeakEntity<Workspace>, cx: &mut AsyncApp| {
            let result = cx.background_executor().spawn(async move {
                let mut cmd = smol::process::Command::new(&command);
                cmd.args(&args).current_dir(&cwd);
                for (key, value) in &env {
                    cmd.env(key, value);
                }
                cmd.output().await
            }).await;

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
        }).detach();
    }

    /// Best working directory for AI agents. Priority:
    /// 1. Open Pro Tools session's grandparent (gives context for the session
    ///    folder AND its sibling folders)
    /// 2. Pasta ativa (user-selected working folder)
    /// 3. suite_root (~/ProTools_Suite)
    fn agent_cwd(&self) -> PathBuf {
        if let Some(session) = &self.session_path {
            let session_path = Path::new(session);
            if let Some(grandparent) = session_path.parent().and_then(|p| p.parent()) {
                if grandparent.is_dir() {
                    return grandparent.to_path_buf();
                }
            }
        }
        self.pasta_ativa.clone().unwrap_or_else(suite_root)
    }

    fn run_automation(
        &self,
        entry_id: &str,
        entry_label: &str,
        prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resolved_prompt = if let Some(session) = &self.session_path {
            prompt.replace("{session_path}", session)
        } else {
            prompt.replace("{session_path}", "<no session open>")
        };

        // Collapse multi-line prompts into a single line to avoid
        // `zsh: parse error near '\n'` when spawning `claude -p "..."`.
        let resolved_prompt = resolved_prompt
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        let Some(command) = self.agent_backend.command() else {
            cx.write_to_clipboard(ClipboardItem::new_string(resolved_prompt));
            return;
        };

        // Shell-escape the prompt with single quotes (POSIX-safe) and bake
        // it into the command string so `build_no_quote` doesn't split it
        // into separate unquoted tokens.
        let escaped = resolved_prompt.replace("'", "'\\''");
        let flags = self.agent_backend.headless_flags();
        let full_command = format!("{command} {flags} -p '{escaped}'");

        let spawn = SpawnInTerminal {
            id: TaskId(format!("automation-{}", entry_id)),
            label: entry_label.to_string(),
            full_label: entry_label.to_string(),
            command: Some(full_command),
            args: vec![],
            command_label: entry_label.to_string(),
            cwd: Some(self.agent_cwd()),
            use_new_terminal: true,
            allow_concurrent_runs: false,
            reveal: RevealStrategy::Always,
            show_command: true,
            show_rerun: true,
            ..Default::default()
        };

        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.spawn_in_terminal(spawn, window, cx).detach();
        });
    }

    fn pick_pasta_ativa(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    write_pasta_ativa(&path);
                    let _ = this.update(cx, |this, cx| {
                        this.pasta_ativa = Some(path);
                        this.pastas_recentes = read_pastas_recentes();
                        cx.notify();
                    });
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
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, {
                let tool_id = tool_id.clone();
                let tool_label = tool_label.clone();
                move |window, cx| GlobalShortcutModal::new(tool_id, tool_label, window, cx)
            });
        });
    }

    // -- Rendering helpers --

    fn render_session_status(&self, cx: &App) -> impl IntoElement {
        let (dot_color, label_text) = match &self.session_name {
            Some(name) => (Color::Created, format!("Pro Tools: {}", name)),
            None => (Color::Muted, "Pro Tools: Desconectado".to_string()),
        };

        let dot_hsla = dot_color.color(cx);

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(dot_hsla),
            )
            .child(
                Label::new(label_text)
                    .size(LabelSize::Small)
                    .color(dot_color),
            )
    }

    fn render_pasta_ativa(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let display = match &self.pasta_ativa {
            Some(p) => p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            None => "(nenhuma)".to_string(),
        };

        let label_color = if self.pasta_ativa.is_some() {
            Color::Default
        } else {
            Color::Muted
        };

        ButtonLike::new("pasta-ativa-btn")
            .full_width()
            .size(ButtonSize::Medium)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Folder)
                            .color(Color::Accent)
                            .size(IconSize::Small),
                    )
                    .child(
                        v_flex()
                            .items_start()
                            .child(
                                Label::new("Pasta Ativa")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(display).color(label_color)),
                    ),
            )
            .on_click(cx.listener(|this, _, _window, cx| {
                this.pick_pasta_ativa(cx);
            }))
    }

    fn render_pastas_recentes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let recentes: Vec<_> = self
            .pastas_recentes
            .iter()
            .filter(|p| Some(p.as_path()) != self.pasta_ativa.as_deref())
            .take(5)
            .cloned()
            .collect();

        if recentes.is_empty() {
            return v_flex().into_any_element();
        }

        v_flex()
            .pl_6()
            .gap_0p5()
            .children(recentes.into_iter().map(|path| {
                let display: SharedString = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
                    .into();
                let full_path = path.to_string_lossy().to_string();
                let click_path = path;

                Button::new(
                    SharedString::from(format!("recente-{}", display)),
                    display,
                )
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .icon(IconName::Folder)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text(full_path))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    write_pasta_ativa(&click_path);
                    this.pasta_ativa = Some(click_path.clone());
                    this.pastas_recentes = read_pastas_recentes();
                    cx.notify();
                }))
            }))
            .into_any_element()
    }

    fn render_delivery_status(&self, _cx: &App) -> impl IntoElement {
        let status = &self.delivery_status;
        let has_any = status.tv_count > 0
            || status.net_count > 0
            || status.spot_count > 0
            || status.mp3_count > 0;

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .px_1()
                    .mb_2()
                    .gap_2()
                    .child(
                        Label::new("ENTREGA")
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    )
                    .child(Divider::horizontal().color(DividerColor::BorderVariant)),
            )
            .when(!has_any, |el| {
                el.child(
                    h_flex()
                        .px_2()
                        .child(
                            Label::new("Nenhum arquivo em 4_Finalizados/")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
            })
            .when(has_any, |el| {
                el.child(
                    h_flex()
                        .px_2()
                        .gap_4()
                        .child(Self::delivery_badge("TV", status.tv_count, status.tv_count > 0))
                        .child(Self::delivery_badge("NET", status.net_count, status.net_count > 0))
                        .child(Self::delivery_badge("SPOT", status.spot_count, status.spot_count > 0))
                        .child(Self::delivery_badge("MP3", status.mp3_count, status.mp3_count > 0)),
                )
                .children(status.warnings.iter().map(|w| {
                    h_flex()
                        .px_2()
                        .child(
                            Label::new(format!("  {}", w))
                                .color(Color::Warning)
                                .size(LabelSize::XSmall),
                        )
                }))
            })
    }

    fn delivery_badge(label: &str, count: usize, ok: bool) -> impl IntoElement {
        let indicator = if ok { " OK" } else { " --" };
        let color = if ok { Color::Created } else { Color::Muted };

        h_flex()
            .gap_1()
            .child(Label::new(format!("{}: {}", label, count)).size(LabelSize::Small))
            .child(Label::new(indicator).size(LabelSize::XSmall).color(color))
    }

    fn section_header(title: &str) -> impl IntoElement {
        h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .child(
                Label::new(title.to_string())
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
    }

    /// Build tool buttons for a given tier. Pre-computes keybindings to avoid
    /// capturing `cx` inside the `.map()` closure (Rust 2024 lifetime rules).
    fn build_tool_buttons(
        &self,
        tier: ToolTier,
        button_size: ButtonSize,
        cx: &mut Context<Self>,
    ) -> Vec<ButtonLike> {
        let entity = cx.entity().downgrade();
        let tools: Vec<ToolEntry> = self
            .tools
            .iter()
            .filter(|t| t.tier == tier && !t.hidden)
            .cloned()
            .collect();

        let icon_color = if tier == ToolTier::Featured {
            Color::Accent
        } else {
            Color::Muted
        };

        tools
            .into_iter()
            .map(|tool| {
                let tool_icon = icon_for_tool(&tool.icon);
                let tool_id = tool.id.clone();
                let tool_label: SharedString = tool.label.clone().into();
                let tool_description: SharedString = tool.description.clone().into();

                let is_background = self.background_tools.contains(&tool_id);

                let toggle_tool_id = tool_id.clone();
                let toggle_entity = entity.clone();

                let globe_tool_id = tool_id.clone();
                let globe_tool_label = tool_label.to_string();
                let globe_entity = entity.clone();

                let runtime_path = self.runtime_path.clone();
                let agent_tools_path = self.agent_tools_path.clone();
                let workspace = self.workspace.clone();
                let session_path = self.session_path.clone();
                let pasta_ativa = self.pasta_ativa.clone();

                ButtonLike::new(format!("dashboard-btn-{}", tool_id))
                    .full_width()
                    .size(button_size)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::new(tool_icon)
                                    .color(icon_color)
                                    .size(IconSize::Small),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .items_start()
                                    .child(Label::new(tool_label))
                                    .child(
                                        Label::new(tool_description)
                                            .color(Color::Muted)
                                            .size(LabelSize::XSmall),
                                    ),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "bg-toggle-{}",
                                        toggle_tool_id
                                    )),
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
                                    let _ = toggle_entity.update(cx, |this, cx| {
                                        if this.background_tools.contains(&tool_id) {
                                            this.background_tools.remove(&tool_id);
                                        } else {
                                            this.background_tools.insert(tool_id);
                                        }
                                        write_background_tools(&this.background_tools);
                                        cx.notify();
                                    });
                                }),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "globe-{}",
                                        globe_tool_id
                                    )),
                                    IconName::Keyboard,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Create global shortcut"))
                                .on_click(move |_, window, cx| {
                                    let tool_id = globe_tool_id.clone();
                                    let tool_label = globe_tool_label.clone();
                                    let _ = globe_entity.update(cx, |this, cx| {
                                        this.open_global_shortcut_modal(
                                            tool_id,
                                            tool_label,
                                            window,
                                            cx,
                                        );
                                    });
                                }),
                            ),
                    )
                    .on_click(move |_, window, cx| {
                        let runtime_path = runtime_path.clone();
                        let agent_tools_path = agent_tools_path.clone();
                        let pasta_ativa = pasta_ativa.clone();
                        let session_path = session_path.clone();
                        if is_background {
                            let _ = workspace.update(cx, |_workspace, cx| {
                                Self::spawn_tool_background(
                                    &tool,
                                    &runtime_path,
                                    &agent_tools_path,
                                    &session_path,
                                    &pasta_ativa,
                                    cx,
                                );
                            });
                        } else {
                            let _ = workspace.update(cx, |workspace, cx| {
                                Self::spawn_tool_entry(
                                    &tool,
                                    &runtime_path,
                                    &agent_tools_path,
                                    &session_path,
                                    &pasta_ativa,
                                    workspace,
                                    window,
                                    cx,
                                );
                            });
                        }
                    })
            })
            .collect()
    }

    fn render_featured_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let buttons = self.build_tool_buttons(ToolTier::Featured, ButtonSize::Large, cx);
        v_flex()
            .w_full()
            .gap_1()
            .child(Self::section_header(ToolTier::Featured.label()))
            .when_some(self.tools_error.as_ref(), |el, err| {
                el.child(
                    Label::new(format!("Parse error: {}", err))
                        .color(Color::Error)
                        .size(LabelSize::XSmall),
                )
            })
            .children(buttons)
    }

    fn render_standard_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let buttons = self.build_tool_buttons(ToolTier::Standard, ButtonSize::Medium, cx);
        v_flex()
            .w_full()
            .gap_1()
            .child(Self::section_header(ToolTier::Standard.label()))
            .children(buttons)
    }

    /// Render the compact tools section (small 2-column grid, label only, tooltip for description).
    fn render_compact_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();

        let compact: Vec<ToolEntry> = self
            .tools
            .iter()
            .filter(|t| t.tier == ToolTier::Compact && !t.hidden)
            .cloned()
            .collect();

        let compact_buttons: Vec<_> = compact
            .into_iter()
            .map(|tool| {
                let tool_icon = icon_for_tool(&tool.icon);
                let tool_id = tool.id.clone();
                let tool_label: SharedString = tool.label.clone().into();
                let tool_description = tool.description.clone();

                let is_background = self.background_tools.contains(&tool_id);

                let toggle_tool_id = tool_id.clone();
                let toggle_entity = entity.clone();

                let globe_tool_id = tool_id.clone();
                let globe_tool_label = tool_label.to_string();
                let globe_entity = entity.clone();

                let runtime_path = self.runtime_path.clone();
                let agent_tools_path = self.agent_tools_path.clone();
                let workspace = self.workspace.clone();
                let session_path = self.session_path.clone();
                let pasta_ativa = self.pasta_ativa.clone();

                ButtonLike::new(format!("dashboard-btn-{}", tool_id))
                    .width(gpui::DefiniteLength::Fraction(0.48))
                    .size(ButtonSize::Compact)
                    .tooltip(Tooltip::text(tool_description))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .items_center()
                            .child(
                                Icon::new(tool_icon)
                                    .color(Color::Muted)
                                    .size(IconSize::XSmall),
                            )
                            .child(
                                Label::new(tool_label)
                                    .size(LabelSize::Small)
                                    .into_any_element(),
                            )
                            .child(div().flex_grow())
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "bg-toggle-{}",
                                        toggle_tool_id
                                    )),
                                    IconName::ToolTerminal,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(if is_background {
                                    Color::Muted
                                } else {
                                    Color::Accent
                                })
                                .tooltip(Tooltip::text(if is_background {
                                    "Background mode"
                                } else {
                                    "Terminal mode"
                                }))
                                .on_click(move |_, _, cx| {
                                    let tool_id = toggle_tool_id.clone();
                                    let _ = toggle_entity.update(cx, |this, cx| {
                                        if this.background_tools.contains(&tool_id) {
                                            this.background_tools.remove(&tool_id);
                                        } else {
                                            this.background_tools.insert(tool_id);
                                        }
                                        write_background_tools(&this.background_tools);
                                        cx.notify();
                                    });
                                }),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "globe-{}",
                                        globe_tool_id
                                    )),
                                    IconName::Keyboard,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .on_click(move |_, window, cx| {
                                    let tool_id = globe_tool_id.clone();
                                    let tool_label = globe_tool_label.clone();
                                    let _ =
                                        globe_entity.update(cx, |this, cx| {
                                            this.open_global_shortcut_modal(
                                                tool_id,
                                                tool_label,
                                                window,
                                                cx,
                                            );
                                        });
                                }),
                            ),
                    )
                    .on_click(move |_, window, cx| {
                        let runtime_path = runtime_path.clone();
                        let agent_tools_path = agent_tools_path.clone();
                        let pasta_ativa = pasta_ativa.clone();
                        let session_path = session_path.clone();
                        if is_background {
                            let _ = workspace.update(cx, |_workspace, cx| {
                                Self::spawn_tool_background(
                                    &tool,
                                    &runtime_path,
                                    &agent_tools_path,
                                    &session_path,
                                    &pasta_ativa,
                                    cx,
                                );
                            });
                        } else {
                            let _ = workspace.update(cx, |workspace, cx| {
                                Self::spawn_tool_entry(
                                    &tool,
                                    &runtime_path,
                                    &agent_tools_path,
                                    &session_path,
                                    &pasta_ativa,
                                    workspace,
                                    window,
                                    cx,
                                );
                            });
                        }
                    })
            })
            .collect();

        v_flex()
            .w_full()
            .gap_1()
            .child(Self::section_header(ToolTier::Compact.label()))
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_1()
                    .children(compact_buttons),
            )
    }

    fn render_automations_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let automations: Vec<AutomationEntry> = self.automations.iter().filter(|a| !a.hidden).cloned().collect();
        let backend = self.agent_backend;
        let badge_label: SharedString = backend.badge_label().into();
        let badge_color = backend.badge_color();

        let entity = cx.entity().downgrade();

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .px_1()
                    .mb_2()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new("AUTOMATIONS")
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    )
                    .child(Divider::horizontal().color(DividerColor::BorderVariant))
                    .child({
                        let entity_claude = entity.clone();
                        let entity_gemini = entity.clone();
                        let entity_copy = entity.clone();

                        ToggleButtonGroup::single_row(
                            "agent-backend-toggle",
                            [
                                ToggleButtonSimple::new("Claude", move |_, _, cx| {
                                    let _ = entity_claude.update(cx, |this, cx| {
                                        this.agent_backend = AgentBackend::Claude;
                                        cx.notify();
                                    });
                                }),
                                ToggleButtonSimple::new("Gemini", move |_, _, cx| {
                                    let _ = entity_gemini.update(cx, |this, cx| {
                                        this.agent_backend = AgentBackend::Gemini;
                                        cx.notify();
                                    });
                                }),
                                ToggleButtonSimple::new("Copy", move |_, _, cx| {
                                    let _ = entity_copy.update(cx, |this, cx| {
                                        this.agent_backend = AgentBackend::CopyOnly;
                                        cx.notify();
                                    });
                                }),
                            ],
                        )
                        .selected_index(backend.index())
                        .style(ToggleButtonGroupStyle::Outlined)
                        .auto_width()
                    }),
            )
            .when_some(self.automations_error.as_ref(), |el, err| {
                el.child(
                    Label::new(format!("Parse error: {}", err))
                        .color(Color::Error)
                        .size(LabelSize::XSmall),
                )
            })
            .when(automations.is_empty(), |el| {
                el.child(
                    h_flex()
                        .px_2()
                        .child(
                            Label::new("No automations found (AUTOMATIONS.toml)")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
            })
            .children(automations.into_iter().enumerate().map({
                move |(idx, entry)| {
                    let icon = icon_for_automation(&entry.icon);
                    let entry_id = entry.id.clone();
                    let entry_label: SharedString = entry.label.clone().into();
                    let entry_description: SharedString = entry.description.clone().into();
                    let entry_prompt = entry.prompt;
                    let badge_label = badge_label.clone();

                    let click_entity = entity.clone();
                    let click_id = entry_id.clone();
                    let click_label = entry_label.clone();
                    let click_prompt = entry_prompt;

                    ButtonLike::new(format!("automation-btn-{}-{}", entry_id, idx))
                        .full_width()
                        .size(ButtonSize::Medium)
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    Icon::new(icon)
                                        .color(Color::Accent)
                                        .size(IconSize::Small),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .items_start()
                                        .child(Label::new(entry_label))
                                        .child(
                                            Label::new(entry_description)
                                                .color(Color::Muted)
                                                .size(LabelSize::XSmall),
                                        ),
                                )
                                .child(
                                    Label::new(badge_label)
                                        .color(badge_color)
                                        .size(LabelSize::XSmall),
                                ),
                        )
                        .on_click(move |_, window, cx| {
                            let _ = click_entity.update(cx, |this, cx| {
                                this.run_automation(
                                    &click_id,
                                    &click_label,
                                    &click_prompt,
                                    window,
                                    cx,
                                );
                            });
                        })
                }
            }))
            .child(
                ButtonLike::new("edit-automations-btn")
                    .full_width()
                    .size(ButtonSize::Medium)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                Icon::new(IconName::FileToml)
                                    .color(Color::Muted)
                                    .size(IconSize::Small),
                            )
                            .child(
                                Label::new("Edit Automations (TOML)")
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            ),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        let user_path = automations_user_toml_path();
                        if !user_path.exists() {
                            std::fs::write(&user_path, "# User automation overrides — entries here override defaults in AUTOMATIONS.toml\n# Add hidden = true to hide a default automation, or define custom ones.\n\n").log_err();
                        }
                        let workspace = this.workspace.clone();
                        cx.spawn_in(window, async move |_this, cx| {
                            let _ = workspace.update_in(cx, |workspace, window, cx| {
                                workspace
                                    .open_abs_path(user_path, OpenOptions::default(), window, cx)
                                    .detach();
                            });
                        })
                        .detach();
                    }))
            )
    }

    fn render_ai_agents_section(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let cwd = self.agent_cwd();

        // Resolve actual binary paths so we can run them directly without a
        // shell wrapper (avoids `.zshrc` errors from `-i` flag).
        let claude_bin = resolve_bin("claude");
        let gemini_bin = resolve_bin("gemini");

        let agents: Vec<(&str, &str, String, Vec<String>)> = vec![
            ("ai-claude", "Open Claude", claude_bin, vec![]),
            ("ai-gemini", "Open Gemini", gemini_bin, vec![]),
        ];

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .px_1()
                    .mb_2()
                    .gap_2()
                    .child(
                        Label::new("AI AGENTS")
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    )
                    .child(Divider::horizontal().color(DividerColor::BorderVariant)),
            )
            .children(agents.into_iter().map({
                move |(id, label, program, args)| {
                    let workspace = workspace.clone();
                    let cwd = cwd.clone();

                    ButtonLike::new(id)
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
                                .child(Label::new(label)),
                        )
                        .on_click(move |_, window, cx| {
                            let workspace = workspace.clone();
                            let args = args.clone();
                            let program = program.clone();
                            let cwd = cwd.clone();
                            let _ = workspace.update(cx, |workspace, cx| {
                                let spawn = SpawnInTerminal {
                                    id: TaskId(format!("ai-agent-{}", id)),
                                    label: label.to_string(),
                                    full_label: label.to_string(),
                                    command_label: label.to_string(),
                                    cwd: Some(cwd),
                                    shell: Shell::WithArguments {
                                        program,
                                        args,
                                        title_override: Some(label.to_string()),
                                    },
                                    use_new_terminal: true,
                                    allow_concurrent_runs: false,
                                    reveal: RevealStrategy::Always,
                                    ..Default::default()
                                };
                                workspace.spawn_in_terminal(spawn, window, cx).detach();
                            });
                        })
                }
            }))
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
                    let pasta_ativa = this.pasta_ativa.clone();
                    if is_background {
                        let _ = this.workspace.update(cx, |_workspace, cx| {
                            Self::spawn_tool_background(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &session_path,
                                &pasta_ativa,
                                cx,
                            );
                        });
                    } else {
                        let _ = this.workspace.update(cx, |workspace, cx| {
                            Self::spawn_tool_entry(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &session_path,
                                &pasta_ativa,
                                workspace,
                                window,
                                cx,
                            );
                        });
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
                    .px_12()
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
                                    .justify_center()
                                    .mb_4()
                                    .gap_4()
                                    .child(
                                        Icon::new(IconName::AudioOn)
                                            .size(IconSize::XLarge)
                                            .color(Color::Accent),
                                    )
                                    .child(
                                        v_flex()
                                            .child(
                                                Headline::new("ProTools Studio Dashboard")
                                                    .size(HeadlineSize::Small),
                                            )
                                            .child(
                                                Label::new(
                                                    "Audio post-production tools",
                                                )
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .italic(),
                                            ),
                                    ),
                            )
                            // Session status bar
                            .child(self.render_session_status(cx))
                            // Pasta ativa
                            .child(self.render_pasta_ativa(cx))
                            .child(self.render_pastas_recentes(cx))
                            // Three-tier tool layout
                            .child(self.render_featured_section(cx))
                            .child(self.render_standard_section(cx))
                            .child(self.render_compact_section(cx))
                            // Edit Tools button
                            .child(
                                ButtonLike::new("edit-tools-btn")
                                    .full_width()
                                    .size(ButtonSize::Medium)
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .child(
                                                Icon::new(IconName::FileToml)
                                                    .color(Color::Muted)
                                                    .size(IconSize::Small),
                                            )
                                            .child(
                                                Label::new("Edit Tools (TOML)")
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            ),
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let user_path = tools_user_toml_path();
                                        if !user_path.exists() {
                                            std::fs::write(&user_path, "# User tool overrides — entries here override defaults in TOOLS.toml\n# Add hidden = true to hide a default tool, or define custom tools.\n\n").log_err();
                                        }
                                        let workspace = this.workspace.clone();
                                        cx.spawn_in(window, async move |_this, cx| {
                                            let _ = workspace.update_in(cx, |workspace, window, cx| {
                                                workspace
                                                    .open_abs_path(user_path, OpenOptions::default(), window, cx)
                                                    .detach();
                                            });
                                        })
                                        .detach();
                                    })),
                            )
                            // AI Agents
                            .child(self.render_ai_agents_section(cx))
                            // Automations
                            .child(self.render_automations_section(cx))
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

    fn prevent_close(&self) -> bool {
        true
    }

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(ItemEvent)) {
        f(*event)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn find_latest_update_returns_none_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_latest_update_in("TOOLS", dir.path()).is_none());
    }

    #[test]
    fn find_latest_update_returns_none_when_no_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("unrelated.toml"), "").unwrap();
        fs::write(dir.path().join("TOOLS.toml"), "").unwrap();
        assert!(find_latest_update_in("TOOLS", dir.path()).is_none());
    }

    #[test]
    fn find_latest_update_picks_highest_number() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("TOOLS.update.1.toml"), "").unwrap();
        fs::write(dir.path().join("TOOLS.update.5.toml"), "").unwrap();
        fs::write(dir.path().join("TOOLS.update.3.toml"), "").unwrap();

        let result = find_latest_update_in("TOOLS", dir.path()).unwrap();
        assert_eq!(result.file_name().unwrap(), "TOOLS.update.5.toml");
    }

    #[test]
    fn find_latest_update_ignores_non_numeric() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("TOOLS.update.abc.toml"), "").unwrap();
        fs::write(dir.path().join("TOOLS.update.2.toml"), "").unwrap();
        fs::write(dir.path().join("TOOLS.update..toml"), "").unwrap();

        let result = find_latest_update_in("TOOLS", dir.path()).unwrap();
        assert_eq!(result.file_name().unwrap(), "TOOLS.update.2.toml");
    }

    #[test]
    fn find_latest_update_ignores_wrong_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AUTOMATIONS.update.10.toml"), "").unwrap();
        fs::write(dir.path().join("TOOLS.update.1.toml"), "").unwrap();

        let tools = find_latest_update_in("TOOLS", dir.path()).unwrap();
        assert_eq!(tools.file_name().unwrap(), "TOOLS.update.1.toml");

        let autos = find_latest_update_in("AUTOMATIONS", dir.path()).unwrap();
        assert_eq!(autos.file_name().unwrap(), "AUTOMATIONS.update.10.toml");
    }

    #[test]
    fn find_latest_update_ignores_wrong_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("TOOLS.update.1.json"), "").unwrap();
        fs::write(dir.path().join("TOOLS.update.2.txt"), "").unwrap();
        assert!(find_latest_update_in("TOOLS", dir.path()).is_none());
    }

    #[test]
    fn find_latest_update_single_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("TOOLS.update.42.toml"), "").unwrap();

        let result = find_latest_update_in("TOOLS", dir.path()).unwrap();
        assert_eq!(result.file_name().unwrap(), "TOOLS.update.42.toml");
    }

    #[test]
    fn version_check_writes_new_version_on_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let version_file = dir.path().join(".release_version");
        let updates = dir.path().join("updates");
        fs::create_dir_all(&updates).unwrap();

        check_version_and_purge("1.0.0", &version_file, &updates);

        assert_eq!(fs::read_to_string(&version_file).unwrap(), "1.0.0");
        // updates/ should still exist (not purged on fresh install)
        assert!(updates.is_dir());
    }

    #[test]
    fn version_check_noop_when_versions_match() {
        let dir = tempfile::tempdir().unwrap();
        let version_file = dir.path().join(".release_version");
        let updates = dir.path().join("updates");
        fs::create_dir_all(&updates).unwrap();
        fs::write(dir.path().join("updates/TOOLS.update.1.toml"), "keep").unwrap();
        fs::write(&version_file, "1.0.0").unwrap();

        check_version_and_purge("1.0.0", &version_file, &updates);

        // Update file should survive — versions match
        assert!(dir.path().join("updates/TOOLS.update.1.toml").exists());
        assert_eq!(fs::read_to_string(&version_file).unwrap(), "1.0.0");
    }

    #[test]
    fn version_check_purges_updates_on_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let version_file = dir.path().join(".release_version");
        let updates = dir.path().join("updates");
        fs::create_dir_all(&updates).unwrap();
        fs::write(dir.path().join("updates/TOOLS.update.1.toml"), "stale").unwrap();
        fs::write(dir.path().join("updates/TOOLS.update.2.toml"), "stale").unwrap();
        fs::write(&version_file, "1.0.0").unwrap();

        check_version_and_purge("1.1.0", &version_file, &updates);

        // Version file updated
        assert_eq!(fs::read_to_string(&version_file).unwrap(), "1.1.0");
        // updates/ recreated empty
        assert!(updates.is_dir());
        assert_eq!(fs::read_dir(&updates).unwrap().count(), 0);
    }

    #[test]
    fn merge_tools_user_overrides_default() {
        let defaults = vec![tool("a", "Default A"), tool("b", "Default B")];
        let user = vec![tool("a", "Custom A")];

        let merged = merge_tools(defaults, user);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].label, "Custom A");
        assert_eq!(merged[1].label, "Default B");
    }

    #[test]
    fn merge_tools_user_adds_new() {
        let defaults = vec![tool("a", "Default A")];
        let user = vec![tool("c", "User Only")];

        let merged = merge_tools(defaults, user);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].label, "Default A");
        assert_eq!(merged[1].label, "User Only");
    }

    #[test]
    fn merge_tools_empty_defaults() {
        let defaults = vec![];
        let user = vec![tool("a", "User A")];

        let merged = merge_tools(defaults, user);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].label, "User A");
    }

    fn tool(id: &str, label: &str) -> ToolEntry {
        ToolEntry {
            id: id.to_string(),
            label: label.to_string(),
            description: String::new(),
            icon: String::new(),
            binary: String::new(),
            cwd: String::new(),
            source: ToolSource::Runtime,
            tier: ToolTier::Standard,
            needs_session: false,
            extra_args: vec![],
            hidden: false,
        }
    }
}
