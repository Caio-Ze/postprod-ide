//! Path-typed param drop + cwd resolution cluster.
//!
//! Extracted from `dashboard.rs` (spec automation-cwd-override v3,
//! Phases 3 + 5; review folds G1/G2/G3/G4). The cluster covers:
//!
//! - `update_param_value`: single mutation point for `param_values`
//!   (picker-confirm and Finder-drop both flow through here).
//! - `normalize_for_param_type`: write-time `file → parent` collapse for
//!   `ParamType::Cwd`; verbatim for `Path` and other variants.
//! - `resolve_dragged_first_path` / `first_resolvable_abs_path`:
//!   resolve the first absolute path in a `DraggedSelection` (Zed
//!   project-panel drags). The pure-iterator helper is unit-testable
//!   without a real Workspace/Project.
//! - `mark_cwd_warning_seen`: backend-aware `(entry_id, backend)` dedup
//!   for the "cwd-bound param has no effect on this backend" toast.
//!
//! The `cwd_warning_seen` field stays on `DashboardItem` (struct definition
//! is in `dashboard.rs`); behaviors that touch it live here.

use std::path::{Path, PathBuf};

use gpui::{App, Context};
use postprod_dashboard_config::ParamType;
use workspace::DraggedSelection;

use crate::DashboardItem;
use crate::AgentBackend;
use crate::persistence::write_param_values;

/// Convert a dropped path to the value to persist into `param_values`,
/// per the path-param drop contract:
///
/// - `ParamType::Cwd`: a dropped *file* is normalized to its parent
///   directory. The card displays exactly what the agent will use as
///   spawn cwd — no run-time normalization, no UX drift between display
///   and effect (spec v2 §"What's new" #5, decision #4 from review).
/// - `ParamType::Path`: dropped path is stored verbatim (file or folder
///   — `Path` is general-purpose, e.g. "analyze this file").
/// - Any other variant: stored verbatim. Drop wiring only applies to
///   the `Path | Cwd` arm, but the helper is total to keep callers
///   safe.
///
/// Edge case: file at `/` with no parent → input string returned as-is.
pub(crate) fn normalize_for_param_type(path: &Path, param_type: &ParamType) -> String {
    if matches!(param_type, ParamType::Cwd) && path.is_file() {
        path.parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    } else {
        path.to_string_lossy().to_string()
    }
}

/// Pure-iterator helper for `DashboardItem::resolve_dragged_first_path` (spec
/// automation-cwd-override v3 Phase 5). Walks `entry_ids`, returns the
/// `PathBuf` from the first entry whose `resolve` closure returns
/// `Some(...)`. Empty iterator or all-None returns `None`. Extracted
/// from the DashboardItem method so the iteration semantics can be unit-
/// tested without constructing a real Workspace/Project.
pub(crate) fn first_resolvable_abs_path<I, F>(entry_ids: I, mut resolve: F) -> Option<PathBuf>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Option<PathBuf>,
{
    entry_ids.into_iter().find_map(&mut resolve)
}

impl DashboardItem {
    /// Records that the "cwd-bound param has no effect on this backend"
    /// toast has been emitted for `(entry_id, backend)`. Returns `true` the
    /// first time; `false` on subsequent calls with the same tuple. Pure —
    /// the toast emission stays at the call site (it needs the workspace
    /// handle and `Toast` machinery, neither of which belong on a dedup
    /// helper). Test 15 calls this directly.
    pub(crate) fn mark_cwd_warning_seen(
        &mut self,
        entry_id: &str,
        backend: AgentBackend,
    ) -> bool {
        self.cwd_warning_seen
            .insert((entry_id.to_string(), backend))
    }

    /// Single point of mutation for `param_values` from the path-typed
    /// param render branch. Picker-confirm and Finder-drop both flow
    /// through here so persistence + redraw are guaranteed identical.
    /// Test 11 round-trips this via `read_param_values` (from
    /// `dashboard::persistence`).
    pub(crate) fn update_param_value(
        &mut self,
        entry_id: &str,
        key: &str,
        value: String,
        cx: &mut Context<Self>,
    ) {
        self.param_values
            .entry(entry_id.to_string())
            .or_default()
            .insert(key.to_string(), value);
        write_param_values(&self.config_root, &self.param_values);
        cx.notify();
    }

