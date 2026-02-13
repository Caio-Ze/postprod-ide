# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

ProTools Studio is a fork of Zed (a GPU-accelerated code editor) rebranded as a native macOS audio post-production IDE. It is a large Rust monorepo (~220 crates) built on the GPUI framework. The main binary is `protools-studio`, defined in `crates/zed/`.

## Build commands

```bash
# Build the main application
cargo build -p zed

# Run the application
cargo run -p zed

# Lint (use this instead of `cargo clippy` directly)
./script/clippy

# Lint a specific crate
./script/clippy -p <crate_name>

# Run all tests for a crate
cargo test -p <crate_name>

# Run a single test
cargo test -p <crate_name> -- <test_name>

# Format code
cargo fmt

# Check formatting
cargo fmt -- --check
```

## Toolchain

- Rust edition 2024, channel 1.93
- Clippy runs in `--release --all-targets --all-features -- --deny warnings` mode
- `./script/clippy` also runs `cargo-machete` (unused deps) and `typos` (spell check) when available locally

## Key crates

| Crate | Purpose |
|-------|---------|
| `gpui` | GPU-accelerated UI framework; provides entity system, rendering, concurrency, and input handling |
| `zed` | Main application binary — ties everything together |
| `editor` | Core text editor and input fields; LSP display layer (inlay hints, completions) |
| `workspace` | Session management, pane/dock layout, state serialization |
| `project` | File tree, project management, LSP communication |
| `ui` | Reusable UI components and patterns |
| `language` | Syntax highlighting, symbol navigation, language support |
| `lsp` | Language Server Protocol client |
| `collab` | Collaboration server |
| `rpc` | Collaboration protocol message definitions |
| `theme` | Theme system and default themes |
| `dashboard` | Dashboard panel view |
| `agent` / `agent_ui` | AI agent logic and UI |
| `db` | SQLite database layer (sqlez) |
| `settings` | Settings management and JSON schema |

## Clippy disallowed methods

These are enforced via `clippy.toml`:
- Use `smol::process::Command` instead of `std::process::Command` (avoids blocking)
- Use `gpui::BackgroundExecutor::timer` instead of `smol::Timer::after` (deterministic tests)
- Use `serde_json::from_slice` instead of `serde_json::from_reader` (performance)
- Use `ns_string()` helper instead of `NSString::alloc` (avoids memory leaks)

# Rust coding guidelines

* Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
* Do not write organizational or comments that summarize the code. Comments should only be written in order to explain "why" the code is written in some way in the case there is a reason that is tricky / non-obvious.
* Prefer implementing functionality in existing files unless it is a new logical component. Avoid creating many small files.
* Avoid using functions that panic like `unwrap()`, instead use mechanisms like `?` to propagate errors.
* Be careful with operations like indexing which may panic if the indexes are out of bounds.
* Never silently discard errors with `let _ =` on fallible operations. Always handle errors appropriately:
  - Propagate errors with `?` when the calling function should handle them
  - Use `.log_err()` or similar when you need to ignore errors but want visibility
  - Use explicit error handling with `match` or `if let Err(...)` when you need custom logic
  - Example: avoid `let _ = client.request(...).await?;` - use `client.request(...).await?;` instead
* When implementing async operations that may fail, ensure errors propagate to the UI layer so users get meaningful feedback.
* Never create files with `mod.rs` paths - prefer `src/some_module.rs` instead of `src/some_module/mod.rs`.
* When creating new crates, prefer specifying the library root path in `Cargo.toml` using `[lib] path = "...rs"` instead of the default `lib.rs`, to maintain consistent and descriptive naming (e.g., `gpui.rs` or `main.rs`).
* Avoid creative additions unless explicitly requested
* Use full words for variable names (no abbreviations like "q" for "queue")
* Use variable shadowing to scope clones in async contexts for clarity, minimizing the lifetime of borrowed references.
  Example:
  ```rust
  executor.spawn({
      let task_ran = task_ran.clone();
      async move {
          *task_ran.borrow_mut() = true;
      }
  });
  ```

# Timers in tests

