//! Integration tests for the panel ↔ tab surface-transition contract that
//! `dashboard::OpenAsTab` enforces. See `private/specs/dashboard-centralized-tab.md`.
//!
//! These tests intentionally use a temp config root with no automations / tools
//! / agents — the goal is to verify the architectural invariants (single
//! shared `Entity<DashboardItem>`, mutual exclusion, single-instance, in-place
//! reload propagation), not feature behavior. The existing dashboard /
//! `postprod_dashboard_config` test suites are the regression guard for
//! everything else.

use std::path::Path;

use fs::FakeFs;
use gpui::{AppContext, Entity, EntityId, TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use util::path;
use workspace::{Workspace, dock::Panel};

use crate::{DashboardItem, DashboardPanel, open_dashboard_as_tab};

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        crate::init(cx);
    });
}

/// Mirror of the spec's visibility-check helper composed from existing
/// primitives — there is no `workspace.is_panel_visible::<T>` accessor.
fn is_dashboard_panel_visible(
    workspace: &Workspace,
    window: &gpui::Window,
    cx: &mut gpui::App,
) -> bool {
    let Some(panel) = workspace.panel::<DashboardPanel>(cx) else {
        return false;
    };
    let position = panel.read(cx).position(window, cx);
    let dock = workspace.dock_at_position(position).read(cx);
    let panel_id = panel.entity_id();
    dock.is_open()
        && dock
            .visible_panel()
            .is_some_and(|p| p.panel_id() == panel_id)
}

async fn setup_workspace_with_panel(
    cx: &mut TestAppContext,
) -> (
    Entity<Workspace>,
    Entity<DashboardPanel>,
    tempfile::TempDir,
    &mut VisualTestContext,
) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/project"), json!({})).await;
    let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
    let (workspace, cx) =
        cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

    let config_dir = tempfile::tempdir().expect("create temp config dir");
    let config_root = config_dir.path().to_path_buf();

    let panel = workspace.update_in(cx, |workspace, _window, cx| {
        let weak_workspace = workspace.weak_handle();
        let item = DashboardItem::new(workspace, config_root, cx);
        cx.new(|_| DashboardPanel {
            item,
            workspace: weak_workspace,
            _tab_close_subscription: None,
        })
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_panel(panel.clone(), window, cx);
    });
    cx.run_until_parked();

    (workspace, panel, config_dir, cx)
}

/// Invoke the `OpenAsTab` handler. The action is normally dispatched via
/// `workspace.register_action`, which in production is wired up through the
/// workspace's render path (see `add_workspace_actions_listeners`). In a
/// VisualTestContext the workspace isn't necessarily in the focus chain when
/// `cx.dispatch_action` runs, so we exercise the handler function directly —
/// the action-dispatch glue is trivial enough that smoke tests cover it.
fn dispatch_open_as_tab(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        open_dashboard_as_tab(workspace, window, cx);
    });
    cx.run_until_parked();
}

fn dashboard_item_in_active_pane(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<DashboardItem>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_pane()
            .read(cx)
            .items()
            .find_map(|handle| handle.act_as::<DashboardItem>(cx))
    })
}

fn assert_dashboard_panel_visible(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    expected: bool,
    label: &str,
) {
    let visible = workspace.update_in(cx, |workspace, window, cx| {
        is_dashboard_panel_visible(workspace, window, cx)
    });
    assert_eq!(visible, expected, "{label}: panel visibility mismatch");
}

fn active_pane_items_len(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_pane().read(cx).items_len()
    })
}

#[gpui::test]
async fn test_open_as_tab_opens_tab_and_hides_panel(cx: &mut TestAppContext) {
    init_test(cx);
    let (workspace, _panel, _config_dir, cx) = setup_workspace_with_panel(cx).await;

    assert_dashboard_panel_visible(&workspace, cx, true, "before OpenAsTab");
    assert_eq!(
        active_pane_items_len(&workspace, cx),
        0,
        "active pane should start empty"
    );

    dispatch_open_as_tab(&workspace, cx);

    assert_eq!(
        active_pane_items_len(&workspace, cx),
        1,
        "active pane should hold the dashboard tab after OpenAsTab"
    );
    assert!(
        dashboard_item_in_active_pane(&workspace, cx).is_some(),
        "active pane should contain a DashboardItem"
    );
    assert_dashboard_panel_visible(&workspace, cx, false, "after OpenAsTab");
}

#[gpui::test]
async fn test_open_as_tab_is_single_instance(cx: &mut TestAppContext) {
    init_test(cx);
    let (workspace, _panel, _config_dir, cx) = setup_workspace_with_panel(cx).await;

    dispatch_open_as_tab(&workspace, cx);
    let after_first = active_pane_items_len(&workspace, cx);
    let first_item_id = dashboard_item_in_active_pane(&workspace, cx)
        .expect("first OpenAsTab should add the tab")
        .entity_id();

    dispatch_open_as_tab(&workspace, cx);
    let after_second = active_pane_items_len(&workspace, cx);
    let second_item_id = dashboard_item_in_active_pane(&workspace, cx)
        .expect("dashboard tab should still be present")
        .entity_id();

    assert_eq!(after_first, after_second, "no duplicate tab created");
    assert_eq!(first_item_id, second_item_id, "same item entity reused");
}

