pub mod note_store;

use anyhow::Result;
use collections::HashMap;
use editor::SelectionEffects;
use editor::{CurrentLineHighlight, Editor, EditorElement, EditorEvent, EditorStyle, actions::Tab};
use gpui::{
    App, Bounds, DEFAULT_ADDITIONAL_WINDOW_SIZE, Entity, EventEmitter, Focusable, PromptLevel,
    Subscription, Task, TextStyle, Tiling, TitlebarOptions, WindowBounds, WindowHandle,
    WindowOptions, actions, point, size, transparent_black,
};
use language::{Buffer, LanguageRegistry, language_settings::SoftWrap};
use language_model::{ConfiguredModel, LanguageModelRegistry};
use note_store::*;
use picker::{Picker, PickerDelegate};
use platform_title_bar::PlatformTitleBar;
use postprod_dashboard_config::{ResolvedAutomationInfo, ResolvedContextInfo};
use release_channel::ReleaseChannel;
use rope::Rope;
use settings::{ActionSequence, Settings};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use theme_settings::ThemeSettings;
use ui::{Divider, ListItem, ListItemSpacing, ListSubHeader, Tooltip, prelude::*};
use ui_input::ErasedEditor;
use util::{ResultExt, TryFutureExt};
use workspace::{MultiWorkspace, Workspace, WorkspaceSettings, client_side_decorations};
use zed_actions::assistant::InlineAssist;

pub fn init(_cx: &mut App) {}

actions!(
    postprod_rules,
    [
        /// Creates a new note in the PostProd Rules window.
        NewNote,
        /// Deletes the selected note.
        DeleteNote,
        /// Duplicates the selected note.
        DuplicateNote,
        /// Toggles whether the selected note is a default note.
        ToggleDefaultNote,
        /// Promotes the active file out of an automation's `[[context]]` into
        /// `config/default-context/` (so every automation includes it).
        AddFileToDefaultContext,
        /// Strips the active file's `[[context]]` entry from its automation's
        /// TOML. Does not delete the file from disk.
        RemoveFileFromAutomationContext,
    ]
);

pub trait InlineAssistDelegate {
    fn assist(
        &self,
        prompt_editor: &Entity<Editor>,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<PostProdRules>,
    );

    fn focus_agent_panel(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool;
}

// ---------------------------------------------------------------------------
// Public types for Dashboard integration
// ---------------------------------------------------------------------------

/// Selection target for deep linking into the window (used in general mode).
pub enum SelectionTarget {
    Note(NoteId),
    PromptFile(String),
}

/// Window operating mode, set at construction.
///
/// `Scoped` carries a `ResolvedAutomationInfo` (defined in
/// `postprod_dashboard_config`), which is the shared cross-crate view of a
/// dashboard automation with its context paths already resolved.
pub enum WindowMode {
    /// Card "Edit Prompt" button — picker scoped to one automation.
    Scoped(ResolvedAutomationInfo),
    /// Header button / keybinding — browse everything.
    General(Option<SelectionTarget>),
}

/// Callbacks for the window to modify TOML context entries via Dashboard.
/// The closures run in the calling window's App context.
pub struct ContextCallbacks {
    pub reorder: Box<dyn Fn(&str, usize, i32, &mut App) + Send + Sync>,
    pub remove: Box<dyn Fn(&str, usize, &mut App) + Send + Sync>,
    pub add_path: Box<dyn Fn(&str, PathBuf, &mut App) + Send + Sync>,
    pub add_script: Box<dyn Fn(&str, String, &mut App) + Send + Sync>,
    /// 10.9: Move a file into `config/default-context/`. The third arg
    /// `Option<usize>` is `Some(toml_index)` when the source is a top-level
    /// `[[context]]` entry to strip after the move; `None` when the source is
    /// a child of a directory `[[context]]` (parent folder entry stays).
    pub add_to_default_context: Box<dyn Fn(PathBuf, &str, Option<usize>, &mut App) + Send + Sync>,
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum PickerSection {
    Prompt,
    DefaultContext,
}

#[derive(Clone, Debug)]
struct PromptFileEntry {
    pub(crate) filename: String,
    pub(crate) display_name: String,
    pub(crate) path: PathBuf,
    #[allow(dead_code)]
    pub(crate) section: PickerSection,
    pub(crate) is_symlink: bool,
}

#[derive(Clone, Debug)]
struct ContextSourceEntry {
    filename: String,
    resolved_path: PathBuf,
    source_type: String,
    is_directory: bool,
    toml_index: usize,
    automation_id: String,
}

#[derive(Clone, Debug, PartialEq)]
enum ActiveEntryId {
    Note(NoteId),
    File(PathBuf),
}

/// 10.4: The role of a file path within the current `PostProdRules` data
/// model. Computed at render time by `classify_file_role` — pure function over
/// `(path, config_root, automations)`. Driving the editor toolbar shape and
/// action-handler dispatch (see 10.11 table).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FileRole {
    Prompt,
    ContextSource {
        automation_id: String,
        toml_index: usize,
    },
    FolderChildOf {
        automation_id: String,
        #[allow(dead_code)]
        parent_toml_index: usize,
    },
    DefaultContext,
    /// File loaded but doesn't match any known section. Defensive fallback —
    /// no toolbar action icons should be active for this role.
    Unknown,
}

/// 10.4: Pure classification — no GPUI, no `Self`. Directly unit-testable.
/// Stateless: derived from `config_root` + each automation's resolved
/// `[[context]]` entries. Robust against picker-state drift — the picker
/// strips section info at confirm time (`ActiveEntryId::File(path)`), so the
/// toolbar derives the role here at render time instead.
pub(crate) fn classify_file_role(
    path: &Path,
    config_root: &Path,
    automations: &[ResolvedAutomationInfo],
) -> FileRole {
    let prompts_dir = config_root.join("config/prompts");
    let default_dir = config_root.join("config/default-context");
    if path.starts_with(&prompts_dir) {
        return FileRole::Prompt;
    }
    if path.starts_with(&default_dir) {
        return FileRole::DefaultContext;
    }
    for automation in automations {
        for (ix, ctx) in automation.contexts.iter().enumerate() {
            if ctx.resolved_path == path {
                return FileRole::ContextSource {
                    automation_id: automation.id.clone(),
                    toml_index: ix,
                };
            }
            if ctx.resolved_path.is_dir() && path.starts_with(&ctx.resolved_path) {
                return FileRole::FolderChildOf {
                    automation_id: automation.id.clone(),
                    parent_toml_index: ix,
                };
            }
        }
    }
    FileRole::Unknown
}

/// 10.7 pre-flight: failure modes for promoting a file into `default-context/`.
/// Stateless / I/O-only-via-explicit-deps so the validation is unit-testable
/// without GPUI. Used by `add_active_file_to_default_context` before opening
/// the confirmation modal.
#[derive(Debug, PartialEq)]
pub(crate) enum PromotionPreflightError {
    NotMarkdown,
    Symlink,
    Collision,
    SourceMissing,
}

/// Pre-flight gate. Returns `Ok(target_path)` when the move can proceed.
pub(crate) fn validate_promotion(
    source_path: &Path,
    target_dir: &Path,
) -> Result<PathBuf, PromotionPreflightError> {
    if source_path.extension().and_then(|s| s.to_str()) != Some("md") {
        return Err(PromotionPreflightError::NotMarkdown);
    }
    let meta = std::fs::symlink_metadata(source_path)
        .map_err(|_| PromotionPreflightError::SourceMissing)?;
    if meta.file_type().is_symlink() {
        return Err(PromotionPreflightError::Symlink);
    }
    let basename = source_path
        .file_name()
        .ok_or(PromotionPreflightError::SourceMissing)?;
    let target = target_dir.join(basename);
    if target.exists() {
        return Err(PromotionPreflightError::Collision);
    }
    Ok(target)
}

/// 10.5: Union of note-state and file-role, consumed by `render_entry_toolbar`
/// to render the editor-toolbar action group on the right.
///
/// File-variant fields (`automation_id`, `toml_index`, `source_path`) are
/// retained for spec parity even though `render_entry_toolbar` only switches
/// on the variant tag — toolbar buttons dispatch GPUI actions, and the
/// action handlers re-derive these values via `classify_file_role` at handler
/// time (10.11). Keeping the data in the role lets future call sites (e.g.
/// status indicators, breadcrumbs) reuse it without re-classifying.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) enum EntryToolbarRole {
    Note {
        default: bool,
    },
    ContextSource {
        automation_id: String,
        toml_index: usize,
        source_path: PathBuf,
    },
    FolderChild {
        automation_id: String,
        source_path: PathBuf,
    },
    Prompt,
    DefaultContext,
    Unknown,
}

// ---------------------------------------------------------------------------
// Window open
// ---------------------------------------------------------------------------

pub fn open_postprod_rules(
    store: Entity<NoteStore>,
    language_registry: Arc<LanguageRegistry>,
    inline_assist_delegate: Box<dyn InlineAssistDelegate>,
    config_root: PathBuf,
    automations: Vec<ResolvedAutomationInfo>,
    context_callbacks: Option<Arc<ContextCallbacks>>,
    mode: WindowMode,
    cx: &mut App,
) -> Task<Result<WindowHandle<PostProdRules>>> {
    cx.spawn(async move |cx| {
        // Close any existing window — always reopen fresh to avoid stale state
        cx.update(|cx| {
            if let Some(existing_window) = cx
                .windows()
                .into_iter()
                .find_map(|window| window.downcast::<PostProdRules>())
            {
                existing_window
                    .update(cx, |_, window, _cx| window.remove_window())
                    .ok();
            }
        });

        cx.update(|cx| {
            let app_id = ReleaseChannel::global(cx).app_id();
            let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
            let window_decorations = match std::env::var("ZED_WINDOW_DECORATIONS") {
                Ok(val) if val == "server" => gpui::WindowDecorations::Server,
                Ok(val) if val == "client" => gpui::WindowDecorations::Client,
                _ => match WorkspaceSettings::get_global(cx).window_decorations {
                    settings::WindowDecorations::Server => gpui::WindowDecorations::Server,
                    settings::WindowDecorations::Client => gpui::WindowDecorations::Client,
                },
            };
            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitlebarOptions {
                        title: Some("Prompts & Notes".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(12.0), px(12.0))),
                    }),
                    app_id: Some(app_id.to_owned()),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_background: cx.theme().window_background_appearance(),
                    window_decorations: Some(window_decorations),
                    window_min_size: Some(DEFAULT_ADDITIONAL_WINDOW_SIZE),
                    // `Floating` (inherited from upstream `rules_library` at
                    // Phase 1) puts the window at NSFloatingWindowLevel on
                    // macOS — above NSOpenPanel's window level. That makes
                    // `cx.prompt_for_paths` (the "+ Add Context" file
                    // picker) physically un-raisable: the OS window server
                    // refuses to bring a Normal-level window in front of a
                    // Floating-level window of the same app, so even
                    // clicking the OS picker leaves it stuck behind. Our
                    // use case is a heavy editing surface, not a quick
                    // reference pop-over, so Normal is the right kind.
                    kind: gpui::WindowKind::Normal,
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        PostProdRules::new(
                            store,
                            language_registry,
                            inline_assist_delegate,
                            config_root,
                            automations,
                            context_callbacks,
                            mode,
                            window,
                            cx,
                        )
                    })
                },
            )
        })
    })
}

// ---------------------------------------------------------------------------
// Main struct
// ---------------------------------------------------------------------------

pub struct PostProdRules {
    title_bar: Option<Entity<PlatformTitleBar>>,
    store: Entity<NoteStore>,
    language_registry: Arc<LanguageRegistry>,
    config_root: PathBuf,
    mode: WindowMode,
    automations: Vec<ResolvedAutomationInfo>,
    context_callbacks: Option<Arc<ContextCallbacks>>,
    note_editors: HashMap<NoteId, NoteEditor>,
    file_editors: HashMap<PathBuf, FileEditor>,
    active_entry: Option<ActiveEntryId>,
    picker: Entity<Picker<PostProdPickerDelegate>>,
    pending_load: Task<()>,
    inline_assist_delegate: Box<dyn InlineAssistDelegate>,
    _subscriptions: Vec<Subscription>,
}

struct NoteEditor {
    title_editor: Entity<Editor>,
    body_editor: Entity<Editor>,
    token_count: Option<u64>,
    pending_token_count: Task<Option<()>>,
    next_title_and_body_to_save: Option<(String, Rope)>,
    pending_save: Option<Task<Option<()>>>,
    _subscriptions: Vec<Subscription>,
}

struct FileEditor {
    title: SharedString,
    body_editor: Entity<Editor>,
    is_symlink: bool,
    #[allow(dead_code)]
    path: PathBuf,
    token_count: Option<u64>,
    pending_token_count: Task<Option<()>>,
    next_body_to_save: Option<Rope>,
    pending_save: Option<Task<Option<()>>>,
    _subscriptions: Vec<Subscription>,
}

// ---------------------------------------------------------------------------
// Picker types
// ---------------------------------------------------------------------------

enum PostProdPickerEntry {
    Header(SharedString),
    Note(NoteMetadata),
    PromptFile(PromptFileEntry),
    DefaultContextFile(PromptFileEntry),
    ContextSource(ContextSourceEntry),
    FolderChild(ContextSourceEntry),
    AddContextAction(String),
    AddNoteAction(String),
    Separator,
}

struct PostProdPickerDelegate {
    store: Entity<NoteStore>,
    prompt_files: Vec<PromptFileEntry>,
    default_context_files: Vec<PromptFileEntry>,
    scoped_automation: Option<ResolvedAutomationInfo>,
    expanded_folders: collections::HashSet<PathBuf>,
    selected_index: usize,
    filtered_entries: Vec<PostProdPickerEntry>,
}