* In GPUI tests, prefer GPUI executor timers over `smol::Timer::after(...)` when you need timeouts, delays, or to drive `run_until_parked()`:
  - Use `cx.background_executor().timer(duration).await` (or `cx.background_executor.timer(duration).await` in `TestAppContext`) so the work is scheduled on GPUI's dispatcher.
  - Avoid `smol::Timer::after(...)` for test timeouts when you rely on `run_until_parked()`, because it may not be tracked by GPUI's scheduler and can lead to "nothing left to run" when pumping.

# GPUI

GPUI is a UI framework which also provides primitives for state and concurrency management.

## Context

Context types allow interaction with global state, windows, entities, and system services. They are typically passed to functions as the argument named `cx`. When a function takes callbacks they come after the `cx` parameter.

* `App` is the root context type, providing access to global state and read and update of entities.
* `Context<T>` is provided when updating an `Entity<T>`. This context dereferences into `App`, so functions which take `&App` can also take `&Context<T>`.
* `AsyncApp` and `AsyncWindowContext` are provided by `cx.spawn` and `cx.spawn_in`. These can be held across await points.

## `Window`

`Window` provides access to the state of an application window. It is passed to functions as an argument named `window` and comes before `cx` when present. It is used for managing focus, dispatching actions, directly drawing, getting user input state, etc.

## Entities

An `Entity<T>` is a handle to state of type `T`. With `thing: Entity<T>`:

* `thing.entity_id()` returns `EntityId`
* `thing.downgrade()` returns `WeakEntity<T>`
* `thing.read(cx: &App)` returns `&T`.
* `thing.read_with(cx, |thing: &T, cx: &App| ...)` returns the closure's return value.
* `thing.update(cx, |thing: &mut T, cx: &mut Context<T>| ...)` allows the closure to mutate the state, and provides a `Context<T>` for interacting with the entity. It returns the closure's return value.
* `thing.update_in(cx, |thing: &mut T, window: &mut Window, cx: &mut Context<T>| ...)` takes a `AsyncWindowContext` or `VisualTestContext`. It's the same as `update` while also providing the `Window`.

Within the closures, the inner `cx` provided to the closure must be used instead of the outer `cx` to avoid issues with multiple borrows.

Trying to update an entity while it's already being updated must be avoided as this will cause a panic.

When  `read_with`, `update`, or `update_in` are used with an async context, the closure's return value is wrapped in an `anyhow::Result`.

`WeakEntity<T>` is a weak handle. It has `read_with`, `update`, and `update_in` methods that work the same, but always return an `anyhow::Result` so that they can fail if the entity no longer exists. This can be useful to avoid memory leaks - if entities have mutually recursive handles to each other they will never be dropped.

## Concurrency

All use of entities and UI rendering occurs on a single foreground thread.

`cx.spawn(async move |cx| ...)` runs an async closure on the foreground thread. Within the closure, `cx` is `&mut AsyncApp`.

When the outer cx is a `Context<T>`, the use of `spawn` instead looks like `cx.spawn(async move |this, cx| ...)`, where `this: WeakEntity<T>` and `cx: &mut AsyncApp`.

To do work on other threads, `cx.background_spawn(async move { ... })` is used. Often this background task is awaited on by a foreground task which uses the results to update state.

Both `cx.spawn` and `cx.background_spawn` return a `Task<R>`, which is a future that can be awaited upon. If this task is dropped, then its work is cancelled. To prevent this one of the following must be done:

* Awaiting the task in some other async context.
* Detaching the task via `task.detach()` or `task.detach_and_log_err(cx)`, allowing it to run indefinitely.
* Storing the task in a field, if the work should be halted when the struct is dropped.

A task which doesn't do anything but provide a value can be created with `Task::ready(value)`.

## Elements

The `Render` trait is used to render some state into an element tree that is laid out using flexbox layout. An `Entity<T>` where `T` implements `Render` is sometimes called a "view".

Example:

```
struct TextWithBorder(SharedString);

impl Render for TextWithBorder {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().border_1().child(self.0.clone())
    }
}
```

Since `impl IntoElement for SharedString` exists, it can be used as an argument to `child`. `SharedString` is used to avoid copying strings, and is either an `&'static str` or `Arc<str>`.

UI components that are constructed just to be turned into elements can instead implement the `RenderOnce` trait, which is similar to `Render`, but its `render` method takes ownership of `self` and receives `&mut App` instead of `&mut Context<Self>`. Types that implement this trait can use `#[derive(IntoElement)]` to use them directly as children.

