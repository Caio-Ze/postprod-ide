//! Round-trip TOML edits for dashboard-owned config files.
//!
//! Every function here is a pure file mutation: read → `toml_edit` edit →
//! write. They preserve comments, formatting, and field ordering. Callers
//! (the dashboard controller, the automation picker) remain responsible for
//! choosing when to spawn background I/O, updating in-memory state after a
//! successful write, and notifying the UI.

use crate::{ContextEntry, PipelineStep, ScheduleConfig};

use anyhow::Result;
use std::path::{Path, PathBuf};

fn read_doc(path: &Path) -> Result<toml_edit::DocumentMut> {
    let content = std::fs::read_to_string(path)?;
    Ok(content.parse::<toml_edit::DocumentMut>()?)
}

fn write_doc(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// Write the `[schedule]` block of an automation file. Creates the table if
/// missing. Writes `enabled` unconditionally and `cron` only when non-empty
/// (matches the prior dashboard behavior, which left the existing `cron`
/// field in place rather than clobbering it with an empty string).
pub fn write_schedule(path: &Path, schedule: &ScheduleConfig) -> Result<()> {
    let mut doc = read_doc(path)?;

    let table = doc
        .entry("schedule")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));

    if let Some(table) = table.as_table_mut() {
        table.insert("enabled", toml_edit::value(schedule.enabled));
        if !schedule.cron.is_empty() {
            table.insert("cron", toml_edit::value(&schedule.cron));
        }
    }

    write_doc(path, &doc)
}

/// Replace the `[[step]]` array of a pipeline TOML with the supplied list.
/// Removes the key entirely when the list is empty.
pub fn write_pipeline_steps(path: &Path, steps: &[PipelineStep]) -> Result<()> {
    let mut doc = read_doc(path)?;

    doc.remove("step");
    if !steps.is_empty() {
        let mut array = toml_edit::ArrayOfTables::new();
        for step in steps {
            array.push(pipeline_step_table(step));
        }
        doc.insert("step", toml_edit::Item::ArrayOfTables(array));
    }

    write_doc(path, &doc)
}

/// Replace the `[[context]]` array of an automation TOML with the supplied
/// list. Removes the key entirely when the list is empty.
pub fn write_context_entries(path: &Path, contexts: &[ContextEntry]) -> Result<()> {
    let mut doc = read_doc(path)?;

    doc.remove("context");
    if !contexts.is_empty() {
        let mut array = toml_edit::ArrayOfTables::new();
        for ctx in contexts {
            array.push(context_entry_table(ctx));
        }
        doc.insert("context", toml_edit::Item::ArrayOfTables(array));
    }

    write_doc(path, &doc)
}

/// Toggle the top-level `skip_default_context` field. Inserts the key when
/// true, removes it when false (matches the prior dashboard behavior).
pub fn set_skip_default_context(path: &Path, enabled: bool) -> Result<()> {
    let mut doc = read_doc(path)?;
    if enabled {
        doc.insert("skip_default_context", toml_edit::value(true));
    } else {
        doc.remove("skip_default_context");
    }
    write_doc(path, &doc)
}

/// Append a single step to a pipeline's `[[step]]` array. Creates the array
/// when missing.
pub fn append_pipeline_step(path: &Path, step: &PipelineStep) -> Result<()> {
    let mut doc = read_doc(path)?;

    let steps = doc
        .entry("step")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));

    if let Some(array) = steps.as_array_of_tables_mut() {
        array.push(pipeline_step_table(step));
    }

    write_doc(path, &doc)
}

/// Append a non-required script context entry to the automation's
/// `[[context]]` array. `label` is usually derived from the script basename.
pub fn append_context_script(path: &Path, script_name: &str, label: &str) -> Result<()> {
    let mut doc = read_doc(path)?;

    let contexts = doc
        .entry("context")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));

    if let Some(array) = contexts.as_array_of_tables_mut() {
        let mut table = toml_edit::Table::new();
        table.insert("source", toml_edit::value("script"));
        table.insert("script", toml_edit::value(script_name));
        table.insert("label", toml_edit::value(label));
        table.insert("required", toml_edit::value(false));
        array.push(table);
    }

    write_doc(path, &doc)
}

/// Information about a freshly-created automation stub on disk.
#[derive(Debug, Clone)]
pub struct CreatedAutomation {
    pub id: String,
    pub path: PathBuf,
}

