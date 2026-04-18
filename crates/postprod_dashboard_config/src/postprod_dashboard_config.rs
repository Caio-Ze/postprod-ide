//! Headless config/domain layer for the dashboard.
//!
//! Owns the TOML schema types, the loaders that read dashboard config
//! (tools, automations, agents, prompts, default contexts, context scripts),
//! the `config_root`-relative path helpers those loaders use, and (via the
//! `edit` submodule) the round-trip TOML mutations that write automation,
//! pipeline, schedule, and context entries back to disk.
//!
//! This crate has no dependency on `gpui`, `ui`, `workspace`, `project`,
//! or `editor` — everything here is pure file/model logic and testable
//! without a window.

pub mod edit;
pub mod watcher_config;

use postprod_scheduler::CatchUpPolicy;
use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

// Maximum size for context content from a single folder entry or total context.
const CONTEXT_SIZE_CAP: usize = 150 * 1024;

// ---------------------------------------------------------------------------
// config_root path helpers
// ---------------------------------------------------------------------------

pub fn config_dir_for(config_root: &Path) -> PathBuf {
    config_root.join("config")
}

pub fn state_dir_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join(".state")
}

pub fn tools_config_dir_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join("tools")
}

pub fn automations_dir_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join("automations")
}

pub fn agents_toml_path_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join("AGENTS.toml")
}

// ---------------------------------------------------------------------------
// TOML-driven tool registry
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolTier {
    Featured,
    Standard,
    Compact,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Runtime,
    Agent,
    Local,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct ParamEntry {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub default: String,
    #[serde(default = "default_param_type")]
    pub param_type: ParamType,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    Text,
    Path,
    Select,
}

fn default_param_type() -> ParamType {
    ParamType::Text
}

pub fn default_order() -> u32 {
    100
}

#[derive(Deserialize, Clone)]
pub struct ToolEntry {
    pub id: String,
    pub label: String,
    pub description: String,
    pub icon: String,
    pub binary: String,
    #[serde(default)]
    pub cwd: String,
    pub source: ToolSource,
    pub tier: ToolTier,
    #[serde(default)]
    pub needs_session: bool,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default = "default_order")]
    pub order: u32,
    #[serde(default, rename = "param")]
    pub params: Vec<ParamEntry>,
}

#[derive(Deserialize)]
struct SingleToolFile {
    tool: ToolEntry,
}

pub fn load_single_tool(path: &Path) -> Result<ToolEntry, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: SingleToolFile = toml::from_str(&content).map_err(|e| {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        format!("{filename}: {e}")
    })?;
    Ok(file.tool)
}

pub fn collect_toml_files(dir: &Path) -> Vec<PathBuf> {
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

pub fn load_tools_registry(config_root: &Path) -> (Vec<ToolEntry>, Option<String>) {
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

#[derive(Deserialize, Serialize, Clone)]
pub struct ContextEntry {
    /// "path" or "script"
    #[serde(rename = "source")]
    pub source_type: String,

    pub label: String,

    /// For source = "path": absolute path to file or folder.
    #[serde(default)]
    pub path: Option<String>,

    /// For source = "script": filename of script in config/context-scripts/.
    #[serde(default)]
    pub script: Option<String>,

    /// If true (default), failure aborts context gathering.
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Deserialize, Clone)]
pub struct AutomationEntry {
    pub id: String,
    pub label: String,
    pub description: String,
    pub icon: String,

    /// Filename of the prompt .md file (searched in config/prompts/).
    #[serde(default)]
    pub prompt_file: Option<String>,

    /// Deprecated: inline prompt. Used as transition fallback when prompt_file is absent.
    #[serde(default)]
    pub prompt: String,

    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default = "default_order")]
    pub order: u32,
    #[serde(default, rename = "param")]
    pub params: Vec<ParamEntry>,

    /// Context entries gathered before the agent runs.
    #[serde(default, rename = "context")]
    pub contexts: Vec<ContextEntry>,

    /// If true, default context entries from config/default-context/ are skipped.
    #[serde(default)]
    pub skip_default_context: bool,

    /// Filesystem path this entry was loaded from (set after deserialization).
    #[serde(skip)]
    pub source_path: Option<PathBuf>,

    #[serde(default)]
    pub schedule: Option<ScheduleConfig>,

    #[serde(default)]
    pub chain: Option<ChainConfig>,

    /// Agent profile to activate when running via native agent backend.
    #[serde(default)]
    pub profile: Option<String>,

    #[serde(default, rename = "type")]
    pub entry_type: Option<String>,

    #[serde(default, rename = "step")]
    pub steps: Vec<PipelineStep>,
}