The style methods on elements are similar to those used by Tailwind CSS.

If some attributes or children of an element tree are conditional, `.when(condition, |this| ...)` can be used to run the closure only when `condition` is true. Similarly, `.when_some(option, |this, value| ...)` runs the closure when the `Option` has a value.

## Input events

Input event handlers can be registered on an element via methods like `.on_click(|event, window, cx: &mut App| ...)`.

Often event handlers will want to update the entity that's in the current `Context<T>`. The `cx.listener` method provides this - its use looks like `.on_click(cx.listener(|this: &mut T, event, window, cx: &mut Context<T>| ...)`.

## Actions

Actions are dispatched via user keyboard interaction or in code via `window.dispatch_action(SomeAction.boxed_clone(), cx)` or `focus_handle.dispatch_action(&SomeAction, window, cx)`.

Actions with no data defined with the `actions!(some_namespace, [SomeAction, AnotherAction])` macro call. Otherwise the `Action` derive macro is used. Doc comments on actions are displayed to the user.

Action handlers can be registered on an element via the event handler `.on_action(|action, window, cx| ...)`. Like other event handlers, this is often used with `cx.listener`.

## Notify

When a view's state has changed in a way that may affect its rendering, it should call `cx.notify()`. This will cause the view to be rerendered. It will also cause any observe callbacks registered for the entity with `cx.observe` to be called.

## Entity events

While updating an entity (`cx: Context<T>`), it can emit an event using `cx.emit(event)`. Entities register which events they can emit by declaring `impl EventEmitter<EventType> for EntityType {}`.

Other entities can then register a callback to handle these events by doing `cx.subscribe(other_entity, |this, other_entity, event, cx| ...)`. This will return a `Subscription` which deregisters the callback when dropped.  Typically `cx.subscribe` happens when creating a new entity and the subscriptions are stored in a `_subscriptions: Vec<Subscription>` field.

## Build guidelines

- Use `./script/clippy` instead of `cargo clippy`

## PTSL Agent Tools

ProTools Studio includes 31 CLI tools (in the sibling `PROTOOLS_SDK_PTSL` repo) that control Pro Tools via gRPC. Full reference: `~/.claude/skills/ptsl-tools/SKILL.md`

**Binary path:** `~/Documents/Rust_projects/PROTOOLS_SDK_PTSL/target/debug/`
**gRPC endpoint:** `http://[::1]:31416` (PTSL protocol v2025.6.0)

### Quick reference (key tools)

| Tool | Purpose |
|------|---------|
| `agent-manage-tracks list/check/markers/solo/hide/inactivate/create/select-clips` | Track management |
| `agent-import-tracks` | Import tracks from source session (imports ALL — use `solo` after) |
| `agent-import-trilha` | Import TRILHA track from source |
| `agent-bounce-export` | Bounce current session to WAV/MP3 |
| `agent-export-loc` | Export & consolidate a track |
| `agent-mute-solo` | Mute/unmute/solo/unsolo tracks |
| `agent-track-volume` | Set track volume in dB (SET only; GET not yet supported) |
| `agent-transport` | Play/stop/status |
| `agent-timeline-selection` | Get/set timeline in/out points |
| `agent-rename-track` / `agent-rename-clip` | Rename tracks or clips |
| `agent-delete-tracks` | Delete tracks from session |
| `agent-save-session-as` | Save session under new name |
| `agent-get-clip-list` | List all clips in session |
| `agent-create-markers` / `agent-copy-markers` | Marker management |
| `agent-import-audio` / `agent-spot-clip` | Import audio to clip list and spot to timeline |
| `agent-apply-audio-filter` | FFmpeg audio filter chain |
| `agent-bounce-normalize-tv` | LUFS normalization (_NET/_TV) |
| `agent-convert-mp3` | WAV to MP3 conversion |
| `agent-maximize-audio` | Peak normalization |
| `agent-transcribe-audio` / `agent-extract-text` / `agent-compare-texts` | Transcription & comparison (requires GROQ_API_KEY) |

### Rules

1. Always use `--output-json` on every tool invocation
2. Always use absolute paths with quoted spaces
3. For subcommand tools, `--output-json` must come BEFORE the subcommand
4. Check `"success": true` in JSON output before proceeding
5. Always verify after write operations with `list`
6. Add `sleep 3-5` between sessions in batch operations
