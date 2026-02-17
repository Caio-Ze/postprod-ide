# PostProd Tools — Automations Guide

## How it works

Each button on the dashboard is a `.toml` file in this folder:

```
~/ProTools_Suite/config/automations/
```

One file = one button. Add a file, get a button. Delete a file, lose a button. The dashboard picks up changes automatically — no restart needed.

## File format

```toml
id = "my-automation"
label = "My Button"
description = "What this button does"
icon = "play"
prompt = """
Using the PTSL agent tools on {session_path}:
1. Verify session is open (use get_session_path)
2. Do the thing
3. Report results
"""
```

### Fields

| Field | Required | What it does |
|-------|----------|-------------|
| `id` | yes | Unique identifier. Must match the filename (without `.toml`) |
| `label` | yes | Button text shown on the dashboard |
| `description` | yes | Short description shown below the label |
| `icon` | yes | Icon name (see below) |
| `prompt` | yes | The instructions the AI agent will follow |
| `hidden` | no | Set to `true` to hide the button without deleting the file |

### Available icons

`play`, `zap`, `mic`, `folder`, `audio`, `sparkle`, `replace`, `arrow_up_right`

## Template variables

The dashboard replaces these variables in your prompt before the agent sees it:

- **`{session_path}`** — the `.ptx` file currently open in Pro Tools
- **`{pasta_ativa}`** — the folder selected on the dashboard

Use these so your prompts work with whatever session or folder is active.

## Prompt style

- Start with `Using the PTSL agent tools on {session_path}:` for single-session tasks
- Start with `Using the PTSL agent tools, process sessions in {pasta_ativa}.` for batch/folder tasks
- List numbered steps (4-8 lines). Be direct.
- Always include a session verification step before PTSL operations
- The agent already knows all the PTSL tools and domain rules from the skill files — don't repeat them

## Meta-automations

Files starting with `_` (underscore) are treated as meta-automations — buttons that manage other automations rather than doing Pro Tools work. They appear at the top of the list with a distinct visual style.

Built-in meta-automations:

| File | What it does |
|------|-------------|
| `_create-automation.toml` | Walks you through creating a new button |
| `_edit-automation.toml` | Helps you modify or delete an existing button |
| `_finetune-automation.toml` | Runs an automation, catches errors, fixes the prompt |

## Tips

- Look at the existing `.toml` files for examples
- Test your prompt by running the button, then use Fine-Tune to fix issues
- To temporarily disable a button, add `hidden = true` instead of deleting the file
- Back up your automations folder before experimenting
