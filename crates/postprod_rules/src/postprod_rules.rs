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
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use theme_settings::ThemeSettings;
use ui::{Divider, ListItem, ListItemSpacing, ListSubHeader, Tooltip, prelude::*};
use ui_input::ErasedEditor;
use util::{ResultExt, TryFutureExt};
use workspace::{MultiWorkspace, Workspace, WorkspaceSettings, client_side_decorations};
use zed_actions::assistant::InlineAssist;

pub fn init(cx: &mut App) {
    cx.observe_new::<Workspace>(|_, _, _cx| {
        // Action registration happens via the key_context on the window element.
        // No global state needed — NoteStore is created by Dashboard per-project.
    })
    .detach();
}

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

    /// Returns whether the Agent panel was focused.
    fn focus_agent_panel(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool;
}

/// Opens or focuses the PostProd Rules window.
///
/// Unlike upstream's `open_rules_library()` which uses a global PromptStore,
/// this accepts an `Entity<NoteStore>` because our store is per-project.
pub fn open_postprod_rules(
    store: Entity<NoteStore>,
    language_registry: Arc<LanguageRegistry>,
    inline_assist_delegate: Box<dyn InlineAssistDelegate>,
    note_to_select: Option<NoteId>,
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
                        if let Some(note_to_select) = note_to_select {
                            rules.load_note(note_to_select, true, window, cx);
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
                            note_to_select,
                            window,
                            cx,
                        )
                    })
                },
            )
        })
    })
}

pub struct PostProdRules {
    title_bar: Option<Entity<PlatformTitleBar>>,
    store: Entity<NoteStore>,
    language_registry: Arc<LanguageRegistry>,
    note_editors: HashMap<NoteId, NoteEditor>,
    active_note_id: Option<NoteId>,
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

enum PostProdPickerEntry {
    Header(SharedString),
    Note(NoteMetadata),
    Separator,
}

struct PostProdPickerDelegate {
    store: Entity<NoteStore>,
    selected_index: usize,
    filtered_entries: Vec<PostProdPickerEntry>,
}

enum PostProdPickerEvent {
    Selected { note_id: NoteId },
    Confirmed { note_id: NoteId },
    Deleted { note_id: NoteId },
    ToggledDefault { note_id: NoteId },
}

impl EventEmitter<PostProdPickerEvent> for Picker<PostProdPickerDelegate> {}

impl PickerDelegate for PostProdPickerDelegate {
    type ListItem = AnyElement;

    fn match_count(&self) -> usize {
        self.filtered_entries.len()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No notes found matching your search.".into())
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix.min(self.filtered_entries.len().saturating_sub(1));

        if let Some(PostProdPickerEntry::Note(note)) =
            self.filtered_entries.get(self.selected_index)
        {
            cx.emit(PostProdPickerEvent::Selected { note_id: note.id });
        }

        cx.notify();
    }

