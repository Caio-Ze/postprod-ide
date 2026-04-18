use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager as NativeHotKeyManager,
    hotkey::{Code, HotKey, Modifiers as GHModifiers},
};
use gpui::{
    App, AsyncApp, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Keystroke, KeystrokeEvent, Render, Subscription, Window,
};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use ui::{
    Button, ButtonStyle, Label, LabelSize, Modal, ModalFooter, ModalHeader, Section, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    ModalView, MultiWorkspace,
    notifications::{
        NotificationId, show_app_notification, simple_message_notification::MessageNotification,
    },
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use postprod_dashboard_config::{ToolEntry, load_tools_registry, state_dir_for};

use crate::config::{
    ensure_global_hotkeys_config, global_hotkeys_toml_path, load_global_hotkeys_config,
};
use crate::dashboard_paths::{resolve_agent_tools_path, resolve_runtime_path, suite_root};
use crate::persistence::read_background_tools;
use crate::resolve_tool_command;

// ---------------------------------------------------------------------------
// Key mapping
// ---------------------------------------------------------------------------

pub(crate) fn gpui_key_to_code(key: &str) -> Option<Code> {
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

pub(crate) fn parse_global_hotkey(keystroke_str: &str) -> Option<HotKey> {
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

pub(crate) fn keystroke_to_display(keystroke_str: &str) -> String {
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

// ---------------------------------------------------------------------------
// Resolved hotkey entry — per-shortcut data including its own config_root
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct ResolvedHotkeyEntry {
    pub(crate) tool_id: String,
    pub(crate) config_root: PathBuf,
    pub(crate) tool: Option<ToolEntry>,
    pub(crate) keystroke_display: String,
}

// ---------------------------------------------------------------------------
// Helper functions for reading volatile state from disk
// ---------------------------------------------------------------------------

fn expand_tilde(path_str: &str) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path_str)
}

pub(crate) fn read_state_string_pub(path: &Path) -> Option<String> {
    read_state_string(path)
}

pub(crate) fn read_param_values_pub(path: &Path, tool_id: &str) -> HashMap<String, String> {
    read_param_values(path, tool_id)
}

fn read_state_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_param_values(path: &Path, tool_id: &str) -> HashMap<String, String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(table) = content.parse::<toml::Table>() else {
        return HashMap::new();
    };
    table
        .get(tool_id)
        .and_then(|v| v.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Notification helper
// ---------------------------------------------------------------------------

struct GlobalHotkeyNotification;

fn show_hotkey_error(cx: &mut App, message: String) {
    show_app_notification(
        NotificationId::unique::<GlobalHotkeyNotification>(),
        cx,
        move |cx| cx.new(|cx| MessageNotification::new(message.clone(), cx)),
    );
}

// ---------------------------------------------------------------------------
// Self-contained hotkey dispatch — no Dashboard traversal
// ---------------------------------------------------------------------------

fn dispatch_hotkey_tool(tool_id: &str, hotkey_entries: &[ResolvedHotkeyEntry], cx: &mut App) {
    let Some(entry) = hotkey_entries.iter().find(|e| e.tool_id == tool_id) else {
        log::warn!(
            "global hotkey: tool '{}' not found in resolved entries",
            tool_id
        );
        show_hotkey_error(
            cx,
            format!(
                "Hotkey failed: tool '{}' not found in dashboard config",
                tool_id
            ),
        );
        return;
    };

    let Some(tool) = &entry.tool else {
        log::warn!("global hotkey: tool config for '{}' not loaded", tool_id);
        show_hotkey_error(
            cx,
            format!(
                "Hotkey failed: tool '{}' not found in config at {}",
                tool_id,
                entry.config_root.display()
            ),
        );
        return;
    };

    let runtime_path = resolve_runtime_path();
    let agent_tools_path = resolve_agent_tools_path();

    let state_dir = state_dir_for(&entry.config_root);
    let active_folder = read_state_string(&state_dir.join("active_folder")).map(PathBuf::from);
    let session_path = if tool.needs_session {
        read_state_string(&state_dir.join("session_path"))
    } else {
        None
    };
    let param_values = read_param_values(&state_dir.join("param_values.toml"), tool_id);

    let (command, args, cwd, env) = resolve_tool_command(
        tool,
        &runtime_path,
        &agent_tools_path,
        &entry.config_root,
        &session_path,
        &active_folder,
        &param_values,
    );

    let background_tools = read_background_tools(&entry.config_root);
    let is_background = background_tools.contains(tool_id);

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
                        log::info!("background hotkey tool '{}': success", tool_label);
                    }
                    Ok(output) => {
                        log::warn!(
                            "background hotkey tool '{}': exit {}: {}",
                            tool_label,
                            output.status,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    Err(e) => {
                        log::error!("background hotkey tool '{}': {}", tool_label, e);
                    }
                }
            })
            .detach();
        return;
    }

    let multi_workspace_handle = cx
        .active_window()
        .and_then(|w| w.downcast::<MultiWorkspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|w| w.downcast::<MultiWorkspace>())
        });

    let Some(multi_workspace) = multi_workspace_handle else {
        log::warn!("global hotkey: no workspace open for tool '{}'", tool_id);
        show_hotkey_error(cx, format!("Hotkey '{}': no workspace open", tool.label));
        return;
    };

    let tool_label = tool.label.clone();
    let tool_id_owned = tool.id.clone();

    multi_workspace
        .update(cx, |multi_workspace, window, cx| {
            let workspace_entity = multi_workspace.workspace().clone();
            workspace_entity.update(cx, |workspace, cx| {
                cx.activate(true);
                let mut spawn = SpawnInTerminal {
                    id: TaskId(format!("dashboard-{}", tool_id_owned)),
                    label: tool_label.clone(),
                    full_label: tool_label.clone(),
                    command: Some(command),
                    args,
                    command_label: tool_label.clone(),
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
            });
        })
        .log_err();
}