/// Information about a freshly-created pipeline stub on disk.
#[derive(Debug, Clone)]
pub struct CreatedPipeline {
    pub id: String,
    pub path: PathBuf,
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

fn pick_unused_path(dir: &Path, base_id: &str) -> (String, PathBuf) {
    let mut id = base_id.to_string();
    let mut path = dir.join(format!("{id}.toml"));
    let mut counter = 2u32;
    while path.exists() {
        id = format!("{base_id}-{counter}");
        path = dir.join(format!("{id}.toml"));
        counter += 1;
    }
    (id, path)
}

/// Create a minimal automation TOML stub in `dir`. Ensures the directory
/// exists and picks a collision-free id by suffixing `-2`, `-3`, ...
pub fn create_automation_stub(dir: &Path, name: &str) -> Result<CreatedAutomation> {
    std::fs::create_dir_all(dir)?;
    let base_id = slugify(name);
    let (id, path) = pick_unused_path(dir, &base_id);
    let content = format!(
        "id = \"{id}\"\nlabel = \"{name}\"\ndescription = \"\"\nicon = \"zap\"\nprompt_file = \"\"\n"
    );
    std::fs::write(&path, content)?;
    Ok(CreatedAutomation { id, path })
}

/// Create a minimal pipeline TOML stub in `dir/pipelines/`. The stub uses
/// the id prefix `pipeline-<slug>` to match the existing dashboard pattern.
pub fn create_pipeline_stub(dir: &Path, name: &str) -> Result<CreatedPipeline> {
    let pipelines_dir = dir.join("pipelines");
    std::fs::create_dir_all(&pipelines_dir)?;
    let base_id = format!("pipeline-{}", slugify(name));
    let (id, path) = pick_unused_path(&pipelines_dir, &base_id);
    let content = format!(
        "id = \"{id}\"\nlabel = \"{name}\"\ndescription = \"\"\nicon = \"zap\"\ntype = \"pipeline\"\n"
    );
    std::fs::write(&path, content)?;
    Ok(CreatedPipeline { id, path })
}

// ---------------------------------------------------------------------------
// Internal table builders — keep the serialized layout in one place so all
// write paths produce identical field ordering and defaults.
// ---------------------------------------------------------------------------

fn pipeline_step_table(step: &PipelineStep) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    if let Some(auto_id) = &step.automation {
        table.insert("automation", toml_edit::value(auto_id.as_str()));
    }
    if let Some(tool_id) = &step.tool {
        table.insert("tool", toml_edit::value(tool_id.as_str()));
    }
    if let Some(group) = step.group {
        table.insert("group", toml_edit::value(group as i64));
    }
    table
}