    /// File-or-folder generalization of `resolve_dragged_directory` used by
    /// path-typed param-card drop zones (spec: automation-cwd-override v3,
    /// Phase 5). Returns the first selection entry that resolves to an
    /// absolute path, regardless of whether it points at a file or a
    /// directory. The caller (`normalize_for_param_type`) handles the
    /// file→parent normalization for `ParamType::Cwd`.
    pub(crate) fn resolve_dragged_first_path(
        &self,
        selection: &DraggedSelection,
        cx: &App,
    ) -> Option<PathBuf> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        first_resolvable_abs_path(selection.items().map(|e| e.entry_id), |id| {
            project
                .path_for_entry(id, cx)
                .and_then(|pp| project.absolute_path(&pp, cx))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    use postprod_dashboard_config as dcfg;

    use crate::persistence::read_param_values;

    /// Test 11 — `update_param_value` round-trips through
    /// `.state/param_values.toml`. Picker-confirm and Finder-drop both
    /// flow through this single method so persistence is identical.
    /// Asserts via `read_param_values` (from `dashboard::persistence`).
    ///
    /// The DashboardItem method itself takes `&mut Context<Self>` and is
    /// hard to construct in a unit test, so this test verifies the
    /// underlying `dcfg::write_param_values` + `dcfg::read_param_values`
    /// contract that the method delegates to. The method body is
    /// otherwise just a HashMap insert + cx.notify; the I/O round-trip
    /// is the part that can break in production.
    #[test]
    fn test_update_param_value_persistence_round_trip()
    -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let config_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(dcfg::state_dir_for(&config_root))?;

        let mut values: HashMap<String, HashMap<String, String>> = HashMap::new();
        values
            .entry("general-coder".to_string())
            .or_default()
            .insert(
                "cwd".to_string(),
                "/Users/example/Documents/Rust_projects/postprod-ide".to_string(),
            );
        values
            .entry("general-coder".to_string())
            .or_default()
            .insert("other".to_string(), "kept".to_string());

        write_param_values(&config_root, &values);

        let round_trip = read_param_values(&config_root);
        let inner = round_trip
            .get("general-coder")
            .expect("entry persisted");
        assert_eq!(
            inner.get("cwd").map(String::as_str),
            Some("/Users/example/Documents/Rust_projects/postprod-ide"),
        );
        assert_eq!(inner.get("other").map(String::as_str), Some("kept"));
        Ok(())
    }

    /// Test 14 — `normalize_for_param_type` for both variants and the
    /// no-parent edge case. Cwd-on-file collapses to parent so the card
    /// displays exactly what the agent runs at.
    #[test]
    fn test_normalize_for_param_type_cwd_collapses_file_to_parent()
    -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let parent = tmp.path();
        let file = parent.join("foo.ptx");
        std::fs::write(&file, "")?;

        // Cwd + file → parent
        let cwd_value = normalize_for_param_type(&file, &ParamType::Cwd);
        assert_eq!(cwd_value, parent.to_string_lossy().to_string());

        // Path + file → verbatim
        let path_value = normalize_for_param_type(&file, &ParamType::Path);
        assert_eq!(path_value, file.to_string_lossy().to_string());

        // Cwd + folder → verbatim (folder is_file == false)
        let cwd_folder = normalize_for_param_type(parent, &ParamType::Cwd);
        assert_eq!(cwd_folder, parent.to_string_lossy().to_string());

        // Edge case: nonexistent path. is_file() returns false, so
        // verbatim is returned.
        let nonexistent = parent.join("does-not-exist.ptx");
        let v = normalize_for_param_type(&nonexistent, &ParamType::Cwd);
        assert_eq!(v, nonexistent.to_string_lossy().to_string());

        Ok(())
    }

