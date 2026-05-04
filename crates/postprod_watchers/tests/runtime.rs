//! Integration tests for `postprod_watchers::WatcherRuntime`.
//!
//! These exercise the spawned per-watcher tasks against `FakeFs` (so we
//! don't depend on the host filesystem to deliver events) and a real
//! tempdir bus root (so we can observe the emitted JSON envelopes via
//! ordinary `std::fs`). The test workflow:
//!
//! 1. Build a `FakeFs` populated with a watched folder.
//! 2. Spawn a `WatcherRuntime` and reconcile a config set.
//! 3. Mutate the FakeFs (create/write/remove file).
//! 4. `cx.run_until_parked()` to drain executor work.
//! 5. List `<bus_root>/<kind>/*.json` to count emits.
//!
//! We don't compare exact filenames — only counts + payloads.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs::{FakeFs, Fs, RemoveOptions};
use gpui::TestAppContext;
use postprod_dashboard_config::watcher_config::{
    TriggerKind, WatcherConfig, WatcherEmit, WatcherTrigger,
};
use postprod_watchers::{WatcherRuntime, WatcherStatus};
use tempfile::TempDir;

const WATCHED: &str = "/watched";

fn cfg(id: &str, on: TriggerKind, glob: &str) -> WatcherConfig {
    WatcherConfig {
        id: id.into(),
        label: id.into(),
        description: String::new(),
        path: WATCHED.into(),
        enabled: true,
        trigger: WatcherTrigger {
            on,
            glob: glob.into(),
            min_size_mb: 0.0,
            debounce_ms: 100,
        },
        emits: vec![WatcherEmit {
            kind: "notification".into(),
            payload: toml::from_str(
                r#"
                title = "t"
                body = "{filename}"
                severity = "info"
            "#,
            )
            .unwrap(),
        }],
    }
}

async fn fresh_fs(cx: &TestAppContext) -> Arc<FakeFs> {
    let fs = FakeFs::new(cx.executor());
    fs.create_dir(Path::new(WATCHED)).await.unwrap();
    fs
}

