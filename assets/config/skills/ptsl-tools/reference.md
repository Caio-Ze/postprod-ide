# Pro Tools Domain Knowledge & Tool Reference

## Pro Tools Domain Knowledge

### Folder structure
Each Pro Tools session lives in its own subfolder:
```
ProjectFolder/
  SessionName/
    SessionName.ptx          <- the session file
    SessionName_V2.ptx       <- versioned session (newer)
    Audio Files/              <- audio media
    LOC/                      <- locution exports (optional)
    Bounced Files/            <- bounce output (optional)
    EXPO_IMPO/                <- import/export staging (optional)
    Session File Backups/     <- auto-backups (ALWAYS SKIP)
```

### Version convention
- A `.ptx` file with no `_V` suffix is the original (oldest) version.
- Files ending in `_V2`, `_V3`, etc. are newer versions. Higher number = newer.
- When multiple versions exist, **always pick the highest version** unless the user says otherwise.
- Example: if a folder has `Session.ptx`, `Session_V2.ptx`, and `Session_V30.ptx`, only use `Session_V30.ptx`.

### Common track names
Sessions often have tracks named LOC (locution/dialogue), V1-V4 (voice versions), TRILHA (music), Audio 1, etc. — but **never assume these exist**. Always run `list` first.

### Bouncing
Bouncing exports the full mix (all unmuted/active tracks) within the current **timeline selection** (in/out points). The standard workflow is:

1. **Set the timeline range** by running `select-clips` on the track with the **longest audio content** — this sets the in/out points to cover the full program duration.
2. **Bounce** with `agent-bounce-export` — this captures everything audible within that range.

TRILHA (music) is usually the longest track since it runs the full program. If unsure which track is longest, check by running `select-clips` on candidates and comparing the returned `in_time`/`out_time` values.

**Important:** `select-clips` doesn't select *what* gets bounced — it sets the *time range*. The bounce captures all unmuted tracks within that range. If you select clips on a shorter track (e.g., a dialogue track that only covers part of the session), the bounce will be truncated.

If the user needs to bounce a specific track in isolation, the approach is: solo that track (mute everything else), set the timeline via `select-clips`, then bounce. Unless the user specifies otherwise, assume they want a full-mix bounce using the longest track for the timeline.

### Sample rate
Typically 48000 Hz for broadcast/post-production, but verify with `check` if it matters for your task.

---

## Tool Reference

All binaries are at `$BIN=~/ProTools_Suite/tools/agent/`

---

### Category 1: PTSL Session Read (non-destructive)

#### agent-manage-tracks list `[self-managing]`
List all tracks in a session with their hidden/inactive status.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json list
```
Returns per-track status: `active`, `hidden`, `inactive`, or `hidden+inactive`.

#### agent-manage-tracks check `[self-managing]`
Check if session can be opened and accessed. Returns track count and sample rate.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json check
```

#### agent-manage-tracks markers `[self-managing]`
List all memory locations (markers) in the session. Returns marker number, name, start_time (samples), and end_time (samples).
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json markers
```

#### agent-get-clip-list `[self-managing]`
List all clips in a session. Returns clip_id, clip_full_name, clip_root_name, clip_type for each clip.
```
$BIN/agent-get-clip-list --session <SESSION.ptx> --output-json
```

#### agent-timeline-selection get `[self-managing]`
Get the current timeline selection (in/out points in samples).
```
$BIN/agent-timeline-selection --session <SESSION.ptx> --output-json get
```
Returns: in_time, out_time, play_start_marker_time, pre/post roll.

#### agent-transport status `[open-session]`
Get the current transport state (Playing, Stopped, Recording, etc.). No session open/close needed.
```
$BIN/agent-transport --output-json status
```

#### agent-manage-tracks select-clips `[self-managing]`
Select all clips on a track, setting the timeline selection to cover the full clip range. **Always run this before bouncing** to ensure the bounce covers the correct timeline. Only selects clips — does not solo, mute, or inactivate anything.
Defaults to track `TRILHA` if `--track` is not specified.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --output-json select-clips
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json select-clips
```
Returns: in_time, out_time (samples).

#### agent-version-match `[self-managing]`
Find V(N) versioned tracks and report which versions exist.
```
$BIN/agent-version-match --session <SESSION.ptx> --output-json [--tolerance <MS>]
```
Default tolerance: 10ms.

---

### Category 2: PTSL Session Write (modifies session)

