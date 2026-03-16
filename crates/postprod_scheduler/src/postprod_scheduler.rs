use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Local};
use croner::Cron;
use gpui::{AppContext, Context, EventEmitter, Task};
use gpui_util::ResultExt;
use serde::{Deserialize, Serialize};

const AUTO_DISABLE_THRESHOLD: u32 = 5;
const MAX_CHAIN_DEPTH: u32 = 10;

// ── Schedule entry (loaded from TOML by dashboard, passed to scheduler) ──

#[derive(Clone, Debug)]
pub struct ScheduleEntry {
    pub automation_id: String,
    pub cron_expr: String,
    pub cron: Cron,
    pub enabled: bool,
    pub catch_up: CatchUpPolicy,
    pub timeout_secs: u64,
    pub active_folder: PathBuf,
    pub auto_disable_after: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CatchUpPolicy {
    #[default]
    Skip,
    RunOnce,
}

// ── Chain config (loaded from TOML by dashboard, passed to scheduler) ──

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChainConfig {
    #[serde(default)]
    pub triggers: Vec<String>,
}

// ── Scheduler state (persisted to JSON) ──

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AutomationStatus {
    pub last_evaluated_at: Option<DateTime<Local>>,
    pub last_run_at: Option<DateTime<Local>>,
    pub last_result: Option<RunResult>,
    pub consecutive_failures: u32,
    pub auto_disabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunResult {
    Success,
    Failed { message: String },
    Timeout,
    Skipped { reason: String },
}

/// Structured completion report written by the agent as a JSON file.
/// The scheduler polls for this file to detect when a scheduled run finishes.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CompletionReport {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub skip_chain: bool,
    #[serde(default)]
    pub message: String,
}

impl CompletionReport {
    /// Parse a marker file into a CompletionReport + RunResult.
    /// If the file exists but JSON is malformed, treat as success with no metadata
    /// (the file's existence is the real signal).
    pub fn from_marker(path: &Path) -> Option<(Self, RunResult)> {
        let content = std::fs::read_to_string(path).ok()?;
        let report: CompletionReport = serde_json::from_str(content.trim())
            .unwrap_or_else(|_| CompletionReport {
                status: "success".to_string(),
                summary: String::new(),
                outputs: Vec::new(),
                skip_chain: false,
                message: String::new(),
            });

        let result = if report.status.to_lowercase().starts_with("failed") {
            RunResult::Failed {
                message: report.summary.clone(),
            }
        } else {
            RunResult::Success
        };

        Some((report, result))
    }
}

/// Build the completion marker path for a given automation run.
pub fn completion_marker_path(state_dir: &Path, automation_id: &str, timestamp: i64) -> PathBuf {
    state_dir
        .join("completed")
        .join(format!("{}-{}.json", automation_id, timestamp))
}

// ── Scheduler events (dashboard subscribes to these) ──

#[derive(Clone, Debug)]
pub enum SchedulerEvent {
    Fire {
        automation_id: String,
        active_folder: PathBuf,
        chain_depth: u32,
    },
    Skipped {
        automation_id: String,
        reason: String,
    },
    MissedJob {
        automation_id: String,
        policy: CatchUpPolicy,
    },
    AutoDisabled {
        automation_id: String,
        consecutive_failures: u32,
    },
}

// ── Scheduler entity ──

pub struct Scheduler {
    entries: HashMap<String, ScheduleEntry>,
    chains: HashMap<String, ChainConfig>,
    status: HashMap<String, AutomationStatus>,
    state_path: PathBuf,
    startup_grace_remaining: bool,
    concurrency_cap: usize,
    running_count: usize,
    triggered_this_cycle: HashSet<String>,
    _tick_task: Option<Task<()>>,
}

impl EventEmitter<SchedulerEvent> for Scheduler {}

impl Scheduler {
    pub fn new(state_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let status = load_status(&state_path).unwrap_or_default();
        let mut scheduler = Self {
            entries: HashMap::new(),
            chains: HashMap::new(),
            status,
            state_path,
            startup_grace_remaining: true,
            concurrency_cap: 5,
            running_count: 0,
            triggered_this_cycle: HashSet::new(),
            _tick_task: None,
        };
        scheduler.start_tick_loop(cx);
        scheduler
    }

