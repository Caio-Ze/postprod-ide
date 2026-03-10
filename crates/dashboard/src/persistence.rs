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
    match toml::from_str(&content) {
        Ok(values) => values,
        Err(e) => {
            log::warn!("param_values.toml: parse error (values reset): {e}");
            HashMap::new()
        }
    }
}

pub(crate) fn write_param_values(config_root: &Path, values: &HashMap<String, HashMap<String, String>>) {
    if let Ok(content) = toml::to_string(values) {
        std::fs::write(param_values_file(config_root), content).log_err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // group_by_section — pure logic tests
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct FakeEntry {
        label: String,
        section: Option<String>,
        order: u32,
    }

    fn fake(label: &str, section: Option<&str>, order: u32) -> FakeEntry {
        FakeEntry {
            label: label.to_string(),
            section: section.map(|s| s.to_string()),
            order,
        }
    }

    fn run_group(entries: &[FakeEntry], section_order: &[String]) -> Vec<(String, Vec<FakeEntry>)> {
        group_by_section(
            entries,
            |e| e.section.as_deref(),
            |e| e.order,
            |e| &e.label,
            section_order,
        )
    }

    #[test]
    fn test_group_by_section_single_section() {
        let entries = vec![fake("A", Some("Tools"), 1), fake("B", Some("Tools"), 2)];
        let groups = run_group(&entries, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "Tools");
        assert_eq!(groups[0].1.len(), 2);
    }

    #[test]
    fn test_group_by_section_multiple_sections() {
        let entries = vec![
            fake("A", Some("Tools"), 1),
            fake("B", Some("Audio"), 1),
            fake("C", Some("Tools"), 2),
        ];
        let groups = run_group(&entries, &[]);
        assert_eq!(groups.len(), 2);
        // BTreeMap order: "Audio" before "Tools"
        assert_eq!(groups[0].0, "Audio");
        assert_eq!(groups[1].0, "Tools");
        assert_eq!(groups[1].1.len(), 2);
    }

    #[test]
    fn test_group_by_section_none_falls_back_to_general() {
        let entries = vec![fake("A", None, 1), fake("B", None, 2)];
        let groups = run_group(&entries, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "General");
    }

    #[test]
    fn test_group_by_section_order_within_section() {
        let entries = vec![
            fake("Z-item", Some("S"), 1),
            fake("A-item", Some("S"), 2),
            fake("M-item", Some("S"), 1),
        ];
        let groups = run_group(&entries, &[]);
        let labels: Vec<&str> = groups[0].1.iter().map(|e| e.label.as_str()).collect();
        // order=1 first (M, Z alphabetically), then order=2 (A)
        assert_eq!(labels, vec!["M-item", "Z-item", "A-item"]);
    }

    #[test]
    fn test_group_by_section_section_order_priority() {
        let entries = vec![
            fake("A", Some("Alpha"), 1),
            fake("B", Some("Beta"), 1),
            fake("C", Some("Charlie"), 1),
        ];
        let section_order = vec!["Charlie".to_string(), "Alpha".to_string()];
        let groups = run_group(&entries, &section_order);

        let section_names: Vec<&str> = groups.iter().map(|(n, _)| n.as_str()).collect();
        // Charlie first (from section_order), Alpha second, Beta last (BTreeMap remainder)
        assert_eq!(section_names, vec!["Charlie", "Alpha", "Beta"]);
    }

    #[test]
    fn test_group_by_section_empty() {
        let entries: Vec<FakeEntry> = vec![];
        let groups = run_group(&entries, &[]);
        assert!(groups.is_empty());
    }

    // -----------------------------------------------------------------------
    // Filesystem round-trip tests
    // -----------------------------------------------------------------------

    fn setup_root() -> Result<(tempfile::TempDir, PathBuf), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(state_dir_for(&root))?;
        Ok((tmp, root))
    }

    #[test]
    fn test_active_folder_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmp, root) = setup_root()?;
        // The folder written must actually exist as a dir for read to return it
        let folder = root.join("my_project");
        std::fs::create_dir(&folder)?;
        write_active_folder(&root, &folder);
        let result = read_active_folder(&root);
        assert_eq!(result, Some(folder));
        Ok(())
    }

    #[test]
    fn test_active_folder_missing_returns_config_root() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmp, root) = setup_root()?;
        // No active_folder file written, config_root is a valid dir
        let result = read_active_folder(&root);
        assert_eq!(result, Some(root));
        Ok(())
    }

    #[test]
    fn test_recent_folders_ordering_and_limit() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmp, root) = setup_root()?;
        // Create 12 real directories
        let mut dirs = Vec::new();
        for i in 0..12 {
            let dir = root.join(format!("folder_{i:02}"));
            std::fs::create_dir(&dir)?;
            dirs.push(dir);
        }
        // Add all 12
        for dir in &dirs {
            add_to_recent_folders(&root, dir);
        }
        let recent = read_recent_folders(&root);
        // Capped at 10
        assert_eq!(recent.len(), 10);
        // Most recent first (folder_11)
        assert_eq!(recent[0], dirs[11]);
        Ok(())
    }

    #[test]
    fn test_background_tools_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmp, root) = setup_root()?;
        let mut tools = HashSet::new();
        tools.insert("bounceAll".to_string());
        tools.insert("exportStem".to_string());
        tools.insert("normalize".to_string());
        write_background_tools(&root, &tools);
        let read_back = read_background_tools(&root);
        assert_eq!(read_back, tools);
        Ok(())
    }

    #[test]
    fn test_param_values_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmp, root) = setup_root()?;
        let mut values = HashMap::new();
        let mut inner = HashMap::new();
        inner.insert("format".to_string(), "wav".to_string());
        inner.insert("depth".to_string(), "24".to_string());
        values.insert("bounceAll".to_string(), inner);
        write_param_values(&root, &values);
        let read_back = read_param_values(&root);
        assert_eq!(read_back, values);
        Ok(())
    }

    #[test]
    fn test_param_values_corrupt_toml_returns_empty() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmp, root) = setup_root()?;
        std::fs::write(param_values_file(&root), "this is {{ not valid toml")?;
        let values = read_param_values(&root);
        assert!(values.is_empty());
        Ok(())
    }
}
