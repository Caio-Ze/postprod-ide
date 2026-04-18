//! Convenience API for `kind = "notification"`.
//!
//! Satellite tools that just want to surface a toast call [`info`],
//! [`success`], [`warning`], or [`error`] — one line, no awareness of the
//! bus envelope. Dashboard-side consumers call [`decode_notification`] on
//! the parsed envelope to validate + coerce the payload before rendering.

use serde::{Deserialize, Serialize};

use crate::bus::{self, EventEnvelope};

/// Hard cap on title length applied by [`decode_notification`].
pub const MAX_TITLE_CHARS: usize = 80;

/// Hard cap on body length applied by [`decode_notification`].
pub const MAX_BODY_CHARS: usize = 512;

/// The `notification` kind discriminator string.
pub const NOTIFICATION_KIND: &str = "notification";

/// Best-effort info notification. Errors swallowed.
pub fn info(title: &str, body: &str) {
    emit("info", title, body, None);
}

/// Best-effort success notification. Errors swallowed.
pub fn success(title: &str, body: &str) {
    emit("success", title, body, None);
}

/// Best-effort warning notification. Errors swallowed.
pub fn warning(title: &str, body: &str) {
    emit("warning", title, body, None);
}

/// Best-effort error notification. Errors swallowed.
pub fn error(title: &str, body: &str) {
    emit("error", title, body, None);
}

/// Best-effort emit with an explicit envelope `source` tag.
pub fn emit_with_source(severity: &str, title: &str, body: &str, source: &str) {
    emit(severity, title, body, Some(source));
}

