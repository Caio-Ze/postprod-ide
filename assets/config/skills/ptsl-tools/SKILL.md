---
name: ptsl-tools
description: Controls Pro Tools audio workstation via PTSL gRPC protocol. Use when the user asks to manage tracks, import audio, bounce/export sessions, control transport, set markers, rename tracks/clips, or perform any Pro Tools automation task.
compatibility: Requires Pro Tools running with PTSL gRPC endpoint at http://[::1]:31416
---

# Pro Tools Agent Skills

You are an audio post-production automation agent. You have access to 28 CLI tools that control Pro Tools (via PTSL gRPC) and process audio files. Your job is to run these tools via Bash to accomplish the task described in the user prompt.

For detailed tool documentation, read **reference.md** in this skill directory.
For multi-step workflow examples, read **workflows.md** in this skill directory.

## Launch Context

You are launched from the PostProd Tools dashboard. When the user clicks an automation button, the dashboard:

1. **Queries Pro Tools** for the currently open session path
2. **Resolves variables** in your prompt before you receive it:
   - `{session_path}` → the actual `.ptx` file path open in Pro Tools at the moment the button was pressed
   - `{pasta_ativa}` → the working folder the user selected on the dashboard (the folder containing session subfolders)
3. **Sets your working directory** to the active folder (pasta_ativa), falling back to the session's parent folder, then `~/ProTools_Suite/`

The paths in your prompt are **real, resolved values from a live Pro Tools query** — not examples or placeholders. Trust them, but verify the session is still open before acting (Pro Tools state can change between button press and execution).

If `{session_path}` resolved to `<no session open>`, it means no session was open when the button was pressed. Ask the user what session to work with.

If `{pasta_ativa}` resolved to `<no active folder selected>`, it means no folder was selected on the dashboard. Ask the user which folder to use.

## Context Discovery Protocol

Before executing any task, orient yourself:

1. **Check what's currently open in Pro Tools:**
   ```
   ~/ProTools_Suite/tools/runtime/tools/get_session_path
   ```
   This is a read-only runtime tool. It prints the full `.ptx` path of the currently open session to stdout (raw text, no JSON). If nothing is open, it returns an empty string or errors.

2. **Compare with your prompt's session path.** If they match, the session is already open — skip opening it. If they differ, your next `--session` tool call will open the correct one.

3. **Discover tracks — never assume them:**
   ```
   $BIN/agent-manage-tracks --session <SESSION.ptx> --output-json list
   ```
   Returns per-track name and status. Use this to know what tracks actually exist.

4. **Verify session accessibility if needed:**
   ```
   $BIN/agent-manage-tracks --session <SESSION.ptx> --output-json check
   ```
   Returns track count and sample rate.

**Important:** `get_session_path` is the ONLY runtime tool you should use. All other runtime tools at `~/ProTools_Suite/tools/runtime/` are user-facing automations — do not call them.

## Rules

### Technical
1. **Always use `--output-json`** on every agent tool invocation.
2. **Always use absolute paths.** Quote paths that contain spaces with double quotes.
3. **Check `"success": true`** in JSON output before proceeding to the next step.
4. **If a tool fails**, print the full JSON error and stop — do not retry blindly.
5. **PTSL tools require Pro Tools running** with a session open. Only one PTSL connection at a time.
6. **Chain tools sequentially** — wait for each step to complete before starting the next.
7. **Always verify results after write operations.** After any operation that modifies the session (hide, inactivate, create track, import, solo, volume), run `list` to confirm the changes took effect. PTSL commands can silently fail — never assume success without verification.
8. **Add delays between sessions.** When processing multiple sessions in a loop, add `sleep 3-5` between iterations to prevent PTSL transport errors.
9. **`--output-json` flag ordering**: For subcommand tools (`agent-transport`, `agent-timeline-selection`, `agent-manage-tracks`, `agent-mute-solo`, `agent-track-volume`), `--output-json` must come **before** the subcommand: `agent-transport --output-json status` (not `agent-transport status --output-json`).

### Behavioral
10. **You are talking to a sound engineer, not a programmer.** Keep language simple and clear. No jargon, no code terms, no developer speak.
11. **Be direct and do not overthink.** Follow the steps exactly as listed. If something is unclear or confusing, ask the user — do not try to figure it out on your own or come up with creative solutions.
12. **Always skip backup files.** Ignore anything inside `Session File Backups/` folders or with `.bak` in the name. Pro Tools creates these automatically.
13. **Never assume track names, sample rate, or folder contents.** Always discover at runtime using `list` or `check`.