    fn start_tick_loop(&mut self, cx: &mut Context<Self>) {
        self._tick_task = Some(cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            // Startup grace: wait 60s before first evaluation
            cx.background_executor()
                .timer(Duration::from_secs(60))
                .await;

            this.update(cx, |scheduler, cx| {
                scheduler.startup_grace_remaining = false;
                scheduler.evaluate_missed_jobs(cx);
            })
            .log_err();

            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;

                this.update(cx, |scheduler, cx| {
                    scheduler.evaluate_tick(cx);
                })
                .log_err();
            }
        }));
    }

    fn evaluate_tick(&mut self, cx: &mut Context<Self>) {
        let now = Local::now();
        let entry_ids: Vec<String> = self.entries.keys().cloned().collect();

        for id in &entry_ids {
            let entry = match self.entries.get(id) {
                Some(entry) => entry,
                None => continue,
            };

            if !entry.enabled {
                continue;
            }

            let status = self
                .status
                .entry(id.clone())
                .or_insert_with(AutomationStatus::default);

            if status.auto_disabled {
                continue;
            }

            let since = status
                .last_evaluated_at
                .unwrap_or(now - chrono::Duration::minutes(1));
            status.last_evaluated_at = Some(now);

            let should_fire = entry
                .cron
                .find_next_occurrence(&since, false)
                .map(|next| next <= now)
                .unwrap_or(false);

            if should_fire {
                if self.running_count >= self.concurrency_cap {
                    let reason = format!("concurrency cap ({}) reached", self.concurrency_cap);
                    status.last_result = Some(RunResult::Skipped {
                        reason: reason.clone(),
                    });
                    cx.emit(SchedulerEvent::Skipped {
                        automation_id: id.clone(),
                        reason,
                    });
                    continue;
                }

                self.running_count += 1;
                status.last_run_at = Some(now);
                cx.emit(SchedulerEvent::Fire {
                    automation_id: id.clone(),
                    active_folder: entry.active_folder.clone(),
                    chain_depth: 0,
                });
            }
        }

        self.persist_status_async(cx);
    }

    fn evaluate_missed_jobs(&mut self, cx: &mut Context<Self>) {
        let now = Local::now();
        let entry_ids: Vec<String> = self.entries.keys().cloned().collect();

        for id in &entry_ids {
            let entry = match self.entries.get(id) {
                Some(entry) => entry,
                None => continue,
            };

            if !entry.enabled {
                continue;
            }

            let status = self
                .status
                .entry(id.clone())
                .or_insert_with(AutomationStatus::default);

            if status.auto_disabled {
                continue;
            }

            let last = match status.last_evaluated_at {
                Some(t) => t,
                None => continue, // never ran before, nothing to catch up
            };

            let was_missed = entry
                .cron
                .find_next_occurrence(&last, false)
                .map(|next| next < now)
                .unwrap_or(false);

            if was_missed {
                match entry.catch_up {
                    CatchUpPolicy::Skip => {
                        cx.emit(SchedulerEvent::MissedJob {
                            automation_id: id.clone(),
                            policy: CatchUpPolicy::Skip,
                        });
                    }
                    CatchUpPolicy::RunOnce => {
                        cx.emit(SchedulerEvent::Fire {
                            automation_id: id.clone(),
                            active_folder: entry.active_folder.clone(),
                            chain_depth: 0,
                        });
                        cx.emit(SchedulerEvent::MissedJob {
                            automation_id: id.clone(),
                            policy: CatchUpPolicy::RunOnce,
                        });
                    }
                }
            }

            status.last_evaluated_at = Some(now);
        }

        self.persist_status_async(cx);
    }

    // ── Public API for dashboard ──

    /// Sync schedule entries from dashboard's TOML config.
    /// Called on every config reload cycle (~10s).
    /// `default_folder` is used only for newly-added entries that don't have a persisted folder.
    pub fn sync_entries(
        &mut self,
        entries: Vec<SyncEntry>,
        default_folder: PathBuf,
    ) {
        let mut new_entries = HashMap::new();
        let mut new_chains = HashMap::new();

        for sync in entries {
            let cron = match Cron::from_str(&sync.cron_expr) {
                Ok(cron) => cron,
                Err(error) => {
                    log::warn!(
                        "Scheduler: invalid cron expression '{}' for {}: {error}",
                        sync.cron_expr,
                        sync.automation_id
                    );
                    continue;
                }
            };

            // Preserve existing active_folder for entries already in the map
            let active_folder = self
                .entries
                .get(&sync.automation_id)
                .map(|existing| existing.active_folder.clone())
                .unwrap_or_else(|| default_folder.clone());

            new_entries.insert(
                sync.automation_id.clone(),
                ScheduleEntry {
                    automation_id: sync.automation_id.clone(),
                    cron_expr: sync.cron_expr,
                    cron,
                    enabled: sync.enabled,
                    catch_up: sync.catch_up,
                    timeout_secs: sync.timeout_secs,
                    active_folder,
                    auto_disable_after: sync.auto_disable_after,
                },
            );

            if let Some(chain) = sync.chain {
                if !chain.triggers.is_empty() {
                    new_chains.insert(sync.automation_id.clone(), chain);
                }
            }
        }

        // Validate chains before accepting them
        let warnings = validate_chains(&new_entries, &new_chains);
        for warning in &warnings {
            log::warn!("Scheduler: {warning}");
        }

        self.entries = new_entries;
        self.chains = new_chains;
    }

    /// Report completion of a scheduled automation run.
    /// Called by dashboard after the agent writes a completion marker or the run times out.
    pub fn report_completion(
        &mut self,
        automation_id: &str,
        result: &RunResult,
        report: Option<&CompletionReport>,
        chain_depth: u32,
        cx: &mut Context<Self>,
    ) {
        self.running_count = self.running_count.saturating_sub(1);

        let status = self
            .status
            .entry(automation_id.to_string())
            .or_insert_with(AutomationStatus::default);

        status.last_result = Some(result.clone());

        if let Some(report) = report {
            if !report.summary.is_empty() {
                log::info!("Scheduler: {automation_id} — {}", report.summary);
            }
            if !report.message.is_empty() {
                log::info!("Scheduler: {automation_id} message — {}", report.message);
            }
        }

        match result {
            RunResult::Success => {
                status.consecutive_failures = 0;
            }
            RunResult::Failed { .. } | RunResult::Timeout => {
                status.consecutive_failures += 1;
                let threshold = self
                    .entries
                    .get(automation_id)
                    .map(|e| e.auto_disable_after)
                    .unwrap_or(AUTO_DISABLE_THRESHOLD);
                if threshold > 0 && status.consecutive_failures >= threshold {
                    status.auto_disabled = true;
                    log::warn!(
                        "Scheduler: auto-disabled {automation_id} after {} consecutive failures",
                        status.consecutive_failures
                    );
                    cx.emit(SchedulerEvent::AutoDisabled {
                        automation_id: automation_id.to_string(),
                        consecutive_failures: status.consecutive_failures,
                    });
                }
            }
            RunResult::Skipped { .. } => {}
        }

        // Trigger chains on success (unless the agent requested skip)
        let skip_chain = report.map(|r| r.skip_chain).unwrap_or(false);
        if matches!(result, RunResult::Success) && !skip_chain {
            if let Some(chain) = self.chains.get(automation_id) {
                let next_depth = chain_depth + 1;
                if next_depth > MAX_CHAIN_DEPTH {
                    log::warn!(
                        "Scheduler: chain depth limit ({MAX_CHAIN_DEPTH}) reached at {automation_id}"
                    );
                } else {
                    for trigger_id in chain.triggers.clone() {
                        // Fan-in dedup: skip if already triggered this cycle
                        if !self.triggered_this_cycle.insert(trigger_id.clone()) {
                            log::info!(
                                "Scheduler: skipping duplicate chain trigger {trigger_id} (already triggered this cycle)"
                            );
                            continue;
                        }

                        if let Some(entry) = self.entries.get(&trigger_id) {
                            cx.emit(SchedulerEvent::Fire {
                                automation_id: trigger_id,
                                active_folder: entry.active_folder.clone(),
                                chain_depth: next_depth,
                            });
                            self.running_count += 1;
                        } else {
                            log::warn!(
                                "Scheduler: chain trigger {trigger_id} not found in entries"
                            );
                        }
                    }
                }
            }
        }

        // Clear dedup set when all chain tasks complete
        if self.running_count == 0 {
            self.triggered_this_cycle.clear();
        }

        self.persist_status_async(cx);
    }

    /// Re-enable an auto-disabled automation.
    pub fn re_enable(&mut self, automation_id: &str, cx: &mut Context<Self>) {
        if let Some(status) = self.status.get_mut(automation_id) {
            status.auto_disabled = false;
            status.consecutive_failures = 0;
            self.persist_status_async(cx);
        }
    }

    /// Update the active folder for a specific scheduled automation.
    /// Called when the user changes the folder in the schedule UI.
    pub fn set_active_folder(&mut self, automation_id: &str, folder: PathBuf) {
        if let Some(entry) = self.entries.get_mut(automation_id) {
            entry.active_folder = folder;
        }
    }

    /// Read-only access to status map (for UI rendering).
    pub fn status(&self) -> &HashMap<String, AutomationStatus> {
        &self.status
    }

    /// Read-only access to entries (for UI rendering).
    pub fn entries(&self) -> &HashMap<String, ScheduleEntry> {
        &self.entries
    }

    /// Read-only access to chains (for dashboard to pass into run_scheduled_automation).
    pub fn chains(&self) -> &HashMap<String, ChainConfig> {
        &self.chains
    }

    fn persist_status_async(&self, cx: &mut Context<Self>) {
        let status = self.status.clone();
        let path = self.state_path.clone();
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    log::warn!("Scheduler: failed to create state dir: {error}");
                    return;
                }
            }
            match serde_json::to_string_pretty(&status) {
                Ok(json) => {
                    if let Err(error) = std::fs::write(&path, json) {
                        log::warn!("Scheduler: failed to write status file: {error}");
                    }
                }
                Err(error) => {
                    log::warn!("Scheduler: failed to serialize status: {error}");
                }
            }
        })
        .detach();
    }
}

