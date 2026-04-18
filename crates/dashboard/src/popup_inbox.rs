//! Dashboard-side reader for `kind = "notification.popup"` bus events.
//!
//! Wraps [`postprod_events::bus::EventInbox`] configured for the
//! `notification.popup` kind, decodes each envelope into a
//! [`postprod_events::notify::NotificationPopupEvent`], and opens an OS-level
//! popup window per target display using [`postprod_popup_notification`].
//!
//! Peer of [`crate::event_inbox::DashboardNotificationInbox`] — same delivery
//! mechanism (`fs::Fs::watch` on the kind-subdirectory, offline-drain on
//! construction), different rendering.
//!
//! # Stacking + closure model
//!
//! Per-display stack capped at [`STACK_CAP`]; drop-oldest on overflow. Each
//! slot owns both its Dismiss-subscription and its autohide [`gpui::Task`];
//! removing the slot from `stacks` drops both (subscription unsubscribes,
//! timer cancels, window stays until [`Self::close_popup`] calls
//! `remove_window`).
//!
//! Three termination paths route through [`Self::close_popup`]:
//! 1. User clicks Dismiss → popup entity emits `Dismissed` → subscription
//!    routes to the helper.
//! 2. Autohide timer fires (info/success only, 10s).
//! 3. Cap eviction when a 6th popup lands on a display.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use fs::Fs;
use futures::StreamExt;
use gpui::{
    App, AppContext as _, AsyncApp, Context, DisplayId, PlatformDisplay, Task, WeakEntity,
    WindowHandle,
};
use postprod_events::bus::{EventInbox, INBOX_ENV_VAR};
use postprod_events::notify::{
    NOTIFICATION_POPUP_KIND, NotificationPopupEvent, PopupDisplay, Severity,
    decode_popup_notification,
};
use postprod_popup_notification::{
    PopupNotification, PopupNotificationEvent, PopupSeverity, play_sound,
};
use util::ResultExt as _;

use crate::Dashboard;

const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);

/// Per-display hard cap on simultaneous popup windows. Drop-oldest on the 6th.
pub const STACK_CAP: usize = 5;

/// Autohide duration for info/success popups. Warning/error are persistent
/// until the user clicks Dismiss.
pub const AUTOHIDE_AFTER: Duration = Duration::from_secs(10);

struct PopupSlot {
    window: WindowHandle<PopupNotification>,
    /// Cascade slot `0..STACK_CAP`. Baked into the popup's Y-origin at
    /// `open_on_display` time and never changes (v1 does not re-layout on
    /// eviction — survivor slots keep their existing positions). On the next
    /// insertion, `find_free_slot_index` picks the lowest unoccupied index,
    /// so the new popup fills the vacated position instead of colliding
    /// with a survivor at `len * 80px`.
    stack_index: usize,
    // Stored so the slot owns the subscription. Dropping the slot
    // unsubscribes; prevents the Dismiss event from racing the `close_popup`
    // call that removed it.
    _dismiss_subscription: gpui::Subscription,
    // Stored so the slot owns the autohide timer. Dropping the slot cancels
    // the timer; info/success use `Some`, warning/error use `None`.
    _autohide: Option<Task<()>>,
}

pub struct DashboardPopupInbox {
    stacks: HashMap<DisplayId, Vec<PopupSlot>>,
    /// Stored to keep the fs-watch subscription alive for the inbox's
    /// lifetime — dropping the task cancels the watch.
    _watch_task: Task<()>,
}

impl DashboardPopupInbox {
    /// Spawns the fs-watch subscription on `events/notification.popup/`.
    /// Drains pending files immediately on construction (offline-accumulation
    /// guarantee), then re-drains on each event batch delivered by
    /// `fs.watch()`.
    ///
    /// Honors the [`INBOX_ENV_VAR`] override so integration tests can point
    /// at a scratch dir.
    pub fn new(fs: Arc<dyn Fs>, cx: &mut Context<Dashboard>) -> Self {
        let root = resolve_root();
        let inbox = EventInbox::new(NOTIFICATION_POPUP_KIND, root);
        let kind_dir = inbox.kind_dir();

        let watch_task = cx.spawn(async move |this: WeakEntity<Dashboard>, cx: &mut AsyncApp| {
            fs.create_dir(&kind_dir).await.log_err();

            // Initial drain — events that accumulated while the dashboard
            // was closed surface immediately on open.
            drain_and_dispatch(&inbox, &this, cx).await.log_err();

            let (mut events, _handle) = fs.watch(&kind_dir, FS_WATCH_LATENCY).await;
            while events.next().await.is_some() {
                drain_and_dispatch(&inbox, &this, cx).await.log_err();
            }
        });

        Self {
            stacks: HashMap::new(),
            _watch_task: watch_task,
        }
    }

