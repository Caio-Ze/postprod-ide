use util::ResultExt as _;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::paths::state_dir_for;

// ---------------------------------------------------------------------------
// Active folder helpers
// ---------------------------------------------------------------------------

fn active_folder_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("active_folder")
}

pub(crate) fn read_active_folder(config_root: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(active_folder_file(config_root))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| {
            if config_root.is_dir() {
                Some(config_root.to_path_buf())
            } else {
                None
            }
        })
}

pub(crate) fn write_active_folder(config_root: &Path, path: &Path) {
    std::fs::write(active_folder_file(config_root), path.to_string_lossy().as_bytes()).log_err();
    add_to_recent_folders(config_root, path);
}

// ---------------------------------------------------------------------------
// Recent folders helpers
// ---------------------------------------------------------------------------

fn recent_folders_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("recent_folders")
}

pub(crate) fn read_recent_folders(config_root: &Path) -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(recent_folders_file(config_root)) else {
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

pub(crate) fn add_to_recent_folders(config_root: &Path, path: &Path) {
    let mut recent = read_recent_folders(config_root);
    recent.retain(|p| p != path);
    recent.insert(0, path.to_path_buf());
    recent.truncate(10);
    let content = recent
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(recent_folders_file(config_root), content).log_err();
}

// ---------------------------------------------------------------------------
// Destination folder helpers
// ---------------------------------------------------------------------------

fn destination_folder_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("destination_folder")
}

fn recent_destinations_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("recent_destinations")
}

pub(crate) fn read_destination_folder(config_root: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(destination_folder_file(config_root))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

pub(crate) fn read_recent_destinations(config_root: &Path) -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(recent_destinations_file(config_root)) else {
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

pub(crate) fn write_destination_folder(config_root: &Path, path: &Path) {
    std::fs::write(destination_folder_file(config_root), path.to_string_lossy().as_bytes()).log_err();
    add_to_destination_recent(config_root, path);
}

pub(crate) fn add_to_destination_recent(config_root: &Path, path: &Path) {
    let mut recent = read_recent_destinations(config_root);
    recent.retain(|p| p != path);
    recent.insert(0, path.to_path_buf());
    recent.truncate(10);
    let content = recent
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(recent_destinations_file(config_root), content).log_err();
}

// ---------------------------------------------------------------------------
// Background tools persistence
// ---------------------------------------------------------------------------

fn background_tools_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("background_tools")
}

pub(crate) fn read_background_tools(config_root: &Path) -> HashSet<String> {
    let Ok(content) = std::fs::read_to_string(background_tools_file(config_root)) else {
        return HashSet::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

pub(crate) fn write_background_tools(config_root: &Path, set: &HashSet<String>) {
    let mut entries: Vec<_> = set.iter().cloned().collect();
    entries.sort();
    let content = entries.join("\n");
    std::fs::write(background_tools_file(config_root), content).log_err();
}

// ---------------------------------------------------------------------------
// Collapsed sections persistence
// ---------------------------------------------------------------------------

fn collapsed_sections_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("collapsed_sections")
}

pub(crate) fn read_collapsed_sections(config_root: &Path) -> HashSet<String> {
    let Ok(content) = std::fs::read_to_string(collapsed_sections_file(config_root)) else {
        return HashSet::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

pub(crate) fn write_collapsed_sections(config_root: &Path, set: &HashSet<String>) {
    let mut entries: Vec<_> = set.iter().cloned().collect();
    entries.sort();
    let content = entries.join("\n");
    std::fs::write(collapsed_sections_file(config_root), content).log_err();
}

// ---------------------------------------------------------------------------
// Section order (optional user-managed file)
// ---------------------------------------------------------------------------

fn section_order_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("section_order")
}

pub(crate) fn read_section_order(config_root: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(section_order_file(config_root)) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Section grouping
// ---------------------------------------------------------------------------

pub(crate) fn group_by_section<T, F, G, H>(
    entries: &[T],
    get_section: F,
    get_order: G,
    get_label: H,
    section_order: &[String],
) -> Vec<(String, Vec<T>)>
where
    T: Clone,
    F: Fn(&T) -> Option<&str>,
    G: Fn(&T) -> u32,
    H: Fn(&T) -> &str,
{
    let mut groups: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for entry in entries {
        let key = get_section(entry)
            .unwrap_or("General")
            .to_string();
        groups.entry(key).or_default().push(entry.clone());
    }

    for items in groups.values_mut() {
        items.sort_by(|a, b| {
            get_order(a)
                .cmp(&get_order(b))
                .then_with(|| get_label(a).cmp(get_label(b)))
        });
    }

    let mut result: Vec<(String, Vec<T>)> = Vec::with_capacity(groups.len());

    for name in section_order {
        if let Some(items) = groups.remove(name) {
            result.push((name.clone(), items));
        }
    }

    for (name, items) in groups {
        result.push((name, items));
    }

    result
}

// ---------------------------------------------------------------------------
// Param values persistence
// ---------------------------------------------------------------------------

fn param_values_file(config_root: &Path) -> PathBuf {
    state_dir_for(config_root).join("param_values.toml")
}

pub(crate) fn read_param_values(config_root: &Path) -> HashMap<String, HashMap<String, String>> {
    let Ok(content) = std::fs::read_to_string(param_values_file(config_root)) else {
        return HashMap::new();
    };
    toml::from_str(&content).unwrap_or_default()
}

pub(crate) fn write_param_values(config_root: &Path, values: &HashMap<String, HashMap<String, String>>) {
    if let Ok(content) = toml::to_string(values) {
        std::fs::write(param_values_file(config_root), content).log_err();
    }
}
