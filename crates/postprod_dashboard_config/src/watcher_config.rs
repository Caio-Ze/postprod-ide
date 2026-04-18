//! Folder Watcher TOML schema + loader.
//!
//! See `private/specs/event-notifications.md` § "Folder Watchers" for the
//! full design. One `.toml` file per watcher under
//! `config_root/config/watchers/`, mirroring the per-file convention used
//! by automations and tools.
//!
//! **Intentional shape divergence:** [`load_watchers`] returns
//! `Vec<Result<WatcherConfig, LoadError>>` (per-file `Result`s) rather than
//! the `(Vec<Entry>, Option<String>)` shape used by
//! `load_automations_registry` / `load_tools_registry`. The WATCHERS
//! section needs to render a malformed-TOML state on a **specific card**,
//! not as a section-level banner — so per-file error attribution is
//! preserved here.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{collect_toml_files, config_dir_for};

/// Default for `WatcherConfig::enabled` when the field is omitted from the
/// TOML. Watchers are on by default at parse time; the `[+ Add Watcher]`
/// template explicitly writes `enabled = false` so abandoned stubs do not
/// auto-activate (per D17).
fn default_true() -> bool {
    true
}

/// Default for `WatcherTrigger::glob`.
fn default_glob() -> String {
    "*".to_string()
}

/// Default for `WatcherTrigger::debounce_ms`.
fn default_debounce() -> u64 {
    500
}

/// Subdirectory holding watcher TOMLs, relative to `config_root/config/`.
const WATCHERS_SUBDIR: &str = "watchers";

#[derive(Debug, Clone, Deserialize)]
pub struct WatcherConfig {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub trigger: WatcherTrigger,
    #[serde(rename = "emit")]
    pub emits: Vec<WatcherEmit>,
}

// `WatcherConfig` derives `Hash` via the manual impls on `WatcherTrigger`
// and `WatcherEmit` below.
impl Hash for WatcherConfig {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.label.hash(state);
        self.description.hash(state);
        self.path.hash(state);
        self.enabled.hash(state);
        self.trigger.hash(state);
        // Vec<T: Hash>: Hash hashes the length + each element in order.
        self.emits.hash(state);
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatcherTrigger {
    pub on: TriggerKind,
    #[serde(default = "default_glob")]
    pub glob: String,
    #[serde(default)]
    pub min_size_mb: f64,
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
}

// Manual `Hash`: `f64: !Hash` in Rust because NaN breaks Hash/Eq
// consistency. Bit-cast to `u64` for stable hashing — equal `f64` values
// hash equally (NaN is rejected at TOML parse time, so the bit-cast is
// safe in practice).
impl Hash for WatcherTrigger {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.on.hash(state);
        self.glob.hash(state);
        self.min_size_mb.to_bits().hash(state);
        self.debounce_ms.hash(state);
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    FileCreated,
    FileModified,
    FileDeleted,
    Any,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatcherEmit {
    pub kind: String,
    /// Everything else is kind-specific. Captured as a TOML table; the
    /// kind-specific deserialization (e.g. `NotificationPayload`) happens
    /// at emit time inside `postprod_watchers`.
    #[serde(flatten)]
    pub payload: toml::Table,
}

// Manual `Hash`: `toml::Table: !Hash`. Hash via canonical-string
// serialization. `unwrap_or_default()` on a freshly-deserialized table
// can fail only on serializer-internal panics, which are irrelevant for
// hashing — empty string is a safe fallback that still differentiates
// across configs (the rest of the fields participate in the hash).
impl Hash for WatcherEmit {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        toml::to_string(&self.payload)
            .unwrap_or_default()
            .hash(state);
    }
}

/// Per-file load error surfaced by [`load_watchers`]. Carries the source
/// path so the dashboard's WATCHERS section can render the failure on
/// the specific card and offer "open file" via the gear menu.
#[derive(Debug, Clone)]
pub struct LoadError {
    pub path: PathBuf,
    pub detail: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.detail)
    }
}

impl std::error::Error for LoadError {}

/// Returns the absolute directory holding watcher TOMLs.
pub fn watchers_config_dir_for(config_root: &Path) -> PathBuf {
    config_dir_for(config_root).join(WATCHERS_SUBDIR)
}

/// Load every watcher TOML at `config_root/config/watchers/`. Each entry
/// is a `Result` so a malformed file shows as a `✗ malformed TOML: <detail>`
/// card without taking down the whole section.
pub fn load_watchers(config_root: &Path) -> Vec<Result<WatcherConfig, LoadError>> {
    let dir = watchers_config_dir_for(config_root);
    let paths = collect_toml_files(&dir);

    paths
        .into_iter()
        .map(|path| match load_single_watcher(&path) {
            Ok(cfg) => Ok(cfg),
            Err(detail) => Err(LoadError {
                path: path.clone(),
                detail,
            }),
        })
        .collect()
}

