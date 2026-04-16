//! Dashboard runtime: prompt assembly, backend routing, automation and
//! pipeline execution, completion polling.
//!
//! Per `private/specs/dashboard-functional-extraction.md` (Decision 5), this
//! stays an internal module rather than a crate because:
//!
//! - it depends on `Workspace`, `AgentPanel`, `SpawnInTerminal`, and live
//!   panel state;
//! - the backend contract (terminal vs. native vs. copy-only) is still being
//!   clarified, notably around the Step 12b profile-application race
//!   described in `private/specs/dashboard-bakeoff-branch3.md`.
//!
//! This module concentrates:
//!
//! - `run_automation` / `run_pipeline` — manual and pipeline runs
//! - `spawn_automation_in_terminal` / `spawn_completion_poller` — scheduled
//!   runs that poll a JSON marker file instead of awaiting a terminal
//! - `gather_context_blocking` — off-thread context assembly
//! - `build_temp_file_terminal_command` — prompt-file-based terminal spawn
//! - `apply_agent_profile_to_thread` — native-backend profile application
//!   (see the note attached to that function regarding Step 12b)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_settings::AgentProfileId;
use agent_ui::AgentPanel;
use futures::channel::oneshot;
use gpui::{App, AppContext, AsyncApp, ClipboardItem, Context, Window};
use postprod_dashboard_config as dcfg;
use postprod_scheduler::{CompletionReport, RunResult, completion_marker_path};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use util::ResultExt as _;
use workspace::{Toast, notifications::NotificationId};

use crate::paths::resolve_bin;
use crate::{
    AgentBackend, AutomationEntry, AutomationRunStatus, ContextLauncherToast, Dashboard,
    MAX_PIPELINE_DEPTH, collect_step_groups, resolve_tool_command,
};

impl Dashboard {
    pub(crate) fn run_automation(
        &mut self,
        entry_id: &str,
        entry_label: &str,
        prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.automation_status.get(entry_id),
            Some(AutomationRunStatus::GatheringContext)
        ) {
            return;
        }

        let fallback_prompt = self.resolve_variables(prompt, entry_id);

        // Check if this automation has chain config (for terminal backends only)
        let has_chain = self
            .automations
            .iter()
            .find(|a| a.id == entry_id)
            .and_then(|a| a.chain.as_ref())
            .is_some_and(|c| !c.triggers.is_empty());

        // Prepare chain tracking for terminal backends
        let chain_marker = if has_chain {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let state_dir = dcfg::state_dir_for(&self.config_root);
            let marker_path = completion_marker_path(&state_dir, entry_id, timestamp);
            if let Some(parent) = marker_path.parent() {
                std::fs::create_dir_all(parent).log_err();
            }
            Some(marker_path)
        } else {
            None
        };

        // Gather notes for standalone run (LMDB reads are microseconds)
        let notes_section = self.gather_notes_for_automation(entry_id, cx);

        // Capture values needed by the async block
        let workspace = self.workspace.clone();
        let agent_backend = self.agent_backend;
        let backends = self.backends.clone();
        let agent_cwd = self.agent_cwd();
        let entry_id = entry_id.to_string();
        let entry_label = entry_label.to_string();

        // Collect context entries (default + per-automation) and per-automation profile
        let (contexts, automation_profile) = {
            let entry = self.automations.iter().find(|a| a.id == entry_id);
            let mut all_contexts = if entry.is_some_and(|e| !e.skip_default_context) {
                self.default_contexts.clone()
            } else {
                Vec::new()
            };
            let profile = entry.and_then(|e| e.profile.clone());
            if let Some(e) = entry {
                all_contexts.extend(e.contexts.clone());
            }
            (all_contexts, profile)
        };
        let config_root = self.config_root.clone();
        let session_path_for_env = self.session_path.clone().unwrap_or_default();
        let active_folder_for_env = self
            .active_folder
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let destination_for_env = self
            .destination_folder
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let param_values_for_env = self
            .param_values
            .get(&entry_id)
            .cloned()
            .unwrap_or_default();

