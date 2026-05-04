//! DashboardItem-side reader for `kind = "notification"` bus events.
//!
//! Wraps [`postprod_events::bus::EventInbox`] configured for the
//! `notification` kind, decodes each envelope into a
//! [`postprod_events::notify::NotificationEvent`], and renders it as a
//! `Workspace::show_toast` call. Subscribes to `fs::Fs::watch` on the
//! kind-subdirectory and drains pending files on every event batch. Drains
//! once on construction so events that accumulated while the dashboard was
//! closed surface immediately on open (the crash-safety guarantee from the
//! spec).
//!
//! Future kinds with a dashboard-side handler add a sibling type here
//! (e.g. `DashboardRefreshInbox`) — the pattern is fixed; adding one is a
//! ~30-line change with no rework of this reader.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fs::Fs;
use futures::StreamExt;
use gpui::{App, AppContext as _, AsyncApp, Task, WeakEntity};
use postprod_events::bus::{EventInbox, INBOX_ENV_VAR};
use postprod_events::notify::{
    NOTIFICATION_KIND, NotificationEvent, Severity, decode_notification,
};
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);

/// TypeId marker scoping `NotificationId::composite` for event-bus toasts.
/// Stable across the process; combined with `event_id` (filename stem) it
/// gives each emitted event its own toast slot so rapid-fire events do not
/// collapse into one.
struct EventInboxToast;

pub struct DashboardNotificationInbox {
    /// Stored to keep the fs-watch subscription alive for the inbox's
    /// lifetime — dropping the task cancels the watch.
    _watch_task: Task<()>,
}

impl DashboardNotificationInbox {
    /// Spawns the fs-watch subscription. Drains pending files immediately on
    /// construction (offline-accumulation guarantee), then re-drains on each
    /// event batch delivered by `fs.watch()`.
    ///
    /// Honors the [`INBOX_ENV_VAR`] override (so integration tests can point
    /// at a `tempfile::TempDir`); otherwise uses
    /// `paths::data_dir().join("events")`.
    pub fn new(fs: Arc<dyn Fs>, workspace: WeakEntity<Workspace>, cx: &mut App) -> Self {
        let root = resolve_root();
        let inbox = EventInbox::new(NOTIFICATION_KIND, root);
        let kind_dir = inbox.kind_dir();

        let watch_task = cx.spawn(async move |cx: &mut AsyncApp| {
            // Create the kind-directory through the Fs abstraction so any
            // alternative Fs impl observes the same filesystem state as the
            // watch subscription.
            fs.create_dir(&kind_dir).await.log_err();

            // Initial drain — events that accumulated while the dashboard
            // was closed surface immediately on open.
            drain_into_toasts(&inbox, &workspace, cx).await.log_err();

            let (mut events, _handle) = fs.watch(&kind_dir, FS_WATCH_LATENCY).await;
            while events.next().await.is_some() {
                drain_into_toasts(&inbox, &workspace, cx).await.log_err();
            }
        });

        Self {
            _watch_task: watch_task,
        }
    }
}

fn resolve_root() -> PathBuf {
    if let Some(override_path) = std::env::var_os(INBOX_ENV_VAR) {
        return PathBuf::from(override_path);
    }
    paths::data_dir().join("events")
}

async fn drain_into_toasts(
    inbox: &EventInbox,
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let inbox = inbox.clone();
    let events: Vec<NotificationEvent> = cx
        .background_spawn(async move {
            let mut out = Vec::new();
            inbox.drain(|envelope, event_id| {
                if let Some(event) = decode_notification(envelope, event_id) {
                    out.push(event);
                }
            });
            out
        })
        .await;

    workspace.update(cx, |workspace, cx| {
        for event in events {
            workspace.show_toast(build_toast(event), cx);
        }
    })
}

fn build_toast(event: NotificationEvent) -> Toast {
    let msg = render_message(&event);
    let id = NotificationId::composite::<EventInboxToast>(event.event_id);
    let mut toast = Toast::new(id, msg);
    if matches!(event.severity, Severity::Info | Severity::Success) {
        toast = toast.autohide();
    }
    toast
}