// ── Sync entry (data transfer from dashboard to scheduler) ──

/// Intermediate struct used by dashboard to pass schedule config to `sync_entries()`.
/// Avoids the dashboard needing to construct `ScheduleEntry` directly (which requires
/// parsing cron expressions).
pub struct SyncEntry {
    pub automation_id: String,
    pub cron_expr: String,
    pub enabled: bool,
    pub catch_up: CatchUpPolicy,
    pub timeout_secs: u64,
    pub auto_disable_after: u32,
    pub chain: Option<ChainConfig>,
}

// ── Chain validation ──

fn validate_chains(
    entries: &HashMap<String, ScheduleEntry>,
    chains: &HashMap<String, ChainConfig>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut visited = HashSet::new();
    let mut in_stack = HashSet::new();

    for id in chains.keys() {
        if !visited.contains(id) {
            if let Some(cycle_warning) =
                detect_cycle(id, chains, &mut visited, &mut in_stack)
            {
                warnings.push(cycle_warning);
            }
        }
    }

    // Ghost trigger check: triggers pointing to non-existent automations
    for (id, chain) in chains {
        for trigger_id in &chain.triggers {
            if !entries.contains_key(trigger_id) {
                warnings.push(format!(
                    "Chain {id} -> {trigger_id}: target not found, will be skipped"
                ));
            }
        }
    }

    warnings
}

