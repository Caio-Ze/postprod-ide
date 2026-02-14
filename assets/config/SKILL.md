---
name: ptsl-tools
description: Controls Pro Tools audio workstation via PTSL gRPC protocol. Use when the user asks to manage tracks, import audio, bounce/export sessions, control transport, set markers, rename tracks/clips, or perform any Pro Tools automation task.
compatibility: Requires Pro Tools running with PTSL gRPC endpoint at http://[::1]:31416
---

# Pro Tools Agent Skills

You are an audio post-production automation agent. You have access to 31 CLI tools that control Pro Tools (via PTSL gRPC) and process audio files. Your job is to run these tools via Bash to accomplish the task described in the user prompt.

## Self-Improvement Protocol

When you encounter an error, discover a limitation, or find a workaround:
1. Fix the immediate problem for the user
2. Edit this SKILL.md to add the learning to the "Agent Learnings" section at the bottom
3. If a Known Limitation is wrong or incomplete, update it directly
4. If a tool reference is incorrect, fix it in place

## Environment

- **Binary path:** `~/ProTools_Suite/tools/agent/`
- **Test session:** `~/ProTools_Suite/1_Sessões/Pro tools_EDITSESSION/Pro tools_EDITSESSION_V4.ptx`
- **Audio files:** `~/ProTools_Suite/1_Sessões/Pro tools_EDITSESSION/Audio Files/`
- **EXPO_IMPO folder:** `~/ProTools_Suite/1_Sessões/Pro tools_EDITSESSION/EXPO_IMPO/`
- **Bounce folder:** `~/ProTools_Suite/Bounce/`
- **Known tracks:** LOC, V1, V2, V3, V4, TRILHA, Audio 1 (may vary per session)
- **Session sample rate:** 48000 Hz
- **GROQ_API_KEY:** set in environment (required for transcribe/extract/compare tools)

## Rules

1. **Always use `--output-json`** on every tool invocation.
2. **Always use absolute paths.** Quote paths that contain spaces with double quotes.
3. **Check `"success": true`** in JSON output before proceeding to the next step.
4. **If a tool fails**, print the full JSON error and stop — do not retry blindly.
5. **PTSL tools require Pro Tools running** with the session open. Only one PTSL connection at a time.
6. **Chain tools sequentially** — wait for each step to complete before starting the next.
7. **Always verify results after write operations.** After any operation that modifies the session (hide, inactivate, create track, import, solo), run `list` to confirm the changes took effect. PTSL commands can silently fail — never assume success without verification.
8. **Add delays between sessions.** When processing multiple sessions in a loop, add `sleep 3-5` between iterations to prevent PTSL transport errors.

## Known Limitations

- **`agent-import-tracks --track`**: The PTSL Import API does NOT support filtering by track name. It always imports ALL tracks from the source session. Use `solo` after import to hide+inactivate unwanted tracks.
- **`PT_ForbiddenTrackType`**: Some track types (video, VCA, routing) cannot be inactivated. They can still be hidden. The `solo` command reports which tracks could not be fully disabled.
- **Silent failures**: PTSL commands may return `"success": true` without actually applying changes. Always verify with `list` (which shows hidden/inactive status per track).
- **`--output-json` flag ordering**: For subcommand tools (`agent-transport`, `agent-timeline-selection`, `agent-manage-tracks`), `--output-json` must come **before** the subcommand: `agent-transport --output-json status` (not `agent-transport status --output-json`).
- **`agent-transport` requires open session**: Transport state returns "Unknown" when no session is open in Pro Tools. Play/stop commands require an open session.
- **`agent-rename-clip` operates on open session**: Unlike other tools, it does NOT open/close a session — it operates on whatever session is currently open in Pro Tools.
- **`agent-track-volume` SET works, GET not yet implemented**: SET (command 150) works — value is direct dB (e.g. `2.0` = +2dB). Requires `track_id` internally (the tool handles this automatically). **Track automation mode must be Read, Touch, or Latch** — if the track is in "Off" mode the fader will not move. GET (command 149) returns `PT_UnsupportedCommand`, so the `get`, `up`, `down` subcommands will fail until Avid adds support.
- **`agent-mute-solo` cannot mute Video/MasterFader tracks**: Same restriction as inactivate — some track types don't support mute/solo via PTSL.

