//! Folder Watcher runtime for the PostProd IDE dashboard.
//!
//! A *watcher* is a user-configurable event emitter that turns "a file
//! appeared / changed / disappeared in this folder" into bus events
//! (`postprod_events`). v1 watchers ship as TOML files under
//! `config_root/config/watchers/<id>.toml` and emit `notification` kind
//! events; future kinds become additive (new consumer, no watcher rework).
//!
//! The full spec — TOML schema, ignore rules, template variables, hash-gated
//! reconciliation — lives at `private/specs/event-notifications.md`.
//!
//! ## Public surface
//!
//! - [`WatcherId`] — newtype around the TOML `id` string.
//! - [`WatcherStatus`] — what the dashboard renders on each card.
//! - [`WatcherError`] — validation failure type returned by [`validate`].
//! - [`WatcherRuntime`] — owns the running per-watcher tasks; `reconcile`
//!   is hash-gated (D19) so the dashboard's 10 s reload tick is free when
//!   the config set hasn't changed.
//! - [`expand_template`] — variable expansion used by the per-watcher task
//!   (exposed for testability and so future emit-kinds can reuse it).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use chrono::Utc;
use fs::{Fs, PathEventKind};
use futures::StreamExt;
use gpui::{App, AppContext as _, Task};
use postprod_dashboard_config::watcher_config::{TriggerKind, WatcherConfig, WatcherEmit};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Stable per-watcher identifier. Derived 1:1 from `WatcherConfig::id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatcherId(pub String);

impl From<&str> for WatcherId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for WatcherId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for WatcherId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Card-level watcher status, rendered by the dashboard's WATCHERS section.
#[derive(Debug, Clone)]
pub enum WatcherStatus {
    /// Watcher running; no events since start.
    Idle,
    /// Last successful emit timestamp (UTC). Card renders relative time.
    LastEmit(chrono::DateTime<chrono::Utc>),
    /// Watcher failed to start or has stopped on an error.
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error("`id` must not be empty")]
    EmptyId,
    #[error("`label` must not be empty")]
    EmptyLabel,
    #[error("`path` must not be empty")]
    EmptyPath,
    #[error("at least one `[[emit]]` block is required")]
    EmptyEmits,
    #[error("`[[emit]]` #{idx} has empty `kind`")]
    EmptyEmitKind { idx: usize },
    #[error("invalid glob {glob:?}: {detail}")]
    InvalidGlob { glob: String, detail: String },
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Defensive runtime validation of a `WatcherConfig`. The TOML loader in
/// `postprod_dashboard_config` already enforces these invariants for files
/// it accepts; this is the second guard for any caller that constructs
/// configs in code.
pub fn validate(cfg: &WatcherConfig) -> Result<(), WatcherError> {
    if cfg.id.trim().is_empty() {
        return Err(WatcherError::EmptyId);
    }
    if cfg.label.trim().is_empty() {
        return Err(WatcherError::EmptyLabel);
    }
    if cfg.path.trim().is_empty() {
        return Err(WatcherError::EmptyPath);
    }
    if cfg.emits.is_empty() {
        return Err(WatcherError::EmptyEmits);
    }
    for (idx, emit) in cfg.emits.iter().enumerate() {
        if emit.kind.trim().is_empty() {
            return Err(WatcherError::EmptyEmitKind { idx: idx + 1 });
        }
    }
    // Compile glob defensively to surface bad patterns at load time
    // rather than at first event.
    if let Err(err) = globset::Glob::new(&cfg.trigger.glob) {
        return Err(WatcherError::InvalidGlob {
            glob: cfg.trigger.glob.clone(),
            detail: err.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Expand `~` and `$VAR` / `${VAR}` references against the current
/// process environment. Unknown env vars expand to empty.
pub fn resolve_watched_path(raw: &str) -> PathBuf {
    let with_home = if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest).to_string_lossy().into_owned()
        } else {
            raw.to_string()
        }
    } else if raw == "~" {
        dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw.to_string())
    } else {
        raw.to_string()
    };
    PathBuf::from(expand_env_vars(&with_home))
}

fn expand_env_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '}' {
                        chars.next();
                        break;
                    }
                    name.push(nc);
                    chars.next();
                }
                if let Ok(val) = std::env::var(&name) {
                    out.push_str(&val);
                }
            }
            Some(c2) if c2.is_ascii_alphabetic() || *c2 == '_' => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc.is_ascii_alphanumeric() || nc == '_' {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Ok(val) = std::env::var(&name) {
                    out.push_str(&val);
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Hardcoded ignore rules
// ---------------------------------------------------------------------------

/// Ignore filenames matching any of:
/// - leading `.` (covers `.DS_Store`, `.foo.tmp`, atomic-write temp files,
///   macOS metadata, dot-prefixed editor swaps)
/// - suffix `.tmp` / `.swp` / `.swo` / `.swn` / `~` (covers Vim/Neovim swaps
///   and Emacs backups even when not dot-prefixed)
///
/// Per spec § "Trigger semantics". Not configurable in v1.
pub fn is_ignored_filename(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    const SUFFIXES: &[&str] = &[".tmp", ".swp", ".swo", ".swn", "~"];
    SUFFIXES.iter().any(|s| name.ends_with(s))
}

// ---------------------------------------------------------------------------
// Template variable expansion
// ---------------------------------------------------------------------------

/// Variables available to template strings inside `[[emit]]`. Built per
/// triggering event by the per-watcher task.
#[derive(Debug, Clone)]
pub struct TemplateVars {
    pub path: String,
    pub filename: String,
    pub stem: String,
    pub ext: String,
    pub size_bytes: u64,
    pub size_mb: f64,
    pub folder: String,
    pub trigger: &'static str,
}

impl TemplateVars {
    pub fn for_event(
        absolute_path: &Path,
        watched_folder: &Path,
        kind: PathEventKind,
        size_bytes: u64,
    ) -> Self {
        let filename = absolute_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = absolute_path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = absolute_path
            .extension()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size_mb = (size_bytes as f64) / (1024.0 * 1024.0);
        // Round size_mb to one decimal for display: 12.345 → 12.3.
        let size_mb_rounded = (size_mb * 10.0).round() / 10.0;
        Self {
            path: absolute_path.to_string_lossy().into_owned(),
            filename,
            stem,
            ext,
            size_bytes,
            size_mb: size_mb_rounded,
            folder: watched_folder.to_string_lossy().into_owned(),
            trigger: trigger_label(kind),
        }
    }

    fn lookup(&self, name: &str) -> String {
        match name {
            "path" => self.path.clone(),
            "filename" => self.filename.clone(),
            "stem" => self.stem.clone(),
            "ext" => self.ext.clone(),
            "size_bytes" => self.size_bytes.to_string(),
            "size_mb" => format!("{:.1}", self.size_mb),
            "folder" => self.folder.clone(),
            "trigger" => self.trigger.to_string(),
            _ => String::new(),
        }
    }
}

fn trigger_label(kind: PathEventKind) -> &'static str {
    match kind {
        PathEventKind::Created => "created",
        PathEventKind::Changed => "modified",
        PathEventKind::Removed => "deleted",
        PathEventKind::Rescan => "rescan",
    }
}

/// Replace `{name}` tokens in `template` with values from `vars`. Unknown
/// names expand to the empty string (per spec). Standalone `{` characters
/// pass through.
pub fn expand_template(template: &str, vars: &TemplateVars) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        while let Some(&nc) = chars.peek() {
            if nc == '}' {
                chars.next();
                closed = true;
                break;
            }
            name.push(nc);
            chars.next();
        }
        if closed {
            out.push_str(&vars.lookup(&name));
        } else {
            // Unterminated — treat as literal.
            out.push('{');
            out.push_str(&name);
        }
    }
    out
}

/// Recursively walk a `toml::Value` and apply [`expand_template`] to every
/// string leaf. Used to expand the kind-specific payload (e.g.
/// notification's `title` / `body` / `source`) at emit time.
pub fn expand_payload(value: &toml::Value, vars: &TemplateVars) -> toml::Value {
    match value {
        toml::Value::String(s) => toml::Value::String(expand_template(s, vars)),
        toml::Value::Array(items) => {
            toml::Value::Array(items.iter().map(|v| expand_payload(v, vars)).collect())
        }
        toml::Value::Table(map) => {
            let mut out = toml::map::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), expand_payload(v, vars));
            }
            toml::Value::Table(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Trigger matching
// ---------------------------------------------------------------------------

/// Decide whether a `PathEvent` should fire the configured trigger.
///
/// `PathEvent::kind = None` is treated as `Any`-match (FSEvents couldn't
/// determine the event kind; conservative to emit if the watcher would
/// have accepted any kind). `PathEventKind::Rescan` is always ignored
/// (it's a worktree-rescan hint, not a file event).
fn matches_trigger(configured: TriggerKind, observed: Option<PathEventKind>) -> bool {
    match observed {
        None => configured == TriggerKind::Any
            || matches!(
                configured,
                TriggerKind::FileCreated
                    | TriggerKind::FileModified
                    | TriggerKind::FileDeleted
            ),
        Some(PathEventKind::Rescan) => false,
        Some(kind) => match configured {
            TriggerKind::Any => true,
            TriggerKind::FileCreated => kind == PathEventKind::Created,
            TriggerKind::FileModified => kind == PathEventKind::Changed,
            TriggerKind::FileDeleted => kind == PathEventKind::Removed,
        },
    }
}

// ---------------------------------------------------------------------------
// WatcherRuntime
// ---------------------------------------------------------------------------

/// Owns the set of running per-watcher tasks. The dashboard creates one
/// of these in `Dashboard::new`, calls [`Self::reconcile`] from the
/// dedicated 10 s `_watchers_reload_task`, and subscribes to status
/// updates via [`Self::status_receiver`].
pub struct WatcherRuntime {
    tasks: HashMap<WatcherId, Task<()>>,
    status_tx: smol::channel::Sender<(WatcherId, WatcherStatus)>,
    status_rx: smol::channel::Receiver<(WatcherId, WatcherStatus)>,
    /// Hash of the currently-running config set. `reconcile` short-
    /// circuits when the incoming hash matches (D19). On first call,
    /// `None` so any incoming set triggers a spawn.
    last_config_hash: Option<u64>,
}

impl Default for WatcherRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl WatcherRuntime {
    pub fn new() -> Self {
        let (status_tx, status_rx) = smol::channel::unbounded();
        Self {
            tasks: HashMap::new(),
            status_tx,
            status_rx,
            last_config_hash: None,
        }
    }

    /// Returns a clone of the status channel receiver so the dashboard's
    /// status-listener loop can subscribe.
    pub fn status_receiver(&self) -> smol::channel::Receiver<(WatcherId, WatcherStatus)> {
        self.status_rx.clone()
    }

    /// Number of running per-watcher tasks (for tests + telemetry).
    pub fn running_count(&self) -> usize {
        self.tasks.len()
    }

    /// Test-only accessor to compare task identity across reconciles
    /// (D19 hash short-circuit verification).
    #[doc(hidden)]
    pub fn tasks_for_test(&self) -> &HashMap<WatcherId, Task<()>> {
        &self.tasks
    }

    /// Hash-gated blanket stop-all/start-all reconciliation. Per D5+D19:
    ///
    /// 1. Compute the hash of `configs` (stable, derived).
    /// 2. If equal to `last_config_hash`, return immediately — no-op.
    /// 3. Otherwise drop all running tasks (cancels their fs-watch
    ///    subscriptions), spawn a fresh task per enabled+valid watcher,
    ///    update `last_config_hash`.
    pub fn reconcile(
        &mut self,
        configs: Vec<WatcherConfig>,
        fs: Arc<dyn Fs>,
        bus_root: PathBuf,
        cx: &App,
    ) {
        let new_hash = hash_configs(&configs);
        if Some(new_hash) == self.last_config_hash {
            return;
        }

        // Blanket stop-all (drop cancels each task's spawn).
        self.tasks.clear();

        for cfg in configs {
            if !cfg.enabled {
                continue;
            }
            let id = WatcherId(cfg.id.clone());
            match validate(&cfg) {
                Ok(()) => {
                    let task = spawn_watcher_task(
                        id.clone(),
                        cfg,
                        fs.clone(),
                        bus_root.clone(),
                        self.status_tx.clone(),
                        cx,
                    );
                    self.tasks.insert(id, task);
                }
                Err(err) => {
                    let _ = self
                        .status_tx
                        .try_send((id, WatcherStatus::Error(err.to_string())));
                }
            }
        }

        self.last_config_hash = Some(new_hash);
    }
}

fn hash_configs(configs: &[WatcherConfig]) -> u64 {
    let mut hasher = DefaultHasher::new();
    configs.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Per-watcher task
// ---------------------------------------------------------------------------

fn spawn_watcher_task(
    id: WatcherId,
    cfg: WatcherConfig,
    fs: Arc<dyn Fs>,
    bus_root: PathBuf,
    status_tx: smol::channel::Sender<(WatcherId, WatcherStatus)>,
    cx: &App,
) -> Task<()> {
    cx.background_spawn(async move {
        if let Err(err) = run_watcher(&id, &cfg, fs, bus_root, &status_tx).await {
            let _ = status_tx.try_send((id, WatcherStatus::Error(err.to_string())));
        }
    })
}

async fn run_watcher(
    id: &WatcherId,
    cfg: &WatcherConfig,
    fs: Arc<dyn Fs>,
    bus_root: PathBuf,
    status_tx: &smol::channel::Sender<(WatcherId, WatcherStatus)>,
) -> anyhow::Result<()> {
    let watched_folder = resolve_watched_path(&cfg.path);
    if !fs.is_dir(&watched_folder).await {
        anyhow::bail!("folder not found: {}", watched_folder.display());
    }

    // Compile glob once.
    let glob = globset::GlobBuilder::new(&cfg.trigger.glob)
        .literal_separator(false)
        .build()
        .with_context(|| format!("invalid glob: {}", cfg.trigger.glob))?
        .compile_matcher();

    // Initial idle status — card shows "idle" until the first emit.
    let _ = status_tx.try_send((id.clone(), WatcherStatus::Idle));

    let (mut events, _handle) = fs.watch(&watched_folder, FS_WATCH_LATENCY).await;

    // Per-path debounce: maps PathBuf → "next eligible emit time."
    // Leading-edge: the first event in a window emits immediately, the
    // remainder are suppressed until the window closes. Rationale: simpler
    // than trailing-edge with a dedicated timer task, and v1's primary
    // need (collapse atomic-write fire-storms) is satisfied.
    let mut last_emit: HashMap<PathBuf, SystemTime> = HashMap::new();
    let debounce = Duration::from_millis(cfg.trigger.debounce_ms);

    while let Some(batch) = events.next().await {
        for event in batch {
            // Filter to immediate children of the watched folder (non-recursive).
            if event.path.parent() != Some(&watched_folder) {
                continue;
            }
            let Some(name) = event.path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_ignored_filename(name) {
                continue;
            }
            if !matches_trigger(cfg.trigger.on, event.kind) {
                continue;
            }
            if !glob.is_match(name) {
                continue;
            }

            // Determine effective event kind for downstream (None → match
            // whatever trigger was configured for; if Any, treat as Created).
            let effective_kind = event.kind.unwrap_or(match cfg.trigger.on {
                TriggerKind::FileCreated | TriggerKind::Any => PathEventKind::Created,
                TriggerKind::FileModified => PathEventKind::Changed,
                TriggerKind::FileDeleted => PathEventKind::Removed,
            });
            if effective_kind == PathEventKind::Rescan {
                continue;
            }

            // Size + min_size_mb filter (skipped on delete).
            let (size_bytes, exists) = if effective_kind == PathEventKind::Removed {
                (0u64, false)
            } else {
                match fs.metadata(&event.path).await {
                    Ok(Some(meta)) => (meta.len, true),
                    _ => (0u64, false),
                }
            };
            if exists && cfg.trigger.min_size_mb > 0.0 {
                let size_mb = (size_bytes as f64) / (1024.0 * 1024.0);
                if size_mb < cfg.trigger.min_size_mb {
                    continue;
                }
            }

            // Debounce: suppress repeats on the same path inside the window.
            let now = SystemTime::now();
            if let Some(prev) = last_emit.get(&event.path) {
                if now.duration_since(*prev).unwrap_or_default() < debounce {
                    continue;
                }
            }
            last_emit.insert(event.path.clone(), now);

            let vars =
                TemplateVars::for_event(&event.path, &watched_folder, effective_kind, size_bytes);

            for emit in &cfg.emits {
                emit_one(&bus_root, emit, &vars);
            }

            let _ = status_tx.try_send((id.clone(), WatcherStatus::LastEmit(Utc::now())));
        }
    }
    Ok(())
}

const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);

fn emit_one(bus_root: &Path, emit: &WatcherEmit, vars: &TemplateVars) {
    // Expand all string fields under the kind-specific payload.
    let payload_value = toml::Value::Table(emit.payload.clone());
    let expanded = expand_payload(&payload_value, vars);

    // Convert the TOML table to JSON for the envelope payload + extract
    // the optional `source` field if present (envelope-level).
    let mut json_payload = toml_to_json(&expanded);
    let source = json_payload
        .as_object_mut()
        .and_then(|m| m.remove("source"))
        .and_then(|v| v.as_str().map(str::to_string));

    postprod_events::bus::emit_to(bus_root, &emit.kind, json_payload, source.as_deref());
}

fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Value::from(*f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                obj.insert(k.clone(), toml_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

#[cfg(test)]
mod tests;