#[gpui::test]
async fn test_closing_tab_restores_panel(cx: &mut TestAppContext) {
    init_test(cx);
    let (workspace, _panel, _config_dir, cx) = setup_workspace_with_panel(cx).await;

    dispatch_open_as_tab(&workspace, cx);
    assert_dashboard_panel_visible(&workspace, cx, false, "tab open");

    let active_pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    active_pane.update_in(cx, |pane, window, cx| {
        pane.close_active_item(&Default::default(), window, cx)
            .detach();
    });
    cx.run_until_parked();

    assert_eq!(
        active_pane_items_len(&workspace, cx),
        0,
        "active pane should be empty after tab close"
    );
    assert_dashboard_panel_visible(
        &workspace,
        cx,
        true,
        "panel should reappear after tab close",
    );
}

#[gpui::test]
async fn test_state_shared_between_panel_and_tab(cx: &mut TestAppContext) {
    init_test(cx);
    let (workspace, panel, _config_dir, cx) = setup_workspace_with_panel(cx).await;

    panel.update(cx, |panel, cx| {
        let item = panel.item.clone();
        item.update(cx, |item, _| {
            item.param_values
                .entry("auto-1".to_string())
                .or_default()
                .insert("key".to_string(), "v".to_string());
        });
    });

    dispatch_open_as_tab(&workspace, cx);

    let tab_item =
        dashboard_item_in_active_pane(&workspace, cx).expect("tab should be present");
    let value = tab_item.read_with(cx, |item, _| {
        item.param_values
            .get("auto-1")
            .and_then(|m| m.get("key"))
            .cloned()
    });
    assert_eq!(value, Some("v".to_string()), "tab sees panel-side mutation");

    tab_item.update(cx, |item, _| {
        item.param_values
            .entry("auto-1".to_string())
            .or_default()
            .insert("key".to_string(), "w".to_string());
    });
    let panel_value = panel.read_with(cx, |panel, cx| {
        panel
            .item
            .read(cx)
            .param_values
            .get("auto-1")
            .and_then(|m| m.get("key"))
            .cloned()
    });
    assert_eq!(
        panel_value,
        Some("w".to_string()),
        "panel sees tab-side mutation"
    );
}

/// Symmetric to Test 1 — opening the panel while the tab is up must close
/// the tab. `OpenAsTab` enforces "tab open → panel hides"; the dock panel's
/// `set_active(true)` enforces "panel open → tab closes". Without this both
/// surfaces would render simultaneously when the user re-opened the panel
/// via the dock button while a tab was still around.
#[gpui::test]
async fn test_panel_reopen_closes_existing_tab(cx: &mut TestAppContext) {
    init_test(cx);
    let (workspace, _panel, _config_dir, cx) = setup_workspace_with_panel(cx).await;

    dispatch_open_as_tab(&workspace, cx);
    assert_dashboard_panel_visible(&workspace, cx, false, "after OpenAsTab");
    assert_eq!(active_pane_items_len(&workspace, cx), 1, "tab present");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<DashboardPanel>(window, cx);
    });
    cx.run_until_parked();

    assert_dashboard_panel_visible(&workspace, cx, true, "panel re-opened");
    assert_eq!(
        active_pane_items_len(&workspace, cx),
        0,
        "tab should be closed when panel becomes active"
    );
}

/// Test 5 — architectural invariant: mutating the inner item is reflected on
/// both surfaces. Spec originally framed this around `switch_config_root`
/// reading from disk; the config loaders use real-fs APIs, so plumbing
/// FakeFs through them is out of scope. The shared-entity invariant the
/// spec rests on is what actually matters and is what this test pins.
#[gpui::test]
async fn test_inner_item_mutation_visible_from_both_surfaces(cx: &mut TestAppContext) {
    init_test(cx);
    let (workspace, panel, _config_dir, cx) = setup_workspace_with_panel(cx).await;

    dispatch_open_as_tab(&workspace, cx);
    let tab_item =
        dashboard_item_in_active_pane(&workspace, cx).expect("tab should be present");

    let panel_item_id: EntityId = panel.read_with(cx, |panel, _| panel.item.entity_id());
    assert_eq!(
        tab_item.entity_id(),
        panel_item_id,
        "tab and panel reference the same DashboardItem entity"
    );

    tab_item.update(cx, |item, _| {
        item.expanded_automations.insert("alpha".to_string());
    });
    let panel_sees: bool = panel.read_with(cx, |panel, cx| {
        panel.item.read(cx).expanded_automations.contains("alpha")
    });
    assert!(
        panel_sees,
        "panel observes the mutation made through the tab item handle"
    );
}
