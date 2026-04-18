// Tests for `postprod_watchers`. Pure-function tests live here; tests that
// need an executor + FakeFs spawn live in `tests/runtime.rs` (integration
// tests at the crate level so they can use FakeFs without infecting the
// production build).

use super::*;
use postprod_dashboard_config::watcher_config::{
    TriggerKind, WatcherConfig, WatcherEmit, WatcherTrigger,
};

fn sample_cfg(id: &str) -> WatcherConfig {
    WatcherConfig {
        id: id.into(),
        label: id.into(),
        description: String::new(),
        path: "/tmp".into(),
        enabled: true,
        trigger: WatcherTrigger {
            on: TriggerKind::FileCreated,
            glob: "*".into(),
            min_size_mb: 0.0,
            debounce_ms: 500,
        },
        emits: vec![WatcherEmit {
            kind: "notification".into(),
            payload: toml::from_str(
                r#"
                title = "t"
                body = "b"
                severity = "info"
            "#,
            )
            .unwrap(),
        }],
    }
}

// Test 24: validate accepts valid config.
#[test]
fn validate_accepts_valid_config() {
    assert!(validate(&sample_cfg("ok")).is_ok());
}

// Test 25: validate rejects empty fields with actionable errors.
#[test]
fn validate_rejects_empty_id() {
    let mut cfg = sample_cfg("ok");
    cfg.id = String::new();
    assert!(matches!(validate(&cfg), Err(WatcherError::EmptyId)));
}

#[test]
fn validate_rejects_empty_path() {
    let mut cfg = sample_cfg("ok");
    cfg.path = String::new();
    assert!(matches!(validate(&cfg), Err(WatcherError::EmptyPath)));
}

#[test]
fn validate_rejects_empty_emits() {
    let mut cfg = sample_cfg("ok");
    cfg.emits.clear();
    assert!(matches!(validate(&cfg), Err(WatcherError::EmptyEmits)));
}

#[test]
fn validate_rejects_empty_emit_kind() {
    let mut cfg = sample_cfg("ok");
    cfg.emits[0].kind = String::new();
    assert!(matches!(
        validate(&cfg),
        Err(WatcherError::EmptyEmitKind { idx: 1 })
    ));
}

#[test]
fn validate_rejects_invalid_glob() {
    let mut cfg = sample_cfg("ok");
    cfg.trigger.glob = "[invalid".into();
    assert!(matches!(validate(&cfg), Err(WatcherError::InvalidGlob { .. })));
}

// Test 32: hardcoded ignore rules — dotfiles + suffix matches.
#[test]
fn ignore_rules_cover_all_documented_cases() {
    // Dotfile rules
    assert!(is_ignored_filename(".DS_Store"));
    assert!(is_ignored_filename(".foo.tmp"));
    assert!(is_ignored_filename(".swp"));
    assert!(is_ignored_filename(".anything"));

    // Suffix rules (no dot prefix)
    assert!(is_ignored_filename("foo.swp"));
    assert!(is_ignored_filename("foo.swo"));
    assert!(is_ignored_filename("foo.swn"));
    assert!(is_ignored_filename("foo.tmp"));
    assert!(is_ignored_filename("backup~"));

    // Real file passes
    assert!(!is_ignored_filename("foo.wav"));
    assert!(!is_ignored_filename("audio.mp3"));
    assert!(!is_ignored_filename("session.ptx"));
}

// Test 37: template expansion.
#[test]
fn template_vars_expand_correctly() {
    let vars = TemplateVars {
        path: "/Users/me/folder/foo.wav".into(),
        filename: "foo.wav".into(),
        stem: "foo".into(),
        ext: "wav".into(),
        size_bytes: 12_900_000,
        size_mb: 12.3,
        folder: "/Users/me/folder".into(),
        trigger: "created",
    };

    assert_eq!(expand_template("{filename}", &vars), "foo.wav");
    assert_eq!(expand_template("{stem}", &vars), "foo");
    assert_eq!(expand_template("{ext}", &vars), "wav");
    assert_eq!(expand_template("{size_mb}", &vars), "12.3");
    assert_eq!(expand_template("{size_bytes}", &vars), "12900000");
    assert_eq!(expand_template("{path}", &vars), "/Users/me/folder/foo.wav");
    assert_eq!(expand_template("{folder}", &vars), "/Users/me/folder");
    assert_eq!(expand_template("{trigger}", &vars), "created");

    // Composite + literal text + unknown.
    assert_eq!(
        expand_template("{filename} ({size_mb} MB) — {trigger}", &vars),
        "foo.wav (12.3 MB) — created"
    );
    assert_eq!(expand_template("{whatever}", &vars), "");
    // Standalone braces.
    assert_eq!(expand_template("no vars here", &vars), "no vars here");
    // Unterminated brace — pass through as literal.
    assert_eq!(expand_template("foo{bar", &vars), "foo{bar");
}