fn context_entry_table(ctx: &ContextEntry) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.insert("source", toml_edit::value(&ctx.source_type));
    table.insert("label", toml_edit::value(&ctx.label));
    if let Some(path) = &ctx.path {
        table.insert("path", toml_edit::value(path.as_str()));
    }
    if let Some(script) = &ctx.script {
        table.insert("script", toml_edit::value(script.as_str()));
    }
    if !ctx.required {
        table.insert("required", toml_edit::value(false));
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_single_automation;

    fn tmp_automation(body: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("auto.toml");
        std::fs::write(&path, body).expect("write");
        (tmp, path)
    }

    #[test]
    fn schedule_write_round_trip_preserves_comments() {
        let body = "\
id = \"auto\"
label = \"Auto\"
# keep this comment
description = \"x\"
icon = \"zap\"
prompt = \"p\"
";
        let (_tmp, path) = tmp_automation(body);
        let schedule = ScheduleConfig {
            enabled: true,
            cron: "0 * * * *".into(),
            ..Default::default()
        };
        write_schedule(&path, &schedule).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("# keep this comment"), "comment lost:\n{out}");
        assert!(out.contains("enabled = true"));
        assert!(out.contains("cron = \"0 * * * *\""));
    }

    #[test]
    fn schedule_write_updates_existing_table() {
        let body = "\
id = \"a\"
label = \"A\"
description = \"\"
icon = \"zap\"
prompt = \"p\"

[schedule]
enabled = false
cron = \"old\"
";
        let (_tmp, path) = tmp_automation(body);
        let schedule = ScheduleConfig {
            enabled: true,
            cron: "0 9 * * *".into(),
            ..Default::default()
        };
        write_schedule(&path, &schedule).unwrap();

        let entry = load_single_automation(&path, path.parent().unwrap()).unwrap();
        let sched = entry.schedule.expect("schedule");
        assert!(sched.enabled);
        assert_eq!(sched.cron, "0 9 * * *");
    }

    #[test]
    fn schedule_write_preserves_existing_cron_when_empty_supplied() {
        let body = "\
id = \"a\"
label = \"A\"
description = \"\"
icon = \"zap\"
prompt = \"p\"

[schedule]
enabled = true
cron = \"0 9 * * *\"
";
        let (_tmp, path) = tmp_automation(body);
        let schedule = ScheduleConfig {
            enabled: false,
            cron: String::new(),
            ..Default::default()
        };
        write_schedule(&path, &schedule).unwrap();

        let entry = load_single_automation(&path, path.parent().unwrap()).unwrap();
        let sched = entry.schedule.expect("schedule");
        assert!(!sched.enabled);
        // The prior cron remains — matching the dashboard's historical behavior
        // (empty cron strings do not clobber a populated field).
        assert_eq!(sched.cron, "0 9 * * *");
    }

    #[test]
    fn pipeline_steps_round_trip() {
        let body = "\
id = \"pipe\"
label = \"Pipe\"
description = \"\"
icon = \"zap\"
type = \"pipeline\"

[[step]]
automation = \"old-step\"
";
        let (_tmp, path) = tmp_automation(body);

        let steps = vec![
            PipelineStep {
                automation: Some("scan".into()),
                tool: None,
                group: None,
            },
            PipelineStep {
                automation: Some("review".into()),
                tool: None,
                group: Some(2),
            },
            PipelineStep {
                automation: None,
                tool: Some("launcher".into()),
                group: None,
            },
        ];
        write_pipeline_steps(&path, &steps).unwrap();

        let entry = load_single_automation(&path, path.parent().unwrap()).unwrap();
        assert!(entry.is_pipeline());
        assert_eq!(entry.steps.len(), 3);
        assert_eq!(entry.steps[0].automation.as_deref(), Some("scan"));
        assert_eq!(entry.steps[1].group, Some(2));
        assert_eq!(entry.steps[2].tool.as_deref(), Some("launcher"));
    }

    #[test]
    fn pipeline_steps_empty_removes_step_key() {
        let body = "\
id = \"pipe\"
label = \"Pipe\"
description = \"\"
icon = \"zap\"
type = \"pipeline\"

[[step]]
automation = \"scan\"
";
        let (_tmp, path) = tmp_automation(body);
        write_pipeline_steps(&path, &[]).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(!out.contains("[[step]]"));
        assert!(!out.contains("automation = \"scan\""));
    }

    #[test]
    fn context_entries_round_trip() {
        let body = "\
id = \"auto\"
label = \"Auto\"
description = \"\"
icon = \"zap\"
prompt = \"p\"
";
        let (_tmp, path) = tmp_automation(body);
        let contexts = vec![
            ContextEntry {
                source_type: "path".into(),
                label: "Notes".into(),
                path: Some("/tmp/notes.md".into()),
                script: None,
                required: true,
            },
            ContextEntry {
                source_type: "script".into(),
                label: "Status".into(),
                path: None,
                script: Some("status.sh".into()),
                required: false,
            },
        ];
        write_context_entries(&path, &contexts).unwrap();

        let entry = load_single_automation(&path, path.parent().unwrap()).unwrap();
        assert_eq!(entry.contexts.len(), 2);
        assert_eq!(entry.contexts[0].source_type, "path");
        assert!(entry.contexts[0].required);
        assert_eq!(entry.contexts[1].source_type, "script");
        assert!(!entry.contexts[1].required);
    }

    #[test]
    fn set_skip_default_context_toggles_key() {
        let body = "\
id = \"a\"
label = \"A\"
description = \"\"
icon = \"zap\"
prompt = \"p\"
";
        let (_tmp, path) = tmp_automation(body);

        set_skip_default_context(&path, true).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("skip_default_context = true"));

        set_skip_default_context(&path, false).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(!out.contains("skip_default_context"));
    }

    #[test]
    fn append_pipeline_step_preserves_existing_steps() {
        let body = "\
id = \"pipe\"
label = \"Pipe\"
description = \"\"
icon = \"zap\"
type = \"pipeline\"

[[step]]
automation = \"first\"
";
        let (_tmp, path) = tmp_automation(body);
        let step = PipelineStep {
            automation: Some("second".into()),
            tool: None,
            group: None,
        };
        append_pipeline_step(&path, &step).unwrap();

        let entry = load_single_automation(&path, path.parent().unwrap()).unwrap();
        assert_eq!(entry.steps.len(), 2);
        assert_eq!(entry.steps[0].automation.as_deref(), Some("first"));
        assert_eq!(entry.steps[1].automation.as_deref(), Some("second"));
    }

    #[test]
    fn append_context_script_adds_non_required_entry() {
        let body = "\
id = \"a\"
label = \"A\"
description = \"\"
icon = \"zap\"
prompt = \"p\"
";
        let (_tmp, path) = tmp_automation(body);
        append_context_script(&path, "status.sh", "Status").unwrap();

        let entry = load_single_automation(&path, path.parent().unwrap()).unwrap();
        assert_eq!(entry.contexts.len(), 1);
        assert_eq!(entry.contexts[0].source_type, "script");
        assert_eq!(entry.contexts[0].script.as_deref(), Some("status.sh"));
        assert_eq!(entry.contexts[0].label, "Status");
        assert!(!entry.contexts[0].required);
    }

    #[test]
    fn create_automation_stub_unique_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let first = create_automation_stub(dir, "My Automation").unwrap();
        assert_eq!(first.id, "my-automation");
        assert!(first.path.exists());

        let second = create_automation_stub(dir, "My Automation").unwrap();
        assert_eq!(second.id, "my-automation-2");
        assert!(second.path.exists());
        assert_ne!(first.path, second.path);

        // Loadable via the standard loader
        let entry = load_single_automation(&first.path, dir).unwrap();
        assert_eq!(entry.id, "my-automation");
        assert_eq!(entry.label, "My Automation");
    }

    #[test]
    fn create_pipeline_stub_goes_into_pipelines_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let created = create_pipeline_stub(dir, "Quality Cycle").unwrap();
        assert_eq!(created.id, "pipeline-quality-cycle");
        assert!(created.path.starts_with(dir.join("pipelines")));

        let entry = load_single_automation(&created.path, dir).unwrap();
        assert!(entry.is_pipeline());
    }
}
