//! DashboardItem-local config adapter module.
//!
//! The shared schema types, loaders, resolvers, and config-root path helpers
//! live in the `postprod_dashboard_config` crate. This file keeps only
//! dashboard-local concerns:
//!
//! - UI icon mapping for tools and automations (depends on `ui::IconName`)
//! - `FolderTarget` — panel-only helper enum
//! - `GlobalHotkeyEntry` + loader (separate `global-hotkeys.toml` schema)

use serde::{Deserialize, Serialize};
use ui::IconName;
use util::ResultExt as _;

use std::path::PathBuf;

#[derive(Clone, Copy)]
pub(crate) enum FolderTarget {
    Active,
    Destination,
}

pub(crate) fn icon_for_tool(name: &str) -> IconName {
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

pub(crate) fn icon_for_automation(name: &str) -> IconName {
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
// Global hotkey config types (loaded from ~/.config/postprod-ide/global-hotkeys.toml)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct GlobalHotkeyEntry {
    pub(crate) keystroke: String,
    pub(crate) tool_id: String,
    #[serde(default)]
    pub(crate) config_root: Option<String>,
}

#[derive(Deserialize)]
struct GlobalHotkeysFile {
    #[serde(default)]
    hotkey: Vec<GlobalHotkeyEntry>,
}

pub(crate) fn global_hotkeys_toml_path() -> PathBuf {
    paths::config_dir().join("global-hotkeys.toml")
}

pub(crate) fn ensure_global_hotkeys_config() {
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

pub(crate) fn load_global_hotkeys_config() -> Vec<GlobalHotkeyEntry> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icon_for_tool_known() {
        assert_eq!(icon_for_tool("play_filled"), IconName::PlayFilled);
        assert_eq!(icon_for_tool("mic"), IconName::Mic);
        assert_eq!(icon_for_tool("trash"), IconName::Trash);
        assert_eq!(icon_for_tool("folder"), IconName::Folder);
    }

    #[test]
    fn test_icon_for_tool_unknown_fallback() {
        assert_eq!(icon_for_tool("nonexistent"), IconName::Sparkle);
        assert_eq!(icon_for_tool(""), IconName::Sparkle);
    }
}
