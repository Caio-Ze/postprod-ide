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
use language_model::{
    ConfiguredModel, LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage, Role,
};
use note_store::*;
use picker::{Picker, PickerDelegate};
use platform_title_bar::PlatformTitleBar;
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

/// Selection target for deep linking into the window.
pub enum SelectionTarget {
    Note(NoteId),
    PromptFile(String),
}

/// Lightweight automation info passed by Dashboard.
#[derive(Clone, Debug)]
pub struct AutomationInfo {
    pub id: String,
    pub label: String,
    pub prompt_file: Option<String>,
    pub contexts: Vec<ContextInfo>,
    pub skip_default_context: bool,
}

/// Lightweight context entry info for display in the window.
#[derive(Clone, Debug)]
pub struct ContextInfo {
    pub source_type: String, // "path" or "script"
    pub label: String,
    pub required: bool,
}

/// Callbacks for the window to modify TOML context entries via Dashboard.
/// The closures run in the calling window's App context.
pub struct ContextCallbacks {
    pub reorder: Box<dyn Fn(&str, usize, i32, &mut App) + Send + Sync>,
    pub remove: Box<dyn Fn(&str, usize, &mut App) + Send + Sync>,
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

#[derive(Clone, Debug, PartialEq)]
enum ActiveEntryId {
    Note(NoteId),
    File(PathBuf),
}

// ---------------------------------------------------------------------------
// Window open
// ---------------------------------------------------------------------------

pub fn open_postprod_rules(
    store: Entity<NoteStore>,
    language_registry: Arc<LanguageRegistry>,
    inline_assist_delegate: Box<dyn InlineAssistDelegate>,
    config_root: PathBuf,
    automations: Vec<AutomationInfo>,
    context_callbacks: Option<Arc<ContextCallbacks>>,
    selection: Option<SelectionTarget>,
    cx: &mut App,
) -> Task<Result<WindowHandle<PostProdRules>>> {
    cx.spawn(async move |cx| {
        let existing_window = cx.update(|cx| {
            let existing_window = cx
                .windows()
                .into_iter()
                .find_map(|window| window.downcast::<PostProdRules>());
            if let Some(existing_window) = existing_window {
                existing_window
                    .update(cx, |rules, window, cx| {
                        match &selection {
                            Some(SelectionTarget::Note(id)) => {
                                rules.load_note(*id, true, window, cx);
                            }
                            Some(SelectionTarget::PromptFile(filename)) => {
                                rules.select_prompt_file(filename, window, cx);
                            }
                            None => {}
                        }
                        window.activate_window()
                    })
                    .ok();
                Some(existing_window)
            } else {
                None
            }
        });

        if let Some(existing_window) = existing_window {
            return Ok(existing_window);
        }

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
                    kind: gpui::WindowKind::Floating,
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
                            selection,
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
    #[allow(dead_code)]
    config_root: PathBuf,
    automations: Vec<AutomationInfo>,
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
    Separator,
}

struct PostProdPickerDelegate {
    store: Entity<NoteStore>,
    prompt_files: Vec<PromptFileEntry>,
    default_context_files: Vec<PromptFileEntry>,
    selected_index: usize,
    filtered_entries: Vec<PostProdPickerEntry>,
}

enum PostProdPickerEvent {
    Selected { entry_id: ActiveEntryId },
    Confirmed { entry_id: ActiveEntryId },
    Deleted { note_id: NoteId },
    ToggledDefault { note_id: NoteId },
}

impl EventEmitter<PostProdPickerEvent> for Picker<PostProdPickerDelegate> {}

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
        let display_name = filename
            .trim_end_matches(".md")
            .replace(['-', '_'], " ");
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

fn entry_id_for_picker_entry(entry: &PostProdPickerEntry) -> Option<ActiveEntryId> {
    match entry {
        PostProdPickerEntry::Note(n) => Some(ActiveEntryId::Note(n.id)),
        PostProdPickerEntry::PromptFile(f) | PostProdPickerEntry::DefaultContextFile(f) => {
            Some(ActiveEntryId::File(f.path.clone()))
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
            | Some(PostProdPickerEntry::DefaultContextFile(_)) => true,
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
        let search = self.store.read(cx).search(query.clone(), cancellation_flag, cx);

        let prompt_files = self.prompt_files.clone();
        let default_context_files = self.default_context_files.clone();

        let prev_entry_id = self
            .filtered_entries
            .get(self.selected_index)
            .and_then(entry_id_for_picker_entry);

        cx.spawn_in(window, async move |this, cx| {
            let (filtered_entries, selected_index) = cx
                .background_spawn(async move {
                    let note_matches = search.await;
                    let query_lower = query.to_lowercase();

                    // Filter prompt files by query
                    let filtered_prompts: Vec<_> = if query.is_empty() {
                        prompt_files
                    } else {
                        prompt_files
                            .into_iter()
                            .filter(|f| f.display_name.to_lowercase().contains(&query_lower))
                            .collect()
                    };

                    // Filter default context files by query
                    let filtered_dc: Vec<_> = if query.is_empty() {
                        default_context_files
                    } else {
                        default_context_files
                            .into_iter()
                            .filter(|f| f.display_name.to_lowercase().contains(&query_lower))
                            .collect()
                    };

                    let mut filtered_entries = Vec::new();

                    // Section: PROMPTS
                    if !filtered_prompts.is_empty() {
                        filtered_entries
                            .push(PostProdPickerEntry::Header("PROMPTS".into()));
                        for f in filtered_prompts {
                            filtered_entries.push(PostProdPickerEntry::PromptFile(f));
                        }
                        filtered_entries.push(PostProdPickerEntry::Separator);
                    }

                    // Section: NOTES
                    let (default_notes, other_notes): (Vec<_>, Vec<_>) =
                        note_matches.into_iter().partition(|note| note.default);

                    if !default_notes.is_empty() || !other_notes.is_empty() {
                        filtered_entries
                            .push(PostProdPickerEntry::Header("NOTES".into()));

                        if !default_notes.is_empty() {
                            for note in default_notes {
                                filtered_entries.push(PostProdPickerEntry::Note(note));
                            }
                        }
                        for note in other_notes {
                            filtered_entries.push(PostProdPickerEntry::Note(note));
                        }
                        filtered_entries.push(PostProdPickerEntry::Separator);
                    }

                    // Section: DEFAULT CONTEXT
                    if !filtered_dc.is_empty() {
                        filtered_entries
                            .push(PostProdPickerEntry::Header("DEFAULT CONTEXT".into()));
                        for f in filtered_dc {
                            filtered_entries
                                .push(PostProdPickerEntry::DefaultContextFile(f));
                        }
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
                                            cx.emit(PostProdPickerEvent::ToggledDefault {
                                                note_id,
                                            })
                                        })),
                                ),
                        )
                        .into_any_element(),
                )
            }
            PostProdPickerEntry::PromptFile(file) => {
                let label: SharedString = file.display_name.clone().into();
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
                        .child(Label::new(label).truncate().mr_10())
                        .end_slot::<Label>(Some(
                            Label::new(file.filename.clone())
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ))
                        .into_any_element(),
                )
            }
            PostProdPickerEntry::DefaultContextFile(file) => {
                let label: SharedString = file.display_name.clone().into();
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
                        .child(Label::new(label).truncate().mr_10())
                        .when(file.is_symlink, |this| {
                            this.end_slot::<Label>(Some(
                                Label::new("symlink")
                                    .color(Color::Warning)
                                    .size(LabelSize::XSmall),
                            ))
                        })
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
        automations: Vec<AutomationInfo>,
        context_callbacks: Option<Arc<ContextCallbacks>>,
        selection: Option<SelectionTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let prompt_files =
            scan_md_files(&config_root.join("config/prompts"), PickerSection::Prompt);
        let default_context_files = scan_md_files(
            &config_root.join("config/default-context"),
            PickerSection::DefaultContext,
        );

        let picker_delegate = PostProdPickerDelegate {
            store: store.clone(),
            prompt_files,
            default_context_files,
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

        match selection {
            Some(SelectionTarget::Note(id)) => {
                this.load_note(id, true, window, cx);
            }
            Some(SelectionTarget::PromptFile(filename)) => {
                this.select_prompt_file(&filename, window, cx);
            }
            None => {}
        }

        this
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
        }
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
                .trim_end_matches(".md")
                .replace(['-', '_'], " ")
                .into();

            self.pending_load = cx.spawn_in(window, async move |this, cx| {
                let content = cx
                    .background_executor()
                    .spawn(async move { std::fs::read_to_string(&load_path) })
                    .await;
                let markdown = language_registry.language_for_name("Markdown").await;

                this.update_in(cx, |this, window, cx| match content {
                    Ok(content) => {
                        let editor_path = path.clone();
                        let body_editor = cx.new(|cx| {
                            let buffer = cx.new(|cx| {
                                let mut buffer = Buffer::local(content, cx);
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

    pub fn delete_note(
        &mut self,
        note_id: NoteId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub fn duplicate_note(
        &mut self,
        note_id: NoteId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                store.save(new_id, Some(title.into()), false, Vec::new(), body.into(), cx)
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
            Some(ActiveEntryId::Note(id)) => {
                self.note_editors.get(id).map(|e| &e.body_editor)
            }
            Some(ActiveEntryId::File(path)) => {
                self.file_editors.get(path).map(|e| &e.body_editor)
            }
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
        let Some(ConfiguredModel { model, .. }) =
            LanguageModelRegistry::read_global(cx).default_model()
        else {
            return;
        };
        if let Some(note) = self.note_editors.get_mut(&note_id) {
            let body = note
                .body_editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
                .read(cx)
                .as_rope()
                .clone();
            note.pending_token_count = cx.spawn_in(window, async move |this, cx| {
                async move {
                    cx.background_executor()
                        .timer(Duration::from_secs(1))
                        .await;
                    let token_count = cx
                        .update(|_, cx| {
                            model.count_tokens(
                                LanguageModelRequest {
                                    thread_id: None,
                                    prompt_id: None,
                                    intent: None,
                                    messages: vec![LanguageModelRequestMessage {
                                        role: Role::System,
                                        content: vec![body.to_string().into()],
                                        cache: false,
                                        reasoning_details: None,
                                    }],
                                    tools: Vec::new(),
                                    tool_choice: None,
                                    stop: Vec::new(),
                                    temperature: None,
                                    thinking_allowed: true,
                                    thinking_effort: None,
                                    speed: None,
                                },
                                cx,
                            )
                        })?
                        .await?;

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
        let Some(ConfiguredModel { model, .. }) =
            LanguageModelRegistry::read_global(cx).default_model()
        else {
            return;
        };
        if let Some(file_editor) = self.file_editors.get_mut(&path) {
            let body = file_editor
                .body_editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
                .read(cx)
                .as_rope()
                .clone();
            file_editor.pending_token_count = cx.spawn_in(window, async move |this, cx| {
                async move {
                    cx.background_executor()
                        .timer(Duration::from_secs(1))
                        .await;
                    let token_count = cx
                        .update(|_, cx| {
                            model.count_tokens(
                                LanguageModelRequest {
                                    thread_id: None,
                                    prompt_id: None,
                                    intent: None,
                                    messages: vec![LanguageModelRequestMessage {
                                        role: Role::System,
                                        content: vec![body.to_string().into()],
                                        cache: false,
                                        reasoning_details: None,
                                    }],
                                    tools: Vec::new(),
                                    tool_choice: None,
                                    stop: Vec::new(),
                                    temperature: None,
                                    thinking_allowed: true,
                                    thinking_effort: None,
                                    speed: None,
                                },
                                cx,
                            )
                        })?
                        .await?;

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

    fn render_static_title(&self, title: &SharedString, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .pl_1()
            .child(
                Label::new(title.clone())
                    .size(LabelSize::Large)
                    .color(Color::Default),
            )
    }

    fn render_duplicate_note_button(&self) -> impl IntoElement {
        IconButton::new("duplicate-note", IconName::BookCopy)
            .tooltip(move |_window, cx| {
                Tooltip::for_action("Duplicate Note", &DuplicateNote, cx)
            })
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(DuplicateNote), cx);
            })
    }

    fn render_note_controls(&self, default: bool) -> impl IntoElement {
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
                            Some(note_metadata.default),
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

                        let mut panel = self.render_entry_inner(
                            None,
                            Some(&title),
                            &file_editor.body_editor,
                            focus_handle,
                            token_count,
                            model,
                            None, // files don't have default toggle
                            is_symlink,
                            cx,
                        );
                        if let Some(automation) = &automation {
                            if !automation.contexts.is_empty() {
                                panel = panel.child(self.render_context_list(automation, cx));
                            }
                        }
                        Some(panel)
                    }
                }
            }))
    }

    fn render_note_assignment(
        &self,
        note_id: NoteId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.toggle_note_automation_assignment(
                                    note_id,
                                    auto_id.clone(),
                                    cx,
                                );
                            })),
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
    fn automation_for_prompt_file(&self, path: &Path) -> Option<&AutomationInfo> {
        let filename = path.file_name()?.to_str()?;
        self.automations
            .iter()
            .find(|a| a.prompt_file.as_deref() == Some(filename))
    }

    fn render_context_list(
        &self,
        automation: &AutomationInfo,
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
        note_default: Option<bool>, // Some = note (show controls), None = file
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
                            .when_some(note_default, |this, default| {
                                this.child(self.render_note_controls(default))
                            }),
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
                .on_action(cx.listener(|this, &DeleteNote, window, cx| {
                    this.delete_active_note(window, cx)
                }))
                .on_action(cx.listener(|this, &DuplicateNote, window, cx| {
                    this.duplicate_active_note(window, cx)
                }))
                .on_action(cx.listener(|this, &ToggleDefaultNote, window, cx| {
                    this.toggle_default_for_active_note(window, cx)
                }))
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
}