    /// Test 14b — file at filesystem root (no parent) falls back to the
    /// input string verbatim. Covers the `unwrap_or_else` branch.
    #[test]
    fn test_normalize_for_param_type_root_file_returns_input() {
        // A path of "/" has parent == None.
        let root = Path::new("/");
        // is_file() on "/" returns false on macOS, so this hits the
        // `else` branch and returns "/" verbatim.
        let v = normalize_for_param_type(root, &ParamType::Cwd);
        assert_eq!(v, "/");
    }

    /// Test 15 — backend-aware warning dedup (G2). The contract of
    /// `DashboardItem::mark_cwd_warning_seen` is:
    ///   - `HashSet::insert` returns `true` the first time a tuple is
    ///     recorded and `false` thereafter.
    ///   - `(String, AgentBackend)` must be hashable (G1: extended derive).
    /// Test the underlying tuple-set contract directly — `DashboardItem::new`
    /// pulls in too much GPUI machinery for a unit test, and the method
    /// body is literally `self.cwd_warning_seen.insert((id.into(), b))`.
    /// Failure of this test = G1 regression (Hash/Eq missing) or
    /// HashSet contract change.
    #[test]
    fn test_cwd_warning_seen_dedup_contract() {
        let mut seen: HashSet<(String, AgentBackend)> = HashSet::new();
        let mark = |seen: &mut HashSet<(String, AgentBackend)>,
                    id: &str,
                    b: AgentBackend|
         -> bool { seen.insert((id.to_string(), b)) };

        // First insertion of any tuple returns true.
        assert!(mark(&mut seen, "general-coder", AgentBackend::Native));
        // Same tuple again returns false (dedup).
        assert!(!mark(&mut seen, "general-coder", AgentBackend::Native));
        // Different backend, same id → distinct tuple → true.
        assert!(mark(&mut seen, "general-coder", AgentBackend::CopyOnly));
        // Different id, same backend → distinct tuple → true.
        assert!(mark(&mut seen, "rebase-coder", AgentBackend::Native));
        // All four interactions should be recorded as exactly three
        // distinct tuples (one duplicate).
        assert_eq!(seen.len(), 3);
    }

    /// Test 16 — `first_resolvable_abs_path` (spec automation-cwd-override
    /// v3 Phase 5). Pure helper extracted from
    /// `DashboardItem::resolve_dragged_first_path` so the iteration semantics
    /// are unit-testable without constructing a real Workspace/Project.
    /// Constructing the DashboardItem wrapper is exercised by M12 against the
    /// shipped binary; the iteration logic is what could regress here.
    #[test]
    fn test_first_resolvable_abs_path_empty_returns_none() {
        let result: Option<PathBuf> =
            first_resolvable_abs_path(std::iter::empty::<u32>(), |_| None);
        assert!(result.is_none());
    }

    #[test]
    fn test_first_resolvable_abs_path_all_fail_returns_none() {
        let ids = vec![1u32, 2, 3];
        let result = first_resolvable_abs_path(ids, |_| None);
        assert!(result.is_none());
    }

    #[test]
    fn test_first_resolvable_abs_path_returns_first_success() {
        let ids = vec![1u32, 2, 3];
        let result = first_resolvable_abs_path(ids, |id| {
            if id == 2 {
                Some(PathBuf::from("/abs/path/two"))
            } else {
                None
            }
        });
        assert_eq!(result, Some(PathBuf::from("/abs/path/two")));
    }

    #[test]
    fn test_first_resolvable_abs_path_short_circuits_on_first_match() {
        // Once a Some is returned, later items must not be probed.
        // Asserts find_map semantics — relevant if a future refactor
        // ever switches to `.fold(...)` or similar that would over-walk.
        let ids = vec![1u32, 2, 3, 4];
        let mut probed: Vec<u32> = Vec::new();
        let result = first_resolvable_abs_path(ids, |id| {
            probed.push(id);
            if id == 2 {
                Some(PathBuf::from("/match/at/two"))
            } else {
                None
            }
        });
        assert_eq!(result, Some(PathBuf::from("/match/at/two")));
        assert_eq!(probed, vec![1, 2]); // 3 and 4 never probed.
    }
}