        // If chain is configured but backend can't support it, warn
        if has_chain && matches!(agent_backend, AgentBackend::Native | AgentBackend::CopyOnly) {
            log::warn!(
                "dashboard: automation '{}' has chain config but uses non-terminal backend — chains require a terminal backend",
                entry_id
            );
        }

        // Spawn the completion poller before the async block (needs &self + Context)
        if let Some(marker_path) = &chain_marker {
            if !matches!(agent_backend, AgentBackend::Native | AgentBackend::CopyOnly) {
                let timeout_secs = self
                    .automations
                    .iter()
                    .find(|a| a.id == entry_id)
                    .and_then(|a| a.schedule.as_ref())
                    .map(|s| s.timeout)
                    .unwrap_or(3600);
                self.spawn_completion_poller(
                    entry_id.clone(),
                    marker_path.clone(),
                    timeout_secs,
                    0,
                    cx,
                );
            }
        }

        self.automation_status
            .insert(entry_id.to_string(), AutomationRunStatus::GatheringContext);
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            // Phase 1: Gather context on background thread.
            let context_result: Result<String, String> = if contexts.is_empty() {
                Ok(String::new())
            } else {
                let contexts = contexts.clone();
                let config_root = config_root.clone();
                let session = session_path_for_env.clone();
                let folder = active_folder_for_env.clone();
                let dest = destination_for_env.clone();
                let params = param_values_for_env.clone();
                cx.background_executor().spawn(async move {
                    gather_context_blocking(
                        &contexts, &config_root, &session, &folder, &dest, &params,
                    )
                }).await
            };