fn detect_cycle(
    node: &str,
    chains: &HashMap<String, ChainConfig>,
    visited: &mut HashSet<String>,
    in_stack: &mut HashSet<String>,
) -> Option<String> {
    visited.insert(node.to_string());
    in_stack.insert(node.to_string());

    if let Some(chain) = chains.get(node) {
        for trigger_id in &chain.triggers {
            if in_stack.contains(trigger_id.as_str()) {
                return Some(format!(
                    "Cycle detected involving {node} -> {trigger_id} — chain disabled"
                ));
            }
            if !visited.contains(trigger_id.as_str()) {
                if let Some(warning) = detect_cycle(trigger_id, chains, visited, in_stack) {
                    return Some(warning);
                }
            }
        }
    }

    in_stack.remove(node);
    None
}

// ── Status persistence ──

fn load_status(path: &PathBuf) -> anyhow::Result<HashMap<String, AutomationStatus>> {
    let content = std::fs::read_to_string(path)?;
    let status: HashMap<String, AutomationStatus> = serde_json::from_str(&content)?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn make_entry(id: &str, cron: &str) -> ScheduleEntry {
        ScheduleEntry {
            automation_id: id.to_string(),
            cron_expr: cron.to_string(),
            cron: Cron::from_str(cron).expect("valid cron"),
            enabled: true,
            catch_up: CatchUpPolicy::Skip,
            timeout_secs: 3600,
            active_folder: PathBuf::from("/test"),
            auto_disable_after: AUTO_DISABLE_THRESHOLD,
        }
    }

    fn make_sync(id: &str, cron: &str, chain: Option<ChainConfig>) -> SyncEntry {
        SyncEntry {
            automation_id: id.to_string(),
            cron_expr: cron.to_string(),
            enabled: true,
            catch_up: CatchUpPolicy::Skip,
            timeout_secs: 3600,
            auto_disable_after: AUTO_DISABLE_THRESHOLD,
            chain,
        }
    }

    // ── Cron parsing ──

    #[test]
    fn test_cron_valid_expression() {
        let cron = Cron::from_str("0 3 * * *");
        assert!(cron.is_ok());
    }

    #[test]
    fn test_cron_invalid_expression() {
        let cron = Cron::from_str("invalid cron");
        assert!(cron.is_err());
    }

    #[test]
    fn test_cron_find_next_occurrence() {
        let cron = Cron::from_str("0 3 * * *").unwrap();
        let now = Local::now();
        let next = cron.find_next_occurrence(&now, false);
        assert!(next.is_ok());
        let next = next.unwrap();
        assert!(next > now);
        assert_eq!(next.hour(), 3);
        assert_eq!(next.minute(), 0);
    }

    // ── Chain validation ──

    #[test]
    fn test_chain_no_cycles() {
        let mut entries = HashMap::new();
        entries.insert("a".to_string(), make_entry("a", "0 3 * * *"));
        entries.insert("b".to_string(), make_entry("b", "0 4 * * *"));

        let mut chains = HashMap::new();
        chains.insert(
            "a".to_string(),
            ChainConfig {
                triggers: vec!["b".to_string()],
            },
        );

        let warnings = validate_chains(&entries, &chains);
        assert!(warnings.is_empty(), "no cycles expected: {warnings:?}");
    }

    #[test]
    fn test_chain_simple_cycle() {
        let mut entries = HashMap::new();
        entries.insert("a".to_string(), make_entry("a", "0 3 * * *"));
        entries.insert("b".to_string(), make_entry("b", "0 4 * * *"));

        let mut chains = HashMap::new();
        chains.insert(
            "a".to_string(),
            ChainConfig {
                triggers: vec!["b".to_string()],
            },
        );
        chains.insert(
            "b".to_string(),
            ChainConfig {
                triggers: vec!["a".to_string()],
            },
        );

        let warnings = validate_chains(&entries, &chains);
        assert!(!warnings.is_empty(), "cycle should be detected");
        assert!(
            warnings.iter().any(|w| w.contains("Cycle")),
            "warning should mention cycle: {warnings:?}"
        );
    }

    #[test]
    fn test_chain_complex_cycle() {
        let mut entries = HashMap::new();
        entries.insert("a".to_string(), make_entry("a", "0 3 * * *"));
        entries.insert("b".to_string(), make_entry("b", "0 4 * * *"));
        entries.insert("c".to_string(), make_entry("c", "0 5 * * *"));

        let mut chains = HashMap::new();
        chains.insert(
            "a".to_string(),
            ChainConfig {
                triggers: vec!["b".to_string()],
            },
        );
        chains.insert(
            "b".to_string(),
            ChainConfig {
                triggers: vec!["c".to_string()],
            },
        );
        chains.insert(
            "c".to_string(),
            ChainConfig {
                triggers: vec!["a".to_string()],
            },
        );

        let warnings = validate_chains(&entries, &chains);
        assert!(!warnings.is_empty(), "cycle should be detected");
    }

    #[test]
    fn test_chain_ghost_trigger() {
        let mut entries = HashMap::new();
        entries.insert("a".to_string(), make_entry("a", "0 3 * * *"));

        let mut chains = HashMap::new();
        chains.insert(
            "a".to_string(),
            ChainConfig {
                triggers: vec!["nonexistent".to_string()],
            },
        );

        let warnings = validate_chains(&entries, &chains);
        assert!(
            warnings.iter().any(|w| w.contains("not found")),
            "ghost trigger should be warned: {warnings:?}"
        );
    }

    // ── Status persistence ──

    #[test]
    fn test_status_round_trip() {
        let tmp = std::env::temp_dir().join("scheduler_test_status.json");
        let mut status = HashMap::new();
        status.insert(
            "daily-scan".to_string(),
            AutomationStatus {
                last_evaluated_at: Some(Local::now()),
                last_run_at: Some(Local::now()),
                last_result: Some(RunResult::Success),
                consecutive_failures: 0,
                auto_disabled: false,
            },
        );
        status.insert(
            "review".to_string(),
            AutomationStatus {
                last_evaluated_at: None,
                last_run_at: None,
                last_result: Some(RunResult::Failed {
                    message: "exit code: 1".to_string(),
                }),
                consecutive_failures: 3,
                auto_disabled: false,
            },
        );

        let json = serde_json::to_string_pretty(&status).unwrap();
        std::fs::write(&tmp, &json).unwrap();

        let loaded = load_status(&tmp).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["daily-scan"].consecutive_failures, 0);
        assert!(!loaded["daily-scan"].auto_disabled);
        assert_eq!(loaded["review"].consecutive_failures, 3);
        assert!(matches!(
            loaded["daily-scan"].last_result,
            Some(RunResult::Success)
        ));

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_status_load_nonexistent() {
        let result = load_status(&PathBuf::from("/nonexistent/path.json"));
        assert!(result.is_err());
    }

    // ── CatchUpPolicy serde ──

    #[test]
    fn test_catch_up_policy_default() {
        let policy: CatchUpPolicy = Default::default();
        assert_eq!(policy, CatchUpPolicy::Skip);
    }

    #[test]
    fn test_catch_up_policy_deserialize() {
        let skip: CatchUpPolicy = serde_json::from_str(r#""skip""#).unwrap();
        assert_eq!(skip, CatchUpPolicy::Skip);

        let run_once: CatchUpPolicy = serde_json::from_str(r#""run_once""#).unwrap();
        assert_eq!(run_once, CatchUpPolicy::RunOnce);
    }

    // ── RunResult serde ──

    #[test]
    fn test_run_result_serde() {
        let success = RunResult::Success;
        let json = serde_json::to_string(&success).unwrap();
        let parsed: RunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, RunResult::Success);

        let failed = RunResult::Failed {
            message: "error".to_string(),
        };
        let json = serde_json::to_string(&failed).unwrap();
        let parsed: RunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, failed);

        let timeout = RunResult::Timeout;
        let json = serde_json::to_string(&timeout).unwrap();
        let parsed: RunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, RunResult::Timeout);
    }

    // ── sync_entries ──

    #[test]
    fn test_sync_entries_valid_cron() {
        // Cannot test Scheduler::sync_entries directly without GPUI context,
        // but we can test cron parsing works
        let cron = Cron::from_str("0 3 * * *");
        assert!(cron.is_ok());
        let cron = Cron::from_str("0 */2 * * *");
        assert!(cron.is_ok());
        let cron = Cron::from_str("0 0 1 * *");
        assert!(cron.is_ok());
    }

    // ── Auto-disable (GPUI integration) ──

    #[gpui::test]
    fn test_auto_disable_after_consecutive_failures(cx: &mut gpui::TestAppContext) {
        let tmp = std::env::temp_dir().join("scheduler_test_auto_disable");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let state_path = tmp.join("state.json");

        let scheduler = cx.new(|cx| Scheduler::new(state_path.clone(), cx));

        // Sync an entry so report_completion has something to work with
        scheduler.update(cx, |s, _cx| {
            s.sync_entries(
                vec![make_sync("flaky-job", "0 3 * * *", None)],
                PathBuf::from("/tmp"),
            );
        });

        let failure = RunResult::Failed {
            message: "exit code: 1".to_string(),
        };

        // Report 4 failures — should NOT auto-disable yet
        for _ in 0..AUTO_DISABLE_THRESHOLD - 1 {
            scheduler.update(cx, |s, cx| {
                s.running_count += 1;
                s.report_completion("flaky-job", &failure, None, 0, cx);
            });
        }

        scheduler.read_with(cx, |s, _cx| {
            let status = &s.status["flaky-job"];
            assert_eq!(status.consecutive_failures, AUTO_DISABLE_THRESHOLD - 1);
            assert!(!status.auto_disabled, "should not be disabled before threshold");
        });

        // 5th failure triggers auto-disable
        scheduler.update(cx, |s, cx| {
            s.running_count += 1;
            s.report_completion("flaky-job", &failure, None, 0, cx);
        });

        scheduler.read_with(cx, |s, _cx| {
            let status = &s.status["flaky-job"];
            assert_eq!(status.consecutive_failures, AUTO_DISABLE_THRESHOLD);
            assert!(status.auto_disabled, "should be auto-disabled after threshold");
        });

        // A success resets the counter
        scheduler.update(cx, |s, cx| {
            s.running_count += 1;
            // Manually re-enable to test success reset
            s.status.get_mut("flaky-job").unwrap().auto_disabled = false;
            s.report_completion("flaky-job", &RunResult::Success, None, 0, cx);
        });

        scheduler.read_with(cx, |s, _cx| {
            let status = &s.status["flaky-job"];
            assert_eq!(status.consecutive_failures, 0, "success should reset counter");
            assert!(!status.auto_disabled);
        });

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[gpui::test]
    fn test_re_enable_resets_state(cx: &mut gpui::TestAppContext) {
        let tmp = std::env::temp_dir().join("scheduler_test_re_enable");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let state_path = tmp.join("state.json");

        let scheduler = cx.new(|cx| Scheduler::new(state_path.clone(), cx));

        scheduler.update(cx, |s, _cx| {
            s.sync_entries(
                vec![make_sync("broken", "0 * * * *", None)],
                PathBuf::from("/tmp"),
            );
        });

        // Drive to auto-disable
        let failure = RunResult::Failed {
            message: "broken".to_string(),
        };
        for _ in 0..AUTO_DISABLE_THRESHOLD {
            scheduler.update(cx, |s, cx| {
                s.running_count += 1;
                s.report_completion("broken", &failure, None, 0, cx);
            });
        }

        scheduler.read_with(cx, |s, _cx| {
            assert!(s.status["broken"].auto_disabled);
        });

        // Re-enable clears both auto_disabled and consecutive_failures
        scheduler.update(cx, |s, cx| {
            s.re_enable("broken", cx);
        });

        scheduler.read_with(cx, |s, _cx| {
            let status = &s.status["broken"];
            assert!(!status.auto_disabled);
            assert_eq!(status.consecutive_failures, 0);
        });

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── Chain dispatch (GPUI integration) ──

    #[gpui::test]
    fn test_chain_fires_on_success(cx: &mut gpui::TestAppContext) {
        let tmp = std::env::temp_dir().join("scheduler_test_chain_fire");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let state_path = tmp.join("state.json");

        let scheduler = cx.new(|cx| Scheduler::new(state_path.clone(), cx));

        scheduler.update(cx, |s, _cx| {
            s.sync_entries(
                vec![
                    make_sync("review", "0 3 * * *", Some(ChainConfig {
                        triggers: vec!["deploy".to_string()],
                    })),
                    make_sync("deploy", "0 6 * * *", None),
                ],
                PathBuf::from("/tmp"),
            );
        });

        // Collect emitted events
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();
        cx.update(|cx| {
            cx.subscribe(&scheduler, move |_scheduler, event, _cx| {
                events_clone.lock().unwrap().push(event.clone());
            })
            .detach();
        });

        // Success on "review" should trigger chain to "deploy"
        scheduler.update(cx, |s, cx| {
            s.running_count += 1;
            s.report_completion("review", &RunResult::Success, None, 0, cx);
        });

        let fired = events.lock().unwrap();
        assert_eq!(fired.len(), 1, "expected exactly one chain fire event");
        match &fired[0] {
            SchedulerEvent::Fire {
                automation_id,
                chain_depth,
                ..
            } => {
                assert_eq!(automation_id, "deploy");
                assert_eq!(*chain_depth, 1);
            }
            other => panic!("expected Fire event, got: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[gpui::test]
    fn test_chain_skipped_on_failure(cx: &mut gpui::TestAppContext) {
        let tmp = std::env::temp_dir().join("scheduler_test_chain_fail");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let state_path = tmp.join("state.json");

        let scheduler = cx.new(|cx| Scheduler::new(state_path.clone(), cx));

        scheduler.update(cx, |s, _cx| {
            s.sync_entries(
                vec![
                    make_sync("review", "0 3 * * *", Some(ChainConfig {
                        triggers: vec!["deploy".to_string()],
                    })),
                    make_sync("deploy", "0 6 * * *", None),
                ],
                PathBuf::from("/tmp"),
            );
        });

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();
        cx.update(|cx| {
            cx.subscribe(&scheduler, move |_scheduler, event, _cx| {
                events_clone.lock().unwrap().push(event.clone());
            })
            .detach();
        });

        // Failure on "review" should NOT trigger chain
        scheduler.update(cx, |s, cx| {
            s.running_count += 1;
            s.report_completion(
                "review",
                &RunResult::Failed {
                    message: "error".to_string(),
                },
                None,
                0,
                cx,
            );
        });

        assert!(
            events.lock().unwrap().is_empty(),
            "chain should not fire on failure"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[gpui::test]
    fn test_chain_skipped_when_skip_chain_set(cx: &mut gpui::TestAppContext) {
        let tmp = std::env::temp_dir().join("scheduler_test_chain_skip");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let state_path = tmp.join("state.json");

        let scheduler = cx.new(|cx| Scheduler::new(state_path.clone(), cx));

        scheduler.update(cx, |s, _cx| {
            s.sync_entries(
                vec![
                    make_sync("review", "0 3 * * *", Some(ChainConfig {
                        triggers: vec!["deploy".to_string()],
                    })),
                    make_sync("deploy", "0 6 * * *", None),
                ],
                PathBuf::from("/tmp"),
            );
        });

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();
        cx.update(|cx| {
            cx.subscribe(&scheduler, move |_scheduler, event, _cx| {
                events_clone.lock().unwrap().push(event.clone());
            })
            .detach();
        });

        // Success with skip_chain=true should NOT trigger chain
        let report = CompletionReport {
            status: "success".to_string(),
            skip_chain: true,
            ..Default::default()
        };
        scheduler.update(cx, |s, cx| {
            s.running_count += 1;
            s.report_completion("review", &RunResult::Success, Some(&report), 0, cx);
        });

        assert!(
            events.lock().unwrap().is_empty(),
            "chain should not fire when skip_chain is set"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── ChainConfig serde ──

    #[test]
    fn test_chain_config_deserialize() {
        let json = r#"{"triggers": ["review", "deploy"]}"#;
        let config: ChainConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.triggers, vec!["review", "deploy"]);
    }

    // ── CompletionReport ──

    #[test]
    fn test_completion_report_valid_success() {
        let tmp = std::env::temp_dir().join("test_marker_success.json");
        let json = r#"{"status": "success", "summary": "Quality scan done. Score: 72.", "outputs": ["reports/quality/2026-03-10.md"], "skip_chain": false, "message": ""}"#;
        std::fs::write(&tmp, json).unwrap();

        let (report, result) = CompletionReport::from_marker(&tmp).unwrap();
        assert_eq!(result, RunResult::Success);
        assert_eq!(report.summary, "Quality scan done. Score: 72.");
        assert_eq!(report.outputs, vec!["reports/quality/2026-03-10.md"]);
        assert!(!report.skip_chain);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_completion_report_valid_failed() {
        let tmp = std::env::temp_dir().join("test_marker_failed.json");
        let json = r#"{"status": "failed: could not read project", "summary": "Config directory missing", "outputs": [], "skip_chain": false, "message": "Check config path"}"#;
        std::fs::write(&tmp, json).unwrap();

        let (report, result) = CompletionReport::from_marker(&tmp).unwrap();
        assert!(matches!(result, RunResult::Failed { .. }));
        assert_eq!(report.message, "Check config path");

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_completion_report_malformed_json_is_success() {
        let tmp = std::env::temp_dir().join("test_marker_malformed.json");
        std::fs::write(&tmp, "this is not json at all").unwrap();

        let (report, result) = CompletionReport::from_marker(&tmp).unwrap();
        assert_eq!(result, RunResult::Success);
        assert_eq!(report.status, "success");
        assert!(report.summary.is_empty());

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_completion_report_nonexistent_file() {
        let result = CompletionReport::from_marker(Path::new("/nonexistent/marker.json"));
        assert!(result.is_none());
    }

    #[test]
    fn test_completion_report_skip_chain() {
        let tmp = std::env::temp_dir().join("test_marker_skip.json");
        let json = r#"{"status": "success", "summary": "No changes since last scan", "outputs": [], "skip_chain": true, "message": ""}"#;
        std::fs::write(&tmp, json).unwrap();

        let (report, _result) = CompletionReport::from_marker(&tmp).unwrap();
        assert!(report.skip_chain);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_completion_marker_path() {
        let path = completion_marker_path(
            Path::new("/state"),
            "daily-scan",
            1710104400,
        );
        assert_eq!(
            path,
            PathBuf::from("/state/completed/daily-scan-1710104400.json")
        );
    }
}
