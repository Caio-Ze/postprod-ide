# Zed Fork Plan — ProTools Studio

> **Created**: 2026-02-08
>
> Fork Zed into a branded audio post-production app. Keep EVERYTHING —
> file explorer, terminal, AI chat, code editor, dev tools. Add a dashboard
> panel for audio workflows. The app develops itself.

---

## Progress Log

| Date | Week | What was done |
|------|------|---------------|
| 2026-02-08 | Week 1 | **DONE** — Fork, build, rebrand to "ProTools Studio", disable telemetry/auto-update/auth, bundle IDs, data dir |
| 2026-02-08 | Week 2 | **DONE** — Dashboard crate (`crates/dashboard/`), 16 tool cards in 3 categories, pinned tab, clean project panel empty state |
| 2026-02-08 | Week 3 | **DONE** — Default workspace folders, session polling, pasta ativa, delivery status, window title |

---

## What We're Building

A native macOS application called **ProTools Studio** (working name) that:

- Is a full Zed fork — branded, but keeps all capabilities
- Opens with a central **dashboard panel** (tool cards, session status, delivery overview)
- Has Zed's file explorer, terminal, AI chat — untouched
- **Keeps the code editor and dev tools** — the app can edit and rebuild itself
- Spawns the same 21+ runtime/ binaries via terminal
- Uses Gemini free tier for AI chat (already built into Zed)
- Is open source (GPL, inherits from Zed)
- Runtime binaries are separate executables (not GPL, your IP)

---

## The Key Insight: Keep Everything

The original plan was to strip 60% of Zed — remove the editor, LSP, debugger, git, etc.

**New approach: keep it all.**

Why:

1. **Self-updating app** — Open the app's own source code inside the app. Edit a tool,
   `cargo build`, new binary. The software updates itself from within.

2. **AI-assisted development** — Zed's agent reads your Rust crates, understands the code,
   writes improvements, runs tests. You develop the app inside the app.

3. **Dramatically less fork work** — No stripping, no broken dependencies, no compilation
   fixes. Just add a dashboard crate and rebrand.

4. **Upstream tracking becomes possible** — Since changes are purely additive (new crate +
   branding), merging Zed updates is straightforward. You're not fighting deleted code.

5. **Power users can hack** — Audio engineers who want to customize tools, write scripts,
   or create new automations have a full dev environment. The barrier between "user" and
   "contributor" disappears.

6. **New paradigm** — The product is also its own development environment. This doesn't
   really exist yet. Software that ships with the ability to modify and rebuild itself,
   with an AI assistant that understands the codebase.

### What the user sees

- **Default experience:** Dashboard + file explorer + terminal + AI chat. Looks like an
  audio app. The editor, git panel, debugger are there but not in the default layout.
  Audio professionals never notice them.

- **Power user experience:** Open a `.rs` file → full syntax highlighting, LSP, code
  actions. Open the terminal → `cargo build`. The app IS the IDE for the app.

- **Developer experience:** Fork the fork. Add your own tools. The entire platform is
  open source and hackable.

---

## What We Keep vs Build

### Keep (everything from Zed)

| Component | Why Keep |
|-----------|---------|
| File explorer (project_panel) | Users browse session folders |
| Terminal | Tools run here, build commands here |
| AI Agent panel | Gemini/Claude chat, rules, MCP, slash commands |
| **Code editor** | **Edit tool source, configs, scripts — self-updating** |
| **Language servers (LSP)** | **Rust-analyzer for editing Rust tools** |
| **Git integration** | **Version control for tool modifications** |
| **Syntax highlighting** | **Rust, TOML, Markdown, shell scripts** |
| **Search & replace** | **Find across project, refactoring** |
| **Extensions (WASM host)** | **Community extensions still work** |
| Theme system | Dark theme, customize later |
| Settings system | User preferences |
| Workspace/project management | Open folders, recent projects |
| Window management, docks, panes | Layout infrastructure |
| GPUI framework | Build our custom dashboard |

### Remove (minimal — only telemetry and branding)