impl AutomationEntry {
    pub fn is_pipeline(&self) -> bool {
        self.entry_type.as_deref() == Some("pipeline")
    }
}

fn default_timeout() -> u64 {
    3600
}

fn default_auto_disable_after() -> u32 {
    5
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ScheduleConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cron: String,
    #[serde(default)]
    pub catch_up: CatchUpPolicy,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_auto_disable_after")]
    pub auto_disable_after: u32,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ChainConfig {
    #[serde(default)]
    pub triggers: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct PipelineStep {
    #[serde(default)]
    pub automation: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub group: Option<u32>,
}

impl PipelineStep {
    pub fn target_id(&self) -> Option<&str> {
        self.automation.as_deref().or(self.tool.as_deref())
    }

    pub fn is_tool(&self) -> bool {
        self.tool.is_some()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match (&self.automation, &self.tool) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (Some(_), Some(_)) => anyhow::bail!("step has both `automation` and `tool` — pick one"),
            (None, None) => anyhow::bail!("step has neither `automation` nor `tool`"),
        }
    }
}

pub fn load_single_automation(path: &Path, config_root: &Path) -> Result<AutomationEntry, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut entry = toml::from_str::<AutomationEntry>(&content).map_err(|e| {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        format!("{filename}: {e}")
    })?;

    // Resolve prompt: prompt_file takes precedence, inline prompt is fallback.
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    if let Some(ref prompt_filename) = entry.prompt_file {
        if !prompt_filename.is_empty() {
            let prompts_dir = config_root.join("config/prompts");
            match resolve_file_path(&prompts_dir, prompt_filename) {
                Ok(prompt_path) => {
                    entry.prompt = std::fs::read_to_string(&prompt_path).map_err(|e| {
                        format!(
                            "{filename}: failed to read prompt file '{}': {e}",
                            prompt_path.display()
                        )
                    })?;
                }
                Err(e) => return Err(format!("{filename}: {e}")),
            }
        }
    } else if !entry.prompt.is_empty() {
        log::warn!("{filename}: inline `prompt` is deprecated — use `prompt_file` instead");
    }

    // Validate pipeline steps — skip invalid ones with a warning
    if entry.is_pipeline() {
        entry.steps.retain(|step| match step.validate() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("{filename}: skipping invalid pipeline step: {e}");
                false
            }
        });
    }

    Ok(entry)
}

/// Search a directory recursively for a file matching the given bare filename.
/// Returns an error if zero or more than one match is found (ambiguity rejection).
pub fn resolve_file_by_name(dir: &Path, filename: &str) -> Result<PathBuf, String> {
    let matches = collect_files_by_name(dir, filename);
    match matches.len() {
        0 => Err(format!(
            "prompt file '{filename}' not found in {}",
            dir.display()
        )),
        1 => Ok(matches.into_iter().next().expect("checked len == 1")),
        n => Err(format!(
            "prompt file '{filename}' is ambiguous — {n} matches found in {}",
            dir.display(),
        )),
    }
}

fn collect_files_by_name(dir: &Path, target_name: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return result;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            result.extend(collect_files_by_name(&child, target_name));
        } else if child.file_name().is_some_and(|n| n == target_name) {
            result.push(child);
        }
    }
    result
}

/// Expand leading `~` to the user's home directory.
/// Returns the string unchanged if no `~` prefix or if home dir cannot be determined.
pub fn expand_tilde(path_str: &str) -> String {
    if path_str.starts_with('~') {
        match dirs::home_dir() {
            Some(home) => path_str.replacen('~', &home.to_string_lossy(), 1),
            None => path_str.to_string(),
        }
    } else {
        path_str.to_string()
    }
}