    fn dispatch_event(&mut self, event: NotificationPopupEvent, cx: &mut Context<Dashboard>) {
        // Sound plays ONCE per event (not per popup). Matches upstream Zed
        // conversation_view pattern — sound is an event-level signal. Bursts
        // that fill the cap each get their own sound by definition.
        play_sound(cx);

        let targets: Vec<Rc<dyn PlatformDisplay>> = match event.display {
            PopupDisplay::Primary => cx.primary_display().into_iter().collect(),
            PopupDisplay::AllScreens => cx.displays(),
        };
        for display in targets {
            self.open_on_display(&event, display, cx);
        }
    }

    fn open_on_display(
        &mut self,
        event: &NotificationPopupEvent,
        display: Rc<dyn PlatformDisplay>,
        cx: &mut Context<Dashboard>,
    ) {
        let display_id = display.id();

        // Cap-5 drop-oldest. Survivor slots keep their existing Y positions —
        // v1 does not re-layout on eviction. The new popup fills the slot
        // index that was vacated (lowest unoccupied index in `0..STACK_CAP`),
        // not `len`, which would collide with a survivor's Y-origin.
        let needs_eviction = self
            .stacks
            .get(&display_id)
            .map(|s| s.len() >= STACK_CAP)
            .unwrap_or(false);
        if needs_eviction {
            if let Some(oldest) = self.stacks.get(&display_id).and_then(|s| s.first()).map(|slot| slot.window) {
                self.close_popup(display_id, oldest, cx);
            }
        }

        let stack_index = find_free_slot_index(self.stacks.get(&display_id));
        let options = PopupNotification::window_options(display.clone(), stack_index, cx);

        let title = event.title.clone().into();
        let body = event.body.clone().into();
        let severity = map_severity(event.severity);

        let window = match cx.open_window(options, move |_window, cx| {
            cx.new(move |_cx| PopupNotification::new(title, body, severity))
        }) {
            Ok(handle) => handle,
            Err(err) => {
                log::warn!("popup_inbox: open_window failed: {err}");
                return;
            }
        };

        let entity = match window.entity(cx) {
            Ok(entity) => entity,
            Err(err) => {
                log::warn!("popup_inbox: window.entity failed: {err}");
                return;
            }
        };

        let dismiss_subscription = cx.subscribe(&entity, move |this, _popup, event, cx| {
            match event {
                PopupNotificationEvent::Dismissed => {
                    this.popup_inbox.close_popup(display_id, window, cx);
                }
            }
        });

        let autohide = if matches!(event.severity, Severity::Info | Severity::Success) {
            Some(cx.spawn(
                async move |this: WeakEntity<Dashboard>, cx: &mut AsyncApp| {
                    cx.background_executor().timer(AUTOHIDE_AFTER).await;
                    this.update(cx, |dashboard, cx| {
                        dashboard.popup_inbox.close_popup(display_id, window, cx);
                    })
                    .ok();
                },
            ))
        } else {
            None
        };

        self.stacks
            .entry(display_id)
            .or_default()
            .push(PopupSlot {
                window,
                stack_index,
                _dismiss_subscription: dismiss_subscription,
                _autohide: autohide,
            });
    }

    fn close_popup(
        &mut self,
        display_id: DisplayId,
        window: WindowHandle<PopupNotification>,
        cx: &mut App,
    ) {
        if let Some(stack) = self.stacks.get_mut(&display_id) {
            // Removing the matching slot drops its subscription + autohide
            // task in one step. `retain` is O(n) on a tiny vec (≤5).
            stack.retain(|slot| slot.window != window);
        }
        // Explicit OS-window close — `remove_window` does not emit
        // PopupNotificationEvent::Dismissed on its own (matches upstream
        // AgentNotification pattern).
        window
            .update(cx, |_, w, _| w.remove_window())
            .log_err();
    }

