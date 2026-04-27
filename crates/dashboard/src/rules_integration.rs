//! PostProd Rules window integration.
//!
//! The dashboard owns the bridge between its config/automation state and the
//! `postprod_rules` window crate. This module concentrates that bridge:
//!
//! - opening the window in general or scoped mode
//! - building `ResolvedAutomationInfo` from current dashboard automations
//!   (via `postprod_dashboard_config::resolve_automation_info`)
//! - wiring context-mutation callbacks back into dashboard methods
//! - closing the window and reinitializing `NoteStore` on config-root switch
//! - refreshing the scoped window when automations reload
//!
//! Per `private/specs/dashboard-functional-extraction.md` (Decision 4), this
//! stays as an internal module rather than a separate crate because all the
//! logic is tightly bound to `DashboardItem` state, `Window`, `Context<Self>`,
//! and `AsyncApp`.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, AsyncApp, Context, Window};
use postprod_dashboard_config as dcfg;
use postprod_rules::note_store::NoteStore;
use util::ResultExt as _;

use crate::{DashboardItem, PostProdInlineAssist};

impl DashboardItem {
    /// Open the PostProd Rules window in general mode (header button / keybinding).
    pub(crate) fn open_postprod_rules(
        &mut self,
        selection: Option<postprod_rules::SelectionTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = postprod_rules::WindowMode::General(selection);
        self.open_postprod_rules_with_mode(mode, window, cx);
    }

    /// Open the PostProd Rules window in scoped mode for a specific automation.
    pub(crate) fn open_postprod_rules_scoped(
        &mut self,
        automation_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(info) = self.build_automation_info(automation_id) else {
            log::warn!("Automation '{}' not found for scoped edit", automation_id);
            return;
        };
        let mode = postprod_rules::WindowMode::Scoped(info);
        self.open_postprod_rules_with_mode(mode, window, cx);
    }

    fn open_postprod_rules_with_mode(
        &mut self,
        mode: postprod_rules::WindowMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.note_store.clone() else {
            log::warn!("NoteStore not ready yet");
            return;
        };

        let workspace = self.workspace.clone();
        let config_root = self.config_root.clone();
        let automations = self.build_all_automation_infos();
        let context_callbacks = self.build_context_callbacks(cx);

        let task = cx.spawn_in(window, async move |this, cx| {
            let language_registry = cx.update(|_window, cx| {
                workspace
                    .upgrade()
                    .map(|ws| ws.read(cx).app_state().languages.clone())
            })?;
            let Some(language_registry) = language_registry else {
                return anyhow::Ok(());
            };

            let inline_delegate = Box::new(PostProdInlineAssist {
                workspace: workspace.clone(),
            });

            let window_handle = cx
                .update(|_window, cx| {
                    postprod_rules::open_postprod_rules(
                        store,
                        language_registry,
                        inline_delegate,
                        config_root,
                        automations,
                        Some(context_callbacks),
                        mode,
                        cx,
                    )
                })?
                .await?;

            this.update(cx, |dashboard, _cx| {
                dashboard.postprod_rules_window = Some(window_handle);
            })?;

            anyhow::Ok(())
        });
        task.detach_and_log_err(cx);
    }

    /// Build resolved automation info for a single automation by ID.
    pub(crate) fn build_automation_info(
        &self,
        automation_id: &str,
    ) -> Option<dcfg::ResolvedAutomationInfo> {
        let entry = self.automations.iter().find(|a| a.id == automation_id)?;
        Some(dcfg::resolve_automation_info(entry, &self.config_root))
    }

    /// Build resolved automation info for all automations (general-mode note assignment UI).
    fn build_all_automation_infos(&self) -> Vec<dcfg::ResolvedAutomationInfo> {
        self.automations
            .iter()
            .map(|a| dcfg::resolve_automation_info(a, &self.config_root))
            .collect()
    }

    /// Build context callbacks for the PostProd Rules window.
    fn build_context_callbacks(
        &self,
        cx: &mut Context<Self>,
    ) -> Arc<postprod_rules::ContextCallbacks> {
        let dashboard_weak = cx.entity().downgrade();
        Arc::new(postprod_rules::ContextCallbacks {
            reorder: {
                let weak = dashboard_weak.clone();
                Box::new(
                    move |automation_id: &str, from: usize, direction: i32, cx: &mut App| {
                        let auto_id = automation_id.to_string();
                        weak.update(cx, |dashboard, cx| {
                            dashboard.reorder_context_entry(&auto_id, from, direction, cx);
                        })
                        .log_err();
                    },
                )
            },
            remove: {
                let weak = dashboard_weak.clone();
                Box::new(move |automation_id: &str, index: usize, cx: &mut App| {
                    let auto_id = automation_id.to_string();
                    weak.update(cx, |dashboard, cx| {
                        dashboard.remove_context_entry(&auto_id, index, cx);
                    })
                    .log_err();
                })
            },
            add_path: {
                let weak = dashboard_weak.clone();
                Box::new(move |automation_id: &str, path: PathBuf, cx: &mut App| {
                    let auto_id = automation_id.to_string();
                    weak.update(cx, |dashboard, cx| {
                        dashboard.add_context_path_entry(&auto_id, path, cx);
                    })
                    .log_err();
                })
            },
            add_script: {
                let weak = dashboard_weak;
                Box::new(
                    move |automation_id: &str, script_name: String, cx: &mut App| {
                        let auto_id = automation_id.to_string();
                        weak.update(cx, |dashboard, cx| {
                            dashboard.add_context_script_entry(&auto_id, script_name, cx);
                        })
                        .log_err();
                    },
                )
            },
        })
    }

    /// Close the current rules window (tied to the old `NoteStore`) and
    /// kick off a fresh `NoteStore` init for the new config root. Called
    /// from `switch_config_root`.
    pub(crate) fn reinit_rules_for_new_root(&mut self, cx: &mut Context<Self>) {
        if let Some(window_handle) = self.postprod_rules_window.take() {
            cx.update_window(window_handle.into(), |_, window, _cx| {
                window.remove_window();
            })
            .log_err();
        }

        self.note_store = None;
        let db_path = dcfg::state_dir_for(&self.config_root).join("notes.mdb");
        let note_store_future = NoteStore::for_path(db_path, cx);
        self._note_store_init =
            Some(cx.spawn(
                async move |dashboard, cx: &mut AsyncApp| match note_store_future.await {
                    Ok(entity) => {
                        dashboard
                            .update(cx, |dashboard, cx| {
                                dashboard.note_store = Some(entity);
                                dashboard._note_store_init = None;
                                cx.notify();
                            })
                            .log_err();
                    }
                    Err(err) => {
                        log::error!("Failed to reinitialize NoteStore: {:?}", err);
                    }
                },
            ));
    }

    /// After an automation reload, if a scoped rules window is open, rebuild
    /// its `ResolvedAutomationInfo` from the fresh state and push it to the
    /// window. Called from `reload_automations`.
    pub(crate) fn refresh_scoped_rules_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.postprod_rules_window else {
            return;
        };
        let scoped_id: Option<String> = handle
            .update(cx, |rules, _window, _cx| {
                rules.scoped_automation_id().map(String::from)
            })
            .ok()
            .flatten();

        if let Some(automation_id) = scoped_id {
            let info = self.build_automation_info(&automation_id);
            handle
                .update(cx, |rules, window, cx| {
                    rules.update_automation(info, window, cx);
                })
                .log_err();
        }
    }
}