## Tool Reference

All binaries are at `$BIN=~/ProTools_Suite/tools/agent/`

---

### Category 1: PTSL Session Read (non-destructive)

#### agent-manage-tracks list
List all tracks in a session with their hidden/inactive status.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json list
```
Returns per-track status: `active`, `hidden`, `inactive`, or `hidden+inactive`.

#### agent-manage-tracks check
Check if session can be opened and accessed. Returns track count and sample rate.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json check
```

#### agent-manage-tracks markers
List all memory locations (markers) in the session. Returns marker number, name, start_time (samples), and end_time (samples).
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json markers
```

#### agent-get-clip-list
List all clips in a session. Returns clip_id, clip_full_name, clip_root_name, clip_type for each clip.
```
$BIN/agent-get-clip-list --session <SESSION.ptx> --output-json
```

#### agent-timeline-selection get
Get the current timeline selection (in/out points in samples).
```
$BIN/agent-timeline-selection --session <SESSION.ptx> --output-json get
```
Returns: in_time, out_time, play_start_marker_time, pre/post roll.

#### agent-transport status
Get the current transport state (Playing, Stopped, Recording, etc.). No session open/close needed.
```
$BIN/agent-transport --output-json status
```

#### agent-manage-tracks select-clips
Select all clips on a track, setting the timeline selection to cover the full clip range. **Always run this before bouncing** to ensure the bounce covers the correct timeline. Only selects clips — does not solo, mute, or inactivate anything.
Defaults to track `TRILHA` if `--track` is not specified.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json select-clips
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json select-clips
```
Returns: in_time, out_time (samples).

#### agent-version-match
Find V(N) versioned tracks and report which versions exist.
```
$BIN/agent-version-match --session <SESSION.ptx> --output-json [--tolerance <MS>]
```
Default tolerance: 10ms.

---

### Category 2: PTSL Session Write (modifies session)

#### agent-manage-tracks new-session
Create a new Pro Tools session. `--session` = destination folder, `--track` = session name.
```
$BIN/agent-manage-tracks --session <DEST_FOLDER> --track <SESSION_NAME> --output-json new-session
```
Creates a 48kHz / 24-bit / WAV / Stereo Mix session.

#### agent-import-audio
Import an audio file into the Pro Tools Clip List. Returns a clip ID.
```
$BIN/agent-import-audio --file <AUDIO_FILE> --output-json
```

#### agent-spot-clip
Spot a clip to a track at a sample position. Requires clip ID from agent-import-audio.
```
$BIN/agent-spot-clip --clip-id <CLIP_ID> --track <TRACK_NAME> --position <SAMPLE_POS> --output-json
```

#### agent-export-loc
Export a track as consolidated WAV. Selects track, consolidates, exports to EXPO_IMPO.
```
$BIN/agent-export-loc --session <SESSION.ptx> --output-json [--track <TRACK>] [--output-dir <DIR>]
```
Default track: LOC.

#### agent-create-markers
Create markers from a markdown timestamps file (lines like `## 00:01:23 Marker Name`).
```
$BIN/agent-create-markers --session <SESSION.ptx> --file <TIMESTAMPS.md> --output-json [--sample-rate <HZ>]
```
Default sample rate: 48000.

#### agent-copy-markers
Copy markers from a source session to a target session.
```
$BIN/agent-copy-markers --target <TARGET.ptx> --source <SOURCE.ptx> --output-json [--clear]
```

#### agent-import-tracks
Import tracks from a source session into a target session. **Note:** imports ALL tracks regardless of `--track` filter (PTSL limitation). Use `solo` after to keep only desired tracks.
```
$BIN/agent-import-tracks --session <TARGET.ptx> --source <SOURCE.ptx> --output-json [--track <NAME>] [--mode <match|new>] [--markers]
```
Default mode: match.

