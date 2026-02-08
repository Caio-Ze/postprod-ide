use gpui::{
    actions, App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, PathPromptOptions, Render, SharedString, Styled, WeakEntity, Window,
};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use ui::{
    prelude::*, ButtonLike, Divider, DividerColor, Icon, IconName, IconSize, Label, LabelSize,
    Headline, HeadlineSize,
};
use workspace::{
    item::{Item, ItemEvent},
    with_active_or_new_workspace, ProToolsSessionName, Workspace,
};

use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

actions!(
    dashboard,
    [
        /// Show the ProTools Studio Dashboard.
        ShowDashboard
    ]
);

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

pub fn init(cx: &mut App) {
    cx.on_action(|_: &ShowDashboard, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            workspace
                .with_local_workspace(window, cx, |workspace, window, cx| {
                    let existing = workspace
                        .active_pane()
                        .read(cx)
                        .items()
                        .find_map(|item| item.downcast::<Dashboard>());

                    if let Some(existing) = existing {
                        workspace.activate_item(&existing, true, true, window, cx);
                    } else {
                        let dashboard = Dashboard::new(workspace, cx);
                        // Insert at index 0 and pin it
                        workspace.add_item_to_active_pane(
                            Box::new(dashboard),
                            Some(0),
                            true,
                            window,
                            cx,
                        );
                        workspace.active_pane().update(cx, |pane, _cx| {
                            pane.set_pinned_count(pane.pinned_count() + 1);
                        });
                    }
                })
                .detach();
        });
    });
}

/// Open the Dashboard as the initial tab in a new workspace window.
pub fn show_dashboard(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let dashboard = Dashboard::new(workspace, cx);
    workspace.add_item_to_center(Box::new(dashboard), window, cx);
    // Pin the dashboard so it can't be closed with Cmd+W
    workspace.active_pane().update(cx, |pane, _cx| {
        pane.set_pinned_count(pane.pinned_count() + 1);
    });
}

// ---------------------------------------------------------------------------
// Tool registry
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolCategory {
    ProTools,
    Audio,
    File,
}

impl ToolCategory {
    fn label(self) -> &'static str {
        match self {
            Self::ProTools => "PRO TOOLS",
            Self::Audio => "AUDIO",
            Self::File => "FILE",
        }
    }
}

struct ToolCard {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    icon: IconName,
    binary: &'static str,
    cwd: &'static str,
    category: ToolCategory,
}

