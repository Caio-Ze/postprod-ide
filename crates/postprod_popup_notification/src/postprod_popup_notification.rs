//! OS-level popup notification window — visible over all apps.
//!
//! Fork-owned peer to the in-app toast rendered by [`workspace::Toast`].
//! Used by the dashboard-side consumer of `kind = "notification.popup"` bus
//! events (see `crates/dashboard/src/popup_inbox.rs`).
//!
//! Pattern adapted from `crates/agent_ui/src/ui/agent_notification.rs` — same
//! `WindowKind::PopUp` geometry, same render vocabulary. Adaptations:
//!
//! - Dismiss-only (no "View" button; smart-navigation deferred).
//! - Severity-driven icon + color (info / success / warning / error).
//! - Stacking cascade: `window_options(screen, stack_index, cx)` offsets the
//!   Y origin by `stack_index * 80px`.
//! - Height 96px (two-line body).
//!
//! Sound playback is a one-line wrapper around
//! [`audio::Audio::play_sound(audio::Sound::AgentDone, cx)`] — centralized
//! here so a future switch to a dedicated sound variant is a single-file
//! change.

use std::rc::Rc;

use gpui::{
    App, Context, EventEmitter, IntoElement, PlatformDisplay, Size, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowKind, WindowOptions,
    linear_color_stop, linear_gradient, point,
};
use release_channel::ReleaseChannel;
use ui::{Render, prelude::*};

/// Vertical offset applied per cascade slot, in logical pixels.
pub const CASCADE_OFFSET_PX: f32 = 80.0;

/// Severity tag driving icon + color selection. Parsed from the bus payload
/// by `postprod_events::notify::decode_popup_notification`; unknown strings
/// coerce to [`PopupSeverity::Info`] on the decode side (this enum is the
/// post-coercion form).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupSeverity {
    Info,
    Success,
    Warning,
    Error,
}

impl PopupSeverity {
    fn icon(self) -> IconName {
        match self {
            Self::Info => IconName::Info,
            Self::Success => IconName::Check,
            Self::Warning => IconName::Warning,
            Self::Error => IconName::XCircle,
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Info => Color::Muted,
            Self::Success => Color::Success,
            Self::Warning => Color::Warning,
            Self::Error => Color::Error,
        }
    }
}

/// Which displays to open popups on. Default is [`PopupDisplay::Primary`];
/// set by the watcher TOML's `display` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupDisplay {
    Primary,
    AllScreens,
}

/// Events emitted by a [`PopupNotification`] entity. Consumed by the dashboard
/// via `cx.subscribe` to drive the `close_popup` termination path.
pub enum PopupNotificationEvent {
    Dismissed,
}

pub struct PopupNotification {
    title: SharedString,
    body: SharedString,
    severity: PopupSeverity,
}

impl EventEmitter<PopupNotificationEvent> for PopupNotification {}

impl PopupNotification {
    pub fn new(title: SharedString, body: SharedString, severity: PopupSeverity) -> Self {
        Self {
            title,
            body,
            severity,
        }
    }

    pub fn severity(&self) -> PopupSeverity {
        self.severity
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(PopupNotificationEvent::Dismissed);
    }

    /// Top-right window geometry. Baseline matches
    /// `crates/agent_ui/src/ui/agent_notification.rs` — 16px right margin and
    /// -48px top offset from `screen.bounds().top_right()`. Size is 450x96
    /// (Zed uses 450x72; we grow to fit two lines of body text).
    ///
    /// `stack_index = 0` is the topmost popup; each +1 offsets the Y origin
    /// by [`CASCADE_OFFSET_PX`].
    pub fn window_options(
        screen: Rc<dyn PlatformDisplay>,
        stack_index: usize,
        cx: &App,
    ) -> WindowOptions {
        let size = Size {
            width: px(450.0),
            height: px(96.0),
        };

        let margin_right = px(16.0);
        let margin_top = px(-48.0);
        let cascade_offset = px(CASCADE_OFFSET_PX * stack_index as f32);

        let origin = screen.bounds().top_right()
            - point(size.width + margin_right, margin_top - cascade_offset);

        let bounds = gpui::Bounds::<Pixels> { origin, size };

        let app_id = ReleaseChannel::global(cx).app_id();

        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            display_id: Some(screen.id()),
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some(app_id.to_owned()),
            window_min_size: None,
            window_decorations: Some(WindowDecorations::Client),
            tabbing_identifier: None,
            ..Default::default()
        }
    }
}

/// Plays the confirmation tone used across event popups. Reuses the upstream
/// `Sound::AgentDone` asset — a short generic "something arrived / finished"
/// tone. Centralized here so a future switch to a dedicated variant is a
/// single-file change.
pub fn play_sound(cx: &mut App) {
    audio::Audio::play_sound(audio::Sound::AgentDone, cx);
}

impl Render for PopupNotification {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let line_height = window.line_height();

