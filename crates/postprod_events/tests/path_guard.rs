//! Test 13 (spec): the emitter-side hand-rolled path matches
//! `paths::data_dir().join("events")` exactly. Lives as an integration test
//! (rather than in `bus`'s `#[cfg(test)] mod tests`) because the
//! `paths` workspace crate is a `dev-dependency` only — pulling it into a
//! unit-test module would still compile fine, but keeping it in
//! `tests/` keeps the production crate's dependency graph clean
//! (`paths` never bleeds into the `cargo test` build of crates that
//! depend on `postprod_events` itself).

use std::sync::Mutex;

// `set_var` / `remove_var` are racy across tests; ensure single-threaded
// access via a mutex (within this binary, `cargo test` may parallelize).
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn default_bus_root_matches_paths_data_dir() {
    // Defensive: the override must NOT be active.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: serialized via ENV_LOCK; no concurrent access in this test.
    unsafe {
        std::env::remove_var(postprod_events::bus::INBOX_ENV_VAR);
    }

    let hand_rolled = postprod_events::bus::default_bus_root()
        .expect("HOME must be set in the test environment");
    let workspace_path = paths::data_dir().join("events");

    assert_eq!(
        hand_rolled, workspace_path,
        "emitter-side hand-rolled path must match paths::data_dir().join(\"events\") — \
         drift would silently route satellite-tool emits to the wrong directory \
         (see crates/paths/src/paths.rs:102-107)"
    );
}