const TOOLS: &[ToolCard] = &[
    // --- Pro Tools Actions (PTSL gRPC) ---
    ToolCard {
        id: "bounceAll",
        label: "Bounce All (TV + NET + SPOT)",
        description: "Multi-format bounce with LUFS normalization",
        icon: IconName::AudioOn,
        binary: "rust-bounce-to-all-configurable-bin",
        cwd: "bin/Bounce",
        category: ToolCategory::ProTools,
    },
    ToolCard {
        id: "sessionMonitor",
        label: "Session Monitor",
        description: "Real-time session monitoring with script triggers",
        icon: IconName::ToolTerminal,
        binary: "session-monitor",
        cwd: "bin/Session_Monitor",
        category: ToolCategory::ProTools,
    },
    ToolCard {
        id: "importSpotClips",
        label: "Import & Spot Clips",
        description: "Import audio and spot to timeline",
        icon: IconName::ListTree,
        binary: "import-and-spot-clip-notimelimit",
        cwd: "bin/Session_Monitor/bin",
        category: ToolCategory::ProTools,
    },
    ToolCard {
        id: "importSfx",
        label: "Import SFX (Gemini)",
        description: "Import SFX folders with Gemini AI integration",
        icon: IconName::Sparkle,
        binary: "sfx_workflow_v3_json",
        cwd: "bin/SFX_Workflow_Runtime",
        category: ToolCategory::ProTools,
    },
    ToolCard {
        id: "saveIncrement",
        label: "Save + Increment",
        description: "Auto-increment and save Pro Tools session",
        icon: IconName::Check,
        binary: "save_session_increment",
        cwd: "tools",
        category: ToolCategory::ProTools,
    },
    ToolCard {
        id: "tvToSpotWorkflow",
        label: "TV → SPOT Workflow",
        description: "Full TV to SPOT session conversion pipeline",
        icon: IconName::ForwardArrow,
        binary: "from-tv-to-spot-workflow",
        cwd: "bin/Pro_Tools_Batch_Processing_Runtime/bin",
        category: ToolCategory::ProTools,
    },
    ToolCard {
        id: "batchProcessing",
        label: "Batch Processing",
        description: "Interactive batch launcher with fuzzy search",
        icon: IconName::PlayFilled,
        binary: "start-scripts-pro-tools-batch-processing",
        cwd: "bin/Pro_Tools_Batch_Processing_Runtime",
        category: ToolCategory::ProTools,
    },
    ToolCard {
        id: "voiceToText",
        label: "Voice to Text Compare",
        description: "Voice transcription vs script comparison",
        icon: IconName::Mic,
        binary: "RUST_VOICE_TO_TEXT_CLI",
        cwd: "bin/Session_Monitor/bin",
        category: ToolCategory::ProTools,
    },
    // --- Audio Tools (standalone) ---
    ToolCard {
        id: "normalizeAudio",
        label: "Normalize Audio (EBU R128)",
        description: "EBU R128 loudness normalization",
        icon: IconName::AudioOn,
        binary: "audio-normalizer-configurable",
        cwd: "bin/Bounce/bin",
        category: ToolCategory::Audio,
    },
    ToolCard {
        id: "maximizeAudio",
        label: "Maximize Audio Peaks",
        description: "Peak normalization to 0dBFS",
        icon: IconName::ArrowUpRight,
        binary: "audio-normalizer-maximize",
        cwd: "tools",
        category: ToolCategory::Audio,
    },
    ToolCard {
        id: "convertMp3Wav",
        label: "Convert MP3 ↔ WAV",
        description: "Batch MP3/WAV conversion at 320kbps",
        icon: IconName::Replace,
        binary: "wav_mp3_fix_rust",
        cwd: "bin/Pro_Tools_Batch_Processing_Runtime/bin",
        category: ToolCategory::Audio,
    },
    ToolCard {
        id: "tvConverter",
        label: "TV Converter (_TV/_NET)",
        description: "LUFS normalization with _TV/_NET naming",
        icon: IconName::AudioOn,
        binary: "tv_converter",
        cwd: "bin/Bounce/bin",
        category: ToolCategory::Audio,
    },
    ToolCard {
        id: "reduceVideo",
        label: "Reduce Video (strip audio)",
        description: "Strip audio and compress video to 480p",
        icon: IconName::Minimize,
        binary: "video_reducer_remove_audio",
        cwd: "bin/SFX_Workflow_Runtime/bin",
        category: ToolCategory::Audio,
    },
    // --- File Tools (standalone) ---
    ToolCard {
        id: "carrefourRenamer",
        label: "Carrefour Renamer",
        description: "Rename session folders with dates",
        icon: IconName::Folder,
        binary: "carrefour-folder-renamer",
        cwd: "tools",
        category: ToolCategory::File,
    },
    ToolCard {
        id: "tvToSpotRename",
        label: "TV → SPOT Rename",
        description: "Rename WAV files (strip prefix/version)",
        icon: IconName::FileRust,
        binary: "tv_to_spot_rename",
        cwd: "bin/Pro_Tools_Batch_Processing_Runtime/bin",
        category: ToolCategory::File,
    },
    ToolCard {
        id: "createFolderStructure",
        label: "Create Folder Structure (CRF)",
        description: "Create folder structures from clipboard",
        icon: IconName::FolderOpen,
        binary: "pastas_crf_rust",
        cwd: "bin/Pro_Tools_Batch_Processing_Runtime/bin",
        category: ToolCategory::File,
    },
];

// ---------------------------------------------------------------------------
// Delivery status
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct DeliveryStatus {
    tv_count: usize,
    net_count: usize,
    spot_count: usize,
    mp3_count: usize,
    warnings: Vec<String>,
}

fn suite_root() -> PathBuf {
    util::paths::home_dir().join("ProTools_Suite")
}