fn load_single_watcher(path: &Path) -> Result<WatcherConfig, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
    let cfg: WatcherConfig =
        toml::from_str(&content).map_err(|e| format!("parse failed: {e}"))?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &WatcherConfig) -> Result<(), String> {
    if cfg.id.trim().is_empty() {
        return Err("`id` must not be empty".into());
    }
    if cfg.label.trim().is_empty() {
        return Err("`label` must not be empty".into());
    }
    if cfg.path.trim().is_empty() {
        return Err("`path` must not be empty".into());
    }
    if cfg.emits.is_empty() {
        return Err("at least one `[[emit]]` block is required".into());
    }
    for (idx, emit) in cfg.emits.iter().enumerate() {
        if emit.kind.trim().is_empty() {
            return Err(format!("`[[emit]]` #{} has empty `kind`", idx + 1));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use tempfile::TempDir;

    fn write_toml(dir: &Path, name: &str, content: &str) {
        let watchers_dir = dir.join("config").join("watchers");
        std::fs::create_dir_all(&watchers_dir).unwrap();
        std::fs::write(watchers_dir.join(name), content).unwrap();
    }

    fn hash_one<T: Hash>(t: &T) -> u64 {
        let mut h = DefaultHasher::new();
        t.hash(&mut h);
        h.finish()
    }

    // Test 24: valid WatcherConfig passes validation.
    #[test]
    fn valid_config_passes_validation() {
        let dir = TempDir::new().unwrap();
        write_toml(
            dir.path(),
            "downloads.toml",
            r#"
                id = "downloads-exports"
                label = "Exports"
                path = "~/Downloads/exports"

                [trigger]
                on = "file_created"
                glob = "*.wav"
                min_size_mb = 0.1
                debounce_ms = 500

                [[emit]]
                kind = "notification"
                severity = "success"
                title = "t"
                body = "{filename}"
            "#,
        );
        let results = load_watchers(dir.path());
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok(), "expected Ok, got {:?}", results[0]);
        let cfg = results[0].as_ref().unwrap();
        assert_eq!(cfg.id, "downloads-exports");
        assert_eq!(cfg.emits.len(), 1);
        assert_eq!(cfg.emits[0].kind, "notification");
        assert_eq!(cfg.trigger.on, TriggerKind::FileCreated);
        assert_eq!(cfg.trigger.glob, "*.wav");
        assert_eq!(cfg.enabled, true);
    }

    // Test 25: missing path → validation error with actionable message.
    #[test]
    fn missing_path_rejected() {
        let dir = TempDir::new().unwrap();
        write_toml(
            dir.path(),
            "bad.toml",
            r#"
                id = "x"
                label = "y"
                [trigger]
                on = "any"
                [[emit]]
                kind = "notification"
            "#,
        );
        let results = load_watchers(dir.path());
        assert_eq!(results.len(), 1);
        let err = results[0].as_ref().unwrap_err();
        assert!(err.detail.contains("parse") || err.detail.contains("path"),
            "error should mention parse or path, got {}", err.detail);
    }

    // Test 25 cont.: empty emits → validation error.
    #[test]
    fn empty_emits_rejected() {
        let dir = TempDir::new().unwrap();
        write_toml(
            dir.path(),
            "no_emit.toml",
            r#"
                id = "x"
                label = "y"
                path = "~/"
                [trigger]
                on = "any"
                emit = []
            "#,
        );
        let results = load_watchers(dir.path());
        let err = results[0].as_ref().unwrap_err();
        assert!(err.detail.contains("emit"), "error should mention emit, got {}", err.detail);
    }

    // Test 25 cont.: invalid trigger.on → parse error.
    #[test]
    fn invalid_trigger_on_rejected() {
        let dir = TempDir::new().unwrap();
        write_toml(
            dir.path(),
            "bad_trig.toml",
            r#"
                id = "x"
                label = "y"
                path = "~/"
                [trigger]
                on = "bogus_trigger"
                [[emit]]
                kind = "notification"
            "#,
        );
        let results = load_watchers(dir.path());
        let err = results[0].as_ref().unwrap_err();
        assert!(err.detail.contains("parse") || err.detail.contains("bogus"),
            "error should mention parse failure, got {}", err.detail);
    }

    // Hash stability: same config hashes identically (D19 short-circuit
    // depends on stable hashing).
    #[test]
    fn identical_config_hashes_match() {
        let cfg = WatcherConfig {
            id: "a".into(),
            label: "L".into(),
            description: String::new(),
            path: "/tmp".into(),
            enabled: true,
            trigger: WatcherTrigger {
                on: TriggerKind::FileCreated,
                glob: "*.wav".into(),
                min_size_mb: 1.5,
                debounce_ms: 500,
            },
            emits: vec![WatcherEmit {
                kind: "notification".into(),
                payload: toml::from_str(r#"title = "t"
body = "b"
severity = "info""#).unwrap(),
            }],
        };
        assert_eq!(hash_one(&cfg), hash_one(&cfg.clone()));
    }

    // Hash sensitivity: changing min_size_mb (the f64 field) changes the
    // hash — guards the bit-cast Hash impl.
    #[test]
    fn min_size_mb_change_changes_hash() {
        let mut a = WatcherTrigger {
            on: TriggerKind::Any,
            glob: "*".into(),
            min_size_mb: 0.0,
            debounce_ms: 100,
        };
        let h0 = hash_one(&a);
        a.min_size_mb = 1.5;
        let h1 = hash_one(&a);
        assert_ne!(h0, h1);
    }
}
