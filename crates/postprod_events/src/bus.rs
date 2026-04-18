//! Kind-agnostic event bus core.
//!
//! Emitters call [`emit`] / [`emit_to`] to drop a JSON envelope file into
//! `<bus_root>/<kind>/`. Readers construct an [`EventInbox`] for one specific
//! kind and call [`EventInbox::drain`] to process pending files.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Current envelope schema version. Bump only on a breaking schema change;
/// readers log + drop files with unknown versions (see [`EventInbox::drain`]).
pub const ENVELOPE_SCHEMA: u32 = 1;

/// Environment variable that overrides the default bus root for **both**
/// emit and read. Honored by [`default_bus_root`] (emit side) and by any
/// reader that wants to opt into the same override (e.g. integration tests).
pub const INBOX_ENV_VAR: &str = "POSTPROD_EVENTS_INBOX";

/// Hard cap on file size accepted by the reader. Larger files are moved to
/// `rejected/` without parsing (defends against runaway emitters).
pub const MAX_EVENT_BYTES: u64 = 64 * 1024;

/// Hard cap on events processed per [`EventInbox::drain`] call. Excess files
/// stay for the next drain (defends against runaway emitters blocking the
/// foreground thread during a single fs-watch wake-up).
pub const MAX_EVENTS_PER_DRAIN: usize = 50;

/// Process-monotonic counter, zero-padded to 6 digits in filenames.
/// Within-process filename sort == chronological sort by construction.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Envelope fields common to all event kinds. Payload is captured as a raw
/// JSON value so the kind-specific handler can deserialize it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema: u32,
    pub kind: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub payload: serde_json::Value,
}

/// Resolves the default event-bus root.
///
/// Resolution order:
/// 1. The [`INBOX_ENV_VAR`] override, if set.
/// 2. The macOS path `$HOME/Library/Application Support/PostProd Tools/events`.
///
/// Returns `None` only if `HOME` is unset (and the env override is also
/// unset) — emit is best-effort and silently no-ops in that case.
///
/// **Drift note:** the macOS literal here MUST stay in sync with
/// `paths::data_dir()` at `crates/paths/src/paths.rs:102-107`. The integration
/// test in this crate (`tests/path_guard.rs`) asserts equality.
pub fn default_bus_root() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os(INBOX_ENV_VAR) {
        return Some(PathBuf::from(override_path));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/PostProd Tools/events"))
}

/// Best-effort emit using [`default_bus_root`]. Errors are swallowed —
/// emission is a side-effect of the caller's primary work, never load-bearing.
pub fn emit<P: Serialize>(kind: &str, payload: P, source: Option<&str>) {
    let Some(root) = default_bus_root() else {
        log::warn!("postprod_events: HOME unset; skipping emit for kind={kind}");
        return;
    };
    emit_to(&root, kind, payload, source);
}

/// Best-effort emit with an explicit bus root. Use this when the caller
/// already has a resolved root (e.g. the dashboard-side watcher runtime,
/// which receives `bus_root` via `WatcherRuntime::reconcile`). Errors
/// swallowed.
pub fn emit_to<P: Serialize>(bus_root: &Path, kind: &str, payload: P, source: Option<&str>) {
    if let Err(err) = emit_inner(bus_root, kind, payload, source) {
        log::warn!(
            "postprod_events: emit_to({}, kind={kind}) failed: {err}",
            bus_root.display()
        );
    }
}

