# ProTools Studio

A native macOS audio post-production IDE built on top of [Zed](https://github.com/zed-industries/zed). ProTools Studio keeps everything Zed offers — a GPU-accelerated editor, integrated terminal, AI chat, LSP, and git — and adds a dashboard purpose-built for audio workflows with Avid Pro Tools.

![ProTools Studio Dashboard](DOCS/dashboard-screenshot.png)

## Who this is for

Audio post-production for broadcast, advertising, and streaming is a volume game. A single production company might deliver dozens of spots per day — each one requiring the same sequence of steps: open session, import tracks, solo the right stems, bounce, normalize to broadcast loudness standards (-23 LUFS), convert formats, rename files to network specs, and organize deliveries. Multiply that by every editor on the team, every day, every client.

These are hours of mechanical, repetitive work that require precision but not creativity. A missed normalization, a wrong filename, a forgotten MP3 conversion — any of these means a rejected delivery and wasted time.

ProTools Studio was built for this reality. It replaces the manual repetition with one-click tools and AI-driven workflows that handle the entire chain — from open session to verified delivery — while the engineer focuses on the actual creative work.

## How it compares

Audio post-production has a few automation options today, each with trade-offs:

| | **ProTools Studio** | **[SoundFlow](https://soundflow.org/)** | **[Keyboard Maestro](https://www.keyboardmaestro.com/)** | **[py-ptsl](https://github.com/iluvcapra/py-ptsl)** | **Custom scripts** |
|---|---|---|---|---|---|
| **How it talks to PT** | gRPC (PTSL protocol) | SFX protocol + GUI hooks | GUI simulation (clicks, keystrokes) | gRPC (PTSL protocol) | AppleScript / osascript |
| **Breaks on UI changes** | No | Partially | Yes | No | Often |
| **AI** | Autonomous agents (see below) | Chat assistant (premium) | No | No | No |
| **File/folder awareness** | Full IDE (editor, git, tree) | No | No | No | No |
| **Session awareness** | Live polling (5s) | Via SFX framework | Manual | Manual | Manual |
| **System-wide hotkeys** | Yes (CGEventTap) | Yes (MIDI/OSC/keys) | Yes | No | No |
| **Custom tool authoring** | Rust, edit + rebuild in-app | JavaScript | GUI macro builder | Python | Any language |

### What SoundFlow does well

[SoundFlow](https://soundflow.org/) is the industry standard for Pro Tools automation — now [integrated directly into Pro Tools 2025.10](https://www.avid.com/resource-center/soundflow). It ships with 1,700+ pre-built macros, has a JavaScript scripting engine, and supports Stream Deck, MIDI, and OSC triggers. For most users who want ready-made macros and a polished GUI, SoundFlow is the right choice.

### Where ProTools Studio is different

ProTools Studio is not trying to replace SoundFlow's macro library. It solves a different problem: **giving audio engineers an integrated development environment that understands their project structure**.

- **Folder-aware workspace** — The Zed file explorer shows your entire delivery structure (`Sessoes/`, `Imports/`, `Processamento/`, `Finalizados/`, `Arquivo/`). You see your session files, audio exports, and tool source code side by side. No other audio automation tool gives you a project tree with git integration, search across files, and a terminal — all in one window.

- **Autonomous AI agents, not a chat assistant** — SoundFlow's [Session Assistant](https://soundflow.org/session-assistant) is a conversational interface inside Pro Tools: you type "create a track" and it executes that single command. It cannot read your project folders, create files on disk, chain external tools, or work without your input at every step.

  ProTools Studio integrates full-blown coding agents (Claude Code, Gemini CLI) that run autonomously in the integrated terminal. The difference is fundamental:

  | | **ProTools Studio agents** | **SoundFlow Session Assistant** |
  |---|---|---|
  | Reads your file tree | Yes — sees sessions, audio, exports, source code | No — only sees Pro Tools session state |
  | Creates/edits files | Yes — writes scripts, renames exports, moves deliveries | No — limited to Pro Tools track operations |
  | Chains multiple tools | Yes — import tracks → solo → bounce → normalize → rename in one prompt | No — one command at a time |
  | Runs without prompting | Yes — headless mode executes multi-step workflows end-to-end | No — requires human input for each step |
  | Fixes its own tools | Yes — agent reads tool source, edits Rust code, rebuilds | No — macros are fixed scripts |
  | Learns from mistakes | Yes — agents update their own skill files with new patterns | No |
  | Works outside Pro Tools | Yes — file operations, FFmpeg, audio analysis, git | No — Pro Tools only |

  A concrete example: you click "Full Delivery" in the dashboard and the agent autonomously bounces the session, normalizes to -23 LUFS, converts to MP3, renames files for broadcast standards, organizes them into delivery folders, and verifies the output — all from a single click. The agent decides what to do at each step based on what it finds on disk.

- **Native gRPC, not GUI simulation** — Like py-ptsl, ProTools Studio talks to Pro Tools over the official PTSL gRPC protocol. Unlike Keyboard Maestro (which simulates mouse clicks and keystrokes and [breaks when Avid changes the UI](https://duc.avid.com/showthread.php?t=428108)), gRPC commands are stable across Pro Tools versions.

- **The tools are the project** — The 31 CLI tools are Rust binaries that live in the same workspace. You can open a tool's source, fix a bug, `cargo build` in the integrated terminal, and immediately re-run it from the dashboard. The IDE and the runtime are one thing. No other audio tool offers this.

## Built entirely in Rust

Everything — the IDE, the dashboard, and every single CLI tool — is written in Rust. This is not a cosmetic choice. It has real consequences for the people using it:

- **Instant tool execution** — Each of the 31 CLI tools is a compiled native binary. There is no interpreter startup, no VM warmup, no dependency resolution at runtime. A tool launches, does its work, and exits. When you're bouncing 40 sessions in a batch, the difference between a 200ms Rust binary and a 2-second Python script adds up fast.

- **GPU-accelerated UI** — The IDE is built on [GPUI](https://www.gpui.rs/), Zed's GPU rendering framework. The dashboard, the editor, the terminal — everything is rendered on the GPU. Scrolling through a session with hundreds of tracks or scanning a delivery folder with thousands of files stays smooth. There is no Electron, no web view, no garbage collector pausing the UI.

- **Low memory footprint** — Rust has no runtime and no garbage collector. The entire application (IDE + dashboard + terminal + file tree) typically uses a fraction of the memory that Electron-based tools consume. On a machine already running Pro Tools (which is not gentle with RAM), this matters.

- **Single static binaries** — Each tool compiles to a single executable with no external dependencies (except FFmpeg for audio processing tools). No `node_modules/`, no `pip install`, no version conflicts. Copy the binary, run it.

- **Reliability at scale** — Rust's ownership model catches entire categories of bugs (null pointers, data races, use-after-free) at compile time. When a production company runs batch processing across dozens of sessions overnight, the tools either work correctly or refuse to compile. They don't silently corrupt data at 3 AM.

The same language runs from the lowest level (gRPC protocol buffers talking to Pro Tools) to the highest level (GPU-rendered dashboard buttons). There is no glue layer, no FFI boundary, no serialization between languages. It is Rust all the way down.

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

This repository (the IDE) is a fork of [Zed](https://github.com/zed-industries/zed) and is open source. The original Zed code is licensed under AGPL-3.0 and Apache-2.0. New code added for ProTools Studio is licensed under GPL-3.0-or-later.

The companion tool binaries ([PROTOOLS_SDK_PTSL](https://github.com/Caio-Ze/PROTOOLS_SDK_PTSL)) are distributed separately and are not covered by this license.