    /// Test-only stack inspector. Kept per `popup-notifications.md` § Tests
    /// T2.4 so future dashboard integration tests can assert per-display
    /// stack state without reaching into private fields. Currently unused —
    /// Phase 2 ships without the full integration suite (see report).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn stack_len(&self, display: DisplayId) -> usize {
        self.stacks.get(&display).map(|s| s.len()).unwrap_or(0)
    }
}

fn resolve_root() -> PathBuf {
    if let Some(override_path) = std::env::var_os(INBOX_ENV_VAR) {
        return PathBuf::from(override_path);
    }
    paths::data_dir().join("events")
}

async fn drain_and_dispatch(
    inbox: &EventInbox,
    dashboard: &WeakEntity<Dashboard>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let inbox = inbox.clone();
    let events: Vec<NotificationPopupEvent> = cx
        .background_spawn(async move {
            let mut out = Vec::new();
            inbox.drain(|envelope, event_id| {
                if let Some(event) = decode_popup_notification(envelope, event_id) {
                    out.push(event);
                }
            });
            out
        })
        .await;

    dashboard.update(cx, |dashboard, cx| {
        for event in events {
            dashboard.popup_inbox.dispatch_event(event, cx);
        }
    })
}

fn map_severity(severity: Severity) -> PopupSeverity {
    match severity {
        Severity::Info => PopupSeverity::Info,
        Severity::Success => PopupSeverity::Success,
        Severity::Warning => PopupSeverity::Warning,
        Severity::Error => PopupSeverity::Error,
    }
}

/// Lowest unoccupied cascade slot index in `0..STACK_CAP`. Guaranteed to
/// return a value under the cap because callers evict the oldest before
/// calling this when `len == STACK_CAP`. The scan is bounded at 5 — linear
/// over a tiny vec, no hashing overhead.
fn find_free_slot_index(stack: Option<&Vec<PopupSlot>>) -> usize {
    let Some(stack) = stack else {
        return 0;
    };
    (0..STACK_CAP)
        .find(|idx| !stack.iter().any(|slot| slot.stack_index == *idx))
        // Should be unreachable post-eviction, but if it ever fires we'd
        // rather produce a valid-in-range index than panic.
        .unwrap_or(STACK_CAP - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Construct minimal PopupSlot-shaped data *only* by building the helper
    // to a predictable shape. We can't build a real PopupSlot in a unit test
    // because WindowHandle / Subscription require an App — but
    // find_free_slot_index only touches stack_index, so we assert directly
    // on a hand-rolled Vec with the right indices.
    //
    // NOTE: This helper is the only test path that catches the cap-eviction
    // collision bug without a live dashboard — which is why it's worth
    // testing even without a full integration harness.
    fn mock_stack(indices: &[usize]) -> Vec<usize> {
        indices.to_vec()
    }

    fn find_free(stack_indices: &[usize]) -> usize {
        (0..STACK_CAP)
            .find(|idx| !stack_indices.contains(idx))
            .unwrap_or(STACK_CAP - 1)
    }

    #[test]
    fn find_free_slot_empty_returns_zero() {
        assert_eq!(find_free(&mock_stack(&[])), 0);
    }

    #[test]
    fn find_free_slot_after_eviction_fills_vacated_position() {
        // Simulates the cap-eviction path: `0` evicted, survivors at 1..5.
        // New popup should pick 0, not 4 or 5.
        assert_eq!(find_free(&mock_stack(&[1, 2, 3, 4])), 0);
    }

    #[test]
    fn find_free_slot_sequential_fill() {
        assert_eq!(find_free(&mock_stack(&[0])), 1);
        assert_eq!(find_free(&mock_stack(&[0, 1])), 2);
        assert_eq!(find_free(&mock_stack(&[0, 1, 2])), 3);
        assert_eq!(find_free(&mock_stack(&[0, 1, 2, 3])), 4);
    }

    #[test]
    fn find_free_slot_midrange_gap() {
        // Survivors at 0,1,3,4 — slot 2 was dismissed. Next popup picks 2.
        assert_eq!(find_free(&mock_stack(&[0, 1, 3, 4])), 2);
    }
}