#### agent-manage-tracks new-session `[self-managing]`
Create a new Pro Tools session. `--session` = destination folder, `--track` = session name.
```
$BIN/agent-manage-tracks --session <DEST_FOLDER> --track <SESSION_NAME> --output-json new-session
```
Creates a 48kHz / 24-bit / WAV / Stereo Mix session.

#### agent-import-audio `[open-session]`
Import an audio file into the Pro Tools Clip List. Returns a clip ID.
```
$BIN/agent-import-audio --file <AUDIO_FILE> --output-json
```

#### agent-spot-clip `[open-session]`
Spot a clip to a track at a sample position. Requires clip ID from agent-import-audio.
```
$BIN/agent-spot-clip --clip-id <CLIP_ID> --track <TRACK_NAME> --position <SAMPLE_POS> --output-json
```

#### agent-export-loc `[self-managing]`
Export a track as consolidated WAV. Selects track, consolidates, exports to EXPO_IMPO.
```
$BIN/agent-export-loc --session <SESSION.ptx> --output-json [--track <TRACK>] [--output-dir <DIR>]
```
Default track: LOC.

#### agent-create-markers `[self-managing]`
Create markers from a markdown timestamps file (lines like `## 00:01:23 Marker Name`).
Supports timestamp formats: `MM:SS`, `HH:MM:SS`, `HH:MM:SS.mmm`, `HH:MM:SS:FF` (timecode with frames at ~30fps).
```
$BIN/agent-create-markers --session <SESSION.ptx> --file <TIMESTAMPS.md> --output-json [--sample-rate <HZ>]
```
Default sample rate: 48000.

#### agent-copy-markers `[self-managing]`
Copy markers from a source session to a target session.
```
$BIN/agent-copy-markers --target <TARGET.ptx> --source <SOURCE.ptx> --output-json [--clear]
```

#### agent-import-tracks `[self-managing]`
Import tracks from a source session into a target session. **Note:** imports ALL tracks regardless of `--track` filter (PTSL limitation). Use `solo` after to keep only desired tracks.
```
$BIN/agent-import-tracks --session <TARGET.ptx> --source <SOURCE.ptx> --output-json [--track <NAME>] [--mode <match|new>] [--markers]
```
Default mode: match.

#### agent-import-trilha `[self-managing]`
Import a TRILHA (music) track from a source session.
```
$BIN/agent-import-trilha --session <TARGET.ptx> --source <SOURCE.ptx> --output-json [--track <NAME>] [--markers]
```
Default track: TRILHA.

#### agent-rename-track `[self-managing]`
Rename a track in a session. Opens session, renames, saves, closes.
```
$BIN/agent-rename-track --session <SESSION.ptx> --track <CURRENT_NAME> --new-name <NEW_NAME> --output-json
```

#### agent-rename-clip `[open-session]`
Rename a clip in the currently open session. No session open/close — operates on whatever is open.
```
$BIN/agent-rename-clip --clip-name <CURRENT_NAME> --new-name <NEW_NAME> --output-json [--rename-file]
```
`--rename-file` also renames the underlying audio file on disk.

#### agent-save-session-as `[self-managing]`
Save the current session under a new name (same directory as the original).
```
$BIN/agent-save-session-as --session <SESSION.ptx> --name <NEW_NAME> --output-json
```

#### agent-save-session-increment `[self-managing]`
Save the current session with an auto-incremented version name (e.g. Session_V2 -> Session_V3, Session_01 -> Session_02). Computes the next available name automatically.
```
$BIN/agent-save-session-increment --session <SESSION.ptx> --output-json
```

#### agent-delete-tracks `[self-managing]`
Delete one or more tracks from a session. Can specify multiple `--track` flags.
```
$BIN/agent-delete-tracks --session <SESSION.ptx> --track <NAME> [--track <NAME2>] --output-json
```
Returns `success_count`.

#### agent-timeline-selection set `[self-managing]`
Set the timeline selection (in/out points in samples).
```
$BIN/agent-timeline-selection --session <SESSION.ptx> --output-json set --in <SAMPLES> --out <SAMPLES>
```

#### agent-mute-solo `[self-managing]`
Mute, unmute, solo, or unsolo tracks. Can specify multiple `--track` flags. Subcommands: `mute`, `unmute`, `solo`, `unsolo`.
```
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> [--track <NAME2>] --output-json mute
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> --output-json solo
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> --output-json unmute
$BIN/agent-mute-solo --session <SESSION.ptx> --track <NAME> --output-json unsolo
```

