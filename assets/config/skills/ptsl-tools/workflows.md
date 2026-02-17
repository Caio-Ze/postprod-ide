# Common Workflow Chains

Multi-step workflow examples using the agent tools. For individual tool syntax, see **reference.md**.

---

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
# 1. Save a backup copy (saved in the same directory as the original)
$BIN/agent-save-session-as --session "$PTX" --name "Session_Backup" --output-json

# 2. Now safely delete tracks from the original
$BIN/agent-delete-tracks --session "$PTX" --track "unwanted_track_1" --track "unwanted_track_2" --output-json
```

### Save Incremented Version
```bash
# Auto-increment session version (Session_V2 -> Session_V3)
$BIN/agent-save-session-increment --session "$PTX" --output-json
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