    fn can_select(&self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) -> bool {
        match self.filtered_entries.get(ix) {
            Some(PostProdPickerEntry::Note(_)) => true,
            Some(PostProdPickerEntry::Header(_))
            | Some(PostProdPickerEntry::Separator)
            | None => false,
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
        let search = self.store.read(cx).search(query, cancellation_flag, cx);

        let prev_note_id = self
            .filtered_entries
            .get(self.selected_index)
            .and_then(|entry| {
                if let PostProdPickerEntry::Note(note) = entry {
                    Some(note.id)
                } else {
                    None
                }
            });

        cx.spawn_in(window, async move |this, cx| {
            let (filtered_entries, selected_index) = cx
                .background_spawn(async move {
                    let matches = search.await;

                    let (default_notes, other_notes): (Vec<_>, Vec<_>) =
                        matches.into_iter().partition(|note| note.default);

                    let mut filtered_entries = Vec::new();

                    if !default_notes.is_empty() {
                        filtered_entries
                            .push(PostProdPickerEntry::Header("Default Notes".into()));

                        for note in default_notes {
                            filtered_entries.push(PostProdPickerEntry::Note(note));
                        }

                        filtered_entries.push(PostProdPickerEntry::Separator);
                    }

                    for note in other_notes {
                        filtered_entries.push(PostProdPickerEntry::Note(note));
                    }

                    let selected_index = prev_note_id
                        .and_then(|prev_note_id| {
                            filtered_entries.iter().position(|entry| {
                                if let PostProdPickerEntry::Note(note) = entry {
                                    note.id == prev_note_id
                                } else {
                                    false
                                }
                            })
                        })
                        .unwrap_or_else(|| {
                            filtered_entries
                                .iter()
                                .position(|entry| {
                                    matches!(entry, PostProdPickerEntry::Note(_))
                                })
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
        if let Some(PostProdPickerEntry::Note(note)) =
            self.filtered_entries.get(self.selected_index)
        {
            cx.emit(PostProdPickerEvent::Confirmed { note_id: note.id });
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
            PostProdPickerEntry::Header(title) => {
                let tooltip_text =
                    "Default Notes are attached to every standalone automation run.";

                Some(
                    ListSubHeader::new(title.clone())
                        .end_slot(
                            IconButton::new("info", IconName::Info)
                                .style(ButtonStyle::Transparent)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text(tooltip_text))
                                .into_any_element(),
                        )
                        .inset(true)
                        .into_any_element(),
                )
            }
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

impl PostProdRules {
    fn new(
        store: Entity<NoteStore>,
        language_registry: Arc<LanguageRegistry>,
        inline_assist_delegate: Box<dyn InlineAssistDelegate>,
        note_to_select: Option<NoteId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker_delegate = PostProdPickerDelegate {
            store: store.clone(),
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
            note_editors: HashMap::default(),
            active_note_id: None,
            pending_load: Task::ready(()),
            inline_assist_delegate,
            _subscriptions: vec![cx.subscribe_in(&picker, window, Self::handle_picker_event)],
            picker,
        };

        if let Some(note_to_select) = note_to_select {
            this.load_note(note_to_select, true, window, cx);
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
            PostProdPickerEvent::Selected { note_id } => {
                self.load_note(*note_id, false, window, cx);
            }
            PostProdPickerEvent::Confirmed { note_id } => {
                self.load_note(*note_id, true, window, cx);
            }
            PostProdPickerEvent::ToggledDefault { note_id } => {
                self.toggle_default_for_note(*note_id, window, cx);
            }
            PostProdPickerEvent::Deleted { note_id } => {
                self.delete_note(*note_id, window, cx);
            }
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

    pub fn delete_active_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active_note_id) = self.active_note_id {
            self.delete_note(active_note_id, window, cx);
        }
    }

    pub fn duplicate_active_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active_note_id) = self.active_note_id {
            self.duplicate_note(active_note_id, window, cx);
        }
    }

    pub fn toggle_default_for_active_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active_note_id) = self.active_note_id {
            self.toggle_default_for_note(active_note_id, window, cx);
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
            self.set_active_note(Some(note_id), window, cx);
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
                                metadata
                                    .and_then(|m| m.title)
                                    .unwrap_or_default(),
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
                        this.set_active_note(Some(note_id), window, cx);
                        this.count_tokens(note_id, window, cx);
                    }
                    Err(error) => {
                        log::error!("error while loading note: {:?}", error);
                    }
                })
                .ok();
            });
        }
    }