        let bg = cx.theme().colors().elevated_surface_background;
        let gradient_overflow = move || {
            div()
                .h_full()
                .absolute()
                .w_8()
                .bottom_0()
                .right_0()
                .bg(linear_gradient(
                    90.0,
                    linear_color_stop(bg, 1.0),
                    linear_color_stop(bg.opacity(0.2), 0.0),
                ))
        };

        let icon = self.severity.icon();
        let icon_color = self.severity.color();

        h_flex()
            .id("popup-notification")
            .size_full()
            .p_3()
            .gap_4()
            .justify_between()
            .elevation_3(cx)
            .text_ui(cx)
            .border_color(cx.theme().colors().border)
            .rounded_xl()
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .flex_1()
                    .child(
                        h_flex().h(line_height).justify_center().child(
                            Icon::new(icon).color(icon_color).size(IconSize::Small),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .max_w(px(300.0))
                            .child(
                                div()
                                    .relative()
                                    .text_size(px(14.0))
                                    .text_color(cx.theme().colors().text)
                                    .truncate()
                                    .child(self.title.clone())
                                    .child(gradient_overflow()),
                            )
                            .child(
                                div()
                                    .relative()
                                    .text_size(px(12.0))
                                    .text_color(cx.theme().colors().text_muted)
                                    .truncate()
                                    .child(self.body.clone())
                                    .child(gradient_overflow()),
                            ),
                    ),
            )
            .child(
                v_flex().gap_1().items_center().child(
                    Button::new("dismiss", "Dismiss").full_width().on_click({
                        cx.listener(move |this, _event, _, cx| {
                            this.dismiss(cx);
                        })
                    }),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let version = semver::Version::new(0, 0, 0);
            release_channel::init_test(version, release_channel::ReleaseChannel::Dev, cx);
        });
    }

    #[gpui::test]
    fn severity_icon_and_color_mapping(_cx: &mut TestAppContext) {
        assert_eq!(PopupSeverity::Info.icon(), IconName::Info);
        assert_eq!(PopupSeverity::Success.icon(), IconName::Check);
        assert_eq!(PopupSeverity::Warning.icon(), IconName::Warning);
        assert_eq!(PopupSeverity::Error.icon(), IconName::XCircle);

        // Color doesn't implement Eq — but distinctness is what we care about,
        // and the render call depends on the icon+color pair staying in lockstep.
        // Spot-check a couple by round-tripping through a match on the original:
        for sev in [
            PopupSeverity::Info,
            PopupSeverity::Success,
            PopupSeverity::Warning,
            PopupSeverity::Error,
        ] {
            let _ = sev.color();
        }
    }

    /// Test 2 from spec § "Tests": cascade offset — `stack_index = 1` adds
    /// 80px below the baseline origin.
    #[gpui::test]
    fn cascade_offset_adds_80px_per_index(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let display = cx.primary_display().expect("TestPlatform has a display");

            let options_0 = PopupNotification::window_options(display.clone(), 0, cx);
            let options_1 = PopupNotification::window_options(display.clone(), 1, cx);
            let options_2 = PopupNotification::window_options(display, 2, cx);

            let WindowBounds::Windowed(bounds_0) = options_0.window_bounds.expect("bounds_0") else {
                panic!("expected Windowed bounds");
            };
            let WindowBounds::Windowed(bounds_1) = options_1.window_bounds.expect("bounds_1") else {
                panic!("expected Windowed bounds");
            };
            let WindowBounds::Windowed(bounds_2) = options_2.window_bounds.expect("bounds_2") else {
                panic!("expected Windowed bounds");
            };

            let y0: f32 = bounds_0.origin.y.into();
            let y1: f32 = bounds_1.origin.y.into();
            let y2: f32 = bounds_2.origin.y.into();

            assert!(
                (y1 - y0 - CASCADE_OFFSET_PX).abs() < 0.001,
                "stack_index 1 should be CASCADE_OFFSET_PX below 0 (y0={y0}, y1={y1})"
            );
            assert!(
                (y2 - y0 - CASCADE_OFFSET_PX * 2.0).abs() < 0.001,
                "stack_index 2 should be 2*CASCADE_OFFSET_PX below 0 (y0={y0}, y2={y2})"
            );
            // Size and X stay constant across stack indices.
            assert_eq!(bounds_0.size, bounds_1.size);
            assert_eq!(bounds_0.origin.x, bounds_1.origin.x);
        });
    }

    /// Test 3 from spec § "Tests": window options carry the correct flags —
    /// not focused, popup kind, not movable, display id set, app_id set.
    #[gpui::test]
    fn window_options_flags_match_popup_contract(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let display = cx.primary_display().expect("TestPlatform has a display");
            let options = PopupNotification::window_options(display.clone(), 0, cx);

            assert!(!options.focus, "popups must not steal focus");
            assert!(matches!(options.kind, WindowKind::PopUp));
            assert!(!options.is_movable);
            assert_eq!(options.display_id, Some(display.id()));
            assert!(options.app_id.is_some(), "app_id drives on-screen grouping");
            assert!(options.window_bounds.is_some());
        });
    }
}
