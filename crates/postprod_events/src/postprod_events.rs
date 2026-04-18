//! Platform Event Bus for PostProd IDE.
//!
//! General mechanism for cross-tool messaging: any emitter writes a JSON file
//! (atomic temp+rename) into an inbox directory keyed by event *kind*; per-kind
//! readers poll their inbox, parse events, dispatch to a handler, and move
//! files to `processed/` or `rejected/`.
//!
//! This crate ships two layers:
//!
//! - [`bus`] — kind-agnostic envelope + emit/read primitives.
//! - [`notify`] — convenience API for `kind = "notification"` plus the
//!   payload + decode helpers consumed by dashboard-side readers.
//!
//! The crate intentionally has **zero** dependencies on workspace-internal
//! types (e.g. `fs::Fs`, `util::ResultExt`) so satellite tools that live
//! outside the fork workspace can depend on it directly.
//!
//! # Inbox path
//!
//! The default macOS event-bus root is hand-rolled in [`bus::default_bus_root`]
//! as `$HOME/Library/Application Support/PostProd Tools/events` so satellite
//! tools (which cannot depend on the workspace `paths` crate) can resolve it
//! without an extra workspace member. This path **must** stay in sync with
//! the macOS branch of `paths::data_dir()` at
//! `crates/paths/src/paths.rs:102-107`. Drift is guarded by an integration
//! test in this crate (see `tests/`).
//!
//! Both emit and read sides honor the [`bus::INBOX_ENV_VAR`] override
//! (`POSTPROD_EVENTS_INBOX`) so tests and tooling can redirect to a scratch
//! directory.

pub mod bus;
pub mod notify;