fn emit(severity: &str, title: &str, body: &str, source: Option<&str>) {
    let payload = NotificationPayload {
        title: title.to_string(),
        body: body.to_string(),
        severity: severity.to_string(),
    };
    bus::emit(NOTIFICATION_KIND, payload, source);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationPayload {
    pub title: String,
    pub body: String,
    pub severity: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

impl Severity {
    /// Parses a severity string. Unknown values coerce to [`Severity::Info`]
    /// per the spec — defensive, never errors.
    pub fn parse(s: &str) -> Self {
        match s {
            "success" => Self::Success,
            "warning" => Self::Warning,
            "error" => Self::Error,
            // "info" or anything else.
            _ => Self::Info,
        }
    }
}

/// Parsed-and-validated notification. Built by [`decode_notification`] from
/// `(EventEnvelope, event_id)`.
#[derive(Debug, Clone)]
pub struct NotificationEvent {
    pub title: String,
    pub body: String,
    pub severity: Severity,
    pub source: Option<String>,
    pub event_id: String,
}

/// Deserialize the `notification` payload from an envelope plus the source
/// filename stem. Returns `None` if:
///
/// - the envelope kind is not `"notification"`, or
/// - the payload is malformed (missing required `title` / `body` / `severity`),
///
/// in which case the caller treats this as a decode failure (the file should
/// already have moved to `rejected/` by the bus-level reader before reaching
/// this point — this guard is belt-and-suspenders).
///
/// On success, applies the [`MAX_TITLE_CHARS`] / [`MAX_BODY_CHARS`] caps
/// post-parse and coerces unknown severity strings to [`Severity::Info`].
pub fn decode_notification(env: EventEnvelope, event_id: String) -> Option<NotificationEvent> {
    if env.kind != NOTIFICATION_KIND {
        return None;
    }
    let payload: NotificationPayload = serde_json::from_value(env.payload).ok()?;
    let title = truncate_chars(&payload.title, MAX_TITLE_CHARS);
    let body = truncate_chars(&payload.body, MAX_BODY_CHARS);
    let severity = Severity::parse(&payload.severity);
    Some(NotificationEvent {
        title,
        body,
        severity,
        source: env.source,
        event_id,
    })
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{ENVELOPE_SCHEMA, EventEnvelope, EventInbox, INBOX_ENV_VAR};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // The notify helpers (info/success/warning/error/emit_with_source) call
    // `bus::emit`, which in turn uses `default_bus_root()` (env-var driven).
    // We must serialize tests that touch the env var to avoid cross-test
    // races inside the same `cargo test` process.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_inbox<F: FnOnce(&PathBuf, EventInbox)>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = TempDir::new().expect("tempdir");
        // SAFETY: test serialized via ENV_LOCK; no concurrent threads.
        unsafe {
            std::env::set_var(INBOX_ENV_VAR, dir.path());
        }
        let inbox = EventInbox::new(NOTIFICATION_KIND, dir.path().to_path_buf());
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(&dir.path().to_path_buf(), inbox)
        }));
        // SAFETY: same as above.
        unsafe {
            std::env::remove_var(INBOX_ENV_VAR);
        }
        if let Err(p) = res {
            std::panic::resume_unwind(p);
        }
    }

    // Test 14: notify::success("t", "b") writes a notification envelope.
    #[test]
    fn success_writes_notification_envelope() {
        with_inbox(|_root, inbox| {
            success("t", "b");
            let mut got = Vec::new();
            inbox.drain(|env, id| got.push((env, id)));
            assert_eq!(got.len(), 1);
            let env = &got[0].0;
            assert_eq!(env.kind, NOTIFICATION_KIND);
            assert_eq!(env.schema, ENVELOPE_SCHEMA);
            assert_eq!(env.payload["title"], "t");
            assert_eq!(env.payload["body"], "b");
            assert_eq!(env.payload["severity"], "success");
            assert!(env.source.is_none());
        });
    }

    // Test 15: emit_with_source sets the envelope source field.
    #[test]
    fn emit_with_source_sets_source_field() {
        with_inbox(|_root, inbox| {
            emit_with_source("warning", "t", "b", "tool-x");
            let mut got = Vec::new();
            inbox.drain(|env, id| got.push((env, id)));
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].0.source.as_deref(), Some("tool-x"));
            assert_eq!(got[0].0.payload["severity"], "warning");
        });
    }

    fn make_envelope(payload: serde_json::Value) -> EventEnvelope {
        EventEnvelope {
            schema: ENVELOPE_SCHEMA,
            kind: NOTIFICATION_KIND.to_string(),
            timestamp: "2026-04-18T00:00:00-03:00".to_string(),
            source: Some("test".to_string()),
            payload,
        }
    }

    // Test 16: decode_notification on a well-formed payload returns Some.
    #[test]
    fn decode_well_formed_returns_some() {
        let env = make_envelope(serde_json::json!({
            "title": "Done",
            "body": "exported foo.wav",
            "severity": "success",
        }));
        let event = decode_notification(env, "id1".to_string()).expect("Some");
        assert_eq!(event.title, "Done");
        assert_eq!(event.body, "exported foo.wav");
        assert_eq!(event.severity, Severity::Success);
        assert_eq!(event.source.as_deref(), Some("test"));
        assert_eq!(event.event_id, "id1");
    }

    // Test 17: decode_notification on missing title/body/severity returns None.
    #[test]
    fn decode_missing_required_field_returns_none() {
        let cases = vec![
            // Missing title
            serde_json::json!({"body": "b", "severity": "info"}),
            // Missing body
            serde_json::json!({"title": "t", "severity": "info"}),
            // Missing severity
            serde_json::json!({"title": "t", "body": "b"}),
        ];
        for payload in cases {
            let env = make_envelope(payload.clone());
            assert!(
                decode_notification(env, "id".to_string()).is_none(),
                "expected None for payload {payload}"
            );
        }
    }

    // Test 18: unknown severity coerces to Info.
    #[test]
    fn unknown_severity_coerces_to_info() {
        let env = make_envelope(serde_json::json!({
            "title": "t",
            "body": "b",
            "severity": "frobnicate",
        }));
        let event = decode_notification(env, "id".to_string()).expect("Some");
        assert_eq!(event.severity, Severity::Info);
    }

    // Test 19: body > 512 chars is truncated post-decode.
    #[test]
    fn body_truncated_to_max() {
        let big_body = "x".repeat(MAX_BODY_CHARS + 100);
        let env = make_envelope(serde_json::json!({
            "title": "t",
            "body": big_body,
            "severity": "info",
        }));
        let event = decode_notification(env, "id".to_string()).expect("Some");
        assert_eq!(event.body.chars().count(), MAX_BODY_CHARS);
    }

    // Bonus: title also truncated (per the hard-cap rules).
    #[test]
    fn title_truncated_to_max() {
        let big_title = "x".repeat(MAX_TITLE_CHARS + 5);
        let env = make_envelope(serde_json::json!({
            "title": big_title,
            "body": "b",
            "severity": "info",
        }));
        let event = decode_notification(env, "id".to_string()).expect("Some");
        assert_eq!(event.title.chars().count(), MAX_TITLE_CHARS);
    }

    // Bonus: wrong-kind envelope returns None even if reader-side dispatch
    // happens to call decode_notification by mistake.
    #[test]
    fn decode_wrong_kind_returns_none() {
        let mut env = make_envelope(serde_json::json!({
            "title": "t", "body": "b", "severity": "info",
        }));
        env.kind = "bounce.completed".into();
        assert!(decode_notification(env, "id".to_string()).is_none());
    }
}
