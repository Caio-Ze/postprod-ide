use gpui::App;
use util::ResultExt as _;

use std::path::{Path, PathBuf};

#[derive(Default, Clone)]
pub(crate) struct DeliveryStatus {
    pub(crate) tv_count: usize,
    pub(crate) net_count: usize,
    pub(crate) spot_count: usize,
    pub(crate) mp3_count: usize,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn suite_root() -> PathBuf {
    if let Ok(p) = std::env::var("POSTPROD_WORKSPACE") {
        return PathBuf::from(p);
    }
    util::paths::home_dir().join("PostProd_IDE")
}

pub(crate) fn config_dir() -> PathBuf {
    suite_root().join("config")
}

pub(crate) fn state_dir() -> PathBuf {
    config_dir().join(".state")
}

pub(crate) fn tools_dir() -> PathBuf {
    suite_root().join("tools")
}

pub(crate) fn agent_tools_dir() -> PathBuf {
    tools_dir().join("agent")
}

pub(crate) fn runtime_tools_dir() -> PathBuf {
    tools_dir().join("runtime")
}

// Per-folder config path helpers — derive from an arbitrary config_root
pub(crate) fn config_dir_for(config_root: &Path) -> PathBuf {
    config_root.join("config")
}

pub(crate) fn state_dir_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join(".state")
}

pub(crate) fn tools_config_dir_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join("tools")
}

pub(crate) fn automations_dir_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join("automations")
}

pub(crate) fn local_tools_dir_for(config_root: &Path) -> PathBuf {
    config_root.join("tools")
}

pub(crate) fn agents_toml_path_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join("AGENTS.toml")
}

pub(crate) fn ensure_workspace_dirs() {
    // Only create dirs the app owns — everything else comes from user-cloned repos
    for dir in [state_dir(), suite_root().join("deliveries")] {
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

pub(crate) fn scan_delivery_folder() -> DeliveryStatus {
    scan_delivery_in(&suite_root().join("deliveries"))
}

fn scan_delivery_in(dir: &Path) -> DeliveryStatus {
    let mut status = DeliveryStatus::default();

    let Ok(entries) = std::fs::read_dir(dir) else {
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

pub(crate) fn dir_has_content(dir: &Path) -> bool {
    dir.is_dir()
        && std::fs::read_dir(dir)
            .ok()
            .and_then(|mut entries| entries.next())
            .is_some()
}

pub(crate) fn resolve_bin(name: &str) -> String {
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
pub(crate) fn resolve_runtime_path() -> PathBuf {
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
pub(crate) fn resolve_agent_tools_path() -> PathBuf {
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

/// Check whether a folder has dashboard config (tools/ or automations/ with content).
pub(crate) fn folder_has_dashboard_config(folder: &Path) -> bool {
    let config = folder.join("config");
    dir_has_content(&config.join("tools")) || dir_has_content(&config.join("automations"))
}

pub(crate) fn ensure_config_extracted(_cx: &App) {
    ensure_workspace_dirs();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(config_dir_for(&root), PathBuf::from("/tmp/fake/config"));
    }

    #[test]
    fn test_state_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(state_dir_for(&root), PathBuf::from("/tmp/fake/config/.state"));
    }

    #[test]
    fn test_tools_config_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(tools_config_dir_for(&root), PathBuf::from("/tmp/fake/config/tools"));
    }

    #[test]
    fn test_automations_dir_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(automations_dir_for(&root), PathBuf::from("/tmp/fake/config/automations"));
    }

    #[test]
    fn test_agents_toml_path_for() {
        let root = PathBuf::from("/tmp/fake");
        assert_eq!(
            agents_toml_path_for(&root),
            PathBuf::from("/tmp/fake/config/AGENTS.toml")
        );
    }

    #[test]
    fn test_dir_has_content_empty_dir() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        assert!(!dir_has_content(tmp.path()));
        Ok(())
    }

    #[test]
    fn test_dir_has_content_with_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        std::fs::write(tmp.path().join("file.txt"), "hello")?;
        assert!(dir_has_content(tmp.path()));
        Ok(())
    }

    #[test]
    fn test_dir_has_content_nonexistent() {
        assert!(!dir_has_content(Path::new("/nonexistent/path/that/does/not/exist")));
    }

    #[test]
    fn test_scan_delivery_empty() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let status = scan_delivery_in(tmp.path());
        assert_eq!(status.tv_count, 0);
        assert_eq!(status.net_count, 0);
        assert_eq!(status.spot_count, 0);
        assert_eq!(status.mp3_count, 0);
        assert!(status.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn test_scan_delivery_subdirs() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path();

        std::fs::create_dir(dir.join("TV"))?;
        std::fs::write(dir.join("TV/mix1.wav"), "")?;
        std::fs::write(dir.join("TV/mix2.wav"), "")?;

        std::fs::create_dir(dir.join("net"))?;
        std::fs::write(dir.join("net/mix1.wav"), "")?;
        std::fs::write(dir.join("net/mix2.wav"), "")?;

        std::fs::create_dir(dir.join("SPOT"))?;
        std::fs::write(dir.join("SPOT/spot1.wav"), "")?;

        let status = scan_delivery_in(dir);
        assert_eq!(status.tv_count, 2);
        assert_eq!(status.net_count, 2);
        assert_eq!(status.spot_count, 1);
        Ok(())
    }

    #[test]
    fn test_scan_delivery_file_patterns() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path();

        std::fs::write(dir.join("mix_tv.wav"), "")?;
        std::fs::write(dir.join("mix_net.wav"), "")?;
        std::fs::write(dir.join("mix_spot.wav"), "")?;
        std::fs::write(dir.join("reference.mp3"), "")?;

        let status = scan_delivery_in(dir);
        assert_eq!(status.tv_count, 1);
        assert_eq!(status.net_count, 1);
        assert_eq!(status.spot_count, 1);
        assert_eq!(status.mp3_count, 1);
        Ok(())
    }

    #[test]
    fn test_scan_delivery_warnings() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path();

        // Only TV files — should warn about missing NET and SPOT
        std::fs::create_dir(dir.join("tv"))?;
        std::fs::write(dir.join("tv/mix1.wav"), "")?;
        std::fs::write(dir.join("tv/mix2.wav"), "")?;

        let status = scan_delivery_in(dir);
        assert_eq!(status.tv_count, 2);
        assert!(status.warnings.iter().any(|w| w.contains("NET")));
        assert!(status.warnings.iter().any(|w| w.contains("SPOT")));

        // TV + NET with mismatched counts — should warn about mismatch
        std::fs::create_dir(dir.join("net"))?;
        std::fs::write(dir.join("net/mix1.wav"), "")?;

        let status = scan_delivery_in(dir);
        assert_eq!(status.tv_count, 2);
        assert_eq!(status.net_count, 1);
        assert!(status.warnings.iter().any(|w| w.contains("TV (2) != NET (1)")));
        Ok(())
    }
}
