use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager as NativeHotKeyManager,
    hotkey::{Code, HotKey, Modifiers as GHModifiers},
};
use gpui::{
    App, AsyncApp, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Keystroke, KeystrokeEvent, Render, Subscription, Window,
};
use ui::{
    Button, ButtonStyle, Label, LabelSize, Modal, ModalFooter, ModalHeader, Section,
    prelude::*,
};
use workspace::ModalView;
use util::ResultExt as _;

use std::collections::HashMap;
use std::time::Duration;

use crate::config::{
    ensure_global_hotkeys_config, global_hotkeys_toml_path, load_global_hotkeys_config,
};
use crate::dispatch_global_tool;

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
// GlobalHotkeyManager — system-wide shortcuts via CGEventTap
// ---------------------------------------------------------------------------

pub(crate) struct GlobalHotkeyManager {
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

pub(crate) fn save_global_hotkey(keystroke_str: &str, tool_id: &str) {
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

pub(crate) struct GlobalShortcutModal {
    tool_id: String,
    tool_label: String,
    captured_keystroke: Option<String>,
    focus_handle: FocusHandle,
    _intercept_subscription: Option<Subscription>,
}

impl GlobalShortcutModal {
    pub(crate) fn new(
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

    pub(crate) fn save_and_dismiss(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
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
}
