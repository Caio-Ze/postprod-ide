# PostProd IDE

A native macOS app for post-production automation. Built on [Zed](https://github.com/zed-industries/zed), the GPU-accelerated code editor.

The app ships clean — no tools, no automations baked in. You install product repos for your domain, and their tools and automations appear in the dashboard. AI agents (Claude Code, Gemini CLI) can compose tools freely to solve problems we didn't anticipate.

The first product on the platform: **[PostProd Tools](https://github.com/Caio-Ze/postprod-tools)** — 30+ tools that talk directly to Avid Pro Tools via gRPC.

## How it works

Install a product repo and its content shows up in the dashboard automatically. The app reloads configs every 30 seconds — no restarts, no recompilation.

```
~/PostProd_IDE/
├── config/
│   ├── .state/            ← Runtime state (active folder, recent folders, etc.)
│   ├── tools/             ← Tool definitions (.toml) — from product repos
│   └── automations/       ← Automation definitions (.toml) — from product repos
├── tools/
│   ├── runtime/           ← Runtime tool binaries
│   └── agent/             ← Agent tool binaries
└── deliveries/            ← Delivery-ready files
```

Tools are defined in simple TOML files — one per tool. Automations are the same. Add a file, it appears in the dashboard. Delete it, it's gone. The app never writes config files.

## What the dashboard does

- **One-click tool execution** — each tool is a compiled native binary. Click the button, it runs.
- **AI automations** — prompts routed to Claude Code or Gemini CLI in the built-in terminal. Agents see your file tree, chain tools, and run multi-step workflows end-to-end.
- **Session awareness** — detects your open Pro Tools session automatically. Session-aware tools receive the correct session path without you typing anything.
- **System-wide hotkeys** — assign a global shortcut to any tool. Press it from Pro Tools without switching apps.
- **Delivery monitoring** — background scan of your delivery folder. Color-coded badges show what's complete and what's missing.

## The Pro Tools product

The companion **[PostProd Tools](https://github.com/Caio-Ze/postprod-tools)** repo ships 30+ tools that communicate with Pro Tools over the PTSL gRPC protocol — direct commands to the engine, not GUI simulation.

| Category | Tools |
|----------|-------|
| **Session** | Bounce All, Session Monitor, Import & Spot Clips, Save + Increment, Batch Processing |
| **Mixer** | Transport, Mute/Solo, Track Volume, Manage Tracks, Rename Track/Clip, Delete Tracks |
| **Audio** | Normalize (EBU R128), Maximize Peaks, Convert MP3/WAV, TV Converter |
| **File** | Folder Renamer, TV to SPOT Rename, Create Folder Structure |

Runtime tools are full-featured production applications. Agent tools are 28 thin CLI wrappers that expose PTSL commands to AI agents. Both are compiled Rust binaries.

## AI agents

Automations route prompts to autonomous AI agents (Claude Code, Gemini CLI) running in the built-in terminal. The agents have access to the tool binaries, so they can chain operations: import tracks → solo stems → bounce → normalize → rename → organize deliveries — all from a single prompt.

Automations are defined in TOML files. Each product repo ships its own. The dashboard picks up changes automatically.

## Built in Rust and Go

- **Rust** — the IDE (Zed/GPUI), 28 agent tools, all runtime tools, gRPC protocol layer, audio processing pipelines
- **Go** — Session Monitor TUI (Bubble Tea framework), real-time waveform display, concurrent script orchestration

Both produce universal macOS binaries (Apple Silicon + Intel).

## Building

```bash
./script/postprod/dev-deploy          # build release binary
./script/postprod/dev-deploy --run    # build + launch
./script/clippy                       # lint (clippy + cargo-machete + typos)
```

The workspace (`~/PostProd_IDE/`) is managed separately — the build scripts don't touch it.

## Current state

In active daily use for real audio post-production work on macOS. The platform is functional — tools and automations load from product repos, the dashboard reloads configs live, and AI agents compose tools autonomously.

Remaining:
- Code signing for macOS distribution
- Product repo install scripts (currently manual file copy)

## License

This repository (the IDE) is a fork of [Zed](https://github.com/zed-industries/zed) and is open source. The original Zed code is licensed under AGPL-3.0 and Apache-2.0. New code added for PostProd IDE is licensed under GPL-3.0-or-later.

The companion tool binaries ([PostProd Tools](https://github.com/Caio-Ze/postprod-tools)) are distributed separately and are not covered by this license.