#### agent-track-volume `[self-managing]`
Set track fader volume in dB. Value is direct dB: `0.0` = unity, `2.0` = +2dB, `-6.0` = -6dB.
**Requires track automation mode = Read/Touch/Latch** (fader won't move if automation is "Off").
Only `set` works — `get`/`up`/`down` require command 149 which Pro Tools hasn't implemented yet.
```
$BIN/agent-track-volume --session <SESSION.ptx> --track <NAME> --output-json set --value 2.0
$BIN/agent-track-volume --session <SESSION.ptx> --track <NAME> --output-json set --value -6.0
$BIN/agent-track-volume --session <SESSION.ptx> --track <NAME> --output-json set --value 0.0
```

#### agent-transport play / stop `[open-session]`
Start or stop playback. Checks current state first — only toggles if needed (idempotent). No session open/close.
```
$BIN/agent-transport --output-json play
$BIN/agent-transport --output-json stop
```

#### agent-manage-tracks create `[self-managing]`
Create a new stereo audio track.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json create
```

#### agent-manage-tracks hide `[self-managing]`
Hide a track by name.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json hide
```

#### agent-manage-tracks inactivate `[self-managing]`
Set a track as inactive. Fails with `PT_ForbiddenTrackType` for video/VCA/routing tracks.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_NAME> --output-json inactivate
```

#### agent-manage-tracks solo `[self-managing]`
Hide and inactivate ALL tracks except the one specified by `--track` (and Master). Processes tracks one by one, verifies results, and reports any failures.
```
$BIN/agent-manage-tracks --session <SESSION.ptx> --track <TRACK_TO_KEEP> --output-json solo
```
Output includes `verified_hidden`, `verified_inactive`, `still_active`, and `errors` arrays.

#### agent-bounce-export `[open-session]`
Bounce-export the current session to WAV or MP3. Requires a session to be open in Pro Tools.
```
$BIN/agent-bounce-export --output-json [--format <wav|mp3>] [--timeout <SECONDS>]
```
Default format: wav. Default timeout: 30s.

---

### Category 3: Standalone Audio Processing (no Pro Tools needed)

#### agent-apply-audio-filter `[standalone]`
Apply an FFmpeg audio filter chain to a file. Creates a new file with suffix.
```
$BIN/agent-apply-audio-filter <AUDIO_FILE> --output-json [--filter <FFMPEG_CHAIN>] [--sample-rate <HZ>] [--codec <CODEC>] [--suffix <SUFFIX>]
```
Defaults: filter=volume=10dB,compand...,loudnorm; sample-rate=48000; codec=pcm_s24le; suffix=_NET.

#### agent-bounce-normalize-tv `[standalone]`
Two-pass FFmpeg loudnorm normalization. Creates _NET and _TV variants.
```
$BIN/agent-bounce-normalize-tv <INPUT_WAV> --output-json [--target-lufs <LUFS>] [--output-dir <DIR>]
```
Default target: -23.0 LUFS.

#### agent-bounce-organize `[standalone]`
Move/copy a bounced file to the central BOUNCE folder.
```
$BIN/agent-bounce-organize <FILE_PATH> --output-json [--keep-local]
```

#### agent-convert-mp3 `[standalone]`
Convert audio file to MP3 via FFmpeg (libmp3lame VBR).
```
$BIN/agent-convert-mp3 <AUDIO_FILE> --output-json [--quality <0-9>] [--output-dir <DIR>]
```
Default quality: 2 (high).

#### agent-maximize-audio `[standalone]`
Peak-normalize audio to maximize volume without clipping.
```
$BIN/agent-maximize-audio <FILE_OR_FOLDER> --output-json
```

#### agent-extract-text `[standalone]`
Extract plain text from a document (docx, doc, rtf, txt) via macOS textutil.
```
$BIN/agent-extract-text <DOCUMENT_FILE> --output-json
```

---

### Category 4: Text/Transcription (requires GROQ_API_KEY)

#### agent-transcribe-audio `[standalone]`
Transcribe audio using Groq Whisper API.
```
$BIN/agent-transcribe-audio <AUDIO_FILE> --output-json [--model <MODEL>]
```
Default model: whisper-large-v3-turbo.

#### agent-compare-texts `[standalone]`
Compare reference text with transcription using Groq LLM.
```
$BIN/agent-compare-texts --reference <REF_FILE> --transcription <TRANS_FILE> --output-json [--prompt <PROMPT_FILE>] [--model <MODEL>]
```
Default model: llama-3.3-70b-versatile.