// Test 39b: PathEvent::kind = None falls back to "any-match"; Rescan
// always ignored.
#[test]
fn matches_trigger_handles_kind_none_and_rescan() {
    // None matches any configured trigger except Rescan.
    assert!(matches_trigger(TriggerKind::Any, None));
    assert!(matches_trigger(TriggerKind::FileCreated, None));
    assert!(matches_trigger(TriggerKind::FileModified, None));
    assert!(matches_trigger(TriggerKind::FileDeleted, None));

    // Rescan never matches.
    assert!(!matches_trigger(TriggerKind::Any, Some(PathEventKind::Rescan)));
    assert!(!matches_trigger(
        TriggerKind::FileCreated,
        Some(PathEventKind::Rescan)
    ));

    // Specific kinds match only their own trigger (or Any).
    assert!(matches_trigger(
        TriggerKind::FileCreated,
        Some(PathEventKind::Created)
    ));
    assert!(matches_trigger(TriggerKind::Any, Some(PathEventKind::Created)));
    assert!(!matches_trigger(
        TriggerKind::FileCreated,
        Some(PathEventKind::Removed)
    ));
}

// Test 36: unknown emit kind passes through to bus (no panic). Tested
// indirectly via bus tests; here we verify expand_payload preserves the
// kind-agnostic shape.
#[test]
fn expand_payload_preserves_unknown_kind_shape() {
    let vars = TemplateVars {
        path: "/p/foo.wav".into(),
        filename: "foo.wav".into(),
        stem: "foo".into(),
        ext: "wav".into(),
        size_bytes: 0,
        size_mb: 0.0,
        folder: "/p".into(),
        trigger: "created",
    };
    let payload: toml::Value = toml::from_str(
        r#"
        automation_id = "bounce-verify"
        params = { file = "{path}", note = "size {size_bytes}" }
    "#,
    )
    .unwrap();
    let expanded = expand_payload(&payload, &vars);
    let table = expanded.as_table().unwrap();
    assert_eq!(table["automation_id"].as_str(), Some("bounce-verify"));
    let params = table["params"].as_table().unwrap();
    assert_eq!(params["file"].as_str(), Some("/p/foo.wav"));
    assert_eq!(params["note"].as_str(), Some("size 0"));
}

// resolve_watched_path: tilde expansion, env-var expansion.
#[test]
fn resolve_watched_path_expands_tilde() {
    let resolved = resolve_watched_path("~/Downloads");
    if let Some(home) = dirs::home_dir() {
        assert_eq!(resolved, home.join("Downloads"));
    }
}

#[test]
fn resolve_watched_path_expands_env_vars() {
    // Use a deterministic var. SAFETY: tests use a unique var name; no
    // concurrent reads of POSTPROD_TEST_WATCH.
    unsafe {
        std::env::set_var("POSTPROD_TEST_WATCH", "/tmp/x");
    }
    let resolved = resolve_watched_path("$POSTPROD_TEST_WATCH/y");
    assert_eq!(resolved, PathBuf::from("/tmp/x/y"));
    let resolved2 = resolve_watched_path("${POSTPROD_TEST_WATCH}/z");
    assert_eq!(resolved2, PathBuf::from("/tmp/x/z"));
    unsafe {
        std::env::remove_var("POSTPROD_TEST_WATCH");
    }
}

#[test]
fn template_vars_for_event_computes_size_mb() {
    let vars = TemplateVars::for_event(
        Path::new("/p/foo.wav"),
        Path::new("/p"),
        PathEventKind::Created,
        12_900_000,
    );
    // 12,900,000 bytes / 1024 / 1024 = 12.302... → rounded to 12.3
    assert!((vars.size_mb - 12.3).abs() < 0.01, "got {}", vars.size_mb);
    assert_eq!(vars.filename, "foo.wav");
    assert_eq!(vars.stem, "foo");
    assert_eq!(vars.ext, "wav");
    assert_eq!(vars.folder, "/p");
    assert_eq!(vars.trigger, "created");
}