enum PostProdPickerEvent {
    Selected {
        entry_id: ActiveEntryId,
    },
    Confirmed {
        entry_id: ActiveEntryId,
    },
    Deleted {
        note_id: NoteId,
    },
    ToggledDefault {
        note_id: NoteId,
    },
    ContextReorder {
        automation_id: String,
        toml_index: usize,
        direction: i32,
    },
    ContextRemove {
        automation_id: String,
        toml_index: usize,
    },
    AddContextFile {
        automation_id: String,
    },
    #[allow(dead_code)]
    AddContextScript {
        automation_id: String,
    },
    AddScopedNote {
        automation_id: String,
    },
    /// 10.10: Picker hover-row Paperclip on a FolderChild row. Editor-toolbar
    /// Paperclip uses the action-dispatch path (10.11), not this event.
    AddToDefaultContext {
        source_path: PathBuf,
        automation_id: String,
        /// `Some(ix)` for top-level CONTEXT SOURCES; `None` for FolderChild (the
        /// parent folder's `[[context]]` must NOT be stripped).
        toml_index_to_strip: Option<usize>,
    },
}

impl EventEmitter<PostProdPickerEvent> for Picker<PostProdPickerDelegate> {}

impl PostProdPickerDelegate {
    /// Toggle folder expansion in the picker. Inserts/removes child entries in-place.
    fn toggle_folder_expansion(&mut self, folder_path: &Path, cx: &mut Context<Picker<Self>>) {
        if self.expanded_folders.contains(folder_path) {
            // Collapse: remove FolderChild entries following this folder
            self.expanded_folders.remove(folder_path);
            let folder_ix = self
                .filtered_entries
                .iter()
                .position(|e| matches!(e, PostProdPickerEntry::ContextSource(c) if c.resolved_path == folder_path));
            if let Some(ix) = folder_ix {
                let remove_start = ix + 1;
                let remove_count = self.filtered_entries[remove_start..]
                    .iter()
                    .take_while(|e| matches!(e, PostProdPickerEntry::FolderChild(_)))
                    .count();
                self.filtered_entries
                    .drain(remove_start..remove_start + remove_count);
            }
        } else {
            // Expand: scan one level and insert FolderChild entries
            self.expanded_folders.insert(folder_path.to_path_buf());
            let folder_ix = self
                .filtered_entries
                .iter()
                .position(|e| matches!(e, PostProdPickerEntry::ContextSource(c) if c.resolved_path == folder_path));

            let (automation_id, toml_index) = self
                .filtered_entries
                .iter()
                .find_map(|e| match e {
                    PostProdPickerEntry::ContextSource(c) if c.resolved_path == folder_path => {
                        Some((c.automation_id.clone(), c.toml_index))
                    }
                    _ => None,
                })
                .unwrap_or_default();

            if let Some(ix) = folder_ix {
                let mut children = Vec::new();
                if let Ok(read_dir) = std::fs::read_dir(folder_path) {
                    let mut child_paths: Vec<_> = read_dir
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .collect();
                    child_paths.sort();
                    for child_path in child_paths {
                        let filename = child_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        children.push(PostProdPickerEntry::FolderChild(ContextSourceEntry {
                            filename,
                            resolved_path: child_path,
                            source_type: "path".to_string(),
                            is_directory: false,
                            toml_index,
                            automation_id: automation_id.clone(),
                        }));
                    }
                }
                let insert_at = ix + 1;
                for (i, child) in children.into_iter().enumerate() {
                    self.filtered_entries.insert(insert_at + i, child);
                }
            }
        }
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// File scanning helpers
// ---------------------------------------------------------------------------

fn scan_md_files(dir: &Path, section: PickerSection) -> Vec<PromptFileEntry> {
    let mut entries = Vec::new();
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return entries,
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().map_or(true, |ext| ext != "md") {
            continue;
        }
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let display_name = filename.trim_end_matches(".md").replace(['-', '_'], " ");
        let is_symlink = std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        entries.push(PromptFileEntry {
            filename,
            display_name,
            path,
            section: section.clone(),
            is_symlink,
        });
    }
    entries.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    entries
}

/// Build picker entries for scoped mode (one automation's content).
fn build_scoped_entries(
    automation: &ResolvedAutomationInfo,
    prompt_files: &[PromptFileEntry],
    default_context_files: &[PromptFileEntry],
    note_matches: &[NoteMetadata],
    expanded_folders: &collections::HashSet<PathBuf>,
    query_lower: &str,
    entries: &mut Vec<PostProdPickerEntry>,
) {
    let matches_query = |name: &str| -> bool {
        query_lower.is_empty() || name.to_lowercase().contains(query_lower)
    };

    // Section: PROMPT — the automation's prompt file
    if let Some(ref prompt_filename) = automation.prompt_file {
        if let Some(prompt_entry) = prompt_files.iter().find(|f| &f.filename == prompt_filename) {
            if matches_query(&prompt_entry.display_name) {
                entries.push(PostProdPickerEntry::Header("PROMPT".into()));
                entries.push(PostProdPickerEntry::PromptFile(prompt_entry.clone()));
                entries.push(PostProdPickerEntry::Separator);
            }
        }
    }

    // Section: CONTEXT SOURCES — from the automation's context entries
    let filtered_contexts: Vec<_> = automation
        .contexts
        .iter()
        .enumerate()
        .filter(|(_, c)| matches_query(&c.label))
        .collect();
    // Always show section in scoped mode (allows adding even when empty)
    entries.push(PostProdPickerEntry::Header("CONTEXT SOURCES".into()));
    for (ix, ctx) in filtered_contexts {
        let is_directory = ctx.resolved_path.is_dir();
        let filename = ctx
            .resolved_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let source_entry = ContextSourceEntry {
            filename,
            resolved_path: ctx.resolved_path.clone(),
            source_type: ctx.source_type.clone(),
            is_directory,
            toml_index: ix,
            automation_id: automation.id.clone(),
        };
        entries.push(PostProdPickerEntry::ContextSource(source_entry));

        // Insert folder children if expanded
        if is_directory && expanded_folders.contains(&ctx.resolved_path) {
            if let Ok(read_dir) = std::fs::read_dir(&ctx.resolved_path) {
                let mut child_paths: Vec<_> = read_dir
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .collect();
                child_paths.sort();
                for child_path in child_paths {
                    let child_filename = child_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if matches_query(&child_filename) {
                        entries.push(PostProdPickerEntry::FolderChild(ContextSourceEntry {
                            filename: child_filename,
                            resolved_path: child_path,
                            source_type: "path".to_string(),
                            is_directory: false,
                            toml_index: ix,
                            automation_id: automation.id.clone(),
                        }));
                    }
                }
            }
        }
    }
    entries.push(PostProdPickerEntry::AddContextAction(automation.id.clone()));
    entries.push(PostProdPickerEntry::Separator);

    // Section: DEFAULT CONTEXT — skip if automation opts out
    if !automation.skip_default_context {
        let filtered_dc: Vec<_> = default_context_files
            .iter()
            .filter(|f| matches_query(&f.display_name))
            .collect();
        if !filtered_dc.is_empty() {
            entries.push(PostProdPickerEntry::Header("DEFAULT CONTEXT".into()));
            for f in filtered_dc {
                entries.push(PostProdPickerEntry::DefaultContextFile(f.clone()));
            }
            entries.push(PostProdPickerEntry::Separator);
        }
    }

    // Section: NOTES — default notes + notes assigned to this automation
    let automation_id = &automation.id;
    let relevant_notes: Vec<_> = note_matches
        .iter()
        .filter(|n| n.default || n.assigned_automations.iter().any(|a| a == automation_id))
        .cloned()
        .collect();
    entries.push(PostProdPickerEntry::Header("NOTES".into()));
    for note in relevant_notes {
        entries.push(PostProdPickerEntry::Note(note));
    }
    entries.push(PostProdPickerEntry::AddNoteAction(automation.id.clone()));
}

/// Build picker entries for general mode (browse everything).
fn build_general_entries(
    prompt_files: &[PromptFileEntry],
    default_context_files: &[PromptFileEntry],
    note_matches: Vec<NoteMetadata>,
    query_lower: &str,
    entries: &mut Vec<PostProdPickerEntry>,
) {
    // Filter prompt files by query
    let filtered_prompts: Vec<_> = if query_lower.is_empty() {
        prompt_files.to_vec()
    } else {
        prompt_files
            .iter()
            .filter(|f| f.display_name.to_lowercase().contains(query_lower))
            .cloned()
            .collect()
    };

    // Filter default context files by query
    let filtered_dc: Vec<_> = if query_lower.is_empty() {
        default_context_files.to_vec()
    } else {
        default_context_files
            .iter()
            .filter(|f| f.display_name.to_lowercase().contains(query_lower))
            .cloned()
            .collect()
    };

    // Section: PROMPTS
    if !filtered_prompts.is_empty() {
        entries.push(PostProdPickerEntry::Header("PROMPTS".into()));
        for f in filtered_prompts {
            entries.push(PostProdPickerEntry::PromptFile(f));
        }
        entries.push(PostProdPickerEntry::Separator);
    }

    // Section: NOTES
    let (default_notes, other_notes): (Vec<_>, Vec<_>) =
        note_matches.into_iter().partition(|note| note.default);

    if !default_notes.is_empty() || !other_notes.is_empty() {
        entries.push(PostProdPickerEntry::Header("NOTES".into()));
        for note in default_notes {
            entries.push(PostProdPickerEntry::Note(note));
        }
        for note in other_notes {
            entries.push(PostProdPickerEntry::Note(note));
        }
        entries.push(PostProdPickerEntry::Separator);
    }

    // Section: DEFAULT CONTEXT
    if !filtered_dc.is_empty() {
        entries.push(PostProdPickerEntry::Header("DEFAULT CONTEXT".into()));
        for f in filtered_dc {
            entries.push(PostProdPickerEntry::DefaultContextFile(f));
        }
    }
}

/// Scan for executable scripts in a directory.
fn scan_scripts(dir: &Path) -> Vec<String> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut scripts: Vec<String> = read_dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_file() {
                Some(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                )
            } else {
                None
            }
        })
        .collect();
    scripts.sort();
    scripts
}

