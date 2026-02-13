# ProTools Studio

A native macOS audio post-production IDE built on top of [Zed](https://github.com/zed-industries/zed). ProTools Studio keeps everything Zed offers — a GPU-accelerated editor, integrated terminal, AI chat, LSP, and git — and adds a dashboard purpose-built for audio workflows with Avid Pro Tools.

## What makes this different

Most Pro Tools automation tools are either proprietary plugins, fragile AppleScript wrappers, or scattered shell scripts. ProTools Studio takes a different approach:

- **Native gRPC control** — 31 CLI tools communicate directly with Pro Tools via the PTSL (Pro Tools Scripting Language) protocol over gRPC. No GUI scripting, no accessibility hacks.
- **System-wide hotkeys** — Global keyboard shortcuts (via macOS CGEventTap) trigger tools even when Pro Tools is the foreground app. Press a key combo from your DAW and the tool runs.
- **AI agent integration** — One-click automations that dispatch multi-step prompts to Claude Code or Gemini CLI. Agents can chain multiple PTSL tools together for complex workflows (import tracks, solo, bounce, normalize, rename — all from a single prompt).
- **Self-improving skills** — Agent skill files are shared across Claude and Gemini via symlinks to a single canonical source. Agents can update their own documentation as they discover new patterns.
- **TOML-driven automations** — Add or edit automation prompts in a TOML file; the dashboard picks up changes every 30 seconds. No recompilation needed.
- **The IDE edits its own tools** — The Zed editor, terminal, and PTSL tool source code all live in the same workspace. You can edit a tool binary's Rust source, rebuild, and test it without leaving the app.

## Current state

This is an active development project, not a production release. It works on macOS (Apple Silicon and Intel) and has been used daily for real audio post-production work. That said:

- **macOS only** — The global hotkey system and Pro Tools integration are macOS-specific.
- **Requires Pro Tools running locally** — The PTSL gRPC endpoint (`[::1]:31416`) must be available. Without it, the dashboard still opens but session-aware tools won't connect.
- **Runtime binaries not bundled** — Tool binaries live in the companion [PROTOOLS_SDK_PTSL](https://github.com/Caio-Ze/PROTOOLS_SDK_PTSL) repo and are resolved via environment variables. A fresh install requires building that repo too.
- **No code signing** — Use `xattr -cr` on the `.app` bundle after building.
- **No custom app icon yet** — Still ships with the Zed icon.

## Dashboard

The dashboard panel is pinned as the first tab and organizes 27+ tools across four categories:

| Category | Examples |
|----------|----------|
| **ProTools** | Bounce All, Session Monitor, Import & Spot Clips, Save + Increment, Batch Processing |
| **Mixer** | Transport, Mute/Solo, Track Volume, Manage Tracks, Timeline Selection, Bounce Export |
| **Audio** | Normalize (EBU R128), Maximize Peaks, Convert MP3/WAV, TV Converter |
| **File** | Carrefour Renamer, TV to SPOT Rename, Create Folder Structure |

Each tool card has buttons for:
- **Run** — Spawns the tool in a terminal tab
- **In-app shortcut** — Adds a keybinding to the local keymap
- **Global shortcut** — Captures a key combo and registers it system-wide

## Session awareness

A background poller (every 5 seconds) queries Pro Tools for the currently open session. When connected:
- The window title updates to `ProTools Studio — SessionName.ptx`
- A green status indicator appears in the dashboard header
- Session-aware tools automatically receive `--session <path>` arguments

## Build

```bash
# Clone
git clone https://github.com/Caio-Ze/protools-studio.git
cd protools-studio

# Build (always release mode — debug builds are very slow for GPU rendering)
cargo build --release -p protools-studio

# Run
cargo run --release -p protools-studio

# The binary is at target/release/protools-studio
```

### Requirements

- Rust 1.93+ (see `rust-toolchain.toml`)
- macOS 13+ (Ventura or later)
- Xcode Command Line Tools

### Environment variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `PROTOOLS_RUNTIME_PATH` | Path to runtime tool binaries | `~/Documents/Rust_projects/PROTOOLS_SDK_PTSL/target/runtime/` |
| `PROTOOLS_AGENT_TOOLS_PATH` | Path to agent tool binaries | `~/Documents/Rust_projects/PROTOOLS_SDK_PTSL/target/debug/` |

## Project structure

```
protools-studio/
├── crates/
│   ├── dashboard/          # Dashboard panel (tool cards, automations, global hotkeys)
│   ├── zed/                # Main binary (startup, branding, init)
│   ├── workspace/          # Window/pane management, session state
│   ├── gpui/               # GPU-accelerated UI framework
│   └── ...                 # ~220 other Zed crates (editor, terminal, LSP, etc.)
├── assets/
│   └── agent-skills/       # Embedded SKILL.md + AUTOMATIONS.toml defaults
├── DOCS/
│   └── PROJECT_OVERVIEW.md # Detailed development log and architecture notes
├── CLAUDE.md               # AI agent instructions for this codebase
└── old_zed/                # Archived upstream Zed files (docs, CI, Docker, Nix)
```

## Companion repository

The tool binaries that the dashboard invokes live in a separate repo:

**[PROTOOLS_SDK_PTSL](https://github.com/Caio-Ze/PROTOOLS_SDK_PTSL)** — Rust monorepo with 31 CLI tools for Pro Tools automation via gRPC, plus audio processing utilities (FFmpeg-based normalization, format conversion, peak maximization).

## License

This project is a fork of [Zed](https://github.com/zed-industries/zed). The original Zed code is licensed under AGPL-3.0 and Apache-2.0. New code added for ProTools Studio is licensed under GPL-3.0-or-later.
