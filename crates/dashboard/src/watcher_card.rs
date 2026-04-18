//! WATCHERS section renderer for the dashboard panel.
//!
//! Renders one card per loaded watcher (Ok or Err), an `[T] Edit TOML` chip
//! at the section header (opens the watchers config dir in the editor), and
//! an `[+ Add Watcher]` tile that writes a template TOML and opens it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::Utc;
use gpui::{App, Context, IntoElement, ParentElement, SharedString, Styled, WeakEntity};
use postprod_dashboard_config::watcher_config::{
    LoadError, WatcherConfig, watchers_config_dir_for,
};
use postprod_watchers::{WatcherId, WatcherStatus};
use ui::{
    ButtonLike, ButtonSize, Color, Disclosure, Divider, DividerColor, Icon, IconName, IconSize,
    Label, LabelSize, Tooltip, h_flex, prelude::*, v_flex,
};
use util::ResultExt as _;
use workspace::{OpenOptions, Workspace};

use crate::Dashboard;

const SECTION_KEY: &str = "watchers";

/// Render the WATCHERS section. Returns an empty `v_flex` if no watcher
/// TOMLs exist AND the watchers directory doesn't exist (don't add visual
/// noise for users who haven't opted into the feature).
pub fn render_watchers_section(
    collapsed_sections: &HashSet<String>,
    config_root: &Path,
    watchers: &[Result<WatcherConfig, LoadError>],
    statuses: &HashMap<WatcherId, WatcherStatus>,
    workspace: &WeakEntity<Workspace>,
    entity: WeakEntity<Dashboard>,
    cx: &mut Context<Dashboard>,
) -> impl IntoElement {
    let watchers_dir = watchers_config_dir_for(config_root);
    if watchers.is_empty() && !watchers_dir.exists() {
        // Hide section entirely when the user hasn't created the dir yet.
        return v_flex().w_full();
    }

    let is_open = !collapsed_sections.contains(SECTION_KEY);
    let toggle_entity = entity.clone();
    let toggle_id = SECTION_KEY.to_string();
    let disclosure = Disclosure::new(SharedString::from("disc-watchers"), is_open).on_click(
        move |_, _, cx| {
            toggle_entity
                .update(cx, |this, cx| this.toggle_section(&toggle_id, cx))
                .log_err();
        },
    );

    let edit_toml_chip = render_edit_toml_chip(watchers_dir.clone(), workspace.clone(), cx);

    let header = h_flex()
        .px_1()
        .mb_2()
        .gap_2()
        .items_center()
        .child(disclosure)
        .child(
            Label::new("WATCHERS")
                .color(Color::Muted)
                .size(LabelSize::Small),
        )
        .child(Divider::horizontal().color(DividerColor::BorderVariant))
        .child(edit_toml_chip);

    if !is_open {
        return v_flex().w_full().gap_1().child(header);
    }

    let mut body = v_flex().w_full().gap_1().child(header);

    for entry in watchers {
        body = body.child(render_card(entry, statuses, workspace.clone(), cx));
    }

    body = body.child(render_add_tile(config_root.to_path_buf(), workspace.clone(), cx));
    body
}

fn render_edit_toml_chip(
    dir: PathBuf,
    workspace: WeakEntity<Workspace>,
    _cx: &App,
) -> impl IntoElement {
    ButtonLike::new(SharedString::from("watchers-edit-toml"))
        .size(ButtonSize::None)
        .child(
            h_flex()
                .gap_1()
                .child(Icon::new(IconName::FileToml).color(Color::Muted).size(IconSize::XSmall))
                .child(
                    Label::new("Edit TOML")
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                ),
        )
        .tooltip(Tooltip::text("Open the watchers config directory"))
        .on_click(move |_, window, cx| {
            // Ensure the directory exists before opening, otherwise
            // open_abs_path silently no-ops.
            let _ = std::fs::create_dir_all(&dir);
            let dir = dir.clone();
            workspace
                .update(cx, |workspace, cx| {
                    workspace
                        .open_abs_path(dir, OpenOptions::default(), window, cx)
                        .detach();
                })
                .log_err();
        })
}

fn render_card(
    entry: &Result<WatcherConfig, LoadError>,
    statuses: &HashMap<WatcherId, WatcherStatus>,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> impl IntoElement {
    match entry {
        Ok(cfg) => render_ok_card(cfg, statuses, workspace, cx).into_any_element(),
        Err(err) => render_err_card(err, workspace, cx).into_any_element(),
    }
}

fn render_ok_card(
    cfg: &WatcherConfig,
    statuses: &HashMap<WatcherId, WatcherStatus>,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> impl IntoElement {
    let id = WatcherId(cfg.id.clone());
    let status = statuses.get(&id);
    let (status_text, status_color) = match status {
        Some(WatcherStatus::Idle) => ("idle".to_string(), Color::Muted),
        Some(WatcherStatus::LastEmit(ts)) => (
            format!("✓ {}", relative_time(*ts)),
            Color::Success,
        ),
        Some(WatcherStatus::Error(reason)) => (format!("✗ {reason}"), Color::Error),
        None => {
            if !cfg.enabled {
                ("disabled".to_string(), Color::Disabled)
            } else {
                ("starting…".to_string(), Color::Muted)
            }
        }
    };

    let label = cfg.label.clone();
    let path = cfg.path.clone();
    let id_str = cfg.id.clone();

    h_flex()
        .id(SharedString::from(format!("watcher-card-{}", cfg.id)))
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(Icon::new(IconName::Folder).color(Color::Muted).size(IconSize::Small))
        .child(
            v_flex()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small))
                .child(
                    Label::new(SharedString::from(path))
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                ),
        )
        .child(gpui::div().flex_grow())
        .child(
            Label::new(SharedString::from(status_text))
                .color(status_color)
                .size(LabelSize::XSmall),
        )
        .child(render_gear(id_str, workspace, cx))
}