fn scan_delivery_folder() -> DeliveryStatus {
    let dir = suite_root().join("4_Finalizados");
    let mut status = DeliveryStatus::default();

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return status;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();

        if path.is_dir() {
            // Scan subdirectories named TV/, NET/, SPOT/
            let subdir_name = name.as_str();
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                let count = sub_entries
                    .flatten()
                    .filter(|e| e.path().is_file())
                    .count();
                match subdir_name {
                    "tv" => status.tv_count += count,
                    "net" => status.net_count += count,
                    "spot" => status.spot_count += count,
                    _ => {}
                }
            }
        } else if path.is_file() {
            // Classify by filename pattern
            if name.contains("_tv") {
                status.tv_count += 1;
            }
            if name.contains("_net") {
                status.net_count += 1;
            }
            if name.contains("_spot") {
                status.spot_count += 1;
            }
            if name.ends_with(".mp3") {
                status.mp3_count += 1;
            }
        }
    }

    // Generate warnings
    let has_any = status.tv_count > 0
        || status.net_count > 0
        || status.spot_count > 0
        || status.mp3_count > 0;

    if has_any {
        if status.tv_count == 0 {
            status.warnings.push("Falta: arquivos TV".to_string());
        }
        if status.net_count == 0 {
            status.warnings.push("Falta: arquivos NET".to_string());
        }
        if status.spot_count == 0 {
            status.warnings.push("Falta: arquivos SPOT".to_string());
        }
        if status.tv_count > 0 && status.net_count > 0 && status.tv_count != status.net_count {
            status.warnings.push(format!(
                "TV ({}) != NET ({})",
                status.tv_count, status.net_count
            ));
        }
    }

    status
}

// ---------------------------------------------------------------------------
// Pasta ativa helpers
// ---------------------------------------------------------------------------

fn pasta_ativa_file() -> PathBuf {
    suite_root().join(".pasta_ativa")
}