    fn set_active_note(
        &mut self,
        note_id: Option<NoteId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_note_id = note_id;
        self.picker.update(cx, |picker, cx| {
            if let Some(note_id) = note_id {
                if picker
                    .delegate
                    .filtered_entries
                    .get(picker.delegate.selected_index())
                    .is_none_or(|old_selected| {
                        if let PostProdPickerEntry::Note(note) = old_selected {
                            note.id != note_id
                        } else {
                            true
                        }
                    })
                    && let Some(ix) =
                        picker.delegate.filtered_entries.iter().position(|mat| {
                            if let PostProdPickerEntry::Note(note) = mat {
                                note.id == note_id
                            } else {
                                false
                            }
                        })
                {
                    picker.set_selected_index(ix, None, true, window, cx);
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
                        if this.active_note_id == Some(note_id) {
                            this.set_active_note(None, window, cx);
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

    fn focus_active_note(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active_note) = self.active_note_id {
            self.note_editors[&active_note]
                .body_editor
                .update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx));
            cx.stop_propagation();
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
        let Some(active_note_id) = self.active_note_id else {
            cx.propagate();
            return;
        };

        let note_editor = &self.note_editors[&active_note_id].body_editor;
        let Some(ConfiguredModel { provider, .. }) =
            LanguageModelRegistry::read_global(cx).inline_assistant_model()
        else {
            return;
        };

        let initial_prompt = action.prompt.clone();
        if provider.is_authenticated(cx) {
            self.inline_assist_delegate
                .assist(note_editor, initial_prompt, window, cx);
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
        if let Some(note_id) = self.active_note_id
            && let Some(note_editor) = self.note_editors.get(&note_id)
        {
            window.focus(&note_editor.body_editor.focus_handle(cx), cx);
        }
    }

    fn move_up_from_body(
        &mut self,
        _: &zed_actions::editor::MoveUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(note_id) = self.active_note_id
            && let Some(note_editor) = self.note_editors.get(&note_id)
        {
            window.focus(&note_editor.title_editor.focus_handle(cx), cx);
        }
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
                self.count_tokens(note_id, window, cx);
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
                self.count_tokens(note_id, window, cx);
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

    fn count_tokens(&mut self, note_id: NoteId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ConfiguredModel { model, .. }) =
            LanguageModelRegistry::read_global(cx).default_model()
        else {
            return;
        };
        if let Some(note) = self.note_editors.get_mut(&note_id) {
            let editor = &note.body_editor.read(cx);
            let buffer = &editor.buffer().read(cx).as_singleton().unwrap().read(cx);
            let body = buffer.as_rope().clone();
            note.pending_token_count = cx.spawn_in(window, async move |this, cx| {
                async move {
                    const DEBOUNCE_TIMEOUT: Duration = Duration::from_secs(1);

                    cx.background_executor().timer(DEBOUNCE_TIMEOUT).await;
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
                        let note_editor = this.note_editors.get_mut(&note_id).unwrap();
                        note_editor.token_count = Some(token_count);
                        cx.notify();
                    })
                }
                .log_err()
                .await
            });
        }
    }

    fn render_note_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("note-list")
            .capture_action(cx.listener(Self::focus_active_note))
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

    fn render_active_note_editor(
        &self,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_color = cx.theme().colors().text;

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
                        color: text_color,
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

    fn render_active_note(&mut self, cx: &mut Context<PostProdRules>) -> gpui::Stateful<Div> {
        div()
            .id("note-editor")
            .h_full()
            .flex_grow()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .children(self.active_note_id.and_then(|note_id| {
                let note_metadata = self.store.read(cx).metadata(note_id)?;
                let note_editor = &self.note_editors[&note_id];
                let focus_handle = note_editor.body_editor.focus_handle(cx);
                let registry = LanguageModelRegistry::read_global(cx);
                let model = registry.default_model().map(|default| default.model);

                Some(
                    v_flex()
                        .id("note-editor-inner")
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
                                .child(self.render_active_note_editor(
                                    &note_editor.title_editor,
                                    cx,
                                ))
                                .child(
                                    h_flex()
                                        .h_full()
                                        .flex_shrink_0()
                                        .children(note_editor.token_count.map(|token_count| {
                                            let token_count: SharedString =
                                                token_count.to_string().into();
                                            let label_token_count: SharedString =
                                                token_count.to_string().into();

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
                                                                .map(|model| model.name().0)
                                                                .unwrap_or_default()
                                                        ),
                                                        cx,
                                                    )
                                                })
                                                .child(
                                                    Label::new(format!(
                                                        "{} tokens",
                                                        label_token_count
                                                    ))
                                                    .color(Color::Muted),
                                                )
                                        }))
                                        .child(
                                            self.render_note_controls(note_metadata.default),
                                        ),
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
                                        .child(note_editor.body_editor.clone()),
                                ),
                        ),
                )
            }))
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
                        .child(self.render_active_note(cx)),
                ),
            window,
            cx,
            Tiling::default(),
        )
    }
}