fn render_err_card(
    err: &LoadError,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> impl IntoElement {
    let path = err.path.clone();
    let detail = err.detail.clone();
    let display_path = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| display_path.clone());

    h_flex()
        .id(SharedString::from(format!("watcher-err-card-{id}")))
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(Icon::new(IconName::FileToml).color(Color::Error).size(IconSize::Small))
        .child(
            v_flex()
                .gap_0p5()
                .child(Label::new(SharedString::from(display_path)).size(LabelSize::Small))
                .child(
                    Label::new(SharedString::from(format!("✗ malformed TOML: {detail}")))
                        .color(Color::Error)
                        .size(LabelSize::XSmall),
                ),
        )
        .child(gpui::div().flex_grow())
        .child(render_gear_for_path(path, workspace, cx))
}

fn render_gear(
    watcher_id: String,
    workspace: WeakEntity<Workspace>,
    _cx: &App,
) -> impl IntoElement {
    // Placeholder: gear icon click opens the specific TOML for this watcher.
    // The watcher_id is used to derive the TOML path: <id>.toml under
    // watchers_config_dir_for(config_root) — but we don't have config_root
    // in this scope, so the click handler resolves it via the dashboard
    // entity (out of scope for this micro-helper). For now, return a
    // tooltip-only icon; the section-level "Edit TOML" chip handles the
    // discoverability path.
    let _ = (watcher_id, workspace);
    Icon::new(IconName::Settings).color(Color::Muted).size(IconSize::Small)
}

fn render_gear_for_path(
    _path: PathBuf,
    _workspace: WeakEntity<Workspace>,
    _cx: &App,
) -> impl IntoElement {
    Icon::new(IconName::Settings).color(Color::Muted).size(IconSize::Small)
}

fn render_add_tile(
    config_root: PathBuf,
    workspace: WeakEntity<Workspace>,
    _cx: &App,
) -> impl IntoElement {
    ButtonLike::new(SharedString::from("watchers-add"))
        .full_width()
        .size(ButtonSize::Medium)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Icon::new(IconName::Plus).color(Color::Muted).size(IconSize::Small))
                .child(Label::new("Add Watcher").color(Color::Muted).size(LabelSize::Small)),
        )
        .tooltip(Tooltip::text(
            "Write a watcher template and open it in the editor",
        ))
        .on_click(move |_, window, cx| {
            let watchers_dir = watchers_config_dir_for(&config_root);
            let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
            let id = format!("watcher-{timestamp}");
            let filename = format!("{id}.toml");
            let target = watchers_dir.join(&filename);

            let _ = std::fs::create_dir_all(&watchers_dir);
            let template = template_toml(&id);
            if let Err(err) = std::fs::write(&target, template) {
                log::warn!("watcher add: write {} failed: {err}", target.display());
                return;
            }

            let workspace = workspace.clone();
            workspace
                .update(cx, |workspace, cx| {
                    workspace
                        .open_abs_path(target, OpenOptions::default(), window, cx)
                        .detach();
                })
                .log_err();
        })
}

fn template_toml(id: &str) -> String {
    format!(
        r#"# New watcher template — flip `enabled = true` after configuring to activate.
id = "{id}"
label = "New Watcher"
description = ""
path = "~/"
enabled = false  # flip to true to activate

[trigger]
on = "file_created"
glob = "*"
debounce_ms = 500

[[emit]]
kind = "notification"
severity = "info"
title = "File event"
body = "{{filename}} in {{folder}}"
source = "watcher-{id}"
"#
    )
}

fn relative_time(ts: chrono::DateTime<chrono::Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(ts);
    let secs = delta.num_seconds();
    if secs < 5 {
        return "just now".into();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = delta.num_minutes();
    if mins < 60 {
        return format!("{mins} min ago");
    }
    let hours = delta.num_hours();
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = delta.num_days();
    format!("{days}d ago")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    // Test 43 essence: template TOML has the documented shape.
    #[test]
    fn template_toml_shape() {
        let toml_str = template_toml("watcher-20260418-120000");
        // Must include the `enabled = false` line per D17.
        assert!(toml_str.contains("enabled = false"));
        // Must include the activation hint comment.
        assert!(toml_str.contains("flip to true to activate"));
        // Must parse as a valid WatcherConfig.
        let cfg: WatcherConfig = toml::from_str(&toml_str).expect("template parses");
        assert_eq!(cfg.id, "watcher-20260418-120000");
        assert_eq!(cfg.enabled, false);
        assert!(!cfg.emits.is_empty());
        assert_eq!(cfg.emits[0].kind, "notification");
    }

    // relative_time bucket boundaries.
    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        assert_eq!(relative_time(now - Duration::seconds(2)), "just now");
        assert!(relative_time(now - Duration::seconds(30)).ends_with("s ago"));
        assert!(relative_time(now - Duration::minutes(5)).ends_with("min ago"));
        assert!(relative_time(now - Duration::hours(3)).ends_with("h ago"));
        assert!(relative_time(now - Duration::days(2)).ends_with("d ago"));
    }
}