/// Resolve a file by path or by name search in a default directory.
/// - If `path_spec` contains '/', treat as a path (expand ~ if needed)
/// - Otherwise, search `default_dir` recursively for a matching filename
pub fn resolve_file_path(default_dir: &Path, path_spec: &str) -> Result<PathBuf, String> {
    if path_spec.contains('/') {
        let expanded = expand_tilde(path_spec);
        let p = PathBuf::from(&expanded);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("file '{}' does not exist", expanded));
    }
    resolve_file_by_name(default_dir, path_spec)
}

/// Load default context entries from config/default-context/ folder.
pub fn load_default_contexts(config_root: &Path) -> Vec<ContextEntry> {
    let dir = config_root.join("config/default-context");
    if !dir.exists() {
        return Vec::new();
    }

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();

    let mut contexts = Vec::new();
    for path in paths {
        let label = path
            .file_stem()
            .map(|s| s.to_string_lossy().replace(['-', '_'], " "))
            .unwrap_or_default();
        contexts.push(ContextEntry {
            source_type: "path".to_string(),
            label,
            path: Some(path.to_string_lossy().to_string()),
            script: None,
            required: true,
        });
    }
    contexts
}

/// Read a file or folder into a string, respecting the size cap.
/// For folders, reads all files recursively with filename headers.
pub fn read_path_context(path: &Path) -> Result<String, String> {
    if path.is_file() {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
        if content.len() > CONTEXT_SIZE_CAP {
            let truncated = &content[..CONTEXT_SIZE_CAP];
            Ok(format!("{truncated}\n[... truncated at 150KB]"))
        } else {
            Ok(content)
        }
    } else if path.is_dir() {
        read_folder_context(path)
    } else {
        Err(format!("'{}' is not a file or directory", path.display()))
    }
}

fn read_folder_context(dir: &Path) -> Result<String, String> {
    let mut output = String::new();
    let mut total_size: usize = 0;
    let mut file_count: usize = 0;
    let mut truncated = false;

    collect_folder_contents(
        dir,
        dir,
        &mut output,
        &mut total_size,
        &mut file_count,
        &mut truncated,
    );

    if truncated {
        output.push_str(&format!(
            "\n[... truncated at 150KB, {file_count} files total]"
        ));
    }
    Ok(output)
}

fn collect_folder_contents(
    base: &Path,
    dir: &Path,
    output: &mut String,
    total_size: &mut usize,
    file_count: &mut usize,
    truncated: &mut bool,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    children.sort();

    for child in children {
        if *truncated {
            return;
        }
        if child.is_dir() {
            collect_folder_contents(base, &child, output, total_size, file_count, truncated);
        } else if child.is_file() {
            let Ok(content) = std::fs::read_to_string(&child) else {
                continue;
            };
            let relative = child.strip_prefix(base).unwrap_or(&child);
            let header = format!("--- {} ---\n", relative.display());
            let entry_size = header.len() + content.len() + 1;

            if *total_size + entry_size > CONTEXT_SIZE_CAP {
                *truncated = true;
                return;
            }

            output.push_str(&header);
            output.push_str(&content);
            output.push('\n');
            *total_size += entry_size;
            *file_count += 1;
        }
    }
}

/// Collect executable script files from config/context-scripts/ recursively.
pub fn collect_context_scripts(config_root: &Path) -> Vec<PathBuf> {
    let dir = config_root.join("config/context-scripts");
    if !dir.exists() {
        return Vec::new();
    }
    collect_scripts_recursive(&dir)
}

fn collect_scripts_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return result;
    };
    let mut children: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            result.extend(collect_scripts_recursive(&child));
        } else if child.is_file() {
            result.push(child);
        }
    }
    result
}