// ---------------------------------------------------------------------------
// GlobalHotkeyManager — system-wide shortcuts via CGEventTap
// ---------------------------------------------------------------------------

pub(crate) struct GlobalHotkeyManager {
    native_manager: NativeHotKeyManager,
    hotkey_map: HashMap<u32, String>,
    registered_hotkeys: Vec<HotKey>,
    last_config_content: String,
    hotkey_entries: Vec<ResolvedHotkeyEntry>,
    keystroke_map: HashMap<String, String>,
    _poll_task: gpui::Task<()>,
    _bridge_task: gpui::Task<()>,
    _watch_task: gpui::Task<()>,
}

pub(crate) struct GlobalHotkeyManagerHandle(pub(crate) Entity<GlobalHotkeyManager>);

impl gpui::Global for GlobalHotkeyManagerHandle {}

impl GlobalHotkeyManager {
    fn register_hotkeys_from_config(&mut self) {
        for hotkey in &self.registered_hotkeys {
            self.native_manager.unregister(*hotkey).log_err();
        }
        self.registered_hotkeys.clear();
        self.hotkey_map.clear();
        self.hotkey_entries.clear();
        self.keystroke_map.clear();

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

                    let config_root = entry
                        .config_root
                        .map(|s| expand_tilde(&s))
                        .unwrap_or_else(suite_root);

                    let (tools, _) = load_tools_registry(&config_root);
                    let tool = tools.into_iter().find(|t| t.id == entry.tool_id);

                    let keystroke_display = keystroke_to_display(&entry.keystroke);
                    self.keystroke_map
                        .insert(entry.tool_id.clone(), keystroke_display.clone());

                    self.hotkey_entries.push(ResolvedHotkeyEntry {
                        tool_id: entry.tool_id.clone(),
                        config_root,
                        tool,
                        keystroke_display,
                    });
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

    #[allow(dead_code)]
    pub(crate) fn hotkey_display_for(&self, tool_id: &str) -> Option<String> {
        self.keystroke_map.get(tool_id).cloned()
    }

    pub(crate) fn all_hotkey_entries(&self) -> &[ResolvedHotkeyEntry] {
        &self.hotkey_entries
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
        let (async_tx, async_rx) = smol::channel::unbounded();

        let bridge_task = cx.spawn({
            let receiver = receiver.clone();
            async move |_this, cx: &mut AsyncApp| {
                loop {
                    let event = cx
                        .background_executor()
                        .spawn({
                            let receiver = receiver.clone();
                            async move { smol::unblock(move || receiver.recv()).await }
                        })
                        .await;

                    match event {
                        Ok(e) => {
                            if async_tx.send(e).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        let poll_task = cx.spawn({
            async move |this, cx: &mut AsyncApp| {
                while let Ok(event) = async_rx.recv().await {
                    if event.state() == global_hotkey::HotKeyState::Released {
                        continue;
                    }
                    let hotkey_id = event.id();
                    let dispatch_data = this
                        .update(cx, |manager: &mut GlobalHotkeyManager, _cx| {
                            manager
                                .hotkey_map
                                .get(&hotkey_id)
                                .map(|tool_id| (tool_id.clone(), manager.hotkey_entries.clone()))
                        })
                        .ok()
                        .flatten();

                    if let Some((tool_id, entries)) = dispatch_data {
                        log::info!("global hotkey: triggered tool '{tool_id}'");
                        cx.update(|cx| {
                            dispatch_hotkey_tool(&tool_id, &entries, cx);
                        });
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
                })
                .log_err();
            }
        });

        let mut manager = GlobalHotkeyManager {
            native_manager,
            hotkey_map: HashMap::new(),
            registered_hotkeys: Vec::new(),
            last_config_content: String::new(),
            hotkey_entries: Vec::new(),
            keystroke_map: HashMap::new(),
            _poll_task: poll_task,
            _bridge_task: bridge_task,
            _watch_task: watch_task,
        };
        manager.register_hotkeys_from_config();
        manager
    });

    cx.set_global(GlobalHotkeyManagerHandle(entity));
}

pub(crate) fn save_global_hotkey(keystroke_str: &str, tool_id: &str, config_root: &Path) {
    let path = global_hotkeys_toml_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    // Remove any existing entry for this tool_id to avoid duplicates.
    // Use toml_edit for reliable round-trip editing.
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_default();

    if let Some(hotkeys) = doc
        .get_mut("hotkey")
        .and_then(|v| v.as_array_of_tables_mut())
    {
        hotkeys.retain(|table| {
            table
                .get("tool_id")
                .and_then(|v| v.as_str())
                .map(|id| id != tool_id)
                .unwrap_or(true)
        });
    }

    // Append the new entry
    let mut new_table = toml_edit::Table::new();
    new_table.insert("keystroke", toml_edit::value(keystroke_str));
    new_table.insert("tool_id", toml_edit::value(tool_id));

    let config_root_str = config_root.to_string_lossy().to_string();
    new_table.insert("config_root", toml_edit::value(&config_root_str));

    if let Some(hotkeys) = doc
        .get_mut("hotkey")
        .and_then(|v| v.as_array_of_tables_mut())
    {
        hotkeys.push(new_table);
    } else {
        let mut array = toml_edit::ArrayOfTables::new();
        array.push(new_table);
        doc.insert("hotkey", toml_edit::Item::ArrayOfTables(array));
    }

    std::fs::write(&path, doc.to_string()).log_err();
}

// ---------------------------------------------------------------------------
// Global Shortcut Modal — keystroke capture UI
// ---------------------------------------------------------------------------

pub(crate) struct GlobalShortcutModal {
    tool_id: String,
    tool_label: String,
    config_root: PathBuf,
    captured_keystroke: Option<String>,
    focus_handle: FocusHandle,
    _intercept_subscription: Option<Subscription>,
}

impl GlobalShortcutModal {
    pub(crate) fn new(
        tool_id: String,
        tool_label: String,
        config_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let listener = cx.listener(|this, event: &KeystrokeEvent, _window, cx| {
            let keystroke = &event.keystroke;
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
            config_root,
            captured_keystroke: None,
            focus_handle,
            _intercept_subscription: Some(intercept_sub),
        }
    }

    pub(crate) fn save_and_dismiss(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(keystroke_str) = &self.captured_keystroke {
            save_global_hotkey(keystroke_str, &self.tool_id, &self.config_root);
            log::info!("global hotkey: saved {} -> {}", keystroke_str, self.tool_id);

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

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::Code;

    #[test]
    fn test_gpui_key_to_code_letters() {
        assert_eq!(gpui_key_to_code("a"), Some(Code::KeyA));
        assert_eq!(gpui_key_to_code("m"), Some(Code::KeyM));
        assert_eq!(gpui_key_to_code("z"), Some(Code::KeyZ));
    }

    #[test]
    fn test_gpui_key_to_code_digits() {
        assert_eq!(gpui_key_to_code("0"), Some(Code::Digit0));
        assert_eq!(gpui_key_to_code("5"), Some(Code::Digit5));
        assert_eq!(gpui_key_to_code("9"), Some(Code::Digit9));
    }

    #[test]
    fn test_gpui_key_to_code_function_keys() {
        assert_eq!(gpui_key_to_code("f1"), Some(Code::F1));
        assert_eq!(gpui_key_to_code("f12"), Some(Code::F12));
    }

    #[test]
    fn test_gpui_key_to_code_special_keys() {
        assert_eq!(gpui_key_to_code("space"), Some(Code::Space));
        assert_eq!(gpui_key_to_code("enter"), Some(Code::Enter));
        assert_eq!(gpui_key_to_code("escape"), Some(Code::Escape));
        assert_eq!(gpui_key_to_code("up"), Some(Code::ArrowUp));
        assert_eq!(gpui_key_to_code("down"), Some(Code::ArrowDown));
        assert_eq!(gpui_key_to_code("left"), Some(Code::ArrowLeft));
        assert_eq!(gpui_key_to_code("right"), Some(Code::ArrowRight));
        assert_eq!(gpui_key_to_code("-"), Some(Code::Minus));
        assert_eq!(gpui_key_to_code("/"), Some(Code::Slash));
        assert_eq!(gpui_key_to_code("`"), Some(Code::Backquote));
    }

    #[test]
    fn test_gpui_key_to_code_unknown_returns_none() {
        assert_eq!(gpui_key_to_code("capslock"), None);
        assert_eq!(gpui_key_to_code(""), None);
        assert_eq!(gpui_key_to_code("nonexistent"), None);
    }

    #[test]
    fn test_parse_global_hotkey_with_modifiers() {
        let hotkey = parse_global_hotkey("ctrl-alt-a");
        assert!(hotkey.is_some());
        let hotkey = hotkey.unwrap();
        let mods = hotkey.mods;
        assert!(mods.contains(GHModifiers::CONTROL));
        assert!(mods.contains(GHModifiers::ALT));
        assert!(!mods.contains(GHModifiers::SHIFT));
    }

    #[test]
    fn test_parse_global_hotkey_invalid_returns_none() {
        assert!(parse_global_hotkey("ctrl-nonexistent").is_none());
    }

    #[test]
    fn test_keystroke_to_display_symbols() {
        let display = keystroke_to_display("ctrl-shift-cmd-a");
        assert!(display.contains('\u{2303}')); // ⌃ Control
        assert!(display.contains('\u{21E7}')); // ⇧ Shift
        assert!(display.contains('\u{2318}')); // ⌘ Command
        assert!(display.contains('a'));
    }

    #[test]
    fn test_read_state_string_missing_file() {
        assert!(read_state_string(Path::new("/nonexistent/path")).is_none());
    }

    #[test]
    fn test_read_param_values_missing_file() {
        let result = read_param_values(Path::new("/nonexistent/path"), "tool");
        assert!(result.is_empty());
    }

    #[test]
    fn test_read_param_values_with_data() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("param_values.toml");
        std::fs::write(
            &path,
            r#"
[bounceAll]
format = "wav"
depth = "24"

[other]
key = "val"
"#,
        )?;
        let result = read_param_values(&path, "bounceAll");
        assert_eq!(result.get("format").map(|s| s.as_str()), Some("wav"));
        assert_eq!(result.get("depth").map(|s| s.as_str()), Some("24"));
        assert_eq!(result.len(), 2);

        let empty = read_param_values(&path, "nonexistent");
        assert!(empty.is_empty());
        Ok(())
    }
}
