use postprod_scheduler::CatchUpPolicy;
use serde::{Deserialize, Serialize};
use ui::IconName;
use util::ResultExt as _;

use std::path::{Path, PathBuf};

use crate::paths::{agents_toml_path_for, automations_dir_for, tools_config_dir_for};

// ---------------------------------------------------------------------------
// TOML-driven tool registry
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolTier {
    Featured,
    Standard,
    Compact,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolSource {
    Runtime,
    Agent,
    Local,
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

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
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

    #[serde(default)]
    pub(crate) use_context_launcher: bool,

    /// Filesystem path this entry was loaded from (set after deserialization).
    #[serde(skip)]
    pub(crate) source_path: Option<PathBuf>,

    #[serde(default)]
    pub(crate) schedule: Option<ScheduleConfig>,

    #[serde(default)]
    pub(crate) chain: Option<ChainConfig>,
}

fn default_timeout() -> u64 {
    3600
}

#[derive(Deserialize, Clone, Debug, Default)]
pub(crate) struct ScheduleConfig {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) cron: String,
    #[serde(default)]
    pub(crate) catch_up: CatchUpPolicy,
    #[serde(default = "default_timeout")]
    pub(crate) timeout: u64,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub(crate) struct ChainConfig {
    #[serde(default)]
    pub(crate) triggers: Vec<String>,
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
            Ok(mut entry) => {
                entry.source_path = Some(path.clone());
                automations.push(entry);
            }
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

    #[test]
    fn test_load_single_tool_valid() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("test-tool.toml");
        std::fs::write(
            &path,
            r#"
[tool]
id = "bounce"
label = "Bounce All"
description = "Bounces all tracks"
icon = "play_filled"
binary = "bounce-all"
source = "runtime"
tier = "featured"
"#,
        )?;
        let tool = load_single_tool(&path)?;
        assert_eq!(tool.id, "bounce");
        assert_eq!(tool.label, "Bounce All");
        assert_eq!(tool.tier, ToolTier::Featured);
        assert_eq!(tool.source, ToolSource::Runtime);
        // defaults
        assert_eq!(tool.order, 100);
        assert!(!tool.hidden);
        assert!(tool.params.is_empty());
        assert!(tool.extra_args.is_empty());
        Ok(())
    }

    #[test]
    fn test_load_single_tool_with_params() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("tool-with-params.toml");
        std::fs::write(
            &path,
            r#"
[tool]
id = "export"
label = "Export"
description = "Export stems"
icon = "sparkle"
binary = "export-stems"
source = "agent"
tier = "standard"
order = 10

[[tool.param]]
key = "format"
label = "Format"
placeholder = "wav"
default = "wav"

[[tool.param]]
key = "depth"
label = "Bit Depth"
param_type = "select"
options = ["16", "24", "32"]
"#,
        )?;
        let tool = load_single_tool(&path)?;
        assert_eq!(tool.params.len(), 2);
        assert_eq!(tool.params[0].key, "format");
        assert_eq!(tool.params[0].param_type, ParamType::Text);
        assert_eq!(tool.params[1].param_type, ParamType::Select);
        assert_eq!(tool.params[1].options, vec!["16", "24", "32"]);
        assert_eq!(tool.order, 10);
        Ok(())
    }

    #[test]
    fn test_load_single_tool_local_source() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("local-tool.toml");
        std::fs::write(
            &path,
            r#"
[tool]
id = "my-script"
label = "My Script"
description = "A local config script"
icon = "tool_terminal"
binary = "run.sh"
cwd = "scripts"
source = "local"
tier = "standard"
"#,
        )?;
        let tool = load_single_tool(&path)?;
        assert_eq!(tool.id, "my-script");
        assert_eq!(tool.source, ToolSource::Local);
        assert_eq!(tool.cwd, "scripts");
        assert_eq!(tool.binary, "run.sh");
        Ok(())
    }

    #[test]
    fn test_load_single_tool_invalid_toml() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("bad.toml");
        std::fs::write(&path, "this is not valid toml {{{")?;
        match load_single_tool(&path) {
            Err(err) => assert!(err.contains("bad.toml"), "error should mention filename: {err}"),
            Ok(_) => panic!("should fail on invalid TOML"),
        }
        Ok(())
    }

    #[test]
    fn test_load_single_automation_valid() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("auto.toml");
        std::fs::write(
            &path,
            r#"
id = "full-delivery"
label = "Full Delivery"
description = "Run full delivery pipeline"
icon = "play"
prompt = "Run delivery for {session_path}"
"#,
        )?;
        let auto = load_single_automation(&path)?;
        assert_eq!(auto.id, "full-delivery");
        assert_eq!(auto.prompt, "Run delivery for {session_path}");
        assert!(!auto.hidden);
        assert_eq!(auto.order, 100);
        Ok(())
    }

    #[test]
    fn test_collect_toml_files_recursive() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path();
        std::fs::write(dir.join("a.toml"), "")?;
        std::fs::write(dir.join("b.txt"), "")?;
        std::fs::create_dir(dir.join("sub"))?;
        std::fs::write(dir.join("sub/c.toml"), "")?;
        std::fs::write(dir.join("sub/d.json"), "")?;

        let files = collect_toml_files(dir);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.toml", "c.toml"]);
        Ok(())
    }

    #[test]
    fn test_collect_toml_files_empty_dir() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let files = collect_toml_files(tmp.path());
        assert!(files.is_empty());
        Ok(())
    }

    #[test]
    fn test_load_tools_registry_mixed() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let config_root = tmp.path();
        let tools_dir = config_root.join("config").join("tools");
        std::fs::create_dir_all(&tools_dir)?;

        // Valid tool
        std::fs::write(
            tools_dir.join("good1.toml"),
            r#"
[tool]
id = "t1"
label = "Tool 1"
description = "d"
icon = "sparkle"
binary = "t1"
source = "runtime"
tier = "standard"
"#,
        )?;

        // Another valid tool
        std::fs::write(
            tools_dir.join("good2.toml"),
            r#"
[tool]
id = "t2"
label = "Tool 2"
description = "d"
icon = "sparkle"
binary = "t2"
source = "agent"
tier = "compact"
"#,
        )?;

        // Invalid TOML
        std::fs::write(tools_dir.join("bad.toml"), "not valid {{")?;

        let (tools, error) = load_tools_registry(config_root);
        assert_eq!(tools.len(), 2);
        assert!(error.is_some());
        assert!(error.unwrap().contains("bad.toml"));
        Ok(())
    }
}