#### agent-import-trilha
Import a TRILHA (music) track from a source session.
```
$BIN/agent-import-trilha --session <TARGET.ptx> --source <SOURCE.ptx> --output-json [--track <NAME>] [--markers]
```
Default track: TRILHA.

#### agent-rename-track
Rename a track in a session. Opens session, renames, saves, closes.
```
$BIN/agent-rename-track --session <SESSION.ptx> --track <CURRENT_NAME> --new-name <NEW_NAME> --output-json
```

#### agent-rename-clip
Rename a clip in the currently open session. No session open/close — operates on whatever is open.
```
$BIN/agent-rename-clip --clip-name <CURRENT_NAME> --new-name <NEW_NAME> --output-json [--rename-file]
```
`--rename-file` also renames the underlying audio file on disk.

#### agent-save-session-as
Save the current session under a new name and location.
```
$BIN/agent-save-session-as --session <SESSION.ptx> --name <NEW_NAME> --location <DEST_DIR> --output-json
```

#### agent-delete-tracks
Delete one or more tracks from a session. Can specify multiple `--track` flags.
```
$BIN/agent-delete-tracks --session <SESSION.ptx> --track <NAME> [--track <NAME2>] --output-json
```
Returns `success_count`.

#### agent-timeline-selection set
Set the timeline selection (in/out points in samples).
```
$BIN/agent-timeline-selection --session <SESSION.ptx> --output-json set --in <SAMPLES> --out <SAMPLES>
```

#### agent-mute-solo
Mute, unmute, solo, or unsolo tracks. Can specify multiple `--track` flags. Subcommands: `mute`, `unmute`, `solo`, `unsolo`.
```
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> [--track <NAME2>] --output-json mute
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> --output-json solo
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> --output-json unmute
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> --output-json unsolo
```