fn read_pasta_ativa() -> Option<PathBuf> {
    std::fs::read_to_string(pasta_ativa_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn write_pasta_ativa(path: &Path) {
    let _ = std::fs::write(pasta_ativa_file(), path.to_string_lossy().as_bytes());
}

// ---------------------------------------------------------------------------
// Dashboard struct
// ---------------------------------------------------------------------------

pub struct Dashboard {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    runtime_path: PathBuf,
    // Session polling
    session_path: Option<String>,
    session_name: Option<String>,
    _session_poll_task: gpui::Task<()>,
    // Pasta ativa
    pasta_ativa: Option<PathBuf>,
    // Delivery status
    delivery_status: DeliveryStatus,
    _delivery_scan_task: gpui::Task<()>,
}

impl Dashboard {
    pub fn new(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        let runtime_path = std::env::var("PROTOOLS_RUNTIME_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(
                    "/Users/caio_ze/Documents/Rust_projects/PROTOOLS_SDK_PTSL/target/runtime",
                )
            });

        let pasta_ativa = read_pasta_ativa();

        cx.new(|cx| {
            // Spawn session polling task (every 5 seconds)
            let poll_binary = runtime_path
                .join("bin/Session_Monitor/bin/get_session_path");
            let session_poll_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    let binary = poll_binary.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            std::process::Command::new(&binary)
                                .output()
                                .ok()
                                .filter(|o| o.status.success())
                                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                                .filter(|s| !s.is_empty())
                        })
                        .await;

                    let _ = this.update(cx, |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                        let name = result.as_ref().map(|p| {
                            Path::new(p)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string()
                        });
                        dashboard.session_path = result;
                        dashboard.session_name = name;

                        // Update the global for window title
                        let global_name = dashboard
                            .session_name
                            .clone()
                            .unwrap_or_default();
                        cx.set_global(ProToolsSessionName(global_name));

                        cx.notify();
                    });

                    cx.background_executor()
                        .timer(Duration::from_secs(5))
                        .await;
                }
            });

            // Spawn delivery scan task (every 15 seconds)
            let delivery_scan_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
                loop {
                    let status = cx
                        .background_executor()
                        .spawn(async { scan_delivery_folder() })
                        .await;

                    let _ = this.update(cx, |dashboard: &mut Dashboard, cx: &mut Context<Dashboard>| {
                        dashboard.delivery_status = status;
                        cx.notify();
                    });

                    cx.background_executor()
                        .timer(Duration::from_secs(15))
                        .await;
                }
            });

            Self {
                workspace: workspace.weak_handle(),
                focus_handle: cx.focus_handle(),
                runtime_path,
                session_path: None,
                session_name: None,
                _session_poll_task: session_poll_task,
                pasta_ativa,
                delivery_status: DeliveryStatus::default(),
                _delivery_scan_task: delivery_scan_task,
            }
        })
    }

    fn spawn_tool(
        tool: &ToolCard,
        runtime_path: &PathBuf,
        pasta_ativa: &Option<PathBuf>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        // Standalone tools use pasta_ativa as cwd when set
        let is_standalone = matches!(tool.category, ToolCategory::Audio | ToolCategory::File);
        let cwd = if is_standalone {
            if let Some(pa) = &pasta_ativa {
                pa.clone()
            } else {
                runtime_path.join(tool.cwd)
            }
        } else {
            runtime_path.join(tool.cwd)
        };
        let command = runtime_path
            .join(tool.cwd)
            .join(tool.binary)
            .to_string_lossy()
            .to_string();

        let spawn = SpawnInTerminal {
            id: TaskId(format!("dashboard-{}", tool.id)),
            label: tool.label.to_string(),
            full_label: tool.label.to_string(),
            command: Some(command),
            args: vec![],
            command_label: tool.label.to_string(),
            cwd: Some(cwd),
            use_new_terminal: true,
            allow_concurrent_runs: false,
            reveal: RevealStrategy::Always,
            show_command: true,
            show_rerun: true,
            ..Default::default()
        };

        workspace.spawn_in_terminal(spawn, window, cx).detach();
    }

    fn pick_pasta_ativa(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    write_pasta_ativa(&path);
                    let _ = this.update(cx, |this, cx| {
                        this.pasta_ativa = Some(path);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    // -- Rendering helpers --

    fn render_session_status(&self, cx: &App) -> impl IntoElement {
        let (dot_color, label_text) = match &self.session_name {
            Some(name) => (Color::Created, format!("Pro Tools: {}", name)),
            None => (Color::Muted, "Pro Tools: Desconectado".to_string()),
        };

        let dot_hsla = dot_color.color(cx);

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(dot_hsla),
            )
            .child(
                Label::new(label_text)
                    .size(LabelSize::Small)
                    .color(dot_color),
            )
    }

    fn render_pasta_ativa(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let display = match &self.pasta_ativa {
            Some(p) => p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            None => "(nenhuma)".to_string(),
        };

        let label_color = if self.pasta_ativa.is_some() {
            Color::Default
        } else {
            Color::Muted
        };

        ButtonLike::new("pasta-ativa-btn")
            .full_width()
            .size(ButtonSize::Medium)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Folder)
                            .color(Color::Accent)
                            .size(IconSize::Small),
                    )
                    .child(
                        v_flex()
                            .child(
                                Label::new("Pasta Ativa")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(display).color(label_color)),
                    ),
            )
            .on_click(cx.listener(|this, _, _window, cx| {
                this.pick_pasta_ativa(cx);
            }))
    }

    fn render_delivery_status(&self, _cx: &App) -> impl IntoElement {
        let status = &self.delivery_status;
        let has_any = status.tv_count > 0
            || status.net_count > 0
            || status.spot_count > 0
            || status.mp3_count > 0;

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .px_1()
                    .mb_2()
                    .gap_2()
                    .child(
                        Label::new("ENTREGA")
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    )
                    .child(Divider::horizontal().color(DividerColor::BorderVariant)),
            )
            .when(!has_any, |el| {
                el.child(
                    h_flex()
                        .px_2()
                        .child(
                            Label::new("Nenhum arquivo em 4_Finalizados/")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
            })
            .when(has_any, |el| {
                el.child(
                    h_flex()
                        .px_2()
                        .gap_4()
                        .child(Self::delivery_badge("TV", status.tv_count, status.tv_count > 0))
                        .child(Self::delivery_badge("NET", status.net_count, status.net_count > 0))
                        .child(Self::delivery_badge("SPOT", status.spot_count, status.spot_count > 0))
                        .child(Self::delivery_badge("MP3", status.mp3_count, status.mp3_count > 0)),
                )
                .children(status.warnings.iter().map(|w| {
                    h_flex()
                        .px_2()
                        .child(
                            Label::new(format!("  {}", w))
                                .color(Color::Warning)
                                .size(LabelSize::XSmall),
                        )
                }))
            })
    }

    fn delivery_badge(label: &str, count: usize, ok: bool) -> impl IntoElement {
        let indicator = if ok { " OK" } else { " --" };
        let color = if ok { Color::Created } else { Color::Muted };

        h_flex()
            .gap_1()
            .child(Label::new(format!("{}: {}", label, count)).size(LabelSize::Small))
            .child(Label::new(indicator).size(LabelSize::XSmall).color(color))
    }

    fn render_section(
        &self,
        category: ToolCategory,
        tab_offset: usize,
        _cx: &App,
    ) -> impl IntoElement {
        let tools_in_category: Vec<&ToolCard> =
            TOOLS.iter().filter(|t| t.category == category).collect();

        let runtime_path = self.runtime_path.clone();
        let workspace = self.workspace.clone();
        let pasta_ativa = self.pasta_ativa.clone();

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .px_1()
                    .mb_2()
                    .gap_2()
                    .child(
                        Label::new(category.label())
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    )
                    .child(Divider::horizontal().color(DividerColor::BorderVariant)),
            )
            .children(tools_in_category.into_iter().enumerate().map(
                move |(idx, tool)| {
                    let runtime_path = runtime_path.clone();
                    let workspace = workspace.clone();
                    let pasta_ativa = pasta_ativa.clone();
                    let tool_id = tool.id;
                    let tool_label = tool.label;
                    let tool_description = tool.description;
                    let tool_icon = tool.icon;
                    let tool_binary = tool.binary;
                    let tool_cwd = tool.cwd;
                    let tool_category = tool.category;

                    ButtonLike::new(format!("dashboard-btn-{}", tool_id))
                        .tab_index((tab_offset + idx) as isize)
                        .full_width()
                        .size(ButtonSize::Medium)
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    Icon::new(tool_icon)
                                        .color(Color::Muted)
                                        .size(IconSize::Small),
                                )
                                .child(
                                    v_flex()
                                        .child(Label::new(tool_label))
                                        .child(
                                            Label::new(tool_description)
                                                .color(Color::Muted)
                                                .size(LabelSize::XSmall),
                                        ),
                                ),
                        )
                        .on_click(move |_, window, cx| {
                            let runtime_path = runtime_path.clone();
                            let pasta_ativa = pasta_ativa.clone();
                            let _ = workspace.update(cx, |workspace, cx| {
                                let card = ToolCard {
                                    id: tool_id,
                                    label: tool_label,
                                    description: tool_description,
                                    icon: tool_icon,
                                    binary: tool_binary,
                                    cwd: tool_cwd,
                                    category: tool_category,
                                };
                                Self::spawn_tool(
                                    &card,
                                    &runtime_path,
                                    &pasta_ativa,
                                    workspace,
                                    window,
                                    cx,
                                );
                            });
                        })
                },
            ))
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