pub fn load_automations_registry(config_root: &Path) -> (Vec<AutomationEntry>, Option<String>) {
    let dir = automations_dir_for(config_root);
    if !dir.exists() {
        return (Vec::new(), Some(format!("cannot read {}", dir.display())));
    }
    let paths = collect_toml_files(&dir);

    let mut automations = Vec::new();
    let mut errors = Vec::new();

    for path in paths {
        match load_single_automation(&path, config_root) {
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

// ---------------------------------------------------------------------------
// Agent backends — loaded from TOML at runtime
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
pub struct BackendEntry {
    pub id: String,
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub flags: String,
    #[serde(default)]
    #[allow(dead_code)] // Parsed from AGENTS.toml; reserved for backends that use prompt flags
    pub prompt_flag: String,
}

#[derive(Deserialize, Clone)]
pub struct AgentEntry {
    pub id: String,
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub flags: String,
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

pub fn load_agents_config(
    config_root: &Path,
) -> (Vec<BackendEntry>, Vec<AgentEntry>, Option<String>) {
    load_toml_agents(&agents_toml_path_for(config_root))
}

// ---------------------------------------------------------------------------
// Resolved automation metadata
//
// These are the runtime-resolved views of an `AutomationEntry` that
// cross-crate consumers (notably the PostProd Rules window) need. They carry
// only what the consumer can use directly: the automation id/label, the
// prompt-file pointer, and concrete absolute paths for each context entry
// that successfully resolved.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ResolvedAutomationInfo {
    pub id: String,
    pub label: String,
    pub prompt_file: Option<String>,
    pub contexts: Vec<ResolvedContextInfo>,
    pub skip_default_context: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedContextInfo {
    /// "path" or "script"
    pub source_type: String,
    pub label: String,
    pub resolved_path: PathBuf,
    pub required: bool,
}

/// Resolve a single `ContextEntry` against a config root.
///
/// Returns `None` if the referenced path does not exist on disk — callers
/// filter these out rather than failing the whole automation.
pub fn resolve_context_entry(
    entry: &ContextEntry,
    config_root: &Path,
) -> Option<ResolvedContextInfo> {
    let resolved_path = match entry.source_type.as_str() {
        "path" => {
            let raw = entry.path.as_deref().or(Some(entry.label.as_str()))?;
            let path = if let Some(rest) = raw.strip_prefix("~/") {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/"))
                    .join(rest)
            } else if raw == "~" {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
            } else if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                config_root.join(raw)
            };
            if !path.exists() {
                return None;
            }
            path
        }
        "script" => {
            let script = entry.script.as_deref().or(Some(entry.label.as_str()))?;
            let path = config_root.join("config/context-scripts").join(script);
            if !path.exists() {
                return None;
            }
            path
        }
        _ => return None,
    };

    Some(ResolvedContextInfo {
        source_type: entry.source_type.clone(),
        label: entry.label.clone(),
        resolved_path,
        required: entry.required,
    })
}

/// Resolve an `AutomationEntry` into the shared cross-crate view. Contexts
/// that fail to resolve are dropped from the returned list.
pub fn resolve_automation_info(
    entry: &AutomationEntry,
    config_root: &Path,
) -> ResolvedAutomationInfo {
    ResolvedAutomationInfo {
        id: entry.id.clone(),
        label: entry.label.clone(),
        prompt_file: entry.prompt_file.clone(),
        contexts: entry
            .contexts
            .iter()
            .filter_map(|c| resolve_context_entry(c, config_root))
            .collect(),
        skip_default_context: entry.skip_default_context,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // config_root path helpers
    // ---------------------------------------------------------------

    #[test]
    fn test_config_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(config_dir_for(&root), PathBuf::from("/tmp/fake/config"));
    }

    #[test]
    fn test_state_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(
            state_dir_for(&root),
            PathBuf::from("/tmp/fake/config/.state")
        );
    }

    #[test]
    fn test_tools_config_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(
            tools_config_dir_for(&root),
            PathBuf::from("/tmp/fake/config/tools")
        );
    }

    #[test]
    fn test_automations_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(
            automations_dir_for(&root),
            PathBuf::from("/tmp/fake/config/automations")
        );
    }

    #[test]
    fn test_agents_toml_path_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(
            agents_toml_path_for(&root),
            PathBuf::from("/tmp/fake/config/AGENTS.toml")
        );
    }

    // ---------------------------------------------------------------
    // Tool loading
    // ---------------------------------------------------------------

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
            Err(err) => assert!(
                err.contains("bad.toml"),
                "error should mention filename: {err}"
            ),
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
        let auto = load_single_automation(&path, tmp.path())?;
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

    #[test]
    fn test_pipeline_toml_parsing() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("quality-pipeline.toml");
        std::fs::write(
            &path,
            r#"
id = "quality-pipeline"
label = "Quality Pipeline"
description = "Full quality cycle"
icon = "zap"
type = "pipeline"

[[step]]
automation = "daily-scan"

[[step]]
automation = "review"

[[step]]
automation = "doc-scan"
group = 3

[[step]]
automation = "review-doc"
group = 3

[[step]]
tool = "context-launcher"
"#,
        )?;
        let entry = load_single_automation(&path, tmp.path())?;
        assert!(entry.is_pipeline());
        assert_eq!(entry.steps.len(), 5);
        assert_eq!(entry.prompt, "");

        // Sequential steps
        assert_eq!(entry.steps[0].automation.as_deref(), Some("daily-scan"));
        assert!(entry.steps[0].group.is_none());

        // Parallel group
        assert_eq!(entry.steps[2].group, Some(3));
        assert_eq!(entry.steps[3].group, Some(3));

        // Tool step
        assert!(entry.steps[4].is_tool());
        assert_eq!(entry.steps[4].tool.as_deref(), Some("context-launcher"));
        Ok(())
    }

    #[test]
    fn test_pipeline_empty_steps() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("empty.toml");
        std::fs::write(
            &path,
            r#"
id = "empty-pipe"
label = "Empty"
description = ""
icon = "zap"
type = "pipeline"
"#,
        )?;
        let entry = load_single_automation(&path, tmp.path())?;
        assert!(entry.is_pipeline());
        assert!(entry.steps.is_empty());
        Ok(())
    }

    #[test]
    fn test_pipeline_step_validate() {
        let valid_auto = PipelineStep {
            automation: Some("scan".into()),
            tool: None,
            group: None,
        };
        assert!(valid_auto.validate().is_ok());

        let valid_tool = PipelineStep {
            automation: None,
            tool: Some("launcher".into()),
            group: Some(1),
        };
        assert!(valid_tool.validate().is_ok());
        assert!(valid_tool.is_tool());
        assert_eq!(valid_tool.target_id(), Some("launcher"));

        let both = PipelineStep {
            automation: Some("a".into()),
            tool: Some("b".into()),
            group: None,
        };
        assert!(both.validate().is_err());

        let neither = PipelineStep {
            automation: None,
            tool: None,
            group: None,
        };
        assert!(neither.validate().is_err());
    }

    #[test]
    fn test_pipeline_invalid_steps_filtered_on_load() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("bad-steps.toml");
        std::fs::write(
            &path,
            r#"
id = "bad-pipe"
label = "Bad Pipeline"
description = ""
icon = "zap"
type = "pipeline"

[[step]]
automation = "good-step"

[[step]]

[[step]]
automation = "also-good"
tool = "conflict"
"#,
        )?;
        let entry = load_single_automation(&path, tmp.path())?;
        assert_eq!(entry.steps.len(), 1);
        assert_eq!(entry.steps[0].automation.as_deref(), Some("good-step"));
        Ok(())
    }

    #[test]
    fn test_is_pipeline_false_for_regular_automation() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("regular.toml");
        std::fs::write(
            &path,
            r#"
id = "scan"
label = "Scan"
description = "Regular automation"
icon = "sparkle"
prompt = "Do the scan"
"#,
        )?;
        let entry = load_single_automation(&path, tmp.path())?;
        assert!(!entry.is_pipeline());
        assert!(entry.steps.is_empty());
        Ok(())
    }

    // ---------------------------------------------------------------
    // prompt_file resolution
    // ---------------------------------------------------------------

    #[test]
    fn test_resolve_file_by_name_found() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub)?;
        std::fs::write(sub.join("hello.md"), "content")?;

        let result = resolve_file_by_name(tmp.path(), "hello.md");
        assert!(result.is_ok());
        assert_eq!(result.as_ref().unwrap().file_name().unwrap(), "hello.md");
        Ok(())
    }

    #[test]
    fn test_resolve_file_by_name_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let result = resolve_file_by_name(tmp.path(), "missing.md");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
        Ok(())
    }

    #[test]
    fn test_resolve_file_by_name_ambiguous() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        std::fs::create_dir(&dir_a)?;
        std::fs::create_dir(&dir_b)?;
        std::fs::write(dir_a.join("dup.md"), "first")?;
        std::fs::write(dir_b.join("dup.md"), "second")?;

        let result = resolve_file_by_name(tmp.path(), "dup.md");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ambiguous"));
        Ok(())
    }

    #[test]
    fn test_resolve_file_by_name_nonexistent_dir() {
        let result = resolve_file_by_name(Path::new("/nonexistent/dir"), "file.md");
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // expand_tilde + resolve_file_path
    // ---------------------------------------------------------------

    #[test]
    fn test_expand_tilde_with_home() {
        let result = expand_tilde("~/foo/bar.md");
        assert!(!result.starts_with('~'), "tilde should be expanded");
        assert!(result.ends_with("/foo/bar.md"));
    }

    #[test]
    fn test_expand_tilde_no_prefix() {
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }

    #[test]
    fn test_expand_tilde_empty() {
        assert_eq!(expand_tilde(""), "");
    }

    #[test]
    fn test_resolve_file_path_bare_filename() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        std::fs::write(tmp.path().join("hello.md"), "content")?;
        let result = resolve_file_path(tmp.path(), "hello.md");
        assert!(result.is_ok());
        assert_eq!(result?.file_name().unwrap(), "hello.md");
        Ok(())
    }

    #[test]
    fn test_resolve_file_path_absolute() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join("target.md");
        std::fs::write(&file, "content")?;
        let result = resolve_file_path(Path::new("/unused"), &file.to_string_lossy());
        assert!(result.is_ok());
        assert_eq!(result?, file);
        Ok(())
    }

    #[test]
    fn test_resolve_file_path_tilde() -> Result<(), Box<dyn std::error::Error>> {
        // Create a file in the home directory's tmp area to test tilde expansion
        let home = dirs::home_dir().expect("home dir must exist for this test");
        let test_file = home.join(".postprod-test-resolve-tilde.tmp");
        std::fs::write(&test_file, "tilde test")?;

        let result = resolve_file_path(Path::new("/unused"), "~/.postprod-test-resolve-tilde.tmp");
        std::fs::remove_file(&test_file).ok();

        assert!(result.is_ok());
        assert_eq!(result?, test_file);
        Ok(())
    }

    #[test]
    fn test_resolve_file_path_missing() {
        let result = resolve_file_path(Path::new("/unused"), "/no/such/file.md");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist"));
    }

    #[test]
    fn test_prompt_file_loads_content() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let config_root = tmp.path();
        let prompts_dir = config_root.join("config/prompts");
        std::fs::create_dir_all(&prompts_dir)?;
        std::fs::write(prompts_dir.join("test.md"), "Hello from file")?;

        let auto_dir = config_root.join("config/automations");
        std::fs::create_dir_all(&auto_dir)?;
        std::fs::write(
            auto_dir.join("test.toml"),
            r#"
id = "test"
label = "Test"
description = "d"
icon = "zap"
prompt_file = "test.md"
"#,
        )?;

        let entry = load_single_automation(&auto_dir.join("test.toml"), config_root)?;
        assert_eq!(entry.prompt, "Hello from file");
        Ok(())
    }

    #[test]
    fn test_inline_prompt_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("inline.toml");
        std::fs::write(
            &path,
            r#"
id = "inline"
label = "Inline"
description = "d"
icon = "zap"
prompt = "inline content"
"#,
        )?;

        let entry = load_single_automation(&path, tmp.path())?;
        assert_eq!(entry.prompt, "inline content");
        Ok(())
    }

    #[test]
    fn test_prompt_file_missing_rejects_automation() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("missing.toml");
        std::fs::write(
            &path,
            r#"
id = "missing"
label = "Missing"
description = "d"
icon = "zap"
prompt_file = "does-not-exist.md"
"#,
        )?;

        let result = load_single_automation(&path, tmp.path());
        match result {
            Err(e) => assert!(e.contains("not found"), "unexpected error: {e}"),
            Ok(_) => panic!("should fail when prompt file is missing"),
        }
        Ok(())
    }

    // ---------------------------------------------------------------
    // read_path_context (file and folder reading)
    // ---------------------------------------------------------------

    #[test]
    fn test_read_path_context_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        std::fs::write(tmp.path().join("notes.txt"), "some notes")?;

        let result = read_path_context(&tmp.path().join("notes.txt"))?;
        assert_eq!(result, "some notes");
        Ok(())
    }

    #[test]
    fn test_read_path_context_folder() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        std::fs::write(tmp.path().join("a.txt"), "alpha")?;
        std::fs::write(tmp.path().join("b.txt"), "beta")?;

        let result = read_path_context(tmp.path())?;
        assert!(result.contains("--- a.txt ---"));
        assert!(result.contains("alpha"));
        assert!(result.contains("--- b.txt ---"));
        assert!(result.contains("beta"));
        Ok(())
    }

    #[test]
    fn test_read_path_context_folder_with_subdirs() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        std::fs::write(tmp.path().join("root.txt"), "root")?;
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub)?;
        std::fs::write(sub.join("nested.txt"), "nested")?;

        let result = read_path_context(tmp.path())?;
        assert!(result.contains("root.txt"));
        assert!(result.contains("sub/nested.txt") || result.contains("sub\\nested.txt"));
        Ok(())
    }

    #[test]
    fn test_read_path_context_nonexistent() {
        let result = read_path_context(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_path_context_file_truncation() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let big_content = "x".repeat(200 * 1024);
        std::fs::write(tmp.path().join("big.txt"), &big_content)?;

        let result = read_path_context(&tmp.path().join("big.txt"))?;
        assert!(result.len() < big_content.len());
        assert!(result.contains("[... truncated at 150KB]"));
        Ok(())
    }

    // ---------------------------------------------------------------
    // load_default_contexts
    // ---------------------------------------------------------------

    #[test]
    fn test_load_default_contexts_empty() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let contexts = load_default_contexts(tmp.path());
        assert!(contexts.is_empty());
        Ok(())
    }

    #[test]
    fn test_load_default_contexts_loads_entries() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("config/default-context");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("owner-decisions.md"),
            "# Owner Notes\nBinding decisions.",
        )?;

        let contexts = load_default_contexts(tmp.path());
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].source_type, "path");
        assert_eq!(contexts[0].label, "owner decisions");
        assert!(contexts[0].required);
        Ok(())
    }

    #[test]
    fn test_load_default_contexts_multiple_files() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("config/default-context");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("known-bugs.md"), "# Known Bugs\nNone.")?;
        std::fs::write(
            dir.join("tier-protocols.md"),
            "# Tier Guidelines\nThree tiers.",
        )?;

        let contexts = load_default_contexts(tmp.path());
        assert_eq!(contexts.len(), 2);
        Ok(())
    }

    // ---------------------------------------------------------------
    // ContextEntry TOML parsing
    // ---------------------------------------------------------------

    #[test]
    fn test_context_entry_parsing() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("ctx.toml");
        std::fs::write(
            &path,
            r#"
id = "with-ctx"
label = "With Context"
description = "d"
icon = "zap"
prompt = "do something"
skip_default_context = true

[[context]]
source = "path"
path = "/tmp/notes.md"
label = "Notes"

[[context]]
source = "script"
script = "status.sh"
label = "Status"
required = false
"#,
        )?;

        let entry = load_single_automation(&path, tmp.path())?;
        assert_eq!(entry.contexts.len(), 2);
        assert!(entry.skip_default_context);
        assert_eq!(entry.contexts[0].source_type, "path");
        assert_eq!(entry.contexts[0].path.as_deref(), Some("/tmp/notes.md"));
        assert!(entry.contexts[0].required); // default true
        assert_eq!(entry.contexts[1].source_type, "script");
        assert_eq!(entry.contexts[1].script.as_deref(), Some("status.sh"));
        assert!(!entry.contexts[1].required);
        Ok(())
    }

    // ---------------------------------------------------------------
    // collect_context_scripts
    // ---------------------------------------------------------------

    #[test]
    fn test_collect_context_scripts_empty() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let scripts = collect_context_scripts(tmp.path());
        assert!(scripts.is_empty());
        Ok(())
    }

    #[test]
    fn test_collect_context_scripts_finds_files() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("config/context-scripts");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("a.sh"), "#!/bin/bash")?;
        std::fs::write(dir.join("b.sh"), "#!/bin/bash")?;

        let scripts = collect_context_scripts(tmp.path());
        assert_eq!(scripts.len(), 2);
        Ok(())
    }

    // ---------------------------------------------------------------
    // resolve_context_entry / resolve_automation_info
    // ---------------------------------------------------------------

    #[test]
    fn test_resolve_context_entry_path_absolute() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join("notes.md");
        std::fs::write(&file, "content")?;

        let entry = ContextEntry {
            source_type: "path".into(),
            label: "Notes".into(),
            path: Some(file.to_string_lossy().to_string()),
            script: None,
            required: true,
        };
        let resolved = resolve_context_entry(&entry, tmp.path()).expect("should resolve");
        assert_eq!(resolved.resolved_path, file);
        assert_eq!(resolved.source_type, "path");
        Ok(())
    }

    #[test]
    fn test_resolve_context_entry_path_relative_to_config_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join("relative.md");
        std::fs::write(&file, "content")?;

        let entry = ContextEntry {
            source_type: "path".into(),
            label: "Rel".into(),
            path: Some("relative.md".into()),
            script: None,
            required: true,
        };
        let resolved = resolve_context_entry(&entry, tmp.path()).expect("should resolve");
        assert_eq!(resolved.resolved_path, file);
        Ok(())
    }

    #[test]
    fn test_resolve_context_entry_script() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let scripts_dir = tmp.path().join("config/context-scripts");
        std::fs::create_dir_all(&scripts_dir)?;
        let script = scripts_dir.join("status.sh");
        std::fs::write(&script, "#!/bin/bash\necho ok")?;

        let entry = ContextEntry {
            source_type: "script".into(),
            label: "Status".into(),
            path: None,
            script: Some("status.sh".into()),
            required: true,
        };
        let resolved = resolve_context_entry(&entry, tmp.path()).expect("should resolve");
        assert_eq!(resolved.resolved_path, script);
        assert_eq!(resolved.source_type, "script");
        Ok(())
    }

    #[test]
    fn test_resolve_context_entry_missing_path_returns_none() {
        let entry = ContextEntry {
            source_type: "path".into(),
            label: "gone".into(),
            path: Some("/no/such/file".into()),
            script: None,
            required: true,
        };
        assert!(resolve_context_entry(&entry, Path::new("/tmp")).is_none());
    }

    #[test]
    fn test_resolve_context_entry_unknown_source_returns_none() {
        let entry = ContextEntry {
            source_type: "bogus".into(),
            label: "x".into(),
            path: None,
            script: None,
            required: true,
        };
        assert!(resolve_context_entry(&entry, Path::new("/tmp")).is_none());
    }

    #[test]
    fn test_resolve_automation_info_filters_missing_contexts()
    -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let good = tmp.path().join("good.md");
        std::fs::write(&good, "ok")?;

        let entry = AutomationEntry {
            id: "a1".into(),
            label: "A1".into(),
            description: String::new(),
            icon: String::new(),
            prompt_file: None,
            prompt: String::new(),
            hidden: false,
            section: None,
            order: 100,
            params: vec![],
            contexts: vec![
                ContextEntry {
                    source_type: "path".into(),
                    label: "Good".into(),
                    path: Some(good.to_string_lossy().to_string()),
                    script: None,
                    required: true,
                },
                ContextEntry {
                    source_type: "path".into(),
                    label: "Missing".into(),
                    path: Some("/no/such/file".into()),
                    script: None,
                    required: true,
                },
            ],
            skip_default_context: false,
            source_path: None,
            schedule: None,
            chain: None,
            profile: None,
            entry_type: None,
            steps: vec![],
        };

        let resolved = resolve_automation_info(&entry, tmp.path());
        assert_eq!(resolved.id, "a1");
        assert_eq!(resolved.contexts.len(), 1, "missing context is dropped");
        assert_eq!(resolved.contexts[0].label, "Good");
        Ok(())
    }
}