#### agent-track-volume
Set track fader volume in dB. Value is direct dB: `0.0` = unity, `2.0` = +2dB, `-6.0` = -6dB.
**Requires track automation mode = Read/Touch/Latch** (fader won't move if automation is "Off").
Only `set` works — `get`/`up`/`down` require command 149 which Pro Tools hasn't implemented yet.
```
$BIN/agent-track-volume --session <SESSION.ptx> --track <NAME> --output-json set --value 2.0
$BIN/agent-track-volume --session <SESSION.ptx> --track <NAME> --output-json set --value -6.0
$BIN/agent-track-volume --session <SESSION.ptx> --track <NAME> --output-json set --value 0.0
```

#### agent-transport play / stop
Start or stop playback. Checks current state first — only toggles if needed (idempotent). No session open/close.
```
$BIN/agent-transport --output-json play
$BIN/agent-transport --output-json stop
```

#### agent-manage-tracks create
Create a new stereo audio track.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json create
```

#### agent-manage-tracks hide
Hide a track by name.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json hide
```

#### agent-manage-tracks inactivate
Set a track as inactive. Fails with `PT_ForbiddenTrackType` for video/VCA/routing tracks.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json inactivate
```

#### agent-manage-tracks solo
Hide and inactivate ALL tracks except the one specified by `--track` (and Master). Processes tracks one by one, verifies results, and reports any failures.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_TO_KEEP> --output-json solo
```
Output includes `verified_hidden`, `verified_inactive`, `still_active`, and `errors` arrays.

#### agent-bounce-export
Bounce-export the current session to WAV or MP3. Requires a session to be open in Pro Tools.
```
$BIN/agent-bounce-export --output-json [--format <wav|mp3>] [--timeout <SECONDS>]
```
Default format: wav. Default timeout: 30s.

---

### Category 3: Standalone Audio Processing (no Pro Tools needed)

#### agent-apply-audio-filter
Apply an FFmpeg audio filter chain to a file. Creates a new file with suffix.
```
$BIN/agent-apply-audio-filter <AUDIO_FILE> --output-json [--filter <FFMPEG_CHAIN>] [--sample-rate <HZ>] [--codec <CODEC>] [--suffix <SUFFIX>]
```
Defaults: filter=volume=10dB,compand...,loudnorm; sample-rate=48000; codec=pcm_s24le; suffix=_NET.

#### agent-bounce-normalize-tv
Two-pass FFmpeg loudnorm normalization. Creates _NET and _TV variants.
```
$BIN/agent-bounce-normalize-tv <INPUT_WAV> --output-json [--target-lufs <LUFS>] [--output-dir <DIR>]
```
Default target: -23.0 LUFS.

#### agent-bounce-organize
Move/copy a bounced file to the central BOUNCE folder.
```
$BIN/agent-bounce-organize <FILE_PATH> --output-json [--keep-local]
```

#### agent-convert-mp3
Convert audio file to MP3 via FFmpeg (libmp3lame VBR).
```
$BIN/agent-convert-mp3 <AUDIO_FILE> --output-json [--quality <0-9>] [--output-dir <DIR>]
```
Default quality: 2 (high).

#### agent-maximize-audio
Peak-normalize audio to maximize volume without clipping.
```
$BIN/agent-maximize-audio <FILE_OR_FOLDER> --output-json
```

---

### Category 4: Text/Transcription (requires GROQ_API_KEY)

#### agent-transcribe-audio
Transcribe audio using Groq Whisper API.
```
$BIN/agent-transcribe-audio <AUDIO_FILE> --output-json [--model <MODEL>]
```
Default model: whisper-large-v3-turbo.

#### agent-extract-text
Extract plain text from a document (docx, doc, rtf, txt) via macOS textutil.
```
$BIN/agent-extract-text <DOCUMENT_FILE> --output-json
```

#### agent-compare-texts
Compare reference text with transcription using Groq LLM.
```
$BIN/agent-compare-texts --reference <REF_FILE> --transcription <TRANS_FILE> --output-json [--prompt <PROMPT_FILE>] [--model <MODEL>]
```
Default model: llama-3.3-70b-versatile.

---

## Common Workflow Chains

### Import and Spot Audio
```bash
# 1. Import audio file into clip list
RESULT=$($BIN/agent-import-audio --file "/path/to/audio.wav" --output-json)
CLIP_ID=$(echo "$RESULT" | python3 -c "import sys,json; print(json.load(sys.stdin)['clip_id'])")

# 2. Spot the clip to a track at sample position 0
$BIN/agent-spot-clip --clip-id "$CLIP_ID" --track "V1" --position 0 --output-json
```

### Import and Spot Audio at a Marker (e.g. IN)
```bash
# 1. Read markers to find position
MARKERS=$($BIN/agent-manage-tracks --session "/path/to/session.ptx" --output-json markers)
# Parse the "IN" marker start_time from JSON output

# 2. Import audio file into clip list
RESULT=$($BIN/agent-import-audio --file "/path/to/audio.wav" --output-json)
CLIP_ID=$(echo "$RESULT" | python3 -c "import sys,json; print(json.load(sys.stdin)['clip_id'])")

# 3. Spot the clip to a track at the marker's sample position
$BIN/agent-spot-clip --clip-id "$CLIP_ID" --track "LOC" --position <MARKER_SAMPLES> --output-json
```

### Import Only One Track from Another Session
```bash
# 1. Import all tracks (PTSL limitation — cannot filter)
$BIN/agent-import-tracks --session "/path/to/target.ptx" --source "/path/to/source.ptx" --mode new --output-json

# 2. Solo the desired track (hide+inactivate everything else)
$BIN/agent-manage-tracks --session "/path/to/target.ptx" --track "LOC" --output-json solo

# 3. VERIFY the result — always check after write operations
$BIN/agent-manage-tracks --session "/path/to/target.ptx" --output-json list
```

### Batch Session Processing (create, import, solo, bounce)
```bash
# When processing multiple sessions, always:
# - Add sleep 3-5 between sessions to prevent PTSL transport errors
# - Verify each step before proceeding to the next
# - Check bounced file exists before moving it

for SESSION in Session_01 Session_02 ...; do
  # import
  $BIN/agent-import-tracks --session "$PTX" --source "$SOURCE" --mode new --output-json
  sleep 3

  # solo LOC
  $BIN/agent-manage-tracks --session "$PTX" --track LOC --output-json solo

  # verify
  $BIN/agent-manage-tracks --session "$PTX" --output-json list
  sleep 3

  # select all clips on LOC (sets correct timeline for bounce)
  $BIN/agent-manage-tracks --session "$PTX" --track LOC --output-json select-clips

  # bounce
  $BIN/agent-bounce-export --format wav --timeout 60 --output-json
  sleep 4
done
```

### Bounce, Normalize, Convert to MP3
```bash
# 1. Apply audio filter (creates _NET file)
$BIN/agent-apply-audio-filter "/path/to/audio.wav" --output-json

# 2. Normalize to TV standard (-23 LUFS)
$BIN/agent-bounce-normalize-tv "/path/to/audio_NET.wav" --output-json

# 3. Convert to MP3
$BIN/agent-convert-mp3 "/path/to/audio_NET_TV.wav" --output-json
```

### Rename Track After Import
```bash
# 1. Import tracks from source
$BIN/agent-import-tracks --session "$PTX" --source "$SOURCE" --mode new --output-json

# 2. Rename the imported track
$BIN/agent-rename-track --session "$PTX" --track "Audio 1" --new-name "LOC" --output-json

# 3. Verify
$BIN/agent-manage-tracks --session "$PTX" --output-json list
```

### Bounce a Full Track (Select All Clips + Bounce)
```bash
# 1. Select all clips on the track (sets timeline to full clip range)
$BIN/agent-manage-tracks --session "$PTX" --track LOC --output-json select-clips

# 2. Bounce the selection
$BIN/agent-bounce-export --output-json --format wav
```

### Bounce a Segment (Timeline Selection + Bounce)
```bash
# 1. Set in/out points (e.g. first 5 seconds at 48kHz)
$BIN/agent-timeline-selection --session "$PTX" --output-json set --in 0 --out 240000

# 2. Bounce the selection
$BIN/agent-bounce-export --output-json --format wav
```

### Duplicate Session Before Destructive Ops
```bash
# 1. Save a backup copy
$BIN/agent-save-session-as --session "$PTX" --name "Session_Backup" --location "/path/to/backups" --output-json

# 2. Now safely delete tracks from the original
$BIN/agent-delete-tracks --session "$PTX" --track "unwanted_track_1" --track "unwanted_track_2" --output-json
```

### Adjust Track Volume and Solo for Bounce
```bash
# 1. Set TRILHA volume to +2dB (track automation must be Read/Touch/Latch)
$BIN/agent-track-volume --session "$PTX" --track TRILHA --output-json set --value 2.0

# 2. Solo TRILHA and mute other tracks
$BIN/agent-mute-solo --session "$PTX" --track V1 --track V2 --track V3 --output-json mute

# 3. Bounce the result
$BIN/agent-bounce-export --output-json --format wav
```

### Export Track and Process
```bash
# 1. Export LOC track
$BIN/agent-export-loc --session "/path/to/session.ptx" --track LOC --output-json

# 2. Normalize the exported WAV
$BIN/agent-bounce-normalize-tv "/path/to/EXPO_IMPO/exported.wav" --output-json
```

---

## Agent Learnings (auto-updated)

<!-- Agents: add dated entries here when you discover new information -->

- [2026-02-12] SetTrackControlBreakpoints (cmd 150) requires track_id, not track_name
- [2026-02-12] Value for volume is direct dB (2.0 = +2dB)
- [2026-02-12] Track automation mode must be Read/Touch/Latch for volume changes
- [2026-02-12] select-clips defaults to TRILHA when --track is omitted
- [2026-02-12] GetTrackControlBreakpoints (cmd 149) returns PT_UnsupportedCommand — not yet implemented by Avid