impl Render for Dashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pt_count = TOOLS
            .iter()
            .filter(|t| t.category == ToolCategory::ProTools)
            .count();
        let audio_count = TOOLS
            .iter()
            .filter(|t| t.category == ToolCategory::Audio)
            .count();

        h_flex()
            .key_context("Dashboard")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .justify_center()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .relative()
                    .size_full()
                    .px_12()
                    .max_w(px(1100.))
                    .child(
                        v_flex()
                            .id("dashboard-scroll")
                            .flex_1()
                            .justify_center()
                            .max_w_128()
                            .mx_auto()
                            .gap_6()
                            .overflow_y_scroll()
                            // Header
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_center()
                                    .mb_4()
                                    .gap_4()
                                    .child(
                                        Icon::new(IconName::AudioOn)
                                            .size(IconSize::XLarge)
                                            .color(Color::Accent),
                                    )
                                    .child(
                                        v_flex()
                                            .child(
                                                Headline::new("ProTools Studio Dashboard")
                                                    .size(HeadlineSize::Small),
                                            )
                                            .child(
                                                Label::new(
                                                    "Audio post-production tools",
                                                )
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .italic(),
                                            ),
                                    ),
                            )
                            // Session status bar
                            .child(self.render_session_status(cx))
                            // Pasta ativa
                            .child(self.render_pasta_ativa(cx))
                            // Tool sections
                            .child(self.render_section(ToolCategory::ProTools, 0, cx))
                            .child(self.render_section(ToolCategory::Audio, pt_count, cx))
                            .child(self.render_section(
                                ToolCategory::File,
                                pt_count + audio_count,
                                cx,
                            ))
                            // Delivery status
                            .child(self.render_delivery_status(cx)),
                    ),
            )
    }
}

// ---------------------------------------------------------------------------
// Trait impls for center-pane Item
// ---------------------------------------------------------------------------

impl EventEmitter<ItemEvent> for Dashboard {}

impl Focusable for Dashboard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for Dashboard {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Dashboard".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(ItemEvent)) {
        f(*event)
    }
}