fn count_pending(bus_root: &Path, kind: &str) -> usize {
    let dir = bus_root.join(kind);
    std::fs::read_dir(&dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| {
                    let p = e.path();
                    p.is_file()
                        && p.extension().and_then(|x| x.to_str()) == Some("json")
                        && !p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with('.'))
                            .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

fn read_pending(bus_root: &Path, kind: &str) -> Vec<serde_json::Value> {
    let dir = bus_root.join(kind);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension().and_then(|x| x.to_str()) == Some("json")
                        && !p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with('.'))
                            .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    entries
        .into_iter()
        .map(|p| {
            let body = std::fs::read_to_string(&p).unwrap();
            serde_json::from_str::<serde_json::Value>(&body).unwrap()
        })
        .collect()
}

// Test 26: reconcile starts one task per enabled valid watcher.
#[gpui::test]
async fn reconcile_starts_one_task_per_enabled_valid_watcher(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    let configs = vec![
        cfg("a", TriggerKind::Any, "*"),
        cfg("b", TriggerKind::Any, "*"),
        {
            let mut c = cfg("c", TriggerKind::Any, "*");
            c.enabled = false;
            c
        },
    ];
    cx.update(|cx| {
        runtime.reconcile(
            configs,
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(runtime.running_count(), 2, "two enabled valid watchers");
}

// Test 27: reconcile with empty list stops all running tasks.
#[gpui::test]
async fn reconcile_empty_stops_all(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    cx.update(|cx| {
        runtime.reconcile(
            vec![cfg("a", TriggerKind::Any, "*")],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(runtime.running_count(), 1);

    cx.update(|cx| {
        runtime.reconcile(
            vec![],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    assert_eq!(runtime.running_count(), 0);
}

// Helper: drain all pending status messages from the channel, return the
// count of `Idle` messages (each fresh task spawn sends exactly one Idle
// before subscribing to fs.watch).
fn drain_idle_count(
    rx: &smol::channel::Receiver<(postprod_watchers::WatcherId, WatcherStatus)>,
) -> usize {
    let mut count = 0;
    while let Ok((_, status)) = rx.try_recv() {
        if matches!(status, WatcherStatus::Idle) {
            count += 1;
        }
    }
    count
}

// Test 39a: D19 hash short-circuit — identical config list = no restart
// (zero new Idle messages on the second call). Different config = full
// re-spawn (one Idle message per watcher).
#[gpui::test]
async fn reconcile_hash_short_circuits_on_identical_config(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    let rx = runtime.status_receiver();
    let configs = vec![cfg("a", TriggerKind::Any, "*")];

    cx.update(|cx| {
        runtime.reconcile(
            configs.clone(),
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        drain_idle_count(&rx),
        1,
        "first reconcile spawns one task → one Idle"
    );

    // Second reconcile with identical config — hash-equal short-circuit, NO restart.
    cx.update(|cx| {
        runtime.reconcile(
            configs.clone(),
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        drain_idle_count(&rx),
        0,
        "hash-equal reconcile must NOT respawn → no new Idle"
    );

    // Third reconcile with a *different* config — full restart (per D5).
    let mut changed = configs;
    changed[0].label = "different".into();
    cx.update(|cx| {
        runtime.reconcile(
            changed,
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        drain_idle_count(&rx),
        1,
        "config-change reconcile spawns fresh → one new Idle"
    );
}

// Test 28: reconcile-on-change blanket restart (D5). With ONE config
// changed, ALL enabled valid watchers respawn (one Idle per watcher).
#[gpui::test]
async fn reconcile_on_change_restarts_all_tasks(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    let rx = runtime.status_receiver();
    let initial = vec![
        cfg("a", TriggerKind::Any, "*"),
        cfg("b", TriggerKind::Any, "*"),
    ];
    cx.update(|cx| {
        runtime.reconcile(
            initial.clone(),
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(drain_idle_count(&rx), 2, "two initial spawns → two Idle");

    // Change ONE watcher's config; per D5, BOTH should restart → 2 new Idle.
    let mut changed = initial;
    changed[1].label = "changed".into();
    cx.update(|cx| {
        runtime.reconcile(
            changed,
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        drain_idle_count(&rx),
        2,
        "blanket stop-all/start-all per D5: both watchers respawn → 2 new Idle"
    );
}

/// Helper: cross the trailing-edge debounce window. `cfg` defaults
/// `debounce_ms = 100`; advancing 150ms is safely past it for tests that
/// don't customize the window.
fn drain_debounce(cx: &mut TestAppContext) {
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
}

// Test 29: file-create matching trigger+glob+min_size produces exactly
// one bus emit with the expanded payload.
#[gpui::test]
async fn file_create_matching_trigger_emits_once(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    let configs = vec![cfg("a", TriggerKind::FileCreated, "*.wav")];
    cx.update(|cx| {
        runtime.reconcile(
            configs,
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    let target = PathBuf::from(WATCHED).join("foo.wav");
    fs.write(&target, b"hello").await.unwrap();
    cx.run_until_parked();
    drain_debounce(cx);

    let pending = read_pending(bus.path(), "notification");
    assert_eq!(pending.len(), 1, "expected one emit, got {pending:?}");
    let env = &pending[0];
    assert_eq!(env["kind"], "notification");
    assert_eq!(env["payload"]["title"], "t");
    assert_eq!(env["payload"]["body"], "foo.wav");
    assert_eq!(env["payload"]["severity"], "info");
}

// Test 31: glob filter excludes non-matching files.
#[gpui::test]
async fn glob_filter_excludes_non_matching_files(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    cx.update(|cx| {
        runtime.reconcile(
            vec![cfg("a", TriggerKind::FileCreated, "*.wav")],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    fs.write(Path::new(&format!("{WATCHED}/foo.mp3")), b"x")
        .await
        .unwrap();
    cx.run_until_parked();
    drain_debounce(cx);

    assert_eq!(count_pending(bus.path(), "notification"), 0);
}

// Test 35: multiple [[emit]] blocks all fire on each match, in order.
#[gpui::test]
async fn multiple_emits_all_fire(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    let mut c = cfg("a", TriggerKind::FileCreated, "*");
    c.emits.push(WatcherEmit {
        kind: "notification".into(),
        payload: toml::from_str(
            r#"
            title = "second"
            body = "{filename}"
            severity = "warning"
        "#,
        )
        .unwrap(),
    });
    cx.update(|cx| {
        runtime.reconcile(
            vec![c],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    fs.write(Path::new(&format!("{WATCHED}/foo.wav")), b"x")
        .await
        .unwrap();
    cx.run_until_parked();
    drain_debounce(cx);

    let pending = read_pending(bus.path(), "notification");
    assert_eq!(pending.len(), 2, "both emits should fire — got {pending:?}");
    let titles: Vec<_> = pending
        .iter()
        .map(|p| p["payload"]["title"].as_str().unwrap_or(""))
        .collect();
    assert!(titles.contains(&"t") && titles.contains(&"second"));
}

// Test 36: unknown emit kind passes through to bus (event lands in
// events/<kind>/, no crash).
#[gpui::test]
async fn unknown_emit_kind_routed_to_own_subdir(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    let mut c = cfg("a", TriggerKind::FileCreated, "*");
    c.emits[0] = WatcherEmit {
        kind: "automation.trigger".into(),
        payload: toml::from_str(
            r#"
            automation_id = "bounce-verify"
        "#,
        )
        .unwrap(),
    };
    cx.update(|cx| {
        runtime.reconcile(
            vec![c],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    fs.write(Path::new(&format!("{WATCHED}/foo.wav")), b"x")
        .await
        .unwrap();
    cx.run_until_parked();
    drain_debounce(cx);

    assert_eq!(count_pending(bus.path(), "automation.trigger"), 1);
    assert_eq!(count_pending(bus.path(), "notification"), 0);
}

// Test 38: missing watched folder → status channel emits Error.
#[gpui::test]
async fn missing_watched_folder_reports_error(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = FakeFs::new(cx.executor()); // NOTE: no `/watched` dir created
    let mut runtime = WatcherRuntime::new();
    let rx = runtime.status_receiver();
    cx.update(|cx| {
        runtime.reconcile(
            vec![cfg("a", TriggerKind::Any, "*")],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    // Drain status messages — first should be Error("folder not found...").
    let mut saw_error = false;
    while let Ok(msg) = rx.try_recv() {
        if matches!(msg.1, WatcherStatus::Error(ref s) if s.contains("folder not found")) {
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "expected WatcherStatus::Error(folder not found)");
}

// Test 39: non-recursive — file in subdirectory produces zero emits.
#[gpui::test]
async fn subdirectory_files_ignored(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    fs.create_dir(Path::new("/watched/sub")).await.unwrap();
    let mut runtime = WatcherRuntime::new();
    cx.update(|cx| {
        runtime.reconcile(
            vec![cfg("a", TriggerKind::Any, "*")],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    fs.write(Path::new("/watched/sub/foo.wav"), b"x")
        .await
        .unwrap();
    cx.run_until_parked();
    drain_debounce(cx);

    assert_eq!(count_pending(bus.path(), "notification"), 0);
}

// Test 33: atomic-write sequence (.foo.tmp created → renamed to foo.wav)
// produces exactly one file_created emit for foo.wav. The dotfile
// hardcoded-ignore rule MUST suppress the .foo.tmp event.
#[gpui::test]
async fn atomic_write_sequence_emits_once_for_final(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    cx.update(|cx| {
        runtime.reconcile(
            vec![cfg("a", TriggerKind::FileCreated, "*.wav")],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    let tmp = Path::new("/watched/.foo.wav.tmp");
    let final_path = Path::new("/watched/foo.wav");
    fs.write(tmp, b"data").await.unwrap();
    cx.run_until_parked();
    fs.rename(tmp, final_path, Default::default())
        .await
        .unwrap();
    cx.run_until_parked();
    drain_debounce(cx);

    assert_eq!(count_pending(bus.path(), "notification"), 1);
}

// Test 30: trailing-edge debounce. Two events on the same path within
// `debounce_ms` collapse into ONE emit, and crucially that emit reports
// the LATEST file state (per spec: "the last event in the window fires").
// Three writes after the window → second emit, again with the latest data.
#[gpui::test]
async fn debounce_trailing_edge_collapses_to_latest(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let mut runtime = WatcherRuntime::new();
    let mut c = cfg("a", TriggerKind::Any, "*");
    c.trigger.debounce_ms = 200;
    // Make body include `{size_bytes}` so we can prove the trailing emit
    // sees the LATEST size.
    c.emits[0].payload = toml::from_str(
        r#"
        title = "t"
        body = "size {size_bytes}"
        severity = "info"
    "#,
    )
    .unwrap();
    cx.update(|cx| {
        runtime.reconcile(
            vec![c],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    let target = Path::new("/watched/foo.wav");
    fs.write(target, b"AA").await.unwrap(); // 2 bytes
    cx.run_until_parked();
    fs.write(target, b"BBBB").await.unwrap(); // 4 bytes — should win
    cx.run_until_parked();

    // Trailing-edge: nothing has fired yet — both deferred timers are
    // still waiting for the debounce window to close.
    let initial = count_pending(bus.path(), "notification");
    assert_eq!(
        initial, 0,
        "trailing-edge: nothing fires until window closes"
    );

    // Advance the clock past the debounce window. Both deferred tasks
    // wake up; the first sees its seq superseded and exits; the second
    // sees its seq is still latest, fires.
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();

    let after_window = read_pending(bus.path(), "notification");
    assert_eq!(
        after_window.len(),
        1,
        "exactly one emit per debounce window"
    );
    let body = after_window[0]["payload"]["body"].as_str().unwrap();
    assert!(
        body.contains("size 4"),
        "trailing-edge MUST report the LATEST write's size — got body {body:?}"
    );

    // Third write after the window — fresh debounce cycle, second emit
    // with the new size.
    fs.write(target, b"CCCCCCCC").await.unwrap(); // 8 bytes
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();

    let final_pending = read_pending(bus.path(), "notification");
    assert_eq!(
        final_pending.len(),
        2,
        "third write fires its own debounce cycle"
    );
    let last_body = final_pending[1]["payload"]["body"].as_str().unwrap();
    assert!(
        last_body.contains("size 8"),
        "second emit reports latest size — got {last_body:?}"
    );
}

// Test 34: file_deleted trigger fires on removal; min_size_mb does NOT
// apply (file is gone); size_mb expands to 0.0 in template.
#[gpui::test]
async fn file_deleted_trigger_fires_with_zero_size(cx: &mut TestAppContext) {
    let bus = TempDir::new().unwrap();
    let fs = fresh_fs(cx).await;
    let target = Path::new("/watched/foo.wav");
    fs.write(target, b"data").await.unwrap();
    cx.run_until_parked();

    let mut runtime = WatcherRuntime::new();
    let mut c = cfg("a", TriggerKind::FileDeleted, "*.wav");
    // min_size_mb large — would normally exclude small files, but must be
    // ignored on delete.
    c.trigger.min_size_mb = 1000.0;
    // Body shows {size_mb} — should expand to 0.0 on delete.
    c.emits[0].payload = toml::from_str(
        r#"
        title = "removed"
        body = "{filename} was {size_mb} MB"
        severity = "warning"
    "#,
    )
    .unwrap();
    cx.update(|cx| {
        runtime.reconcile(
            vec![c],
            fs.clone() as Arc<dyn Fs>,
            bus.path().to_path_buf(),
            cx,
        );
    });
    cx.run_until_parked();

    fs.remove_file(target, RemoveOptions::default())
        .await
        .unwrap();
    cx.run_until_parked();
    drain_debounce(cx);

    let pending = read_pending(bus.path(), "notification");
    assert_eq!(pending.len(), 1);
    let body = pending[0]["payload"]["body"].as_str().unwrap();
    assert!(
        body.contains("0.0"),
        "body should report size_mb=0.0, got {body}"
    );
}
