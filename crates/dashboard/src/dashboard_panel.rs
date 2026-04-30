// DashboardPanel — dock-shell wrapper that hosts an Entity<DashboardItem>
//
// Mirrors the upstream `TerminalPanel + TerminalView` shape: the Panel
// coordinates dock chrome (position, settings, icon, persistence) while the
// Item presents the content. Both surfaces (the dock panel and an editor-pane
// tab opened via `dashboard::OpenAsTab`) share a single `Entity<DashboardItem>`,
// so state is unified and folder switches propagate automatically.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Styled, Subscription, WeakEntity, Window,
};
use settings::{Settings, update_settings_file};
use ui::{IconName, prelude::*};
use workspace::{
    Event as WorkspaceEvent, SaveIntent, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::ItemHandle,
};

use crate::dashboard_paths::{folder_has_dashboard_config, suite_root};
use crate::{DashboardItem, DashboardSettings, ToggleFocus};

const DASHBOARD_PANEL_KEY: &str = "Dashboard";

pub struct DashboardPanel {
    pub(crate) item: Entity<DashboardItem>,
    pub(crate) workspace: WeakEntity<Workspace>,
    // Set when an `OpenAsTab` action moves the inner item into a pane. Drops
    // when the tab is removed or another `OpenAsTab` overwrites it.
    pub(crate) _tab_close_subscription: Option<Subscription>,
}

impl DashboardPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let config_root = workspace
                .root_paths(cx)
                .into_iter()
                .find(|path| folder_has_dashboard_config(path))
                .map(|arc_path| arc_path.to_path_buf())
                .unwrap_or_else(suite_root);

            let weak_workspace = workspace.weak_handle();
            let item = DashboardItem::new(workspace, config_root, cx);
            cx.new(|_| Self {
                item,
                workspace: weak_workspace,
                _tab_close_subscription: None,
            })
        })
    }
}

impl Focusable for DashboardPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.item.read(cx).focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DashboardPanel {}

impl Render for DashboardPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.item.clone())
    }
}

impl Panel for DashboardPanel {
    fn persistent_name() -> &'static str {
        "Dashboard"
    }

    fn panel_key() -> &'static str {
        DASHBOARD_PANEL_KEY
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        DashboardSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Left | DockPosition::Right | DockPosition::Bottom
        )
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fs) = self
            .workspace
            .upgrade()
            .map(|w| w.read(cx).app_state().fs.clone())
        else {
            return;
        };
        update_settings_file(fs, cx, move |settings, _| {
            settings.dashboard_panel.get_or_insert_default().dock = Some(position.into());
        });
    }

    fn default_size(&self, _window: &Window, cx: &App) -> Pixels {
        DashboardSettings::get_global(cx).default_width
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        DashboardSettings::get_global(cx)
            .button
            .then_some(IconName::AudioOn)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Dashboard")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        DashboardSettings::get_global(cx).starts_open
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    /// Mutual exclusion (symmetric to `OpenAsTab`): when the dock panel
    /// becomes active and a `DashboardItem` tab also exists, close the tab.
    /// `OpenAsTab` already enforces the other direction (opening a tab
    /// hides the panel). Without this hook the user can end up with both
    /// surfaces visible at once if they re-open the panel from the dock
    /// button while a tab is still up.
    ///
    /// Deferred via `window.defer` because `set_active` is invoked from
    /// inside the dock's update path — the workspace itself is already
    /// being updated, so calling `workspace.update` synchronously would
    /// double-borrow.
    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !active {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                let Some(existing) = workspace.item_of_type::<DashboardItem>(cx) else {
                    return;
                };
                let item_id = existing.entity_id();
                let Some(pane) = workspace.pane_for(&existing) else {
                    return;
                };
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)
                        .detach_and_log_err(cx);
                });
            });
        });
    }
}

/// Move the panel-hosted `DashboardItem` into the active editor pane as a tab.
/// Hides the dock panel; subscribes for `WorkspaceEvent::ItemRemoved` so the
/// panel reappears when the tab is closed. If a `DashboardItem` tab already
/// exists, focuses it instead of creating a duplicate.
pub(crate) fn open_dashboard_as_tab(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(existing) = workspace.item_of_type::<DashboardItem>(cx) {
        if let Some(pane) = workspace.pane_for(&existing) {
            let handle: &dyn ItemHandle = &existing;
            if let Some(idx) = pane.read(cx).index_for_item(handle) {
                pane.update(cx, |pane, cx| {
                    pane.activate_item(idx, true, true, window, cx);
                });
            }
        }
        return;
    }

    let Some(panel) = workspace.panel::<DashboardPanel>(cx) else {
        return;
    };
    let item = panel.read(cx).item.clone();
    let item_id = item.entity_id();

    workspace.close_panel::<DashboardPanel>(window, cx);

    let active_pane = workspace.active_pane().clone();
    active_pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(item.clone()), true, true, None, window, cx);
    });

    let workspace_entity = cx.entity();
    panel.update(cx, |panel, panel_cx| {
        let subscription = panel_cx.subscribe_in(
            &workspace_entity,
            window,
            move |_panel, workspace, event, window, cx| {
                if let WorkspaceEvent::ItemRemoved {
                    item_id: removed_id,
                } = event
                    && *removed_id == item_id
                {
                    // Defer the panel focus so it runs after the current
                    // pane-removal effect cycle finishes — re-entering
                    // `panel.update` synchronously here triggers a
                    // double-borrow panic since the subscription callback
                    // already holds `&mut DashboardPanel`.
                    let workspace = workspace.clone();
                    window.defer(cx, move |window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.focus_panel::<DashboardPanel>(window, cx);
                        });
                    });
                }
            },
        );
        panel._tab_close_subscription = Some(subscription);
    });
}
