# PostProd IDE

An automation platform built as a [Zed](https://github.com/zed-industries/zed) fork. Adds a TOML-driven dashboard for running tools, dispatching AI agents, and chaining multi-step workflows — all configurable without rebuilding.

## What it does

Every `.toml` file in a config directory becomes a dashboard card — a tool that runs a binary, an automation that dispatches a prompt to an AI agent, or a pipeline that chains multiple steps together. Edit a TOML, and the dashboard picks it up within seconds.

- **Tools** — run compiled binaries with parameters, session context, and background/terminal modes
- **Automations** — route prompts to Claude, Gemini, or Zed's native agent panel with interpolated context
- **Pipelines** — chain automations and tools into multi-step workflows with group-based parallel execution (multiple agents can run simultaneously within a pipeline step)
- **Scheduler** — cron-based triggers with completion tracking and chain firing
- **Global hotkeys** — system-wide keyboard shortcuts via macOS `CGEventTap`
- **Per-folder configs** — each workspace folder carries its own tools, automations, agent backends, and settings profile
- **Settings profiles** — switching config folders automatically activates a Zed settings profile (theme, fonts, panel layout, UI) per workspace
- **Parameter system** — user-editable fields (text, select, path) interpolated into prompts and persisted across sessions

## Architecture

The app ships clean — no tools, no automations baked in. Domain content comes from product repos that install TOML configs and tool binaries into a workspace directory.

```
~/PostProd_IDE/
  config/
    AGENTS.toml                  # agent backend definitions
    automations/                 # one .toml per automation / pipeline
    tools/                       # one .toml per tool card
    .state/                      # runtime state (auto-managed)
  tools/
    agent/                       # agent tool binaries
    runtime/                     # runtime service binaries
  deliveries/                    # output folder for deliverables
  plugin/
    skills/                      # agent skill files
```

The fork adds two crates (`crates/dashboard/` and `crates/postprod_scheduler/`) with minimal glue across upstream files. Currently 17 overlap files with upstream — this keeps rebases against upstream Zed manageable.

## PostProd Tools

The first product built on this platform: **[PostProd Tools](https://github.com/Caio-Ze/postprod-tools)** — Pro Tools automation for audio post-production. 40+ tools for bouncing, batch processing, session monitoring, and AI-driven workflows.

## Building

macOS only (for now). Requires the same dependencies as Zed:

```bash
# Build release binary
./script/postprod/dev-deploy

# Build and launch
./script/postprod/dev-deploy --run

# Launch with clean sandbox workspace
./script/postprod/dev-clean
```

See the [Zed macOS build docs](./docs/src/development/macos.md) for prerequisites.

## Upstream

Fork of [Zed](https://github.com/zed-industries/zed). `main` mirrors upstream (read-only). Development happens on `postprod`. Rebased onto upstream preview releases roughly every two weeks.

## License

PostProd IDE inherits Zed's licensing. See [LICENSE-GPL](LICENSE-GPL) and [LICENSE-AGPL](LICENSE-AGPL). Third-party license compliance is managed via `cargo-about` — see Zed's [licensing docs](https://github.com/zed-industries/zed#licensing) for details.