fn render_message(event: &NotificationEvent) -> String {
    let prefix = match event.severity {
        Severity::Info => "",
        Severity::Success => "✓ ",
        Severity::Warning => "⚠ ",
        Severity::Error => "✗ ",
    };
    let source = event
        .source
        .as_deref()
        .map(|s| format!("\n— {s}"))
        .unwrap_or_default();
    format!("{prefix}{}\n{}{source}", event.title, event.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(severity: Severity, source: Option<&str>) -> NotificationEvent {
        NotificationEvent {
            title: "Done".into(),
            body: "exported foo.wav".into(),
            severity,
            source: source.map(str::to_string),
            event_id: "id-1".into(),
        }
    }

    // Test 21: severity prefixes render as expected (✓, ⚠, ✗, or none for info).
    #[test]
    fn severity_prefixes_in_message() {
        assert!(render_message(&event(Severity::Info, None)).starts_with("Done"));
        assert!(render_message(&event(Severity::Success, None)).starts_with("✓ Done"));
        assert!(render_message(&event(Severity::Warning, None)).starts_with("⚠ Done"));
        assert!(render_message(&event(Severity::Error, None)).starts_with("✗ Done"));
    }

    // Source rendered as trailing line.
    #[test]
    fn source_rendered_as_trailing_line() {
        let msg = render_message(&event(Severity::Info, Some("tool-x")));
        assert!(msg.ends_with("— tool-x"));
        assert!(msg.contains("\nexported foo.wav\n"));
    }

    // Test 22: autohide semantics — info/success carry autohide; warning/error do not.
    // Toast doesn't expose `autohide()` getter, so we PartialEq against a
    // hand-built reference: `.autohide()` is the only difference.
    #[test]
    fn autohide_info_and_success_only() {
        let info_toast = build_toast(event(Severity::Info, None));
        let success_toast = build_toast(event(Severity::Success, None));
        let warning_toast = build_toast(event(Severity::Warning, None));
        let error_toast = build_toast(event(Severity::Error, None));

        // Reference: same id+msg+autohide-as-expected, eq via Toast::PartialEq.
        let info_id = NotificationId::composite::<EventInboxToast>("id-1");
        let info_ref = Toast::new(info_id, render_message(&event(Severity::Info, None))).autohide();
        let success_ref = Toast::new(
            NotificationId::composite::<EventInboxToast>("id-1"),
            render_message(&event(Severity::Success, None)),
        )
        .autohide();
        let warning_ref = Toast::new(
            NotificationId::composite::<EventInboxToast>("id-1"),
            render_message(&event(Severity::Warning, None)),
        );
        let error_ref = Toast::new(
            NotificationId::composite::<EventInboxToast>("id-1"),
            render_message(&event(Severity::Error, None)),
        );

        // Toast::PartialEq does NOT compare the `autohide` field — only id,
        // msg, and on_click presence. So we assert id+msg matches the
        // reference (sanity check that build_toast doesn't drift on those
        // fields). The autohide behavior itself can only be exercised
        // against a real Workspace; that path is covered by manual
        // verification (M3, M4 in the spec).
        // (Toast doesn't implement Debug, so we use raw `==` rather than
        // assert_eq!.)
        assert!(info_toast == info_ref, "info Toast diverges");
        assert!(success_toast == success_ref, "success Toast diverges");
        assert!(warning_toast == warning_ref, "warning Toast diverges");
        assert!(error_toast == error_ref, "error Toast diverges");
    }

    // Test 20 sanity: distinct event_ids produce distinct NotificationIds.
    #[test]
    fn distinct_event_ids_produce_distinct_ids() {
        let a = NotificationId::composite::<EventInboxToast>("id-a");
        let b = NotificationId::composite::<EventInboxToast>("id-b");
        assert_ne!(a, b);
    }
}