fn entry_id_for_picker_entry(entry: &PostProdPickerEntry) -> Option<ActiveEntryId> {
    match entry {
        PostProdPickerEntry::Note(n) => Some(ActiveEntryId::Note(n.id)),
        PostProdPickerEntry::PromptFile(f) | PostProdPickerEntry::DefaultContextFile(f) => {
            Some(ActiveEntryId::File(f.path.clone()))
        }
        PostProdPickerEntry::ContextSource(c) | PostProdPickerEntry::FolderChild(c) => {
            if c.is_directory {
                None // Directories are grouping nodes, not selectable for editing
            } else {
                Some(ActiveEntryId::File(c.resolved_path.clone()))
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PickerDelegate
// ---------------------------------------------------------------------------

impl PickerDelegate for PostProdPickerDelegate {
    type ListItem = AnyElement;

    fn match_count(&self) -> usize {
        self.filtered_entries.len()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No entries found matching your search.".into())
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix.min(self.filtered_entries.len().saturating_sub(1));

        if let Some(entry_id) = self
            .filtered_entries
            .get(self.selected_index)
            .and_then(entry_id_for_picker_entry)
        {
            cx.emit(PostProdPickerEvent::Selected { entry_id });
        }

        cx.notify();
    }

    fn can_select(&self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) -> bool {
        match self.filtered_entries.get(ix) {
            Some(PostProdPickerEntry::Note(_))
            | Some(PostProdPickerEntry::PromptFile(_))
            | Some(PostProdPickerEntry::DefaultContextFile(_))
            | Some(PostProdPickerEntry::FolderChild(_)) => true,
            Some(PostProdPickerEntry::ContextSource(c)) => !c.is_directory,
            _ => false,
        }
    }

    fn select_on_hover(&self) -> bool {
        false
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search\u{2026}".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let cancellation_flag = Arc::new(AtomicBool::default());
        let search = self
            .store
            .read(cx)
            .search(query.clone(), cancellation_flag, cx);

        let prompt_files = self.prompt_files.clone();
        let default_context_files = self.default_context_files.clone();
        let scoped_automation = self.scoped_automation.clone();
        let expanded_folders = self.expanded_folders.clone();

        let prev_entry_id = self
            .filtered_entries
            .get(self.selected_index)
            .and_then(entry_id_for_picker_entry);

        cx.spawn_in(window, async move |this, cx| {
            let (filtered_entries, selected_index) = cx
                .background_spawn(async move {
                    let note_matches = search.await;
                    let query_lower = query.to_lowercase();

                    let mut filtered_entries = Vec::new();

                    if let Some(ref automation) = scoped_automation {
                        // --- Scoped mode: 4 sections from one automation ---
                        build_scoped_entries(
                            automation,
                            &prompt_files,
                            &default_context_files,
                            &note_matches,
                            &expanded_folders,
                            &query_lower,
                            &mut filtered_entries,
                        );
                    } else {
                        // --- General mode: all prompts, notes, default context ---
                        build_general_entries(
                            &prompt_files,
                            &default_context_files,
                            note_matches,
                            &query_lower,
                            &mut filtered_entries,
                        );
                    }

                    let selected_index = prev_entry_id
                        .and_then(|prev| {
                            filtered_entries.iter().position(|entry| {
                                entry_id_for_picker_entry(entry).as_ref() == Some(&prev)
                            })
                        })
                        .unwrap_or_else(|| {
                            filtered_entries
                                .iter()
                                .position(|entry| entry_id_for_picker_entry(entry).is_some())
                                .unwrap_or(0)
                        });

                    (filtered_entries, selected_index)
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                this.delegate.filtered_entries = filtered_entries;
                this.set_selected_index(
                    selected_index,
                    Some(picker::Direction::Down),
                    true,
                    window,
                    cx,
                );
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(entry_id) = self
            .filtered_entries
            .get(self.selected_index)
            .and_then(entry_id_for_picker_entry)
        {
            cx.emit(PostProdPickerEvent::Confirmed { entry_id });
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.filtered_entries.get(ix)? {
            PostProdPickerEntry::Header(title) => Some(
                ListSubHeader::new(title.clone())
                    .inset(true)
                    .into_any_element(),
            ),
            PostProdPickerEntry::Separator => Some(
                h_flex()
                    .py_1()
                    .child(Divider::horizontal())
                    .into_any_element(),
            ),
            PostProdPickerEntry::Note(note) => {
                let default = note.default;
                let note_id = note.id;

                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(
                            Icon::new(IconName::Notepad)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(note.title.clone().unwrap_or("Untitled".into()))
                                .truncate()
                                .mr_10(),
                        )
                        .end_slot::<IconButton>(default.then(|| {
                            IconButton::new("toggle-default-note", IconName::Paperclip)
                                .toggle_state(true)
                                .icon_color(Color::Accent)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Remove from Default Notes"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(PostProdPickerEvent::ToggledDefault { note_id })
                                }))
                        }))
                        .end_slot_on_hover(
                            h_flex()
                                .child(
                                    IconButton::new("delete-note", IconName::Trash)
                                        .icon_color(Color::Muted)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Delete Note"))
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(PostProdPickerEvent::Deleted { note_id })
                                        })),
                                )
                                .child(
                                    IconButton::new("toggle-default-note", IconName::Plus)
                                        .selected_icon(IconName::Dash)
                                        .toggle_state(default)
                                        .icon_size(IconSize::Small)
                                        .icon_color(if default {
                                            Color::Accent
                                        } else {
                                            Color::Muted
                                        })
                                        .map(|this| {
                                            if default {
                                                this.tooltip(Tooltip::text(
                                                    "Remove from Default Notes",
                                                ))
                                            } else {
                                                this.tooltip(move |_window, cx| {
                                                    Tooltip::with_meta(
                                                        "Add to Default Notes",
                                                        None,
                                                        "Always included in standalone runs.",
                                                        cx,
                                                    )
                                                })
                                            }
                                        })
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(PostProdPickerEvent::ToggledDefault { note_id })
                                        })),
                                ),
                        )
                        .into_any_element(),
                )
            }
            PostProdPickerEntry::PromptFile(file) => {
                let label: SharedString = file.filename.clone().into();
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(
                            Icon::new(IconName::File)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(label).truncate())
                        .into_any_element(),
                )
            }
            PostProdPickerEntry::DefaultContextFile(file) => {
                let label: SharedString = file.filename.clone().into();
                let symlink_target = if file.is_symlink {
                    std::fs::read_link(&file.path)
                        .ok()
                        .map(|t| SharedString::from(format!("symlink to {}", t.display())))
                } else {
                    None
                };
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(
                            Icon::new(IconName::Folder)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(label).truncate())
                        .when(file.is_symlink, |this| {
                            this.end_slot::<Label>(Some(
                                Label::new("symlink")
                                    .color(Color::Warning)
                                    .size(LabelSize::XSmall),
                            ))
                        })
                        .when_some(symlink_target, |this, target| {
                            this.tooltip(Tooltip::text(target))
                        })
                        .into_any_element(),
                )
            }
            PostProdPickerEntry::ContextSource(entry) => {
                let label: SharedString = entry.filename.clone().into();
                let icon = if entry.is_directory {
                    IconName::Folder
                } else if entry.source_type == "script" {
                    IconName::Terminal
                } else {
                    IconName::File
                };
                let is_expanded = self.expanded_folders.contains(&entry.resolved_path);
                let path = entry.resolved_path.clone();
                let full_path: SharedString = path.display().to_string().into();
                let toml_ix = entry.toml_index;
                let ctx_count = self
                    .scoped_automation
                    .as_ref()
                    .map_or(0, |a| a.contexts.len());
                let is_first = toml_ix == 0;
                let is_last = toml_ix + 1 >= ctx_count;

                let auto_id_up = entry.automation_id.clone();
                let auto_id_down = entry.automation_id.clone();
                let auto_id_rm = entry.automation_id.clone();

                let item = ListItem::new(ix)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                    .child(Label::new(label).truncate())
                    .tooltip(Tooltip::text(full_path))
                    .end_slot_on_hover(
                        h_flex()
                            .gap_0p5()
                            .when(!is_first, |this| {
                                this.child(
                                    IconButton::new(
                                        SharedString::from(format!("ctx-up-{ix}")),
                                        IconName::ArrowUp,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Move Up"))
                                    .on_click(cx.listener(
                                        move |_, _, _, cx| {
                                            cx.emit(PostProdPickerEvent::ContextReorder {
                                                automation_id: auto_id_up.clone(),
                                                toml_index: toml_ix,
                                                direction: -1,
                                            });
                                        },
                                    )),
                                )
                            })
                            .when(!is_last, |this| {
                                this.child(
                                    IconButton::new(
                                        SharedString::from(format!("ctx-down-{ix}")),
                                        IconName::ArrowDown,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Move Down"))
                                    .on_click(cx.listener(
                                        move |_, _, _, cx| {
                                            cx.emit(PostProdPickerEvent::ContextReorder {
                                                automation_id: auto_id_down.clone(),
                                                toml_index: toml_ix,
                                                direction: 1,
                                            });
                                        },
                                    )),
                                )
                            })
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("ctx-remove-{ix}")),
                                    IconName::Trash,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Remove"))
                                .on_click(cx.listener(
                                    move |_, _, _, cx| {
                                        cx.emit(PostProdPickerEvent::ContextRemove {
                                            automation_id: auto_id_rm.clone(),
                                            toml_index: toml_ix,
                                        });
                                    },
                                )),
                            ),
                    );

                if entry.is_directory {
                    Some(
                        item.end_slot::<Icon>(Some(
                            Icon::new(if is_expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::Small)
                            .color(Color::Muted),
                        ))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.delegate.toggle_folder_expansion(&path, cx);
                        }))
                        .into_any_element(),
                    )
                } else {
                    // 10.1: Register a no-op click handler so GPUI tags the row as
                    // interactive. Without this, `.end_slot_on_hover(...)` above never
                    // fires its hover state and Up/Down/Trash icons stay hidden.
                    Some(
                        item.on_click(cx.listener(|_, _, _, _| {}))
                            .into_any_element(),
                    )
                }
            }
            PostProdPickerEntry::FolderChild(entry) => {
                let label: SharedString = entry.filename.clone().into();
                let full_path: SharedString = entry.resolved_path.display().to_string().into();
                let source_path = entry.resolved_path.clone();
                let automation_id = entry.automation_id.clone();
                // 10.10: parent_toml_index (entry.toml_index) is intentionally NOT
                // captured — passing it would orphan the parent folder's [[context]].
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(
                            div().pl_4().child(
                                Icon::new(IconName::File)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                        .child(Label::new(label).truncate().mr_10())
                        .tooltip(Tooltip::text(full_path))
                        // 10.3: Paperclip-only hover row. No Trash/Up/Down — the parent
                        // folder is the [[context]] unit; removing one child without
                        // rewriting the directory contents on disk would do nothing.
                        // toml_index_to_strip is None (parent folder entry stays).
                        .end_slot_on_hover(
                            h_flex().gap_0p5().child(
                                IconButton::new(
                                    SharedString::from(format!("folder-child-paperclip-{ix}")),
                                    IconName::Paperclip,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(move |_window, cx| {
                                    Tooltip::with_meta(
                                        "Add to Default Context",
                                        None,
                                        "Always included in every automation's context.",
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(PostProdPickerEvent::AddToDefaultContext {
                                        source_path: source_path.clone(),
                                        automation_id: automation_id.clone(),
                                        toml_index_to_strip: None,
                                    });
                                })),
                            ),
                        )
                        // 10.3: no-op click registers the row as interactive so hover fires.
                        .on_click(cx.listener(|_, _, _, _| {}))
                        .into_any_element(),
                )
            }
            PostProdPickerEntry::AddContextAction(automation_id) => {
                let auto_id = automation_id.clone();
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .start_slot(
                            Icon::new(IconName::Plus)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new("Add Context")
                                .color(Color::Accent)
                                .size(LabelSize::Small),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(PostProdPickerEvent::AddContextFile {
                                automation_id: auto_id.clone(),
                            });
                        }))
                        .into_any_element(),
                )
            }
            PostProdPickerEntry::AddNoteAction(automation_id) => {
                let auto_id = automation_id.clone();
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .start_slot(
                            Icon::new(IconName::Plus)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new("Add Note")
                                .color(Color::Accent)
                                .size(LabelSize::Small),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(PostProdPickerEvent::AddScopedNote {
                                automation_id: auto_id.clone(),
                            });
                        }))
                        .into_any_element(),
                )
            }
        }
    }

    fn render_editor(
        &self,
        editor: &Arc<dyn ErasedEditor>,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Div {
        let editor = editor.as_any().downcast_ref::<Entity<Editor>>().unwrap();

        h_flex()
            .py_1()
            .px_1p5()
            .mx_1()
            .gap_1p5()
            .rounded_sm()
            .bg(cx.theme().colors().editor_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(Icon::new(IconName::MagnifyingGlass).color(Color::Muted))
            .child(editor.clone())
    }
}

// ---------------------------------------------------------------------------
// PostProdRules implementation
// ---------------------------------------------------------------------------

impl PostProdRules {
    fn new(
        store: Entity<NoteStore>,
        language_registry: Arc<LanguageRegistry>,
        inline_assist_delegate: Box<dyn InlineAssistDelegate>,
        config_root: PathBuf,
        automations: Vec<ResolvedAutomationInfo>,
        context_callbacks: Option<Arc<ContextCallbacks>>,
        mode: WindowMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let prompt_files =
            scan_md_files(&config_root.join("config/prompts"), PickerSection::Prompt);
        let default_context_files = scan_md_files(
            &config_root.join("config/default-context"),
            PickerSection::DefaultContext,
        );

        let scoped_automation = match &mode {
            WindowMode::Scoped(info) => Some(info.clone()),
            WindowMode::General(_) => None,
        };

        let picker_delegate = PostProdPickerDelegate {
            store: store.clone(),
            prompt_files,
            default_context_files,
            scoped_automation,
            expanded_folders: collections::HashSet::default(),
            selected_index: 0,
            filtered_entries: Vec::new(),
        };

        let picker = cx.new(|cx| {
            let picker = Picker::list(picker_delegate, window, cx)
                .modal(false)
                .max_height(None);
            picker.focus(window, cx);
            picker
        });

        let mut this = Self {
            title_bar: if !cfg!(target_os = "macos") {
                Some(cx.new(|cx| PlatformTitleBar::new("postprod-rules-title-bar", cx)))
            } else {
                None
            },
            store,
            language_registry,
            config_root,
            mode,
            automations,
            context_callbacks,
            note_editors: HashMap::default(),
            file_editors: HashMap::default(),
            active_entry: None,
            pending_load: Task::ready(()),
            inline_assist_delegate,
            _subscriptions: vec![cx.subscribe_in(&picker, window, Self::handle_picker_event)],
            picker,
        };

        // Extract initial selection to avoid borrow conflict with &mut self methods
        enum InitialSelection {
            PromptFile(String),
            Note(NoteId),
            None,
        }
        let initial = match &this.mode {
            WindowMode::Scoped(info) => match &info.prompt_file {
                Some(pf) => InitialSelection::PromptFile(pf.clone()),
                None => InitialSelection::None,
            },
            WindowMode::General(sel) => match sel {
                Some(SelectionTarget::Note(id)) => InitialSelection::Note(*id),
                Some(SelectionTarget::PromptFile(f)) => InitialSelection::PromptFile(f.clone()),
                None => InitialSelection::None,
            },
        };
        match initial {
            InitialSelection::PromptFile(f) => this.select_prompt_file(&f, window, cx),
            InitialSelection::Note(id) => this.load_note(id, true, window, cx),
            InitialSelection::None => {}
        }

        this
    }

    /// Returns the scoped automation ID, if the window is in scoped mode.
    pub fn scoped_automation_id(&self) -> Option<&str> {
        match &self.mode {
            WindowMode::Scoped(info) => Some(&info.id),
            WindowMode::General(_) => None,
        }
    }

    /// Returns true if the window is in scoped mode.
    pub fn is_scoped(&self) -> bool {
        matches!(self.mode, WindowMode::Scoped(_))
    }

    /// Update the scoped automation info (called by Dashboard during 10s poll).
    /// If the automation no longer exists, close the window.
    pub fn update_automation(
        &mut self,
        info: Option<ResolvedAutomationInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match info {
            Some(info) => {
                self.picker.update(cx, |picker, cx| {
                    picker.delegate.scoped_automation = Some(info.clone());
                    picker.refresh(window, cx);
                });
                if let WindowMode::Scoped(ref mut current) = self.mode {
                    *current = info;
                }
            }
            None => {
                // Automation was deleted — close the window
                window.remove_window();
            }
        }
    }

    fn handle_picker_event(
        &mut self,
        _: &Entity<Picker<PostProdPickerDelegate>>,
        event: &PostProdPickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PostProdPickerEvent::Selected { entry_id } => {
                self.load_entry(entry_id.clone(), false, window, cx);
            }
            PostProdPickerEvent::Confirmed { entry_id } => {
                self.load_entry(entry_id.clone(), true, window, cx);
            }
            PostProdPickerEvent::ToggledDefault { note_id } => {
                self.toggle_default_for_note(*note_id, window, cx);
            }
            PostProdPickerEvent::Deleted { note_id } => {
                self.delete_note(*note_id, window, cx);
            }
            PostProdPickerEvent::ContextReorder {
                automation_id,
                toml_index,
                direction,
            } => {
                if self.context_callbacks.is_some() {
                    let callbacks = self.context_callbacks.clone().unwrap();
                    (callbacks.reorder)(automation_id, *toml_index, *direction, cx);
                    let from = *toml_index;
                    let direction = *direction;
                    self.apply_local_context_change(
                        move |contexts| {
                            let to_signed = from as i32 + direction;
                            if to_signed < 0 {
                                return;
                            }
                            let to = to_signed as usize;
                            if from < contexts.len() && to < contexts.len() {
                                contexts.swap(from, to);
                            }
                        },
                        window,
                        cx,
                    );
                }
            }
            PostProdPickerEvent::ContextRemove {
                automation_id,
                toml_index,
            } => {
                if self.context_callbacks.is_some() {
                    let callbacks = self.context_callbacks.clone().unwrap();
                    (callbacks.remove)(automation_id, *toml_index, cx);
                    let ix = *toml_index;
                    self.apply_local_context_change(
                        move |contexts| {
                            if ix < contexts.len() {
                                contexts.remove(ix);
                            }
                        },
                        window,
                        cx,
                    );
                }
            }
            PostProdPickerEvent::AddContextFile { automation_id } => {
                self.add_context_file(automation_id.clone(), window, cx);
            }
            PostProdPickerEvent::AddContextScript { automation_id } => {
                self.add_context_script(automation_id.clone(), window, cx);
            }
            PostProdPickerEvent::AddScopedNote { automation_id } => {
                self.new_note_for_automation(automation_id.clone(), window, cx);
            }
            PostProdPickerEvent::AddToDefaultContext {
                source_path,
                automation_id,
                toml_index_to_strip,
            } => {
                self.add_active_file_to_default_context(
                    source_path.clone(),
                    automation_id.clone(),
                    *toml_index_to_strip,
                    window,
                    cx,
                );
            }
        }
    }

    /// 10.11: Shared helper used by both the picker FolderChild Paperclip
    /// (event-driven) and the editor-toolbar `&AddFileToDefaultContext` action.
    /// Runs **pre-flight validation first** (extension, symlink, collision —
    /// 10.7 spec: the user shouldn't read "this will move X" then get a toast
    /// saying it can't), then opens the confirmation modal, then on confirm
    /// invokes the `add_to_default_context` callback. The callback (Dashboard
    /// side) runs the EXDEV-safe move + TOML strip + toast + picker refresh.
    fn add_active_file_to_default_context(
        &mut self,
        source_path: PathBuf,
        automation_id: String,
        toml_index_to_strip: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let basename = source_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| source_path.display().to_string());

        // Pre-flight: extension (.md-only) → symlink-refuse → collision.
        // Surfaced via the validation helper so the same logic is unit-tested
        // (10.18 collision, 10.19 symlink, 10.20a non-`.md`).
        let target_dir = self.config_root.join("config/default-context");
        if let Err(err) = validate_promotion(&source_path, &target_dir) {
            let (title, body) = match err {
                PromotionPreflightError::NotMarkdown => (
                    "Cannot promote to default-context",
                    "Only .md files can be promoted to default-context (the picker scans only .md).".to_string(),
                ),
                PromotionPreflightError::Symlink => (
                    "Cannot promote a symlink",
                    "Cannot promote a symlink to default-context. Use the actual file.".to_string(),
                ),
                PromotionPreflightError::Collision => (
                    "Already exists in default-context",
                    format!("{} already exists in default-context/. Cannot move.", basename),
                ),
                PromotionPreflightError::SourceMissing => {
                    log::warn!(
                        "add_to_default_context: source missing: {}",
                        source_path.display()
                    );
                    return;
                }
            };
            self.show_preflight_error(title, &body, window, cx);
            return;
        }

        let body = match toml_index_to_strip {
            Some(_) => format!(
                "This will:\n\
                 • Move {} → config/default-context/{}\n\
                 • Remove the [[context]] entry from {}.toml\n\n\
                 The file will be loaded by every automation as default context going forward. \
                 Other references to the original path will break.",
                source_path.display(),
                basename,
                automation_id
            ),
            None => format!(
                "This will:\n\
                 • Move {} → config/default-context/{}\n\
                 • Leave the parent folder's [[context]] entry in place\n\n\
                 The file will be loaded by every automation as default context going forward. \
                 Other references to the original path will break.",
                source_path.display(),
                basename
            ),
        };
        let confirmation = window.prompt(
            PromptLevel::Warning,
            &format!("Move {} to default-context/?", basename),
            Some(&body),
            &["Move", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if confirmation.await.ok() == Some(0) {
                this.update_in(cx, |this, window, cx| {
                    if let Some(callbacks) = this.context_callbacks.clone() {
                        (callbacks.add_to_default_context)(
                            source_path,
                            &automation_id,
                            toml_index_to_strip,
                            cx,
                        );
                    }
                    // Apply the same change locally — Dashboard cannot push
                    // back via `handle.update` from inside this callback
                    // chain (re-entrant entity update; silently fails). See
                    // `apply_local_context_change` for the reasoning.
                    if let Some(ix) = toml_index_to_strip {
                        this.apply_local_context_change(
                            move |contexts| {
                                if ix < contexts.len() {
                                    contexts.remove(ix);
                                }
                            },
                            window,
                            cx,
                        );
                    }
                    // The new file is now in `config/default-context/` on
                    // disk; rebuild the picker delegate's
                    // `default_context_files` from the directory and refresh
                    // the picker so the new entry shows under DEFAULT
                    // CONTEXT.
                    this.refresh_after_default_context_change(window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn show_preflight_error(
        &self,
        title: &str,
        body: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = window.prompt(PromptLevel::Critical, title, Some(body), &["OK"], cx);
        cx.spawn(async move |_, _| {
            let _ = prompt.await;
        })
        .detach();
    }

    /// 10.8: Re-scan `config/default-context/` for `.md` files and update the
    /// picker delegate. Required after a file has been moved into the dir
    /// (e.g. by `add_file_to_default_context` on the Dashboard side) — the
    /// 10s automations-reload poll picks up automation TOML changes but does
    /// NOT rescan `default-context/`. Without this, the moved file doesn't
    /// appear under DEFAULT CONTEXT until the user closes and reopens the
    /// window.
    pub fn refresh_default_context_files(&mut self, cx: &mut Context<Self>) {
        let dir = self.config_root.join("config/default-context");
        let files = scan_md_files(&dir, PickerSection::DefaultContext);
        self.picker.update(cx, |picker, _cx| {
            picker.delegate.default_context_files = files;
        });
        cx.notify();
    }

    /// 10.7 step 7: Two-step refresh (rebuild data, then re-filter into
    /// `filtered_entries`). Order is load-bearing: reversing it causes
    /// `update_matches` to filter over the stale `default_context_files`
    /// vec. Exposed so the dashboard can call it via the rules window
    /// handle after `add_file_to_default_context` succeeds.
    pub fn refresh_after_default_context_change(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_default_context_files(cx);
        self.picker
            .update(cx, |picker, cx| picker.refresh(window, cx));
    }

    /// 10.13: Editor-toolbar Trash confirmation for files. Picker hover Trash
    /// continues to fire immediately (intentional asymmetry — picker hover is
    /// the surgical/quick path).
    fn remove_active_file_from_automation_context(
        &mut self,
        source_path: PathBuf,
        automation_id: String,
        toml_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let basename = source_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| source_path.display().to_string());
        let confirmation = window.prompt(
            PromptLevel::Warning,
            &format!("Remove {} from {}?", basename, automation_id),
            Some("The file stays on disk; only the [[context]] reference is removed."),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if confirmation.await.ok() == Some(0) {
                this.update_in(cx, |this, window, cx| {
                    if let Some(callbacks) = this.context_callbacks.clone() {
                        (callbacks.remove)(&automation_id, toml_index, cx);
                        this.apply_local_context_change(
                            move |contexts| {
                                if toml_index < contexts.len() {
                                    contexts.remove(toml_index);
                                }
                            },
                            window,
                            cx,
                        );
                    }
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Apply a context-vec mutation locally to `WindowMode::Scoped`'s
    /// `ResolvedAutomationInfo` AND propagate the new value to
    /// `picker.delegate.scoped_automation`, then refresh the picker.
    ///
    /// **Why this exists:** the picker's `update_matches` rebuilds
    /// `filtered_entries` from `scoped_automation`. When the dashboard mutates
    /// its in-memory `automations` Vec via the `ContextCallbacks` (Trash,
    /// Reorder, Add to Default Context, etc.), the rules window's local
    /// `scoped_automation` stays stale until the dashboard's 10s
    /// automations-reload poll calls `update_automation` to push fresh data.
    /// During that ~10s window, `picker.refresh` shows the OLD list.
    ///
    /// We can't fix this by having the dashboard call `handle.update` on
    /// the rules window from inside the callback, because we're already
    /// inside a rules-window update at that point — the re-entrant
    /// `handle.update` returns Err and is silently swallowed. Instead, the
    /// rules window applies the same mutation it just asked the dashboard to
    /// make, locally, on the way back from the callback. This keeps the
    /// picker in sync with the dashboard's authoritative state on the next
    /// 10s poll without any race.
    fn apply_local_context_change(
        &mut self,
        mutate: impl FnOnce(&mut Vec<ResolvedContextInfo>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cloned = if let WindowMode::Scoped(ref mut info) = self.mode {
            mutate(&mut info.contexts);
            Some(info.clone())
        } else {
            None
        };
        self.picker.update(cx, |picker, cx| {
            if let Some(info) = cloned {
                picker.delegate.scoped_automation = Some(info);
            }
            picker.refresh(window, cx);
        });
    }

    /// Read the picker's currently-selected entry and project it to the
    /// data the action handlers need: `(source_path, automation_id,
    /// toml_index_to_strip)` where `toml_index_to_strip` is `Some(ix)` for a
    /// top-level `ContextSource` row and `None` for a `FolderChild` row.
    /// Returns `None` for any other selection (Header, PromptFile, etc.) or
    /// when `set_active_entry` hasn't synced selection to a file yet.
    ///
    /// Preferred over `classify_file_role` for action handlers because:
    ///   1. The picker entry holds the exact `(automation_id, toml_index)`
    ///      tuple already — no path-comparison needed.
    ///   2. Robust when the same file is referenced by multiple automations
    ///      (`classify_file_role` walks all and returns the first match,
    ///      which may be the wrong automation in the picker's perspective).
    ///   3. Robust when a context entry happens to point at a file under
    ///      `config/prompts/` or `config/default-context/` (where
    ///      `classify_file_role`'s prefix check returns `Prompt`/`DefaultContext`).
    fn selected_context_source(
        &self,
        cx: &App,
    ) -> Option<(PathBuf, String, Option<usize>)> {
        let picker = self.picker.read(cx);
        let entry = picker
            .delegate
            .filtered_entries
            .get(picker.delegate.selected_index)?;
        match entry {
            PostProdPickerEntry::ContextSource(c) if !c.is_directory => Some((
                c.resolved_path.clone(),
                c.automation_id.clone(),
                Some(c.toml_index),
            )),
            PostProdPickerEntry::FolderChild(c) => Some((
                c.resolved_path.clone(),
                c.automation_id.clone(),
                None,
            )),
            _ => None,
        }
    }

    /// 10.11: Editor-toolbar `&AddFileToDefaultContext` handler.
    /// Sources `(automation_id, toml_index_to_strip)` from the picker's
    /// currently-selected entry rather than `classify_file_role`.
    fn add_active_file_to_default_context_action(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((source_path, automation_id, toml_index_to_strip)) =
            self.selected_context_source(cx)
        else {
            log::warn!(
                "AddFileToDefaultContext: picker has no ContextSource/FolderChild selected"
            );
            return;
        };
        self.add_active_file_to_default_context(
            source_path,
            automation_id,
            toml_index_to_strip,
            window,
            cx,
        );
    }

    /// 10.11: Editor-toolbar `&RemoveFileFromAutomationContext` handler.
    /// Only fires when the picker selection is a top-level `ContextSource`
    /// (FolderChild has no Trash per the toolbar table).
    fn remove_active_file_from_automation_context_action(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((source_path, automation_id, Some(toml_index))) =
            self.selected_context_source(cx)
        else {
            log::warn!(
                "RemoveFileFromAutomationContext: picker has no top-level ContextSource selected"
            );
            return;
        };
        self.remove_active_file_from_automation_context(
            source_path,
            automation_id,
            toml_index,
            window,
            cx,
        );
    }

    fn load_entry(
        &mut self,
        entry_id: ActiveEntryId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match entry_id {
            ActiveEntryId::Note(id) => self.load_note(id, focus, window, cx),
            ActiveEntryId::File(path) => self.load_file(path, focus, window, cx),
        }
    }

    /// Select a prompt file by filename (for deep linking from Dashboard card).
    pub fn select_prompt_file(
        &mut self,
        filename: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let all_files = self
            .picker
            .read(cx)
            .delegate
            .prompt_files
            .iter()
            .chain(self.picker.read(cx).delegate.default_context_files.iter())
            .find(|f| f.filename == filename)
            .map(|f| f.path.clone());

        if let Some(path) = all_files {
            self.load_file(path, true, window, cx);
        }
    }

    pub fn new_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(metadata) = self.store.read(cx).first()
            && metadata.title.is_none()
        {
            self.load_note(metadata.id, true, window, cx);
            return;
        }

        let note_id = NoteId::new();
        let save = self.store.update(cx, |store, cx| {
            store.save(note_id, None, false, Vec::new(), "".into(), cx)
        });
        self.picker
            .update(cx, |picker, cx| picker.refresh(window, cx));
        cx.spawn_in(window, async move |this, cx| {
            save.await?;
            this.update_in(cx, |this, window, cx| {
                this.load_note(note_id, true, window, cx)
            })
        })
        .detach_and_log_err(cx);
    }

    /// Open the system file picker and add the selected file as a context entry.
    fn add_context_file(
        &mut self,
        automation_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let callbacks = self.context_callbacks.clone();
        let picker = self.picker.clone();
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            if let Ok(Ok(Some(paths))) = result {
                if let Some(path) = paths.into_iter().next() {
                    cx.update(|_window, cx| {
                        if let Some(callbacks) = &callbacks {
                            (callbacks.add_path)(&automation_id, path, cx);
                        }
                    })?;
                    this.update_in(cx, |_this, window, cx| {
                        picker.update(cx, |picker, cx| picker.refresh(window, cx));
                    })?;
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Add a script as context entry by scanning available scripts.
    fn add_context_script(
        &mut self,
        automation_id: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scripts_dir = self.config_root.join("config/context-scripts");
        let scripts = scan_scripts(&scripts_dir);
        if scripts.is_empty() {
            log::info!("No context scripts found in {}", scripts_dir.display());
            return;
        }
        // If only one script, add it directly
        if scripts.len() == 1 {
            if let Some(callbacks) = &self.context_callbacks {
                (callbacks.add_script)(&automation_id, scripts[0].clone(), cx);
            }
            return;
        }
        // For multiple scripts, add the first one (future: show a sub-picker)
        if let Some(callbacks) = &self.context_callbacks {
            (callbacks.add_script)(&automation_id, scripts[0].clone(), cx);
        }
    }

    /// Create a new note pre-assigned to the given automation.
    fn new_note_for_automation(
        &mut self,
        automation_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let note_id = NoteId::new();
        let save = self.store.update(cx, |store, cx| {
            store.save(note_id, None, false, vec![automation_id], "".into(), cx)
        });
        self.picker
            .update(cx, |picker, cx| picker.refresh(window, cx));
        cx.spawn_in(window, async move |this, cx| {
            save.await?;
            this.update_in(cx, |this, window, cx| {
                this.load_note(note_id, true, window, cx)
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn save_note(&mut self, note_id: NoteId, window: &mut Window, cx: &mut Context<Self>) {
        const SAVE_THROTTLE: Duration = Duration::from_millis(500);

        let note_metadata = match self.store.read(cx).metadata(note_id) {
            Some(m) => m,
            None => return,
        };
        let note_editor = match self.note_editors.get_mut(&note_id) {
            Some(e) => e,
            None => return,
        };
        let title = note_editor.title_editor.read(cx).text(cx);
        let body = note_editor.body_editor.update(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
                .read(cx)
                .as_rope()
                .clone()
        });

        let store = self.store.clone();
        let executor = cx.background_executor().clone();

        note_editor.next_title_and_body_to_save = Some((title, body));
        if note_editor.pending_save.is_none() {
            note_editor.pending_save = Some(cx.spawn_in(window, async move |this, cx| {
                async move {
                    loop {
                        let title_and_body = this.update(cx, |this, _| {
                            this.note_editors
                                .get_mut(&note_id)?
                                .next_title_and_body_to_save
                                .take()
                        })?;

                        if let Some((title, body)) = title_and_body {
                            let title = if title.trim().is_empty() {
                                None
                            } else {
                                Some(SharedString::from(title))
                            };
                            cx.update(|_window, cx| {
                                store.update(cx, |store, cx| {
                                    store.save(
                                        note_id,
                                        title,
                                        note_metadata.default,
                                        note_metadata.assigned_automations.clone(),
                                        body,
                                        cx,
                                    )
                                })
                            })?
                            .await
                            .log_err();
                            this.update_in(cx, |this, window, cx| {
                                this.picker
                                    .update(cx, |picker, cx| picker.refresh(window, cx));
                                cx.notify();
                            })?;

                            executor.timer(SAVE_THROTTLE).await;
                        } else {
                            break;
                        }
                    }

                    this.update(cx, |this, _cx| {
                        if let Some(note_editor) = this.note_editors.get_mut(&note_id) {
                            note_editor.pending_save = None;
                        }
                    })
                }
                .log_err()
                .await
            }));
        }
    }

    fn save_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        const SAVE_THROTTLE: Duration = Duration::from_millis(500);

        let file_editor = match self.file_editors.get_mut(&path) {
            Some(e) => e,
            None => return,
        };
        let body = file_editor.body_editor.update(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
                .read(cx)
                .as_rope()
                .clone()
        });

        let executor = cx.background_executor().clone();
        let is_symlink = file_editor.is_symlink;

        file_editor.next_body_to_save = Some(body);
        if file_editor.pending_save.is_none() {
            let save_path = path.clone();
            file_editor.pending_save = Some(cx.spawn_in(window, async move |this, cx| {
                async move {
                    // Symlink warning on first save
                    if is_symlink {
                        let target = std::fs::read_link(&save_path)
                            .map(|t| t.display().to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        let answer = this.update_in(cx, |_, window, cx| {
                            window.prompt(
                                PromptLevel::Warning,
                                &format!(
                                    "This file is a symlink to {}. Editing will modify the shared original. Continue?",
                                    target
                                ),
                                None,
                                &["Continue", "Cancel"],
                                cx,
                            )
                        })?;
                        if answer.await.ok() != Some(0) {
                            return anyhow::Ok(());
                        }
                    }

                    loop {
                        let body = this.update(cx, |this, _| {
                            this.file_editors
                                .get_mut(&save_path)?
                                .next_body_to_save
                                .take()
                        })?;

                        if let Some(body) = body {
                            let text = body.to_string();
                            let write_path = save_path.clone();
                            cx.background_executor()
                                .spawn(async move {
                                    std::fs::write(&write_path, &text)
                                })
                                .await
                                .log_err();

                            executor.timer(SAVE_THROTTLE).await;
                        } else {
                            break;
                        }
                    }

                    this.update(cx, |this, _cx| {
                        if let Some(file_editor) = this.file_editors.get_mut(&save_path) {
                            file_editor.pending_save = None;
                        }
                    })
                }
                .log_err()
                .await
            }));
        }
    }

    pub fn delete_active_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActiveEntryId::Note(note_id)) = self.active_entry {
            self.delete_note(note_id, window, cx);
        }
    }

    pub fn duplicate_active_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActiveEntryId::Note(note_id)) = self.active_entry {
            self.duplicate_note(note_id, window, cx);
        }
    }

    pub fn toggle_default_for_active_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActiveEntryId::Note(note_id)) = self.active_entry {
            self.toggle_default_for_note(note_id, window, cx);
        }
    }

    pub fn toggle_default_for_note(
        &mut self,
        note_id: NoteId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, move |store, cx| {
            if let Some(metadata) = store.metadata(note_id) {
                store
                    .save_metadata(
                        note_id,
                        metadata.title,
                        !metadata.default,
                        metadata.assigned_automations,
                        cx,
                    )
                    .detach_and_log_err(cx);
            }
        });
        self.picker
            .update(cx, |picker, cx| picker.refresh(window, cx));
        cx.notify();
    }

    pub fn load_note(
        &mut self,
        note_id: NoteId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(note_editor) = self.note_editors.get(&note_id) {
            if focus {
                note_editor
                    .body_editor
                    .update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx));
            }
            self.set_active_entry(Some(ActiveEntryId::Note(note_id)), window, cx);
        } else if self.store.read(cx).metadata(note_id).is_some() {
            let language_registry = self.language_registry.clone();
            let note = self.store.read(cx).load(note_id, cx);
            self.pending_load = cx.spawn_in(window, async move |this, cx| {
                let note = note.await;
                let markdown = language_registry.language_for_name("Markdown").await;
                this.update_in(cx, |this, window, cx| match note {
                    Ok(note_body) => {
                        let title_editor = cx.new(|cx| {
                            let mut editor = Editor::single_line(window, cx);
                            let metadata = this.store.read(cx).metadata(note_id);
                            editor.set_placeholder_text("Untitled", window, cx);
                            editor.set_text(
                                metadata.and_then(|m| m.title).unwrap_or_default(),
                                window,
                                cx,
                            );
                            editor
                        });
                        let body_editor = cx.new(|cx| {
                            let buffer = cx.new(|cx| {
                                let mut buffer = Buffer::local(note_body, cx);
                                buffer.set_language(markdown.log_err(), cx);
                                buffer.set_language_registry(language_registry);
                                buffer
                            });

                            let mut editor = Editor::for_buffer(buffer, None, window, cx);
                            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
                            editor.set_show_gutter(false, cx);
                            editor.set_show_wrap_guides(false, cx);
                            editor.set_show_indent_guides(false, cx);
                            editor.set_use_modal_editing(true);
                            editor.set_current_line_highlight(Some(CurrentLineHighlight::None));
                            if focus {
                                window.focus(&editor.focus_handle(cx), cx);
                            }
                            editor
                        });
                        let _subscriptions = vec![
                            cx.subscribe_in(
                                &title_editor,
                                window,
                                move |this, editor, event, window, cx| {
                                    this.handle_note_title_editor_event(
                                        note_id, editor, event, window, cx,
                                    )
                                },
                            ),
                            cx.subscribe_in(
                                &body_editor,
                                window,
                                move |this, editor, event, window, cx| {
                                    this.handle_note_body_editor_event(
                                        note_id, editor, event, window, cx,
                                    )
                                },
                            ),
                        ];
                        this.note_editors.insert(
                            note_id,
                            NoteEditor {
                                title_editor,
                                body_editor,
                                next_title_and_body_to_save: None,
                                pending_save: None,
                                token_count: None,
                                pending_token_count: Task::ready(None),
                                _subscriptions,
                            },
                        );
                        this.set_active_entry(Some(ActiveEntryId::Note(note_id)), window, cx);
                        this.count_tokens_for_note(note_id, window, cx);
                    }
                    Err(error) => {
                        log::error!("error while loading note: {:?}", error);
                    }
                })
                .ok();
            });
        }
    }

    fn load_file(
        &mut self,
        path: PathBuf,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(file_editor) = self.file_editors.get(&path) {
            if focus {
                file_editor
                    .body_editor
                    .update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx));
            }
            self.set_active_entry(Some(ActiveEntryId::File(path)), window, cx);
        } else {
            let language_registry = self.language_registry.clone();
            let load_path = path.clone();
            let is_symlink = std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            let display_name: SharedString = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
                .into();

            let file_path_for_lang = path.clone();
            self.pending_load = cx.spawn_in(window, async move |this, cx| {
                let content = cx
                    .background_executor()
                    .spawn(async move { std::fs::read_to_string(&load_path) })
                    .await;
                let lang_name = language_registry
                    .language_for_file_path(&file_path_for_lang)
                    .map(|l| l.name());
                let language = language_registry
                    .language_for_name(lang_name.as_ref().map_or("Markdown", |n| n.as_ref()))
                    .await;

                this.update_in(cx, |this, window, cx| match content {
                    Ok(content) => {
                        let editor_path = path.clone();
                        let body_editor = cx.new(|cx| {
                            let buffer = cx.new(|cx| {
                                let mut buffer = Buffer::local(content, cx);
                                buffer.set_language(language.log_err(), cx);
                                buffer.set_language_registry(language_registry);
                                buffer
                            });

                            let mut editor = Editor::for_buffer(buffer, None, window, cx);
                            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
                            editor.set_show_gutter(false, cx);
                            editor.set_show_wrap_guides(false, cx);
                            editor.set_show_indent_guides(false, cx);
                            editor.set_use_modal_editing(true);
                            editor.set_current_line_highlight(Some(CurrentLineHighlight::None));
                            if focus {
                                window.focus(&editor.focus_handle(cx), cx);
                            }
                            editor
                        });
                        let _subscriptions = vec![cx.subscribe_in(
                            &body_editor,
                            window,
                            move |this, editor, event, window, cx| {
                                this.handle_file_body_editor_event(
                                    editor_path.clone(),
                                    editor,
                                    event,
                                    window,
                                    cx,
                                )
                            },
                        )];
                        this.file_editors.insert(
                            path.clone(),
                            FileEditor {
                                title: display_name,
                                body_editor,
                                is_symlink,
                                path: path.clone(),
                                token_count: None,
                                pending_token_count: Task::ready(None),
                                next_body_to_save: None,
                                pending_save: None,
                                _subscriptions,
                            },
                        );
                        this.set_active_entry(Some(ActiveEntryId::File(path.clone())), window, cx);
                        this.count_tokens_for_file(path, window, cx);
                    }
                    Err(error) => {
                        log::error!("error while loading file: {:?}", error);
                    }
                })
                .ok();
            });
        }
    }

    fn set_active_entry(
        &mut self,
        entry_id: Option<ActiveEntryId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_entry = entry_id.clone();
        self.picker.update(cx, |picker, cx| {
            if let Some(entry_id) = &entry_id {
                let current_matches = picker
                    .delegate
                    .filtered_entries
                    .get(picker.delegate.selected_index())
                    .and_then(entry_id_for_picker_entry);
                if current_matches.as_ref() != Some(entry_id) {
                    if let Some(ix) =
                        picker.delegate.filtered_entries.iter().position(|mat| {
                            entry_id_for_picker_entry(mat).as_ref() == Some(entry_id)
                        })
                    {
                        picker.set_selected_index(ix, None, true, window, cx);
                    }
                }
            } else {
                picker.focus(window, cx);
            }
        });
        cx.notify();
    }

    pub fn delete_note(&mut self, note_id: NoteId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(metadata) = self.store.read(cx).metadata(note_id) {
            let confirmation = window.prompt(
                PromptLevel::Warning,
                &format!(
                    "Are you sure you want to delete {}",
                    metadata.title.unwrap_or("Untitled".into())
                ),
                None,
                &["Delete", "Cancel"],
                cx,
            );

            cx.spawn_in(window, async move |this, cx| {
                if confirmation.await.ok() == Some(0) {
                    this.update_in(cx, |this, window, cx| {
                        if this.active_entry == Some(ActiveEntryId::Note(note_id)) {
                            this.set_active_entry(None, window, cx);
                        }
                        this.note_editors.remove(&note_id);
                        this.store
                            .update(cx, |store, cx| store.delete(note_id, cx))
                            .detach_and_log_err(cx);
                        this.picker
                            .update(cx, |picker, cx| picker.refresh(window, cx));
                        cx.notify();
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
    }

    pub fn duplicate_note(&mut self, note_id: NoteId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(note) = self.note_editors.get(&note_id) {
            const DUPLICATE_SUFFIX: &str = " copy";
            let title_to_duplicate = note.title_editor.read(cx).text(cx);
            let existing_titles = self
                .note_editors
                .iter()
                .filter(|&(&id, _)| id != note_id)
                .map(|(_, note_editor)| note_editor.title_editor.read(cx).text(cx))
                .filter(|title| title.starts_with(&title_to_duplicate))
                .collect::<collections::HashSet<_>>();

            let title = if existing_titles.is_empty() {
                title_to_duplicate + DUPLICATE_SUFFIX
            } else {
                let mut i = 1;
                loop {
                    let new_title = format!("{title_to_duplicate}{DUPLICATE_SUFFIX} {i}");
                    if !existing_titles.contains(&new_title) {
                        break new_title;
                    }
                    i += 1;
                }
            };

            let new_id = NoteId::new();
            let body = note.body_editor.read(cx).text(cx);
            let save = self.store.update(cx, |store, cx| {
                store.save(
                    new_id,
                    Some(title.into()),
                    false,
                    Vec::new(),
                    body.into(),
                    cx,
                )
            });
            self.picker
                .update(cx, |picker, cx| picker.refresh(window, cx));
            cx.spawn_in(window, async move |this, cx| {
                save.await?;
                this.update_in(cx, |this, window, cx| {
                    this.load_note(new_id, true, window, cx)
                })
            })
            .detach_and_log_err(cx);
        }
    }

    fn focus_active_editor(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        match &self.active_entry {
            Some(ActiveEntryId::Note(id)) => {
                if let Some(editor) = self.note_editors.get(id) {
                    editor
                        .body_editor
                        .update(cx, |e, cx| window.focus(&e.focus_handle(cx), cx));
                    cx.stop_propagation();
                }
            }
            Some(ActiveEntryId::File(path)) => {
                if let Some(editor) = self.file_editors.get(path) {
                    editor
                        .body_editor
                        .update(cx, |e, cx| window.focus(&e.focus_handle(cx), cx));
                    cx.stop_propagation();
                }
            }
            None => {}
        }
    }

    fn focus_picker(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.focus(window, cx));
    }

    pub fn inline_assist(
        &mut self,
        action: &InlineAssist,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body_editor = match &self.active_entry {
            Some(ActiveEntryId::Note(id)) => self.note_editors.get(id).map(|e| &e.body_editor),
            Some(ActiveEntryId::File(path)) => self.file_editors.get(path).map(|e| &e.body_editor),
            None => {
                cx.propagate();
                return;
            }
        };
        let Some(body_editor) = body_editor else {
            cx.propagate();
            return;
        };

        let Some(ConfiguredModel { provider, .. }) =
            LanguageModelRegistry::read_global(cx).inline_assistant_model()
        else {
            return;
        };

        let initial_prompt = action.prompt.clone();
        if provider.is_authenticated(cx) {
            self.inline_assist_delegate
                .assist(body_editor, initial_prompt, window, cx);
        } else {
            for window in cx.windows() {
                if let Some(multi_workspace) = window.downcast::<MultiWorkspace>() {
                    let panel = multi_workspace
                        .update(cx, |multi_workspace, window, cx| {
                            window.activate_window();
                            multi_workspace.workspace().update(cx, |workspace, cx| {
                                self.inline_assist_delegate
                                    .focus_agent_panel(workspace, window, cx)
                            })
                        })
                        .ok();
                    if panel == Some(true) {
                        return;
                    }
                }
            }
        }
    }

    fn move_down_from_title(
        &mut self,
        _: &zed_actions::editor::MoveDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.active_entry {
            Some(ActiveEntryId::Note(id)) => {
                if let Some(e) = self.note_editors.get(id) {
                    window.focus(&e.body_editor.focus_handle(cx), cx);
                }
            }
            Some(ActiveEntryId::File(path)) => {
                if let Some(e) = self.file_editors.get(path) {
                    window.focus(&e.body_editor.focus_handle(cx), cx);
                }
            }
            None => {}
        }
    }

    fn move_up_from_body(
        &mut self,
        _: &zed_actions::editor::MoveUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActiveEntryId::Note(id)) = &self.active_entry {
            if let Some(e) = self.note_editors.get(id) {
                window.focus(&e.title_editor.focus_handle(cx), cx);
            }
        }
        // Files don't have a separate title editor
    }

    fn handle_note_title_editor_event(
        &mut self,
        note_id: NoteId,
        title_editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::BufferEdited => {
                self.save_note(note_id, window, cx);
                self.count_tokens_for_note(note_id, window, cx);
            }
            EditorEvent::Blurred => {
                title_editor.update(cx, |title_editor, cx| {
                    title_editor.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            let cursor = selections.oldest_anchor().head();
                            selections.select_anchor_ranges([cursor..cursor]);
                        },
                    );
                });
            }
            _ => {}
        }
    }

    fn handle_note_body_editor_event(
        &mut self,
        note_id: NoteId,
        body_editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::BufferEdited => {
                self.save_note(note_id, window, cx);
                self.count_tokens_for_note(note_id, window, cx);
            }
            EditorEvent::Blurred => {
                body_editor.update(cx, |body_editor, cx| {
                    body_editor.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            let cursor = selections.oldest_anchor().head();
                            selections.select_anchor_ranges([cursor..cursor]);
                        },
                    );
                });
            }
            _ => {}
        }
    }

    fn handle_file_body_editor_event(
        &mut self,
        path: PathBuf,
        body_editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::BufferEdited => {
                self.save_file(path.clone(), window, cx);
                self.count_tokens_for_file(path, window, cx);
            }
            EditorEvent::Blurred => {
                body_editor.update(cx, |body_editor, cx| {
                    body_editor.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            let cursor = selections.oldest_anchor().head();
                            selections.select_anchor_ranges([cursor..cursor]);
                        },
                    );
                });
            }
            _ => {}
        }
    }

    fn count_tokens_for_note(
        &mut self,
        note_id: NoteId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Upstream PR #54177 removed `LanguageModel::count_tokens`. Use a
        // ~4 chars/token heuristic for the rules-window indicator. Gate on
        // an AI model being configured to preserve the original
        // "show only when AI is set up" UX.
        if LanguageModelRegistry::read_global(cx)
            .default_model()
            .is_none()
        {
            return;
        }
        if let Some(note) = self.note_editors.get_mut(&note_id) {
            let body_len = note
                .body_editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
                .read(cx)
                .as_rope()
                .len() as u64;
            note.pending_token_count = cx.spawn_in(window, async move |this, cx| {
                async move {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let token_count = body_len.div_ceil(4);
                    this.update(cx, |this, cx| {
                        if let Some(e) = this.note_editors.get_mut(&note_id) {
                            e.token_count = Some(token_count);
                            cx.notify();
                        }
                    })
                }
                .log_err()
                .await
            });
        }
    }

    fn count_tokens_for_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // See `count_tokens_for_note` for the heuristic / upstream context.
        if LanguageModelRegistry::read_global(cx)
            .default_model()
            .is_none()
        {
            return;
        }
        if let Some(file_editor) = self.file_editors.get_mut(&path) {
            let body_len = file_editor
                .body_editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
                .read(cx)
                .as_rope()
                .len() as u64;
            file_editor.pending_token_count = cx.spawn_in(window, async move |this, cx| {
                async move {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let token_count = body_len.div_ceil(4);
                    this.update(cx, |this, cx| {
                        if let Some(e) = this.file_editors.get_mut(&path) {
                            e.token_count = Some(token_count);
                            cx.notify();
                        }
                    })
                }
                .log_err()
                .await
            });
        }
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    fn render_note_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("note-list")
            .capture_action(cx.listener(Self::focus_active_editor))
            .px_1p5()
            .h_full()
            .w_64()
            .overflow_x_hidden()
            .bg(cx.theme().colors().panel_background)
            .map(|this| {
                if cfg!(target_os = "macos") {
                    this.child(
                        h_flex()
                            .p(DynamicSpacing::Base04.rems(cx))
                            .h_9()
                            .w_full()
                            .flex_none()
                            .justify_end()
                            .child(
                                IconButton::new("new-note", IconName::Plus)
                                    .tooltip(move |_window, cx| {
                                        Tooltip::for_action("New Note", &NewNote, cx)
                                    })
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(NewNote), cx);
                                    }),
                            ),
                    )
                } else {
                    this.child(
                        h_flex().p_1().w_full().child(
                            Button::new("new-note", "New Note")
                                .full_width()
                                .style(ButtonStyle::Outlined)
                                .start_icon(
                                    Icon::new(IconName::Plus)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(NewNote), cx);
                                }),
                        ),
                    )
                }
            })
            .child(div().flex_grow().child(self.picker.clone()))
    }

    fn render_title_editor(
        &self,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);

        div()
            .w_full()
            .pl_1()
            .border_1()
            .border_color(transparent_black())
            .rounded_sm()
            .group_hover("active-editor-header", |this| {
                this.border_color(cx.theme().colors().border_variant)
            })
            .on_action(cx.listener(Self::move_down_from_title))
            .child(EditorElement::new(
                &editor,
                EditorStyle {
                    background: cx.theme().system().transparent,
                    local_player: cx.theme().players().local(),
                    text: TextStyle {
                        color: cx.theme().colors().text,
                        font_family: settings.ui_font.family.clone(),
                        font_features: settings.ui_font.features.clone(),
                        font_size: HeadlineSize::Medium.rems().into(),
                        font_weight: settings.ui_font.weight,
                        line_height: relative(settings.buffer_line_height.value()),
                        ..Default::default()
                    },
                    scrollbar_width: Pixels::ZERO,
                    syntax: cx.theme().syntax().clone(),
                    status: cx.theme().status().clone(),
                    inlay_hints_style: editor::make_inlay_hints_style(cx),
                    edit_prediction_styles: editor::make_suggestion_styles(cx),
                    ..EditorStyle::default()
                },
            ))
    }

    fn render_static_title(
        &self,
        title: &SharedString,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div().w_full().pl_1().child(
            Label::new(title.clone())
                .size(LabelSize::Large)
                .color(Color::Default),
        )
    }

    fn render_duplicate_note_button(&self) -> impl IntoElement {
        IconButton::new("duplicate-note", IconName::BookCopy)
            .tooltip(move |_window, cx| Tooltip::for_action("Duplicate Note", &DuplicateNote, cx))
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(DuplicateNote), cx);
            })
    }

    /// 10.5: Unified editor toolbar for the active entry. Replaces
    /// `render_note_controls` (notes-only) with a role-aware function that
    /// also handles file roles (CONTEXT SOURCES top-level, FolderChild,
    /// PROMPT, DEFAULT CONTEXT). Buttons dispatch actions; the action
    /// handlers (registered on the `Render` impl) call the shared helpers
    /// `add_active_file_to_default_context` / `remove_active_file_from_automation_context`
    /// which open the same `window.prompt` confirmation modal that the
    /// picker FolderChild Paperclip uses (10.11 single-channel design).
    fn render_entry_toolbar(&self, role: EntryToolbarRole) -> impl IntoElement {
        let action_group = match role {
            EntryToolbarRole::Note { default } => Some(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("toggle-default-note", IconName::Paperclip)
                            .toggle_state(default)
                            .when(default, |this| this.icon_color(Color::Accent))
                            .map(|this| {
                                if default {
                                    this.tooltip(Tooltip::text("Remove from Default Notes"))
                                } else {
                                    this.tooltip(move |_window, cx| {
                                        Tooltip::with_meta(
                                            "Add to Default Notes",
                                            None,
                                            "Always included in standalone runs.",
                                            cx,
                                        )
                                    })
                                }
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleDefaultNote), cx);
                            }),
                    )
                    .child(self.render_duplicate_note_button())
                    .child(
                        IconButton::new("delete-note", IconName::Trash)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Delete Note", &DeleteNote, cx)
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DeleteNote), cx);
                            }),
                    )
                    .into_any_element(),
            ),
            EntryToolbarRole::ContextSource { .. } => Some(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("file-add-default-context", IconName::Paperclip)
                            .tooltip(move |_window, cx| {
                                Tooltip::with_meta(
                                    "Add to Default Context",
                                    None,
                                    "Always included in every automation's context.",
                                    cx,
                                )
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(AddFileToDefaultContext), cx);
                            }),
                    )
                    .child(
                        IconButton::new("file-remove-from-automation", IconName::Trash)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action(
                                    "Remove from Automation Context",
                                    &RemoveFileFromAutomationContext,
                                    cx,
                                )
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(RemoveFileFromAutomationContext),
                                    cx,
                                );
                            }),
                    )
                    .into_any_element(),
            ),
            EntryToolbarRole::FolderChild { .. } => Some(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("folder-child-add-default-context", IconName::Paperclip)
                            .tooltip(move |_window, cx| {
                                Tooltip::with_meta(
                                    "Add to Default Context",
                                    None,
                                    "Always included in every automation's context.",
                                    cx,
                                )
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(AddFileToDefaultContext), cx);
                            }),
                    )
                    .into_any_element(),
            ),
            EntryToolbarRole::Prompt
            | EntryToolbarRole::DefaultContext
            | EntryToolbarRole::Unknown => None,
        };
        h_flex().gap_1().children(action_group)
    }

    fn render_active_editor_panel(
        &mut self,
        cx: &mut Context<PostProdRules>,
    ) -> gpui::Stateful<Div> {
        div()
            .id("entry-editor")
            .h_full()
            .flex_grow()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .children(self.active_entry.clone().and_then(|entry_id| {
                let registry = LanguageModelRegistry::read_global(cx);
                let model = registry.default_model().map(|default| default.model);

                match entry_id {
                    ActiveEntryId::Note(note_id) => {
                        let note_metadata = self.store.read(cx).metadata(note_id)?;
                        let note_editor = &self.note_editors[&note_id];
                        let focus_handle = note_editor.body_editor.focus_handle(cx);
                        let token_count = note_editor.token_count;

                        let mut panel = self.render_entry_inner(
                            Some(&note_editor.title_editor),
                            None,
                            &note_editor.body_editor,
                            focus_handle,
                            token_count,
                            model,
                            EntryToolbarRole::Note {
                                default: note_metadata.default,
                            },
                            false,
                            cx,
                        );
                        panel = panel.child(self.render_note_assignment(note_id, cx));
                        Some(panel)
                    }
                    ActiveEntryId::File(ref path) => {
                        let file_editor = self.file_editors.get(path)?;
                        let focus_handle = file_editor.body_editor.focus_handle(cx);
                        let token_count = file_editor.token_count;
                        let title = file_editor.title.clone();
                        let is_symlink = file_editor.is_symlink;
                        let automation = self.automation_for_prompt_file(path).cloned();

                        // 10.6: Path-based role classification at render time.
                        // Section info from the picker is lost at confirm time
                        // (`ActiveEntryId::File(path)`), so the toolbar derives
                        // the role here.
                        let toolbar_role = match self.classify_file_role(path) {
                            FileRole::Prompt => EntryToolbarRole::Prompt,
                            FileRole::DefaultContext => EntryToolbarRole::DefaultContext,
                            FileRole::ContextSource {
                                automation_id,
                                toml_index,
                            } => EntryToolbarRole::ContextSource {
                                automation_id,
                                toml_index,
                                source_path: path.clone(),
                            },
                            FileRole::FolderChildOf {
                                automation_id,
                                parent_toml_index: _,
                            } => EntryToolbarRole::FolderChild {
                                automation_id,
                                source_path: path.clone(),
                            },
                            FileRole::Unknown => EntryToolbarRole::Unknown,
                        };

                        let mut panel = self.render_entry_inner(
                            None,
                            Some(&title),
                            &file_editor.body_editor,
                            focus_handle,
                            token_count,
                            model,
                            toolbar_role,
                            is_symlink,
                            cx,
                        );
                        // In general mode, show context list below editor.
                        // In scoped mode, context is in the picker.
                        if !self.is_scoped() {
                            if let Some(automation) = &automation {
                                if !automation.contexts.is_empty() {
                                    panel = panel.child(self.render_context_list(automation, cx));
                                }
                            }
                        }
                        Some(panel)
                    }
                }
            }))
    }

    fn render_note_assignment(&self, note_id: NoteId, cx: &mut Context<Self>) -> impl IntoElement {
        let metadata = self.store.read(cx).metadata(note_id);
        let is_default = metadata.as_ref().map_or(false, |m| m.default);
        let assigned = metadata
            .as_ref()
            .map(|m| m.assigned_automations.clone())
            .unwrap_or_default();

        v_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new("Note Assignment")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Applied on standalone runs only")
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        IconButton::new("toggle-default-all", IconName::Paperclip)
                            .toggle_state(is_default)
                            .when(is_default, |this| this.icon_color(Color::Accent))
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_default_for_note(note_id, window, cx);
                            })),
                    )
                    .child(
                        Label::new(if is_default {
                            "Default (attached to all automations)"
                        } else {
                            "Assign to specific automations:"
                        })
                        .size(LabelSize::Small)
                        .color(if is_default {
                            Color::Accent
                        } else {
                            Color::Default
                        }),
                    ),
            )
            .when(!is_default, |this| {
                this.children(self.automations.iter().map(|automation| {
                    let auto_id = automation.id.clone();
                    let is_assigned = assigned.contains(&auto_id);
                    let auto_label: SharedString = automation.label.clone().into();

                    h_flex()
                        .gap_2()
                        .py_0p5()
                        .child(
                            IconButton::new(
                                SharedString::from(format!("assign-{}", auto_id)),
                                if is_assigned {
                                    IconName::Check
                                } else {
                                    IconName::Plus
                                },
                            )
                            .icon_size(IconSize::XSmall)
                            .icon_color(if is_assigned {
                                Color::Accent
                            } else {
                                Color::Muted
                            })
                            .on_click(cx.listener(
                                move |this, _, _window, cx| {
                                    this.toggle_note_automation_assignment(
                                        note_id,
                                        auto_id.clone(),
                                        cx,
                                    );
                                },
                            )),
                        )
                        .child(Label::new(auto_label).size(LabelSize::Small))
                        .into_any_element()
                }))
            })
    }

    fn toggle_note_automation_assignment(
        &mut self,
        note_id: NoteId,
        automation_id: String,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, move |store, cx| {
            if let Some(metadata) = store.metadata(note_id) {
                let mut assigned = metadata.assigned_automations.clone();
                if let Some(pos) = assigned.iter().position(|a| a == &automation_id) {
                    assigned.remove(pos);
                } else {
                    assigned.push(automation_id);
                }
                store
                    .save_metadata(note_id, metadata.title, metadata.default, assigned, cx)
                    .detach_and_log_err(cx);
            }
        });
        cx.notify();
    }

    /// Find the automation that owns the given prompt file.
    fn automation_for_prompt_file(&self, path: &Path) -> Option<&ResolvedAutomationInfo> {
        let filename = path.file_name()?.to_str()?;
        self.automations
            .iter()
            .find(|a| a.prompt_file.as_deref() == Some(filename))
    }

    /// 10.4: Pure path-based classification of a file loaded into the editor.
    /// Method-form thin wrapper over the free function `classify_file_role`
    /// so the latter can be unit-tested without constructing a full
    /// `PostProdRules` (no GPUI required).
    pub(crate) fn classify_file_role(&self, path: &Path) -> FileRole {
        classify_file_role(path, &self.config_root, &self.automations)
    }

    fn render_context_list(
        &self,
        automation: &ResolvedAutomationInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let auto_id = automation.id.clone();
        let callbacks = self.context_callbacks.clone();

        v_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new("Context Sources")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{} entries", automation.contexts.len()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .children(automation.contexts.iter().enumerate().map(|(ix, ctx)| {
                let auto_id_reorder = auto_id.clone();
                let auto_id_remove = auto_id.clone();
                let callbacks_reorder = callbacks.clone();
                let callbacks_remove = callbacks.clone();
                let is_first = ix == 0;
                let is_last = ix == automation.contexts.len() - 1;

                h_flex()
                    .gap_1()
                    .py_0p5()
                    .items_center()
                    .child(
                        Label::new(ctx.source_type.clone())
                            .size(LabelSize::XSmall)
                            .color(if ctx.source_type == "script" {
                                Color::Accent
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        Label::new(ctx.label.clone())
                            .size(LabelSize::Small)
                            .color(Color::Default)
                            .truncate(),
                    )
                    .when(!ctx.required, |this| {
                        this.child(
                            Label::new("optional")
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
                    .child(div().flex_grow())
                    .when_some(callbacks_reorder, |this, cb| {
                        let cb_up = cb.clone();
                        let cb_down = cb;
                        this.when(!is_first, |this| {
                            let auto_id = auto_id_reorder.clone();
                            this.child(
                                IconButton::new(
                                    SharedString::from(format!("ctx-up-{ix}")),
                                    IconName::ArrowUp,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .on_click(move |_, _window, cx| {
                                    (cb_up.reorder)(&auto_id, ix, -1, cx);
                                }),
                            )
                        })
                        .when(!is_last, |this| {
                            let auto_id = auto_id_reorder.clone();
                            this.child(
                                IconButton::new(
                                    SharedString::from(format!("ctx-down-{ix}")),
                                    IconName::ArrowDown,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .on_click(move |_, _window, cx| {
                                    (cb_down.reorder)(&auto_id, ix, 1, cx);
                                }),
                            )
                        })
                    })
                    .when_some(callbacks_remove, |this, cb| {
                        let auto_id = auto_id_remove;
                        this.child(
                            IconButton::new(
                                SharedString::from(format!("ctx-rm-{ix}")),
                                IconName::Close,
                            )
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .on_click(move |_, _window, cx| {
                                (cb.remove)(&auto_id, ix, cx);
                            }),
                        )
                    })
                    .into_any_element()
            }))
    }

    fn render_entry_inner(
        &self,
        title_editor: Option<&Entity<Editor>>,
        static_title: Option<&SharedString>,
        body_editor: &Entity<Editor>,
        focus_handle: gpui::FocusHandle,
        token_count: Option<u64>,
        model: Option<Arc<dyn language_model::LanguageModel>>,
        toolbar_role: EntryToolbarRole, // 10.6: replaces note_default Option<bool>
        is_symlink: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let title_section = if let Some(editor) = title_editor {
            self.render_title_editor(editor, cx).into_any_element()
        } else if let Some(title) = static_title {
            self.render_static_title(title, cx).into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .id("entry-editor-inner")
            .size_full()
            .relative()
            .overflow_hidden()
            .on_click(cx.listener(move |_, _, window, cx| {
                window.focus(&focus_handle, cx);
            }))
            .child(
                h_flex()
                    .group("active-editor-header")
                    .h_12()
                    .px_2()
                    .gap_2()
                    .justify_between()
                    .child(title_section)
                    .child(
                        h_flex()
                            .h_full()
                            .flex_shrink_0()
                            .children(token_count.map(|tc| {
                                let tc_str: SharedString = tc.to_string().into();
                                let label_tc: SharedString = tc_str.to_string().into();
                                div()
                                    .id("token_count")
                                    .mr_1()
                                    .flex_shrink_0()
                                    .tooltip(move |_window, cx| {
                                        Tooltip::with_meta(
                                            "Token Estimation",
                                            None,
                                            format!(
                                                "Model: {}",
                                                model
                                                    .as_ref()
                                                    .map(|m| m.name().0)
                                                    .unwrap_or_default()
                                            ),
                                            cx,
                                        )
                                    })
                                    .child(
                                        Label::new(format!("{} tokens", label_tc))
                                            .color(Color::Muted),
                                    )
                            }))
                            .when(is_symlink, |this| {
                                this.child(
                                    Label::new("symlink")
                                        .color(Color::Warning)
                                        .size(LabelSize::XSmall),
                                )
                            })
                            .child(self.render_entry_toolbar(toolbar_role)),
                    ),
            )
            .child(
                div()
                    .on_action(cx.listener(Self::focus_picker))
                    .on_action(cx.listener(Self::inline_assist))
                    .on_action(cx.listener(Self::move_up_from_body))
                    .h_full()
                    .flex_grow()
                    .child(
                        h_flex()
                            .py_2()
                            .pl_2p5()
                            .h_full()
                            .flex_1()
                            .child(body_editor.clone()),
                    ),
            )
    }
}

impl Render for PostProdRules {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let theme = cx.theme().clone();

        client_side_decorations(
            v_flex()
                .id("postprod-rules")
                .key_context("PostProdRules")
                .on_action(
                    |action_sequence: &ActionSequence, window: &mut Window, cx: &mut App| {
                        for action in &action_sequence.0 {
                            window.dispatch_action(action.boxed_clone(), cx);
                        }
                    },
                )
                .on_action(cx.listener(|this, &NewNote, window, cx| this.new_note(window, cx)))
                .on_action(
                    cx.listener(|this, &DeleteNote, window, cx| {
                        this.delete_active_note(window, cx)
                    }),
                )
                .on_action(cx.listener(|this, &DuplicateNote, window, cx| {
                    this.duplicate_active_note(window, cx)
                }))
                .on_action(cx.listener(|this, &ToggleDefaultNote, window, cx| {
                    this.toggle_default_for_active_note(window, cx)
                }))
                .on_action(cx.listener(
                    |this, &AddFileToDefaultContext, window, cx| {
                        this.add_active_file_to_default_context_action(window, cx)
                    },
                ))
                .on_action(cx.listener(
                    |this, &RemoveFileFromAutomationContext, window, cx| {
                        this.remove_active_file_from_automation_context_action(window, cx)
                    },
                ))
                .size_full()
                .overflow_hidden()
                .font(ui_font)
                .text_color(theme.colors().text)
                .children(self.title_bar.clone())
                .bg(theme.colors().background)
                .child(
                    h_flex()
                        .flex_1()
                        .when(!cfg!(target_os = "macos"), |this| {
                            this.border_t_1().border_color(cx.theme().colors().border)
                        })
                        .child(self.render_note_list(cx))
                        .child(self.render_active_editor_panel(cx)),
                ),
            window,
            cx,
            Tiling::default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_md_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.md"), "# Foo").unwrap();
        std::fs::write(dir.path().join("bar.md"), "# Bar").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "nope").unwrap();

        let entries = scan_md_files(dir.path(), PickerSection::Prompt);
        assert_eq!(entries.len(), 2);
        // Sorted by display_name: "bar" < "foo"
        assert_eq!(entries[0].display_name, "bar");
        assert_eq!(entries[1].display_name, "foo");
        assert!(!entries[0].is_symlink);
        assert!(!entries[1].is_symlink);
    }

    #[test]
    fn scan_detects_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_path = dir.path().join("real.md");
        std::fs::write(&real_path, "# Real").unwrap();
        symlink(&real_path, dir.path().join("linked.md")).unwrap();

        let entries = scan_md_files(dir.path(), PickerSection::DefaultContext);
        assert_eq!(entries.len(), 2);

        let real_entry = entries.iter().find(|e| e.filename == "real.md").unwrap();
        let link_entry = entries.iter().find(|e| e.filename == "linked.md").unwrap();

        assert!(!real_entry.is_symlink);
        assert!(link_entry.is_symlink);
    }

    #[test]
    fn scan_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let entries = scan_md_files(dir.path(), PickerSection::Prompt);
        assert!(entries.is_empty());
    }

    #[test]
    fn scan_missing_dir() {
        let entries = scan_md_files(Path::new("/nonexistent/path"), PickerSection::Prompt);
        assert!(entries.is_empty());
    }

    // -----------------------------------------------------------------------
    // Phase 9 — Scoped picker tests
    // -----------------------------------------------------------------------

    fn make_test_prompt_files(dir: &Path) -> Vec<PromptFileEntry> {
        for name in &["alpha.md", "beta.md", "gamma.md"] {
            std::fs::write(dir.join(name), format!("# {name}")).unwrap();
        }
        scan_md_files(dir, PickerSection::Prompt)
    }

    fn make_test_default_context(dir: &Path) -> Vec<PromptFileEntry> {
        for name in &["project-notes.md", "tier-guidelines.md"] {
            std::fs::write(dir.join(name), format!("# {name}")).unwrap();
        }
        scan_md_files(dir, PickerSection::DefaultContext)
    }

    fn count_entries_of_type(entries: &[PostProdPickerEntry], section: &str) -> usize {
        let mut in_section = false;
        let mut count = 0;
        for entry in entries {
            match entry {
                PostProdPickerEntry::Header(h) if h.as_ref() == section => in_section = true,
                PostProdPickerEntry::Header(_) | PostProdPickerEntry::Separator => {
                    if in_section {
                        break;
                    }
                }
                _ if in_section => count += 1,
                _ => {}
            }
        }
        count
    }

    #[test]
    fn scoped_picker_shows_only_target_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join("config/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let prompt_files = make_test_prompt_files(&prompts_dir);
        let dc_dir = dir.path().join("config/default-context");
        std::fs::create_dir_all(&dc_dir).unwrap();
        let dc_files = make_test_default_context(&dc_dir);

        let automation = ResolvedAutomationInfo {
            id: "test-auto".into(),
            label: "Test Auto".into(),
            prompt_file: Some("beta.md".into()),
            contexts: vec![],
            skip_default_context: false,
        };

        let mut entries = Vec::new();
        build_scoped_entries(
            &automation,
            &prompt_files,
            &dc_files,
            &[],
            &collections::HashSet::default(),
            "",
            &mut entries,
        );

        // Should have exactly 1 prompt file (beta.md)
        let prompt_count = count_entries_of_type(&entries, "PROMPT");
        assert_eq!(prompt_count, 1);

        // Verify it's the right one
        let prompt_entry = entries.iter().find_map(|e| match e {
            PostProdPickerEntry::PromptFile(f) => Some(f),
            _ => None,
        });
        assert_eq!(prompt_entry.map(|f| f.filename.as_str()), Some("beta.md"));
    }

    #[test]
    fn scoped_picker_shows_context_sources() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join("config/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let prompt_files = make_test_prompt_files(&prompts_dir);
        let dc_dir = dir.path().join("config/default-context");
        std::fs::create_dir_all(&dc_dir).unwrap();
        let dc_files = make_test_default_context(&dc_dir);

        // Create test context files
        let ctx_file1 = dir.path().join("context1.md");
        let ctx_file2 = dir.path().join("context2.sh");
        std::fs::write(&ctx_file1, "context content").unwrap();
        std::fs::write(&ctx_file2, "#!/bin/bash").unwrap();

        let automation = ResolvedAutomationInfo {
            id: "test-auto".into(),
            label: "Test Auto".into(),
            prompt_file: Some("alpha.md".into()),
            contexts: vec![
                ResolvedContextInfo {
                    source_type: "path".into(),
                    label: "context1.md".into(),
                    resolved_path: ctx_file1,
                    required: true,
                },
                ResolvedContextInfo {
                    source_type: "script".into(),
                    label: "context2.sh".into(),
                    resolved_path: ctx_file2,
                    required: false,
                },
            ],
            skip_default_context: false,
        };

        let mut entries = Vec::new();
        build_scoped_entries(
            &automation,
            &prompt_files,
            &dc_files,
            &[],
            &collections::HashSet::default(),
            "",
            &mut entries,
        );

        // Context sources: 2 entries + 1 AddContextAction
        let ctx_count = count_entries_of_type(&entries, "CONTEXT SOURCES");
        assert_eq!(ctx_count, 3); // 2 sources + 1 add action
    }

    #[test]
    fn scoped_picker_skips_default_context_when_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join("config/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let prompt_files = make_test_prompt_files(&prompts_dir);
        let dc_dir = dir.path().join("config/default-context");
        std::fs::create_dir_all(&dc_dir).unwrap();
        let dc_files = make_test_default_context(&dc_dir);

        let automation = ResolvedAutomationInfo {
            id: "test-auto".into(),
            label: "Test Auto".into(),
            prompt_file: Some("alpha.md".into()),
            contexts: vec![],
            skip_default_context: true,
        };

        let mut entries = Vec::new();
        build_scoped_entries(
            &automation,
            &prompt_files,
            &dc_files,
            &[],
            &collections::HashSet::default(),
            "",
            &mut entries,
        );

        // No DEFAULT CONTEXT header should exist
        let has_dc = entries.iter().any(
            |e| matches!(e, PostProdPickerEntry::Header(h) if h.as_ref() == "DEFAULT CONTEXT"),
        );
        assert!(!has_dc);
    }

    #[test]
    fn scoped_picker_shows_assigned_and_default_notes() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join("config/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let prompt_files = make_test_prompt_files(&prompts_dir);

        let default_note = NoteMetadata {
            id: NoteId::new(),
            title: Some("Default Note".into()),
            default: true,
            assigned_automations: vec![],
            saved_at: chrono::Utc::now(),
        };
        let assigned_note = NoteMetadata {
            id: NoteId::new(),
            title: Some("Assigned Note".into()),
            default: false,
            assigned_automations: vec!["test-auto".into()],
            saved_at: chrono::Utc::now(),
        };
        let unrelated_note = NoteMetadata {
            id: NoteId::new(),
            title: Some("Unrelated Note".into()),
            default: false,
            assigned_automations: vec!["other-auto".into()],
            saved_at: chrono::Utc::now(),
        };
        let all_notes = vec![default_note, assigned_note, unrelated_note];

        let automation = ResolvedAutomationInfo {
            id: "test-auto".into(),
            label: "Test Auto".into(),
            prompt_file: Some("alpha.md".into()),
            contexts: vec![],
            skip_default_context: true,
        };

        let mut entries = Vec::new();
        build_scoped_entries(
            &automation,
            &prompt_files,
            &[],
            &all_notes,
            &collections::HashSet::default(),
            "",
            &mut entries,
        );

        // Notes section should show 2 notes (default + assigned) + 1 AddNoteAction
        let note_count = count_entries_of_type(&entries, "NOTES");
        assert_eq!(note_count, 3); // 2 notes + 1 add action
    }

    #[test]
    fn folder_expansion_inserts_children() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("reference");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("doc_a.md"), "a").unwrap();
        std::fs::write(folder.join("doc_b.md"), "b").unwrap();

        let automation = ResolvedAutomationInfo {
            id: "test-auto".into(),
            label: "Test Auto".into(),
            prompt_file: None,
            contexts: vec![ResolvedContextInfo {
                source_type: "path".into(),
                label: "reference/".into(),
                resolved_path: folder.clone(),
                required: true,
            }],
            skip_default_context: true,
        };

        let mut expanded = collections::HashSet::default();
        expanded.insert(folder);

        let mut entries = Vec::new();
        build_scoped_entries(&automation, &[], &[], &[], &expanded, "", &mut entries);

        let folder_children: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e, PostProdPickerEntry::FolderChild(_)))
            .collect();
        assert_eq!(folder_children.len(), 2);
    }

    #[test]
    fn folder_expansion_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("empty-folder");
        std::fs::create_dir_all(&folder).unwrap();

        let automation = ResolvedAutomationInfo {
            id: "test-auto".into(),
            label: "Test Auto".into(),
            prompt_file: None,
            contexts: vec![ResolvedContextInfo {
                source_type: "path".into(),
                label: "empty-folder/".into(),
                resolved_path: folder.clone(),
                required: true,
            }],
            skip_default_context: true,
        };

        let mut expanded = collections::HashSet::default();
        expanded.insert(folder);

        let mut entries = Vec::new();
        build_scoped_entries(&automation, &[], &[], &[], &expanded, "", &mut entries);

        let folder_children: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e, PostProdPickerEntry::FolderChild(_)))
            .collect();
        assert_eq!(folder_children.len(), 0);
    }

    #[test]
    fn scan_scripts_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gather.sh"), "#!/bin/bash").unwrap();
        std::fs::write(dir.path().join("collect.py"), "#!/usr/bin/env python3").unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("nested.sh"), "#!/bin/bash").unwrap();

        let scripts = scan_scripts(dir.path());
        // Only files in the directory, not subdirs
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0], "collect.py");
        assert_eq!(scripts[1], "gather.sh");
    }

    // -----------------------------------------------------------------------
    // Phase 10 — File classification + toolbar
    // -----------------------------------------------------------------------

    /// 10.15
    #[test]
    fn classify_file_role_returns_correct_variant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Layout:
        //   <root>/config/prompts/foo.md
        //   <root>/config/default-context/bar.md
        //   <root>/some-place/single.md          (top-level [[context]])
        //   <root>/some-folder/                  (directory [[context]])
        //   <root>/some-folder/inner.md          (FolderChildOf)
        //   <root>/elsewhere/orphan.md           (Unknown)
        let prompts_dir = root.join("config/prompts");
        let dc_dir = root.join("config/default-context");
        let single = root.join("some-place/single.md");
        let folder = root.join("some-folder");
        let inner = folder.join("inner.md");
        let orphan = root.join("elsewhere/orphan.md");
        for d in [
            &prompts_dir,
            &dc_dir,
            &single.parent().unwrap().to_path_buf(),
            &folder,
            &orphan.parent().unwrap().to_path_buf(),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(prompts_dir.join("foo.md"), "# foo").unwrap();
        std::fs::write(dc_dir.join("bar.md"), "# bar").unwrap();
        std::fs::write(&single, "# single").unwrap();
        std::fs::write(&inner, "# inner").unwrap();
        std::fs::write(&orphan, "# orphan").unwrap();

        let automation = ResolvedAutomationInfo {
            id: "auto-1".into(),
            label: "Auto 1".into(),
            prompt_file: None,
            contexts: vec![
                ResolvedContextInfo {
                    source_type: "path".into(),
                    label: "single".into(),
                    resolved_path: single.clone(),
                    required: true,
                },
                ResolvedContextInfo {
                    source_type: "path".into(),
                    label: "folder".into(),
                    resolved_path: folder,
                    required: true,
                },
            ],
            skip_default_context: false,
        };
        let automations = vec![automation];

        assert_eq!(
            classify_file_role(&prompts_dir.join("foo.md"), root, &automations),
            FileRole::Prompt
        );
        assert_eq!(
            classify_file_role(&dc_dir.join("bar.md"), root, &automations),
            FileRole::DefaultContext
        );
        assert_eq!(
            classify_file_role(&single, root, &automations),
            FileRole::ContextSource {
                automation_id: "auto-1".into(),
                toml_index: 0,
            }
        );
        assert_eq!(
            classify_file_role(&inner, root, &automations),
            FileRole::FolderChildOf {
                automation_id: "auto-1".into(),
                parent_toml_index: 1,
            }
        );
        assert_eq!(
            classify_file_role(&orphan, root, &automations),
            FileRole::Unknown
        );
    }

    /// 10.18 — pre-flight rejects collision.
    #[test]
    fn validate_promotion_refuses_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("dc");
        std::fs::create_dir_all(&target_dir).unwrap();
        let source = dir.path().join("note.md");
        std::fs::write(&source, "# note").unwrap();
        std::fs::write(target_dir.join("note.md"), "# pre-existing").unwrap();
        assert_eq!(
            validate_promotion(&source, &target_dir),
            Err(PromotionPreflightError::Collision)
        );
    }

    /// 10.19 — pre-flight rejects symlink source.
    #[test]
    fn validate_promotion_refuses_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("dc");
        std::fs::create_dir_all(&target_dir).unwrap();
        let real = dir.path().join("real.md");
        std::fs::write(&real, "# real").unwrap();
        let link = dir.path().join("link.md");
        symlink(&real, &link).unwrap();
        assert_eq!(
            validate_promotion(&link, &target_dir),
            Err(PromotionPreflightError::Symlink)
        );
    }

    /// 10.20a — pre-flight rejects non-`.md` extensions.
    #[test]
    fn validate_promotion_rejects_non_md_extension() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("dc");
        std::fs::create_dir_all(&target_dir).unwrap();
        let source_toml = dir.path().join("note.toml");
        std::fs::write(&source_toml, "x=1").unwrap();
        assert_eq!(
            validate_promotion(&source_toml, &target_dir),
            Err(PromotionPreflightError::NotMarkdown)
        );
        let source_txt = dir.path().join("note.txt");
        std::fs::write(&source_txt, "txt").unwrap();
        assert_eq!(
            validate_promotion(&source_txt, &target_dir),
            Err(PromotionPreflightError::NotMarkdown)
        );
    }

    /// Sanity: validate_promotion succeeds in the happy path.
    #[test]
    fn validate_promotion_ok_for_fresh_md() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("dc");
        std::fs::create_dir_all(&target_dir).unwrap();
        let source = dir.path().join("fresh.md");
        std::fs::write(&source, "# fresh").unwrap();
        let target = validate_promotion(&source, &target_dir).unwrap();
        assert_eq!(target, target_dir.join("fresh.md"));
    }

    /// 10.20b — pre-flight rejects nonexistent source (user moved/deleted file mid-flight).
    #[test]
    fn validate_promotion_returns_source_missing_for_nonexistent_source() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("dc");
        std::fs::create_dir_all(&target_dir).unwrap();
        let source = dir.path().join("missing.md");
        assert_eq!(
            validate_promotion(&source, &target_dir),
            Err(PromotionPreflightError::SourceMissing)
        );
    }
}