| Component | Why Remove |
|-----------|-----------|
| Telemetry | Privacy — no tracking |
| Auto-update (Zed's) | We control our own updates |
| Zed account/sign-in | Not needed |
| Collaboration (collab, call) | Not needed for v1 (could re-enable later) |
| Welcome page | Replace with our onboarding |

### Build (2 new crates)

| Component | What It Does | Complexity |
|-----------|-------------|------------|
| **`crates/dashboard/`** | Central panel: tool cards, session status, delivery overview, pasta ativa | Medium-High |
| **`crates/onboarding/`** | First-run: quarantine handling, folder setup, AI config | Low |
| **Branding** | Icon, name, splash, about dialog | Low (asset swap) |

**That's it.** Two new crates and a rebrand. Everything else is Zed.

---

## The Dashboard (Center Panel)

The dashboard is a GPUI panel that opens by default in the center dock. Users can switch
between dashboard and editor tabs like any Zed pane. Double-click a file → editor opens.
Click dashboard tab → back to tool cards.

```
┌──────────────────────────────────────────────────────────────────────┐
│  ProTools Studio                              CRF_1047  ▶ Connected  │
├──────────┬───────────────────────────────────────────┬───────────────┤
│          │                                           │               │
│ ARQUIVOS │  ┌─ Pro Tools ──────────────────────────┐ │  AI CHAT      │
│          │  │                                      │ │               │
│ 📁 1_Ses │  │  ╭──────────╮ ╭──────────╮          │ │  Fale o que   │
│ 📁 2_Imp │  │  │ ▶ BOUNCE │ │ ▶ IMPORT │          │ │  precisa...   │
│ 📁 3_Pro │  │  │  TV+NET  │ │  & SPOT  │          │ │               │
│ 📁 4_Fin │  │  │  +SPOT   │ │  CLIPS   │          │ │               │
│ 📁 5_Arq │  │  ╰──────────╯ ╰──────────╯          │ │               │
│          │  │  ╭──────────╮ ╭──────────╮          │ │               │
│ ────────── │  │ ▶ SFX    │ │ ▶ VOICE  │          │ │               │
│ 📌 Bounce│  │  │  GEMINI  │ │   QC     │          │ │               │
│          │  │  ╰──────────╯ ╰──────────╯          │ │               │
│          │  │  ╭──────────╮ ╭──────────╮          │ │               │
│          │  │  │ ▶ TV →   │ │ ▶ BATCH  │          │ │               │
│          │  │  │   SPOT   │ │ PROCESS  │          │ │               │
│          │  │  ╰──────────╯ ╰──────────╯          │ │               │
│          │  │                                      │ │               │
│          │  │  Sessão: CRF_1047_SPOT_V3            │ │               │
│          │  │  Pasta ativa: Bounced Files/          │ │               │
│          │  └──────────────────────────────────────┘ │               │
│          │                                           │               │
│          │  ┌─ Áudio ─────────────────────────────┐ │               │
│          │  │  ╭────────╮ ╭────────╮ ╭────────╮   │ │               │
│          │  │  │Normaliz│ │Maximiz │ │MP3↔WAV │   │ │               │
│          │  │  │ -23dB  │ │  0dBFS │ │ 320kbps│   │ │               │
│          │  │  ╰────────╯ ╰────────╯ ╰────────╯   │ │               │
│          │  └──────────────────────────────────────┘ │               │
│          │                                           │               │
│          │  ┌─ Entrega ───────────────────────────┐ │               │
│          │  │  TV/ 4 ✅  NET/ 4 ✅                 │ │               │
│          │  │  SPOT/ 3 ⚠️  MP3/ 4 ✅                │ │               │
│          │  │  ⚠️ Falta: CRF_1047_CENA4_SPOT.wav   │ │               │
│          │  └──────────────────────────────────────┘ │               │
├──────────┴───────────────────────────────────────────┴───────────────┤
│ Terminal                                                             │
│ > cargo build --release --bin audio-normalizer-configurable          │
│   Compiling audio-normalizer v0.1.0                                  │
│   Finished release [optimized] in 4.2s                               │
│ > _                                                                  │
└──────────────────────────────────────────────────────────────────────┘
```

The terminal shows a `cargo build` — because this app can rebuild its own tools.

---

## Self-Updating App: How It Works

```
┌──────────────────────────────────────────────────────────────┐
│                    ProTools Studio                             │
│                                                               │
│  1. User notices a bug in the normalize tool                  │
│     or wants to change the default LUFS target                │
│                                                               │
│  2. Opens bins/audio-normalizer/src/main.rs                   │
│     → Full editor with Rust syntax, LSP, completions          │
│                                                               │
│  3. Asks AI: "change the default TV target to -24 LUFS"       │
│     → Agent edits the code, shows the diff                    │
│                                                               │
│  4. Terminal: cargo build --release --bin audio-normalizer     │
│     → New binary in seconds                                   │
│                                                               │
│  5. Copy to runtime/: cp target/release/audio-normalizer      │
│        runtime/bin/Bounce/bin/audio-normalizer-configurable    │
│                                                               │
│  6. Click "Normalize" on dashboard → runs updated tool        │
│                                                               │
│  The software just updated itself. From within itself.        │
│  With AI assistance. In under a minute.                       │
└──────────────────────────────────────────────────────────────┘
```

This also means:
- **Bug reports become PRs** — "I found a bug" → AI fixes it → rebuild → done
- **Custom tools** — power users write new Rust tools, add them to the dashboard
- **No release cycle friction** — fix something, rebuild, keep working
- **The AI understands the codebase** — Zed's agent can read every .rs file, every
  Cargo.toml, every config. It's the most context-aware development environment possible.

---

## Learning GPUI

GPUI is Zed's custom UI framework (Apache 2.0). Documentation is sparse, but the learning
strategy is clear: read existing Zed panels and copy their patterns.

### Study These Panels (in order)

1. **`crates/welcome/`** — standalone view with buttons and layout. Closest to our
   dashboard. Study its layout patterns first.

2. **`crates/project_panel/`** — dockable panel, file tree with click handlers. Shows
   panel registration and interactions.

3. **`crates/terminal_panel/`** — spawns processes, displays output. Shows how panels
   interact with the terminal system.

4. **`crates/agent/`** — AI chat panel. Shows text input, message rendering, streaming
   responses, tool integration.

### GPUI Core Concepts

```rust
// A GPUI component is a Rust struct that implements Render
struct ToolCard {
    label: String,
    binary: String,
    cwd: String,
    icon: Icon,
}

impl Render for ToolCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_4()
            .rounded_lg()
            .bg(cx.theme().colors().surface_background)
            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                // Spawn binary in terminal
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(this.icon))
                    .child(Label::new(this.label.clone()))
            )
    }
}
```

Claude Code reads the existing panel code and generates GPUI components following
the same patterns. This is where AI-assisted development dramatically speeds things up.

### Resources

- `crates/gpui/README.md` — framework overview
- `crates/ui/` — Zed's UI component library (buttons, labels, icons, lists)
- `crates/component/` — higher-level components
- https://www.gpui.rs/ — GPUI standalone docs (minimal)
- https://github.com/zed-industries/create-gpui-app — standalone GPUI app template

---

## Week-by-Week Plan

### Week 1: Fork, Build, Rebrand — DONE

**Goal:** Zed fork compiles, runs, and says "ProTools Studio" instead of "Zed."

**What was implemented:**
- Forked zed-industries/zed, built from source on macOS (Xcode 16.4, cmake 4.2.3, Rust 1.93.0)
- Rebranded: Cargo.toml (package name/bin/bundle IDs), release_channel, paths, app_menus, main.rs, zed_actions
- Bundle ID: `com.caio-ze.protools-studio.*`
- Data dir: `~/Library/Application Support/ProTools Studio`
- Telemetry: disabled (send_event is no-op, flush_events writes local log only, no network calls)
- Auto-update: disabled (poll_for_updates always returns false)
- Auth: disabled (authenticate() is no-op)
- Binary: `target/release/protools-studio` (~347MB release build)

**Deliverable:** "ProTools Studio" app launches on macOS. Full Zed under the hood. New branding. No telemetry.

### Week 2: Dashboard Panel — DONE

**Goal:** Central dashboard panel with tool cards that spawn binaries.

**What was implemented:**
- `crates/dashboard/` — single-file crate (`src/dashboard.rs`) registered as center-pane Item
- 16 ToolCards in 3 categories: Pro Tools (8), Audio (5), File (3)
- `TOOLS` const array with id, label, description, icon, binary path, cwd, category
- `spawn_tool()` uses `SpawnInTerminal` to run binaries in Zed's terminal
- Dashboard pinned as first tab (can't be closed with Cmd+W)
- `show_dashboard()` called from main.rs on non-first-launch startup
- Help menu has "Show Dashboard" action
- `PROTOOLS_RUNTIME_PATH` env var with fallback to hardcoded dev path

**File structure** (actual — single file, not multiple):
```
crates/dashboard/
├── Cargo.toml
└── src/
    └── dashboard.rs    # Everything: init, tool registry, Dashboard struct, render, Item impl
```

**Deliverable:** Dashboard with 16 clickable tool cards. Clicking a card runs the tool in the terminal.

### Week 3: Context, Status, Pasta Ativa — DONE

**Goal:** Session awareness, active folder, delivery status, workspace folder management.

**What was implemented:**
- **Default workspace folders**: `~/ProTools_Suite/` with 5 numbered subdirs created on startup, opened as worktree root (project panel shows folder tree)
- **Session polling**: Background task runs `get_session_path` binary every 5s, updates dashboard header with green/gray connection dot
- **Pasta ativa**: Read/write `~/ProTools_Suite/.pasta_ativa`, clickable row in dashboard opens folder picker, standalone tools use it as cwd
- **Delivery status**: Scans `~/ProTools_Suite/4_Finalizados/` every 15s, shows TV/NET/SPOT/MP3 counts with warnings for missing/mismatched formats
- **Window title**: `ProToolsSessionName` GPUI Global set by poll task, read by `update_window_title` — shows "ProTools Studio" or "ProTools Studio — Session.ptx"

**Files changed:**
- `crates/dashboard/src/dashboard.rs` — session polling, pasta ativa, delivery scan, new UI widgets
- `crates/dashboard/Cargo.toml` — added `util` dependency
- `crates/zed/src/main.rs` — create `~/ProTools_Suite/` folders, open as default worktree
- `crates/workspace/src/workspace.rs` — window title with ProToolsSessionName Global

**Deliverable:** Context-aware dashboard. Tools know the active folder and session. Delivery status at a glance. Folders in sidebar.

### Week 4: AI, Onboarding, Package

**Goal:** AI configured for audio, first-run experience, distributable app.

| Day | Task | With Claude Code |
|-----|------|-----------------|
| 1 | AI context: GEMINI.md / .rules file with full tool reference. Test Gemini free tier. | Write rules, test AI tool-running |
| 2 | Onboarding: first-run quarantine handling (xattr -cr runtime/), workspace folder setup. | Modify or replace welcome crate |
| 3 | Bundle runtime/ binaries alongside app. Ensure relative paths resolve from .app bundle. | macOS bundle structure |
| 4 | Package as .dmg or .zip. Test fresh install on clean user account. | Packaging + integration test |
| 5 | Buffer: fix bugs, polish dashboard, test all tools + AI chat + self-rebuild flow. | End-to-end testing |

**Deliverable:** Distributable ProTools Studio.app. Dashboard, AI chat, self-updating, everything works.

---

## What Claude Code Does For You

| Task | Without AI | With Claude Code |
|------|-----------|-----------------|
| Understand Zed's 216-crate architecture | Days of reading | "Explain how project_panel registers itself" → instant |
| Learn GPUI patterns | Weeks of sparse docs | "Build a clickable card component like project_panel does" → working code |
| Write dashboard panel | Days per widget | "Build a tool card grid following Zed's UI patterns" → generates it |
| Wire terminal integration | Hours of reading | "How does Zed spawn a process in terminal? Do the same for my tool" → done |
| Fix build after removing collab | Trial and error | "Remove the collab crate and fix all compilation errors" → minutes |
| Fix macOS bundling | Stack Overflow | "Binary can't find runtime/ from .app bundle, fix path resolution" → fix |

The pattern: Claude reads Zed's source, understands the patterns, generates new code following those same patterns. "Learn a 1M-line codebase" becomes "ask Claude about the specific 500 lines you need."

---

## Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| GPUI too hard to learn | Claude reads the source and generates components. Start with welcome crate (simplest). |
| Removing collab/telemetry breaks build | Small, targeted removals. Compile after each one. Much less risky than stripping 60%. |
| Runtime binaries can't find deps | Keep exact same runtime/ structure. Resolve paths relative to app bundle. |
| AI chat not useful for audio | Write a good rules file. Test and iterate. This is markdown, not code. |
| macOS app signing | Skip for now. xattr -cr like before. Sign when Avid partnership happens. |
| Upstream Zed gets amazing feature | Since changes are additive, you CAN merge upstream if you want. Not locked out. |
| Project takes longer than 4 weeks | VS Code extension already ships. This is v2, not blocking. |

---

## What Ships vs What Exists

| Feature | VS Code Extension (v1) | ProTools Studio (v2) |
|---------|----------------------|---------------------|
| UI | Sidebar tree view (hack) | Native GPUI dashboard (designed) |
| AI chat | Needs Antigravity | Built in (Gemini free) |
| Performance | Electron (300MB RAM) | Native Rust/Metal (~50MB RAM) |
| Branding | "Extension by caio-ze" | "ProTools Studio" standalone app |
| Distribution | .vsix inside a .zip | .app or .dmg |
| Avid pitch | "VS Code extension" | "Standalone application" |
| Code language | TypeScript + Rust | 100% Rust |
| Self-updating | No | Yes — edit source, rebuild, run |
| Code editor | Hidden (it's VS Code, but user doesn't use it) | Available — double-click any .rs file |
| Developer tools | Hidden | Available — git, LSP, debugger, search |
| Extensible | VS Code extensions | Zed extensions + source modification |
| Open source | Extension: MIT | App: GPL, Runtime: proprietary |
| Status | Done, shipping to clients | v2, parallel development |

---

## The Self-Updating Paradigm

This is the genuinely new thing. No other audio tool does this:

```
Traditional software:
  Developer writes code → builds → ships update → user installs → weeks/months

ProTools Studio:
  User (or AI) edits code INSIDE the app → builds → runs → seconds

  The app contains:
  ├── Its own source code (bins/)
  ├── A full code editor with LSP (Zed's editor)
  ├── A terminal for building (cargo build)
  ├── An AI that understands the codebase (Zed's agent)
  └── The runtime binaries it builds (runtime/)
```

**For audio professionals:** They never touch the editor. They use the dashboard and AI chat.

**For power users:** They open a .rs file, ask the AI to change something, rebuild. No separate IDE needed.

**For the developer (you):** You develop the app inside the app. Claude Code in the terminal, Zed's agent in the chat panel, Rust-analyzer in the editor. It's the most natural workflow possible for a Rust developer.

**For Avid:** "This is a self-evolving audio automation platform. The community can contribute tools. The AI can modify and improve the software. It's the future of audio software development."

---

## File Structure (actual)

```
protools-studio/                       # Forked from zed-industries/zed
├── crates/
│   ├── gpui/                          # Zed's UI framework
│   ├── ui/                            # Component library
│   ├── editor/                        # Code editor (KEPT — self-updating)
│   ├── workspace/                     # Window/pane management (modified: window title)
│   ├── zed/                           # Main app crate (modified: rebrand, startup, folders)
│   ├── ... (all other Zed crates)     # Untouched
│   │
│   └── dashboard/                     # NEW — audio tool dashboard
│       ├── Cargo.toml                 # deps: gpui, task, ui, workspace, util
│       └── src/
│           └── dashboard.rs           # Everything in one file:
│                                      #   - init(), show_dashboard()
│                                      #   - TOOLS const (16 tool cards)
│                                      #   - Dashboard struct (session polling, pasta ativa, delivery)
│                                      #   - ProToolsSessionName global
│                                      #   - Render impl with all widgets
│                                      #   - Item impl (center-pane tab)
│
├── docs/
│   └── ZED_FORK_PLAN.md              # This file
│
├── Cargo.toml                         # Workspace (Zed + dashboard)
└── README.md
```

**Runtime binaries** live in PROTOOLS_SDK_PTSL repo (`target/runtime/`), not in this repo. Path set via `PROTOOLS_RUNTIME_PATH` env var.

**User workspace** created at `~/ProTools_Suite/` on startup:
```
~/ProTools_Suite/
├── 1_Sessões/
├── 2_Imports/
├── 3_Processamento/
├── 4_Finalizados/
├── 5_Arquivo/
└── .pasta_ativa                       # Active folder path (single line)
```

---

## Timeline Summary

| Week | What | Deliverable |
|------|------|------------|
| 1 | Fork + rebrand + minimal cleanup | App launches as "ProTools Studio" with full Zed capabilities |
| 2 | Build dashboard panel with tool cards | 16+ clickable tools, spawns binaries in terminal |
| 3 | Context: session status, pasta ativa, delivery, folder management | Dashboard knows the session, active folder, delivery state |
| 4 | AI config + onboarding + packaging | Rules file, first-run, .app bundle, ready to distribute |

**Total: 4 weeks of focused work with Claude Code.**

VS Code extension keeps shipping to existing clients. This is the v2 track.

---

## Why This Works

1. **You write Rust daily.** Zed is Rust. No language barrier.
2. **Claude Code reads Zed's source.** You don't need to understand 1M lines — Claude does.
3. **You're adding, not subtracting.** 2 new crates + branding. No broken dependencies.
4. **Everything is free.** File explorer, terminal, AI chat, editor, LSP, git — all kept.
5. **Your tools already work.** 21 binaries, battle-tested. The app is a new shell.
6. **Upstream tracking is possible.** Additive changes merge cleanly.
7. **GPL is fine.** App is open source. Runtime binaries are separate executables.
8. **Self-updating is the killer feature.** No other audio tool can modify and rebuild itself.