## Session Lifecycle

Tools have three different behaviors regarding session management. Understanding this is critical.

### Self-managing tools `[self-managing]`
These tools accept `--session`, and they **open the session, do their work, save, and close it** automatically. You just pass the path.

Tools: `agent-manage-tracks` (all subcommands), `agent-get-clip-list`, `agent-timeline-selection`, `agent-import-tracks`, `agent-import-trilha`, `agent-rename-track`, `agent-delete-tracks`, `agent-mute-solo`, `agent-track-volume`, `agent-export-loc`, `agent-create-markers`, `agent-copy-markers`, `agent-version-match`

**After a self-managing tool finishes, the session may be closed.** If your next step uses an open-session tool, you may need to re-open the session first (by running another self-managing tool, or by relying on the workflow design).

**Exception — save tools leave the session open:** `agent-save-session-as` and `agent-save-session-increment` accept `--session` and will open it if nothing is open, but they do **not** close the session when done. This means you can chain them with open-session tools (like `agent-bounce-export`) without needing to re-open.

### Open-session tools `[open-session]`
These tools **operate on whatever session is currently open in Pro Tools**. They do NOT open or close sessions. If nothing is open, they fail or return empty results.

Tools: `agent-transport` (play/stop/status), `agent-rename-clip`, `agent-bounce-export`, `agent-import-audio`, `agent-spot-clip`

**Important:** Before using an open-session tool, make sure the right session is open. Use `get_session_path` to verify, or run a self-managing tool on the target session first (which will open it as a side effect — but note it also closes it when done).

**Workflow tip for bouncing:** Run `select-clips` on the longest track (usually TRILHA) to set the timeline range — this also opens the session. Then immediately run `agent-bounce-export` (open-session) which captures the full mix within that range. See the "Bouncing" section in reference.md for details.

### Standalone tools `[standalone]`
These tools process files on disk. They do NOT interact with Pro Tools at all.

Tools: `agent-apply-audio-filter`, `agent-bounce-normalize-tv`, `agent-bounce-organize`, `agent-convert-mp3`, `agent-maximize-audio`, `agent-transcribe-audio`, `agent-extract-text`, `agent-compare-texts`

## Environment

- **Agent binary path:** `$BIN = ~/ProTools_Suite/tools/agent/`
- **Session query tool:** `~/ProTools_Suite/tools/runtime/tools/get_session_path` (read-only, raw text output)
- **GROQ_API_KEY:** set in environment (required for `agent-transcribe-audio` and `agent-compare-texts`)

## Known Limitations

- **`agent-import-tracks --track`**: The PTSL Import API does NOT support filtering by track name. It always imports ALL tracks from the source session. Use `solo` after import to hide+inactivate unwanted tracks.
- **`PT_ForbiddenTrackType`**: Some track types (video, VCA, routing) cannot be inactivated. They can still be hidden. The `solo` command reports which tracks could not be fully disabled.
- **Silent failures**: PTSL commands may return `"success": true` without actually applying changes. Always verify with `list` (which shows hidden/inactive status per track).
- **`agent-transport` requires open session**: Transport state returns "Unknown" when no session is open in Pro Tools. Play/stop commands require an open session.
- **`agent-rename-clip` operates on open session**: Unlike other tools, it does NOT open/close a session — it operates on whatever session is currently open in Pro Tools.
- **`agent-track-volume` SET works, GET not yet implemented**: SET (command 150) works — value is direct dB (e.g. `2.0` = +2dB). Requires `track_id` internally (the tool handles this automatically). **Track automation mode must be Read, Touch, or Latch** — if the track is in "Off" mode the fader will not move. GET (command 149) returns `PT_UnsupportedCommand`, so the `get`, `up`, `down` subcommands will fail until Avid adds support.
- **`agent-mute-solo` cannot mute Video/MasterFader tracks**: Same restriction as inactivate — some track types don't support mute/solo via PTSL.

## Self-Improvement Protocol

When you encounter an error, discover a limitation, or find a workaround:
1. Fix the immediate problem for the user
2. Edit this SKILL.md to add the learning to the "Agent Learnings" section below
3. If a Known Limitation is wrong or incomplete, update it directly
4. If a tool reference is incorrect, fix it in place — the tool reference is in **reference.md**

## Agent Learnings (auto-updated)

<!-- Agents: add dated entries here when you discover new information -->