            let enriched_prompt = match context_result {
                Ok(ctx) if ctx.is_empty() && notes_section.is_empty() => fallback_prompt.clone(),
                Ok(ctx) if ctx.is_empty() => format!("{notes_section}\n\n{fallback_prompt}"),
                Ok(ctx) if notes_section.is_empty() => format!("{ctx}\n=== End of pre-loaded context ===\n\n{fallback_prompt}"),
                Ok(ctx) => format!("{ctx}\n=== End of pre-loaded context ===\n\n{notes_section}\n\n{fallback_prompt}"),
                Err(reason) => {
                    log::warn!("context gathering failed for '{}': {reason}", entry_id);
                    let ws_for_toast = workspace.clone();
                    let fallback = fallback_prompt.clone();
                    let backends_for_toast = backends.clone();
                    let cwd_for_toast = agent_cwd.clone();
                    let id_for_toast = entry_id.clone();
                    let label_for_toast = entry_label.clone();
                    let profile_for_toast = automation_profile.clone();

                    workspace.update_in(cx, |workspace, _window, cx| {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<ContextLauncherToast>(),
                                format!("Context gathering failed for '{}': {}", entry_label, reason),
                            )
                            .on_click(
                                "Run without context",
                                move |window, cx| {
                                    if agent_backend == AgentBackend::CopyOnly {
                                        cx.write_to_clipboard(ClipboardItem::new_string(fallback.clone()));
                                        return;
                                    }
                                    if agent_backend == AgentBackend::Native {
                                        let prompt = fallback.clone();
                                        let profile = profile_for_toast.clone();
                                        ws_for_toast.update(cx, |workspace, cx| {
                                            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                                panel.update(cx, |panel, cx| {
                                                    panel.new_external_thread_with_auto_submit(prompt, window, cx);
                                                    apply_agent_profile_to_thread(panel, &profile, cx);
                                                });
                                                workspace.focus_panel::<AgentPanel>(window, cx);
                                            }
                                        }).log_err();
                                        return;
                                    }
                                    // Terminal fallback
                                    if let Some(spawn) = build_temp_file_terminal_command(
                                        &fallback, &id_for_toast, &label_for_toast,
                                        agent_backend, &backends_for_toast, &cwd_for_toast, &None,
                                    ) {
                                        ws_for_toast.update(cx, |workspace, cx| {
                                            workspace.spawn_in_terminal(spawn, window, cx).detach();
                                        }).log_err();
                                    }
                                },
                            ),
                            cx,
                        );
                    }).log_err();

                    let failed_id = entry_id.clone();
                    this.update(cx, |dashboard, cx| {
                        dashboard
                            .automation_status
                            .insert(failed_id, AutomationRunStatus::Failed(reason));
                        cx.notify();
                    }).log_err();
                    return;
                }
            };

            let done_id = entry_id.clone();
            this.update(cx, |dashboard, cx| {
                dashboard.automation_status.remove(&done_id);
                cx.notify();
            }).log_err();

            // Phase 2: Route to backend.
            if agent_backend == AgentBackend::Native {
                let prompt = enriched_prompt;
                workspace.update_in(cx, |workspace, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.new_external_thread_with_auto_submit(prompt, window, cx);
                            apply_agent_profile_to_thread(panel, &automation_profile, cx);
                        });
                        workspace.focus_panel::<AgentPanel>(window, cx);
                    }
                }).log_err();
                return;
            }

            if agent_backend == AgentBackend::CopyOnly {
                cx.update(|_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(enriched_prompt));
                }).log_err();
                return;
            }

            // Terminal backend: write temp file and spawn
            if let Some(spawn) = build_temp_file_terminal_command(
                &enriched_prompt, &entry_id, &entry_label,
                agent_backend, &backends, &agent_cwd, &chain_marker,
            ) {
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.spawn_in_terminal(spawn, window, cx).detach();
                }).log_err();
            }
        })
        .detach();
    }

    /// Resolves prompt, appends completion instruction, spawns terminal.
    /// Returns (marker_path, terminal_task) for the caller to poll/race.
    /// Both `run_scheduled_automation()` and `run_pipeline()` use this —
    /// they differ only in how they handle completion.
    pub(crate) fn spawn_automation_in_terminal(
        &self,
        automation_id: &str,
        active_folder: &Path,
        reveal: RevealStrategy,
        label_prefix: &str,
        cx: &mut Context<Self>,
    ) -> Option<(PathBuf, gpui::Task<()>)> {
        let entry = match self.automations.iter().find(|a| a.id == automation_id) {
            Some(e) => e.clone(),
            None => {
                log::warn!("spawn_automation_in_terminal: automation {automation_id} not found");
                return None;
            }
        };

        let backend_config = match self.backends.iter().find(|b| !b.command.is_empty()) {
            Some(b) => b.clone(),
            None => {
                log::warn!("spawn_automation_in_terminal: no CLI backend configured");
                return None;
            }
        };

        // Build completion marker path
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let state_dir = dcfg::state_dir_for(&self.config_root);
        let marker_path = completion_marker_path(&state_dir, automation_id, timestamp);
        if let Some(parent) = marker_path.parent() {
            std::fs::create_dir_all(parent).log_err();
        }

        let resolved_prompt = self.resolve_variables(&entry.prompt, &entry.id);

        // Collect context entries (default + per-automation)
        let all_contexts = {
            let mut contexts = if !entry.skip_default_context {
                self.default_contexts.clone()
            } else {
                Vec::new()
            };
            contexts.extend(entry.contexts.clone());
            contexts
        };
        let config_root = self.config_root.clone();
        let session_for_env = self.session_path.clone().unwrap_or_default();
        let folder_for_env = active_folder.to_string_lossy().to_string();
        let dest_for_env = self
            .destination_folder
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let params_for_env = self
            .param_values
            .get(&entry.id)
            .cloned()
            .unwrap_or_default();

        let command = resolve_bin(&backend_config.command);
        let flags = backend_config.flags;
        let agent_cwd = self.agent_cwd();
        let entry_id = entry.id;
        let entry_label = entry.label;

        let display_label = if label_prefix.is_empty() {
            entry_label
        } else {
            format!("[{label_prefix}] {}", entry_label)
        };

        let completion_instruction = Self::completion_report_instruction(&marker_path);

        let workspace = self.workspace.clone();
        let window_handle = self.window_handle;

        let (tx, rx) = oneshot::channel();

        cx.spawn(async move |_this, cx: &mut AsyncApp| -> anyhow::Result<()> {
            // Gather context on background thread
            let context_text = if all_contexts.is_empty() {
                String::new()
            } else {
                match cx.background_executor().spawn(async move {
                    gather_context_blocking(
                        &all_contexts, &config_root, &session_for_env,
                        &folder_for_env, &dest_for_env, &params_for_env,
                    )
                }).await {
                    Ok(text) => text,
                    Err(reason) => {
                        log::warn!("context gathering failed for pipeline step '{entry_id}': {reason}");
                        String::new()
                    }
                }
            };

            // Build enriched prompt with context + completion instruction
            let enriched_prompt = if context_text.is_empty() {
                format!("{resolved_prompt}{completion_instruction}")
            } else {
                format!("{context_text}\n=== End of pre-loaded context ===\n\n{resolved_prompt}{completion_instruction}")
            };

            // Write prompt to temp file
            let temp_path = std::env::temp_dir().join(format!("postprod_prompt_{entry_id}.md"));
            if let Err(e) = std::fs::write(&temp_path, &enriched_prompt) {
                log::error!("failed to write temp prompt file: {e}");
                return Ok(());
            }

            let temp_escaped = temp_path.display().to_string().replace('\'', "'\\''");
            let full_command = format!("cat '{temp_escaped}' | {command} {flags}");

            let spawn = SpawnInTerminal {
                id: TaskId(format!("automation-{entry_id}")),
                label: display_label.clone(),
                full_label: display_label.clone(),
                command: Some(full_command),
                args: vec![],
                command_label: display_label,
                cwd: Some(agent_cwd),
                use_new_terminal: true,
                allow_concurrent_runs: false,
                reveal,
                show_command: false,
                show_rerun: true,
                ..Default::default()
            };

            let Some(window_handle) = window_handle else {
                log::warn!("spawn_automation_in_terminal: window handle not yet available");
                return Ok(());
            };
            let Some(workspace) = workspace.upgrade() else {
                log::warn!("spawn_automation_in_terminal: workspace released");
                return Ok(());
            };
            if let Ok(terminal_task) = window_handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.spawn_in_terminal(spawn, window, cx)
                })
            }) {
                tx.send(terminal_task).ok();
            }
            Ok(())
        })
        .detach();

        let terminal_task = cx.background_spawn(async move {
            if let Ok(inner_task) = rx.await {
                let _ = inner_task.await;
            }
        });

        Some((marker_path, terminal_task))
    }

    pub(crate) fn run_pipeline(
        &mut self,
        pipeline_entry: &AutomationEntry,
        active_folder: &Path,
        depth: u32,
        reveal: RevealStrategy,
        cx: &mut Context<Self>,
    ) {
        let pipeline_id = pipeline_entry.id.clone();

        if self.active_pipelines.contains(&pipeline_id) {
            log::warn!("Pipeline '{pipeline_id}' is already running — skipping");
            return;
        }
        if depth >= MAX_PIPELINE_DEPTH {
            log::warn!("Pipeline depth limit ({MAX_PIPELINE_DEPTH}) reached at '{pipeline_id}'");
            return;
        }
        if pipeline_entry.steps.is_empty() {
            log::warn!("Pipeline '{pipeline_id}' has no steps — nothing to run");
            return;
        }

        self.active_pipelines.insert(pipeline_id.clone());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.pipeline_cancel_flags
            .insert(pipeline_id.clone(), cancel_flag.clone());
        cx.notify();

        let steps = pipeline_entry.steps.clone();
        let active_folder = active_folder.to_path_buf();
        let config_root = self.config_root.clone();
        let automations = self.automations.clone();
        let tools = self.tools.clone();
        let runtime_path = self.runtime_path.clone();
        let agent_tools_path = self.agent_tools_path.clone();
        let session_path = self.session_path.clone();
        let param_values = self.param_values.clone();

        let label_prefix = if reveal == RevealStrategy::Never {
            "Scheduled"
        } else {
            "Pipeline"
        };

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let groups = collect_step_groups(&steps);
            let mut pipeline_failed = false;
            let mut failure_message = String::new();

            'outer: for group in &groups {
                if pipeline_failed {
                    break;
                }
                if cancel_flag.load(Ordering::Relaxed) {
                    pipeline_failed = true;
                    failure_message = "Pipeline cancelled by user".to_string();
                    break;
                }

                let mut step_futures = Vec::new();
                let mut term_watchers: Vec<gpui::Task<()>> = Vec::new();

                for step in group {
                    let Some(target_id) = step.target_id() else {
                        continue;
                    };

                    if step.is_tool() {
                        // Tool step: resolve command and spawn
                        let tool = tools.iter().find(|t| t.id == target_id).cloned();
                        let Some(tool) = tool else {
                            log::warn!("Pipeline '{pipeline_id}': tool '{target_id}' not found — skipping step");
                            continue;
                        };
                        let tool_params = param_values.get(&tool.id).cloned().unwrap_or_default();
                        let runtime_path = runtime_path.clone();
                        let agent_tools_path = agent_tools_path.clone();
                        let config_root = config_root.clone();
                        let session_path = session_path.clone();
                        let active_folder = active_folder.clone();
                        let future = cx.background_executor().spawn(async move {
                            let (command, args, cwd, env) = resolve_tool_command(
                                &tool,
                                &runtime_path,
                                &agent_tools_path,
                                &config_root,
                                &session_path,
                                &Some(active_folder),
                                &tool_params,
                            );
                            let mut cmd = smol::process::Command::new(&command);
                            cmd.args(&args).current_dir(&cwd);
                            for (key, value) in &env {
                                cmd.env(key, value);
                            }
                            match cmd.status().await {
                                Ok(status) => status.success(),
                                Err(e) => {
                                    log::error!("Pipeline tool '{}' failed to start: {e}", tool.id);
                                    false
                                }
                            }
                        });
                        step_futures.push(future);
                    } else {
                        // Automation step: check if target is itself a pipeline
                        let target_entry = automations.iter().find(|a| a.id == target_id).cloned();
                        let Some(target_entry) = target_entry else {
                            log::warn!("Pipeline '{pipeline_id}': automation '{target_id}' not found — skipping step");
                            continue;
                        };

                        if target_entry.is_pipeline() {
                            // Nested pipeline: spawn recursively via this.update
                            let target = target_entry.clone();
                            let active_folder = active_folder.clone();
                            let this = this.clone();
                            let new_depth = depth + 1;

                            // For nested pipelines, spawn a marker-based poll
                            let state_dir = dcfg::state_dir_for(&config_root);
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            let nested_marker = completion_marker_path(&state_dir, target_id, timestamp);

                            // Fire the nested pipeline
                            this.update(cx, |dashboard, cx| {
                                dashboard.run_pipeline(
                                    &target,
                                    &active_folder,
                                    new_depth,
                                    reveal,
                                    cx,
                                );
                            }).log_err();

                            // Poll for the nested pipeline's completion marker
                            let marker = nested_marker;
                            let executor = cx.background_executor().clone();
                            let future = executor.spawn(async move {
                                // Nested pipelines write their own marker when done.
                                // However, since the nested pipeline manages its own marker
                                // lifecycle, we poll for the active_pipelines set instead.
                                // For now, we consider nested pipeline steps as fire-and-forget
                                // and always succeed (the nested pipeline tracks its own status).
                                // TODO: poll nested pipeline completion properly
                                let _ = marker;
                                true
                            });
                            step_futures.push(future);
                        } else {
                            // Regular automation: spawn in terminal and poll marker
                            let target_id = target_id.to_string();
                            let active_folder = active_folder.clone();
                            let label_prefix = label_prefix.to_string();
                            let this = this.clone();

                            let spawn_result = this.update(cx, |dashboard, cx| {
                                dashboard.spawn_automation_in_terminal(
                                    &target_id,
                                    &active_folder,
                                    reveal,
                                    &label_prefix,
                                    cx,
                                )
                            }).ok().flatten();

                            let Some((marker_path, terminal_task)) = spawn_result else {
                                log::warn!("Pipeline '{pipeline_id}': failed to spawn automation '{target_id}'");
                                continue;
                            };

                            let (term_tx, mut term_rx) = oneshot::channel::<()>();
                            let term_watcher = cx.background_executor().spawn(async move {
                                let _ = terminal_task.await;
                                term_tx.send(()).ok();
                            });
                            term_watchers.push(term_watcher);

                            let cancel = cancel_flag.clone();
                            let spawner = cx.background_executor().clone();
                            let timer_executor = spawner.clone();
                            let future = spawner.spawn({
                                let marker_path = marker_path.clone();
                                async move {
                                    let poll_interval = Duration::from_secs(10);
                                    let timeout = Duration::from_secs(3600);
                                    let start = std::time::Instant::now();
                                    loop {
                                        timer_executor.timer(poll_interval).await;

                                        if cancel.load(Ordering::Relaxed) {
                                            log::info!("Pipeline step '{target_id}' cancelled by user");
                                            return false;
                                        }

                                        if marker_path.exists() {
                                            let result = CompletionReport::from_marker(&marker_path);
                                            std::fs::remove_file(&marker_path).log_err();
                                            return match result {
                                                Some((_report, RunResult::Success)) => true,
                                                Some((report, _)) => {
                                                    log::warn!("Pipeline step '{target_id}' failed: {}", report.summary);
                                                    false
                                                }
                                                None => true,
                                            };
                                        }

                                        if matches!(term_rx.try_recv(), Ok(Some(()))) {
                                            log::warn!("Pipeline step '{target_id}': terminal exited without completion marker");
                                            return false;
                                        }

                                        if start.elapsed() > timeout {
                                            log::warn!("Pipeline step '{target_id}' timed out");
                                            return false;
                                        }
                                    }
                                }
                            });
                            step_futures.push(future);
                        }
                    }
                }

                // Wait for all steps in this group to complete
                let results = futures::future::join_all(step_futures).await;
                drop(term_watchers);
                for (i, success) in results.iter().enumerate() {
                    if !success {
                        pipeline_failed = true;
                        if let Some(step) = group.get(i) {
                            failure_message = format!(
                                "Step '{}' failed",
                                step.target_id().unwrap_or("unknown"),
                            );
                        }
                        break 'outer;
                    }
                }
            }

            // Write pipeline's own completion marker
            let state_dir = dcfg::state_dir_for(&config_root);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let pipeline_marker = completion_marker_path(&state_dir, &pipeline_id, timestamp);
            if let Some(parent) = pipeline_marker.parent() {
                std::fs::create_dir_all(parent).log_err();
            }
            let status = if pipeline_failed { format!("failed: {failure_message}") } else { "success".to_string() };
            let report = serde_json::json!({
                "status": status,
                "summary": if pipeline_failed { &failure_message } else { "All steps completed" },
                "outputs": [],
                "skip_chain": false,
                "message": ""
            });
            std::fs::write(&pipeline_marker, report.to_string()).log_err();

            // Remove from active set and clean up cancel flag
            this.update(cx, |dashboard, cx| {
                dashboard.active_pipelines.remove(&pipeline_id);
                dashboard.pipeline_cancel_flags.remove(&pipeline_id);
                cx.notify();
            }).log_err();
        })
        .detach();
    }

    pub(crate) fn spawn_completion_poller(
        &self,
        automation_id: String,
        marker_path: PathBuf,
        timeout_secs: u64,
        chain_depth: u32,
        cx: &mut Context<Self>,
    ) {
        let scheduler = self.scheduler.downgrade();

        cx.spawn(
            async move |_this, cx: &mut AsyncApp| -> anyhow::Result<()> {
                // Poll for the completion marker file instead of awaiting the terminal process.
                // The agent writes this JSON file as its final action.
                let poll_interval = Duration::from_secs(10);
                let marker = marker_path.clone();
                let executor = cx.background_executor().clone();
                let completion = async {
                    loop {
                        executor.timer(poll_interval).await;
                        if marker.exists() {
                            return CompletionReport::from_marker(&marker);
                        }
                    }
                };
                let timeout = async {
                    executor.timer(Duration::from_secs(timeout_secs)).await;
                    None::<(CompletionReport, RunResult)>
                };

                let outcome = smol::future::or(completion, timeout).await;

                let (report, result) = match outcome {
                    Some((report, result)) => {
                        // Clean up the marker file
                        std::fs::remove_file(&marker_path).log_err();
                        (Some(report), result)
                    }
                    None => (None, RunResult::Timeout),
                };

                // Report result back to scheduler
                scheduler
                    .update(cx, |scheduler, cx| {
                        scheduler.report_completion(
                            &automation_id,
                            &result,
                            report.as_ref(),
                            chain_depth,
                            cx,
                        );
                    })
                    .log_err();

                Ok(())
            },
        )
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Free functions — native-backend profile application, context assembly,
// and temp-file terminal command construction.
// ---------------------------------------------------------------------------

/// Apply a per-automation profile to the active native agent thread.
///
/// **KNOWN BUG — Step 12b (see `private/specs/dashboard-bakeoff-branch3.md`).**
/// The profile is applied synchronously after `new_external_thread_with_auto_submit`
/// returns, but thread creation resolves asynchronously: `ConversationView::new`
/// returns `ServerState::Loading` and only flips to `Connected` once an async
/// `load_task` completes. At the moment this function runs, `as_native_thread`
/// returns `None`, so the `set_profile` closure never executes. This silent
/// failure is the behavior the owner verified on 2026-04-07.
///
/// This module preserves that behavior verbatim. The move to `runtime.rs`
/// must not be read as a resolution of Step 12b. The real fix (pushing the
/// profile into `AgentInitialContent` before `ThreadView::send`) is tracked
/// separately and must land at its own owner-approved change set.
pub(crate) fn apply_agent_profile_to_thread(
    panel: &AgentPanel,
    profile: &Option<String>,
    cx: &mut App,
) {
    if let Some(profile_name) = profile {
        let profile_id = AgentProfileId(profile_name.as_str().into());
        if let Some(cv) = panel.active_conversation_view() {
            if let Some(thread) = cv.read(cx).as_native_thread(cx) {
                thread.update(cx, |thread, cx| {
                    thread.set_profile(profile_id, cx);
                });
            }
        }
    }
}

/// Gather all context entries, returning assembled text or an error if a required entry fails.
pub(crate) fn gather_context_blocking(
    contexts: &[dcfg::ContextEntry],
    config_root: &Path,
    session_path: &str,
    active_folder: &str,
    destination_folder: &str,
    param_values: &HashMap<String, String>,
) -> Result<String, String> {
    use std::process::Command;

    let mut output = String::new();
    let mut total_size: usize = 0;
    let context_cap = 150 * 1024;

    for entry in contexts {
        if total_size >= context_cap {
            output.push_str("\n[... context truncated at 150KB]\n");
            break;
        }

        let content = match entry.source_type.as_str() {
            "path" => {
                let Some(path_str) = &entry.path else {
                    if entry.required {
                        return Err(format!(
                            "context '{}': source=path but no path field",
                            entry.label
                        ));
                    }
                    continue;
                };
                let expanded = dcfg::expand_tilde(path_str);
                let path = Path::new(&expanded);
                match dcfg::read_path_context(path) {
                    Ok(text) => text,
                    Err(e) => {
                        if entry.required {
                            return Err(format!("context '{}': {e}", entry.label));
                        }
                        log::warn!("context '{}': {e} (skipping, not required)", entry.label);
                        continue;
                    }
                }
            }
            "script" => {
                let Some(script_name) = &entry.script else {
                    if entry.required {
                        return Err(format!(
                            "context '{}': source=script but no script field",
                            entry.label
                        ));
                    }
                    continue;
                };
                let scripts_dir = config_root.join("config/context-scripts");
                let script_path = match dcfg::resolve_file_path(&scripts_dir, script_name) {
                    Ok(p) => p,
                    Err(e) => {
                        if entry.required {
                            return Err(format!("context '{}': {e}", entry.label));
                        }
                        log::warn!("context '{}': {e} (skipping, not required)", entry.label);
                        continue;
                    }
                };

                // This runs on a background thread via gather_context_blocking(),
                // so blocking .output() is intentional and correct here.
                #[allow(clippy::disallowed_methods)]
                let result =
                    Command::new("sh")
                        .arg("-c")
                        .arg(script_path.to_string_lossy().as_ref())
                        .env("POSTPROD_ACTIVE_FOLDER", active_folder)
                        .env("POSTPROD_SESSION_PATH", session_path)
                        .env("POSTPROD_DESTINATION_FOLDER", destination_folder)
                        .envs(param_values.iter().map(|(k, v)| {
                            (format!("POSTPROD_PARAM_{}", k.to_uppercase()), v.as_str())
                        }))
                        .output();

                match result {
                    Ok(out) if out.status.success() => {
                        String::from_utf8_lossy(&out.stdout).to_string()
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let msg = format!(
                            "script exited {}: {}",
                            out.status,
                            stderr.lines().next().unwrap_or("")
                        );
                        if entry.required {
                            return Err(format!("context '{}': {msg}", entry.label));
                        }
                        log::warn!("context '{}': {msg} (skipping, not required)", entry.label);
                        continue;
                    }
                    Err(e) => {
                        if entry.required {
                            return Err(format!(
                                "context '{}': failed to run script: {e}",
                                entry.label
                            ));
                        }
                        log::warn!("context '{}': {e} (skipping, not required)", entry.label);
                        continue;
                    }
                }
            }
            other => {
                log::warn!(
                    "context '{}': unknown source type '{other}', skipping",
                    entry.label
                );
                continue;
            }
        };

        let section = format!("=== Context: {} ===\n{}\n\n", entry.label, content);
        total_size += section.len();
        output.push_str(&section);
    }

    Ok(output)
}

/// Build a SpawnInTerminal that reads the prompt from a temp file via stdin piping.
/// The prompt (with any gathered context prepended) is written to /tmp/postprod_prompt_{id}.md,
/// then executed as: `cat <temp_file> | <command> <flags>`.
/// Returns None if the backend config is missing or the temp file cannot be written.
pub(crate) fn build_temp_file_terminal_command(
    prompt: &str,
    entry_id: &str,
    entry_label: &str,
    agent_backend: AgentBackend,
    backends: &[dcfg::BackendEntry],
    agent_cwd: &PathBuf,
    chain_marker: &Option<PathBuf>,
) -> Option<SpawnInTerminal> {
    // Append completion instruction if chained
    let final_prompt = if let Some(marker_path) = chain_marker {
        let mut p = prompt.to_string();
        p.push_str(&Dashboard::completion_report_instruction(marker_path));
        p
    } else {
        prompt.to_string()
    };

    // Write to temp file
    let temp_path = std::env::temp_dir().join(format!("postprod_prompt_{entry_id}.md"));
    if let Err(e) = std::fs::write(&temp_path, &final_prompt) {
        log::error!("failed to write temp prompt file: {e}");
        return None;
    }

    let backend_id = agent_backend.backend_id();
    let config = backends.iter().find(|b| b.id == backend_id)?;

    let command = resolve_bin(&config.command);
    let flags = &config.flags;
    let temp_escaped = temp_path.display().to_string().replace('\'', "'\\''");
    let full_command = format!("cat '{temp_escaped}' | {command} {flags}");

    Some(SpawnInTerminal {
        id: TaskId(format!("automation-{}", entry_id)),
        label: entry_label.to_string(),
        full_label: entry_label.to_string(),
        command: Some(full_command),
        args: vec![],
        command_label: entry_label.to_string(),
        cwd: Some(agent_cwd.clone()),
        use_new_terminal: true,
        allow_concurrent_runs: false,
        reveal: RevealStrategy::Always,
        show_command: true,
        show_rerun: true,
        ..Default::default()
    })
}
