use serde::{Deserialize, Serialize};
use ui::IconName;
use util::ResultExt as _;

use std::path::{Path, PathBuf};

use crate::paths::{agents_toml_path_for, automations_dir_for, tools_config_dir_for};

// ---------------------------------------------------------------------------
// TOML-driven tool registry
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolTier {
    Featured,
    Standard,
    Compact,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolSource {
    Runtime,
    Agent,
}

#[derive(Clone, Copy)]
pub(crate) enum FolderTarget {
    Active,
    Destination,
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct ParamEntry {
    pub(crate) key: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) placeholder: String,
    #[serde(default)]
    pub(crate) default: String,
    #[serde(default = "default_param_type")]
    pub(crate) param_type: ParamType,
    #[serde(default)]
    pub(crate) options: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ParamType {
    Text,
    Path,
    Select,
}

fn default_param_type() -> ParamType {
    ParamType::Text
}

pub(crate) fn default_order() -> u32 {
    100
}

#[derive(Deserialize, Clone)]
pub(crate) struct ToolEntry {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) icon: String,
    pub(crate) binary: String,
    #[serde(default)]
    pub(crate) cwd: String,
    pub(crate) source: ToolSource,
    pub(crate) tier: ToolTier,
    #[serde(default)]
    pub(crate) needs_session: bool,
    #[serde(default)]
    pub(crate) extra_args: Vec<String>,
    #[serde(default)]
    pub(crate) hidden: bool,
    #[serde(default)]
    pub(crate) section: Option<String>,
    #[serde(default = "default_order")]
    pub(crate) order: u32,
    #[serde(default, rename = "param")]
    pub(crate) params: Vec<ParamEntry>,
}

#[derive(Deserialize)]
struct SingleToolFile {
    tool: ToolEntry,
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

pub(crate) fn load_single_tool(path: &Path) -> Result<ToolEntry, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: SingleToolFile = toml::from_str(&content).map_err(|e| {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        format!("{filename}: {e}")
    })?;
    Ok(file.tool)
}

pub(crate) fn collect_toml_files(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return result;
    };
    let mut children: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            result.extend(collect_toml_files(&child));
        } else if child.extension().is_some_and(|ext| ext == "toml") {
            result.push(child);
        }
    }
    result
}

pub(crate) fn load_tools_registry(config_root: &Path) -> (Vec<ToolEntry>, Option<String>) {
    let dir = tools_config_dir_for(config_root);
    let paths = collect_toml_files(&dir);

    let mut tools = Vec::new();
    let mut errors = Vec::new();

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
pub(crate) struct AutomationEntry {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) icon: String,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) hidden: bool,
    #[serde(default)]
    pub(crate) section: Option<String>,
    #[serde(default = "default_order")]
    pub(crate) order: u32,
    #[serde(default, rename = "param")]
    pub(crate) params: Vec<ParamEntry>,
}

pub(crate) fn load_single_automation(path: &Path) -> Result<AutomationEntry, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str::<AutomationEntry>(&content).map_err(|e| {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        format!("{filename}: {e}")
    })
}

pub(crate) fn load_automations_registry(config_root: &Path) -> (Vec<AutomationEntry>, Option<String>) {
    let dir = automations_dir_for(config_root);
    if !dir.exists() {
        return (Vec::new(), Some(format!("cannot read {}", dir.display())));
    }
    let paths = collect_toml_files(&dir);

    let mut automations = Vec::new();
    let mut errors = Vec::new();

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
// Agent backends — loaded from TOML at runtime
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
pub(crate) struct BackendEntry {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) flags: String,
    #[serde(default)]
    pub(crate) prompt_flag: String,
}

#[derive(Deserialize, Clone)]
pub(crate) struct AgentEntry {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) flags: String,
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

pub(crate) fn load_agents_config(config_root: &Path) -> (Vec<BackendEntry>, Vec<AgentEntry>, Option<String>) {
    load_toml_agents(&agents_toml_path_for(config_root))
}

// ---------------------------------------------------------------------------
// Global hotkey config types (loaded from ~/.config/postprod-ide/global-hotkeys.toml)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
pub(crate) struct GlobalHotkeyEntry {
    pub(crate) keystroke: String,
    pub(crate) tool_id: String,
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