fn emit_inner<P: Serialize>(
    bus_root: &Path,
    kind: &str,
    payload: P,
    source: Option<&str>,
) -> anyhow::Result<()> {
    let kind_dir = bus_root.join(kind);
    std::fs::create_dir_all(&kind_dir)?;

    let now = chrono::Local::now();
    let ts = now.format("%Y-%m-%dT%H-%M-%S").to_string();
    let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
    // PID padded to 6 digits — sufficient on macOS (default pid_max ≈ 99999).
    // Linux pids can exceed 6 digits; if/when Linux ships, widen the field
    // *and* keep zero-pad consistent so filename lex-sort stays correct.
    let pid = std::process::id();
    let filename = format!("{ts}-{counter:06}-{pid:06}.json");
    let final_path = kind_dir.join(&filename);
    let tmp_path = kind_dir.join(format!(".{filename}.tmp"));

    let envelope = EventEnvelope {
        schema: ENVELOPE_SCHEMA,
        kind: kind.to_string(),
        timestamp: now.to_rfc3339(),
        source: source.map(str::to_string),
        payload: serde_json::to_value(&payload)?,
    };
    let json = serde_json::to_string(&envelope)?;

    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Reader for one specific event kind. Each kind-handler owns one of these.
///
/// `Clone` is required because consumers (e.g. the dashboard-side
/// `DashboardNotificationInbox`) clone the inbox into spawned async blocks.
/// Per-clone state (warning-dedupe sets) is shared via `Arc<Mutex<…>>` so
/// dedupe is correct across clones.
#[derive(Clone)]
pub struct EventInbox {
    kind: String,
    root: PathBuf,
    warned_versions: Arc<Mutex<HashSet<u32>>>,
    warned_error_hashes: Arc<Mutex<HashSet<u64>>>,
}

impl EventInbox {
    /// `root` is the event-bus root (`…/events`); the inbox reads from
    /// `root/<kind>/`. The kind-subdirectory and its `processed/` /
    /// `rejected/` siblings are created on first drain.
    pub fn new(kind: impl Into<String>, root: PathBuf) -> Self {
        Self {
            kind: kind.into(),
            root,
            warned_versions: Arc::new(Mutex::new(HashSet::new())),
            warned_error_hashes: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Returns the kind-subdirectory backing this inbox (`<root>/<kind>`).
    /// Useful for fs-watch subscriptions.
    pub fn kind_dir(&self) -> PathBuf {
        self.root.join(&self.kind)
    }

    /// Drain up to [`MAX_EVENTS_PER_DRAIN`] pending events. Invokes
    /// `on_event(envelope, event_id)` for each successfully parsed file
    /// matching this inbox's kind, where `event_id` is the filename stem
    /// (stable, unique per emitted file — used to build composite
    /// notification IDs so rapid-fire events don't collapse into one slot).
    ///
    /// Successful events move to `processed/`. Malformed JSON, oversize
    /// files, and wrong-kind envelopes move to `rejected/`. Unknown
    /// envelope schema versions are logged at `warn!` (deduped by version
    /// number) and moved to `processed/` (forward-compat — treat as
    /// consumed).
    pub fn drain(&self, mut on_event: impl FnMut(EventEnvelope, String)) {
        let kind_dir = self.kind_dir();
        let processed_dir = kind_dir.join("processed");
        let rejected_dir = kind_dir.join("rejected");

        if let Err(err) = std::fs::create_dir_all(&kind_dir) {
            log::warn!(
                "postprod_events: create_dir_all({}) failed: {err}",
                kind_dir.display()
            );
            return;
        }
        let _ = std::fs::create_dir_all(&processed_dir);
        let _ = std::fs::create_dir_all(&rejected_dir);

        let entries = match std::fs::read_dir(&kind_dir) {
            Ok(e) => e,
            Err(err) => {
                log::warn!(
                    "postprod_events: read_dir({}) failed: {err}",
                    kind_dir.display()
                );
                return;
            }
        };

        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    return false;
                };
                if name.starts_with('.') {
                    return false;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    return false;
                }
                // Non-recursive: must be a regular file at the kind-dir root.
                path.is_file()
            })
            .collect();

        paths.sort();
        paths.truncate(MAX_EVENTS_PER_DRAIN);

        for path in paths {
            self.process_one(&path, &processed_dir, &rejected_dir, &mut on_event);
        }
    }

    fn process_one(
        &self,
        path: &Path,
        processed_dir: &Path,
        rejected_dir: &Path,
        on_event: &mut dyn FnMut(EventEnvelope, String),
    ) {
        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => {
                log::warn!("postprod_events: non-utf8 filename {:?}", path);
                return;
            }
        };
        let event_id = filename
            .strip_suffix(".json")
            .map(str::to_string)
            .unwrap_or_else(|| filename.clone());

        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(err) => {
                log::warn!(
                    "postprod_events: metadata({}) failed: {err}",
                    path.display()
                );
                return;
            }
        };
        if metadata.len() > MAX_EVENT_BYTES {
            log::warn!(
                "postprod_events: file {filename} ({} bytes) exceeds {MAX_EVENT_BYTES}-byte cap; moving to rejected/",
                metadata.len()
            );
            let _ = std::fs::rename(path, rejected_dir.join(&filename));
            return;
        }

        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(err) => {
                log::warn!("postprod_events: read({}) failed: {err}", path.display());
                return;
            }
        };

        let envelope: EventEnvelope = match serde_json::from_slice(&bytes) {
            Ok(env) => env,
            Err(err) => {
                let err_str = err.to_string();
                let mut hasher = DefaultHasher::new();
                err_str.hash(&mut hasher);
                let hash = hasher.finish();
                let mut warned = self
                    .warned_error_hashes
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if warned.insert(hash) {
                    log::warn!(
                        "postprod_events: parse error in {filename} — {err_str} (further identical errors deduped)"
                    );
                }
                let _ = std::fs::rename(path, rejected_dir.join(&filename));
                return;
            }
        };

        if envelope.schema != ENVELOPE_SCHEMA {
            let mut warned = self
                .warned_versions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if warned.insert(envelope.schema) {
                log::warn!(
                    "postprod_events: envelope schema {} not supported (current = {ENVELOPE_SCHEMA}); moving to processed/, further identical-version files deduped",
                    envelope.schema
                );
            }
            let _ = std::fs::rename(path, processed_dir.join(&filename));
            return;
        }

        if envelope.kind != self.kind {
            log::warn!(
                "postprod_events: file {filename} declares kind {:?} but reader is {:?}; moving to rejected/",
                envelope.kind, self.kind
            );
            let _ = std::fs::rename(path, rejected_dir.join(&filename));
            return;
        }

        on_event(envelope, event_id);
        let _ = std::fs::rename(path, processed_dir.join(&filename));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fresh_inbox(kind: &str) -> (TempDir, EventInbox) {
        let dir = TempDir::new().expect("tempdir");
        let inbox = EventInbox::new(kind, dir.path().to_path_buf());
        (dir, inbox)
    }

    fn list_pending(dir: &Path, kind: &str) -> Vec<PathBuf> {
        let kind_dir = dir.join(kind);
        let mut entries: Vec<_> = fs::read_dir(&kind_dir)
            .map(|it| {
                it.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        entries
    }

    fn list_processed(dir: &Path, kind: &str) -> Vec<PathBuf> {
        fs::read_dir(dir.join(kind).join("processed"))
            .map(|it| {
                it.filter_map(Result::ok)
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list_rejected(dir: &Path, kind: &str) -> Vec<PathBuf> {
        fs::read_dir(dir.join(kind).join("rejected"))
            .map(|it| {
                it.filter_map(Result::ok)
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    }

    // Test 1: emit writes a well-formed JSON envelope at <root>/<kind>/<file>.json
    #[test]
    fn emit_writes_envelope_at_correct_path() {
        let dir = TempDir::new().expect("tempdir");
        emit_to(dir.path(), "notification", serde_json::json!({"hi": 1}), Some("src"));

        let pending = list_pending(dir.path(), "notification");
        assert_eq!(pending.len(), 1, "exactly one .json file at <root>/notification/");
        let body = fs::read_to_string(&pending[0]).unwrap();
        let env: EventEnvelope = serde_json::from_str(&body).unwrap();
        assert_eq!(env.schema, ENVELOPE_SCHEMA);
        assert_eq!(env.kind, "notification");
        assert_eq!(env.source.as_deref(), Some("src"));
        assert_eq!(env.payload["hi"], 1);
        assert!(!env.timestamp.is_empty());
    }

    // Test 2: atomic rename — no .json visible during write (we only check
    // post-condition: the temp file is not visible to the *.json glob).
    #[test]
    fn atomic_rename_leaves_no_temp_file_visible() {
        let dir = TempDir::new().expect("tempdir");
        emit_to(dir.path(), "notification", serde_json::json!({}), None);
        let kind_dir = dir.path().join("notification");
        let entries: Vec<_> = fs::read_dir(&kind_dir).unwrap().filter_map(Result::ok).collect();
        for entry in &entries {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s == "processed" || s == "rejected" {
                continue;
            }
            assert!(!s.starts_with('.'), "no leftover temp dotfile, got {s}");
            assert!(s.ends_with(".json"), "only .json files visible, got {s}");
        }
    }

    // Test 3: filename format is fixed-width and sorts chronologically across
    // 1000 rapid emits from the same pid.
    #[test]
    fn filename_sort_is_chronological_within_process() {
        let dir = TempDir::new().expect("tempdir");
        for i in 0..1000 {
            emit_to(dir.path(), "notification", serde_json::json!({"i": i}), None);
        }
        let pending = list_pending(dir.path(), "notification");
        assert_eq!(pending.len(), 1000);

        // Read back in sorted order (list_pending sorts) and verify the payload
        // counter is monotonic.
        let mut last_i: i64 = -1;
        for path in pending {
            let body = fs::read_to_string(&path).unwrap();
            let env: EventEnvelope = serde_json::from_str(&body).unwrap();
            let i = env.payload["i"].as_i64().unwrap();
            assert!(i > last_i, "monotonic: {i} > {last_i}");
            last_i = i;
        }
    }

    // Test 4: env override redirects writes.
    #[test]
    fn env_override_redirects_default_bus_root() {
        // Ensure HOME is set to something we don't care about; the override
        // dominates regardless.
        let dir = TempDir::new().expect("tempdir");
        // SAFETY: tests are not parallelized inside this single test, but
        // env vars are process-global. Keep this scoped to the test.
        // SAFETY: setting an env var is unsafe in Rust 2024 because of
        // potential data races with concurrent threads. This is a single-
        // threaded test, so it's fine.
        unsafe {
            std::env::set_var(INBOX_ENV_VAR, dir.path());
        }
        let resolved = default_bus_root().expect("override path");
        unsafe {
            std::env::remove_var(INBOX_ENV_VAR);
        }
        assert_eq!(resolved, dir.path());
    }

    // Test 5: drain on a valid envelope matching the kind calls the callback
    // and moves the file to processed/.
    #[test]
    fn drain_consumes_valid_event() {
        let (dir, inbox) = fresh_inbox("notification");
        emit_to(dir.path(), "notification", serde_json::json!({"a": 1}), None);

        let mut got = Vec::new();
        inbox.drain(|env, id| got.push((env, id)));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.payload["a"], 1);
        assert!(!got[0].1.is_empty(), "event_id is the filename stem");

        assert!(list_pending(dir.path(), "notification").is_empty());
        assert_eq!(list_processed(dir.path(), "notification").len(), 1);
    }

    // Test 6: drain on malformed JSON moves to rejected/, no callback.
    #[test]
    fn drain_malformed_json_rejected_no_callback() {
        let (dir, inbox) = fresh_inbox("notification");
        let kind_dir = dir.path().join("notification");
        fs::create_dir_all(&kind_dir).unwrap();
        fs::write(kind_dir.join("2026-04-18T00-00-00-000000-000001.json"), "not json")
            .unwrap();

        let mut called = false;
        inbox.drain(|_, _| called = true);
        assert!(!called);
        assert!(list_pending(dir.path(), "notification").is_empty());
        assert_eq!(list_rejected(dir.path(), "notification").len(), 1);
    }

    // Test 7: schema-version mismatch — file moves to processed/, single warn
    // log per unique unknown version. We can't easily assert log output without
    // a logging fixture, so we assert the placement + that the dedupe set
    // grows by one.
    #[test]
    fn drain_unknown_schema_version_moves_to_processed_with_dedupe() {
        let (dir, inbox) = fresh_inbox("notification");
        let kind_dir = dir.path().join("notification");
        fs::create_dir_all(&kind_dir).unwrap();

        let env_v2 = serde_json::json!({
            "schema": 999,
            "kind": "notification",
            "timestamp": "2026-04-18T00:00:00-03:00",
            "payload": {}
        });
        fs::write(
            kind_dir.join("2026-04-18T00-00-00-000000-000001.json"),
            env_v2.to_string(),
        )
        .unwrap();
        fs::write(
            kind_dir.join("2026-04-18T00-00-00-000001-000001.json"),
            env_v2.to_string(),
        )
        .unwrap();

        inbox.drain(|_, _| {});
        // Both files moved to processed/.
        assert!(list_pending(dir.path(), "notification").is_empty());
        assert_eq!(list_processed(dir.path(), "notification").len(), 2);
        // Dedupe set holds exactly one version.
        let warned = inbox.warned_versions.lock().unwrap();
        assert_eq!(warned.len(), 1);
        assert!(warned.contains(&999));
    }

    // Test 8: wrong-kind envelope in this reader's directory → rejected/.
    #[test]
    fn drain_wrong_kind_rejected() {
        let (dir, inbox) = fresh_inbox("notification");
        let kind_dir = dir.path().join("notification");
        fs::create_dir_all(&kind_dir).unwrap();
        let bad = serde_json::json!({
            "schema": 1,
            "kind": "bounce.completed",
            "timestamp": "2026-04-18T00:00:00-03:00",
            "payload": {}
        });
        fs::write(
            kind_dir.join("2026-04-18T00-00-00-000000-000001.json"),
            bad.to_string(),
        )
        .unwrap();

        let mut called = false;
        inbox.drain(|_, _| called = true);
        assert!(!called);
        assert_eq!(list_rejected(dir.path(), "notification").len(), 1);
    }

    // Test 9: file > 64 KiB → rejected without parsing.
    #[test]
    fn drain_oversize_file_rejected_unparsed() {
        let (dir, inbox) = fresh_inbox("notification");
        let kind_dir = dir.path().join("notification");
        fs::create_dir_all(&kind_dir).unwrap();
        let big = "x".repeat((MAX_EVENT_BYTES + 1) as usize);
        fs::write(kind_dir.join("2026-04-18T00-00-00-000000-000001.json"), big).unwrap();

        let mut called = false;
        inbox.drain(|_, _| called = true);
        assert!(!called);
        assert_eq!(list_rejected(dir.path(), "notification").len(), 1);
    }

    // Test 10: drain processes at most 50 events per call.
    #[test]
    fn drain_caps_at_max_per_call() {
        let (dir, inbox) = fresh_inbox("notification");
        for _ in 0..60 {
            emit_to(dir.path(), "notification", serde_json::json!({}), None);
        }
        assert_eq!(list_pending(dir.path(), "notification").len(), 60);

        let mut count = 0;
        inbox.drain(|_, _| count += 1);
        assert_eq!(count, MAX_EVENTS_PER_DRAIN);
        assert_eq!(
            list_pending(dir.path(), "notification").len(),
            60 - MAX_EVENTS_PER_DRAIN
        );
    }

    // Test 11: already-processed files under processed/ not re-read (drain is
    // non-recursive at the kind-dir root).
    #[test]
    fn drain_does_not_re_read_processed() {
        let (dir, inbox) = fresh_inbox("notification");
        emit_to(dir.path(), "notification", serde_json::json!({}), None);
        let mut count = 0;
        inbox.drain(|_, _| count += 1);
        assert_eq!(count, 1);

        // Second drain — files are now under processed/, should be skipped.
        let mut count2 = 0;
        inbox.drain(|_, _| count2 += 1);
        assert_eq!(count2, 0);
    }

    // Test 23: events in a sibling kind subdirectory are NOT seen by this
    // reader. Guards against accidental cross-kind consumption (the
    // notification reader must not consume bounce.completed envelopes
    // even if the bus root is shared).
    #[test]
    fn drain_ignores_sibling_kind_subdirectories() {
        let dir = TempDir::new().expect("tempdir");
        // Emit to a sibling kind that this reader is NOT configured for.
        emit_to(dir.path(), "bounce.completed", serde_json::json!({}), None);
        emit_to(dir.path(), "bounce.completed", serde_json::json!({}), None);

        // Construct a notification reader on the same root.
        let inbox = EventInbox::new("notification", dir.path().to_path_buf());
        let mut count = 0;
        inbox.drain(|_, _| count += 1);
        assert_eq!(count, 0, "notification reader must not consume sibling-kind events");

        // The sibling-kind files are still pending in their own subdir.
        assert_eq!(list_pending(dir.path(), "bounce.completed").len(), 2);
    }

    // Test 12: unknown envelope fields ignored, not an error.
    #[test]
    fn drain_unknown_envelope_fields_ignored() {
        let (dir, inbox) = fresh_inbox("notification");
        let kind_dir = dir.path().join("notification");
        fs::create_dir_all(&kind_dir).unwrap();
        let with_extra = serde_json::json!({
            "schema": 1,
            "kind": "notification",
            "timestamp": "2026-04-18T00:00:00-03:00",
            "source": "tool",
            "payload": {"title": "t", "body": "b", "severity": "info"},
            "future_field": "should be ignored",
        });
        fs::write(
            kind_dir.join("2026-04-18T00-00-00-000000-000001.json"),
            with_extra.to_string(),
        )
        .unwrap();

        let mut got = 0;
        inbox.drain(|_, _| got += 1);
        assert_eq!(got, 1);
        assert_eq!(list_processed(dir.path(), "notification").len(), 1);
    }
}
