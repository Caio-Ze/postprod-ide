//! Shared card component for the dashboard panel.
//!
//! `DashboardCard` wraps Zed's `ListItem` to provide a consistent card shell
//! used by automation, pipeline, and tool cards. It handles:
//!
//! - Start slot: themed icon container
//! - Label + description
//! - End slot: action buttons
//! - Elevation shadow via `ElevationIndex::ElevatedSurface`
//! - `DynamicSpacing` for all padding/gaps
//! - Semantic colors only (no manual opacity calculations)
//! - Expandable content area below the header row

use gpui::{
    AnyElement, App, ClickEvent, Div, Hsla, IntoElement, MouseButton, ParentElement,
    SharedString, Styled, Window,
};
use ui::{
    Color, DynamicSpacing, Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing,
    prelude::*,
};

// ---------------------------------------------------------------------------
// Icon container
// ---------------------------------------------------------------------------

/// A themed icon badge: icon centered in a rounded, colored container.
///
/// Two sizes are available — `Normal` (36px, 8px rounding, `IconSize::Medium`)
/// for featured / automation / pipeline cards, and `Compact` (28px, 6px
/// rounding, `IconSize::Small`) for standard tool cards.
pub struct CardIcon {
    icon: IconName,
    icon_color: Color,
    bg: Option<Hsla>,
    compact: bool,
}

impl CardIcon {
    pub fn new(icon: IconName) -> Self {
        Self {
            icon,
            icon_color: Color::Accent,
            bg: None,
            compact: false,
        }
    }

    pub fn color(mut self, color: Color) -> Self {
        self.icon_color = color;
        self
    }

    pub fn bg(mut self, bg: Hsla) -> Self {
        self.bg = Some(bg);
        self
    }

    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    fn into_element(self, cx: &App) -> Div {
        let (size, rounding, icon_size) = if self.compact {
            (px(28.), px(6.), IconSize::Small)
        } else {
            (px(36.), px(8.), IconSize::Medium)
        };

        let bg = self
            .bg
            .unwrap_or_else(|| cx.theme().colors().element_background);

        div()
            .flex_shrink_0()
            .size(size)
            .rounded(rounding)
            .bg(bg)
            .flex()
            .items_center()
            .justify_center()
            .child(Icon::new(self.icon).size(icon_size).color(self.icon_color))
    }
}

// ---------------------------------------------------------------------------
// Accent strip
// ---------------------------------------------------------------------------

/// A thin vertical accent strip on the left edge of the card.
fn accent_strip(color: Hsla) -> Div {
    div().w(px(3.)).h_full().flex_shrink_0().bg(color)
}

// ---------------------------------------------------------------------------
// DashboardCard
// ---------------------------------------------------------------------------

/// A reusable card component for the dashboard panel.
///
/// Visual structure:
///
/// ```text
/// ┌─[accent]──────────────────────────────────────┐
/// │  [icon]  Title              [end-slot buttons] │
/// │          Description                           │
/// │  [expanded content: context, prompt, steps]    │
/// └───────────────────────────────────────────────-┘
/// ```
///
/// Uses `ElevationIndex::ElevatedSurface` (via `elevation_2`) for the outer
/// container and `DynamicSpacing` for all internal gaps and padding.
pub struct DashboardCard {
    id: SharedString,
    icon: CardIcon,
    label: SharedString,
    description: Option<SharedString>,
    accent: Option<Hsla>,
    end_slot: Option<AnyElement>,
    expanded_content: Option<AnyElement>,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    group_name: Option<SharedString>,
    spacing: ListItemSpacing,
}

impl DashboardCard {
    pub fn new(id: impl Into<SharedString>, icon: CardIcon, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon,
            label: label.into(),
            description: None,
            accent: None,
            end_slot: None,
            expanded_content: None,
            on_click: None,
            group_name: None,
            spacing: ListItemSpacing::Dense,
        }
    }

    /// Optional description shown below the label in muted small text.
    pub fn description(mut self, desc: impl Into<SharedString>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Left-edge accent strip color. If omitted, no accent strip is shown.
    pub fn accent(mut self, color: Hsla) -> Self {
        self.accent = Some(color);
        self
    }

    /// Action buttons or badges placed at the trailing edge of the header row.
    ///
    /// The end slot includes a `mouse_down` handler that calls
    /// `prevent_default` + `stop_propagation` so button clicks don't trigger
    /// the card's `on_click`.
    pub fn end_slot(mut self, el: impl IntoElement) -> Self {
        self.end_slot = Some(el.into_any_element());
        self
    }

    /// Content shown below the header row when the card is expanded
    /// (context entries, prompt text, pipeline step tree, etc.).
    pub fn expanded_content(mut self, el: impl IntoElement) -> Self {
        self.expanded_content = Some(el.into_any_element());
        self
    }

    /// Click handler for the card body (typically toggles expansion).
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    /// Group name for hover-reveal end-slot children (via GPUI `.group()`).
    pub fn group_name(mut self, name: impl Into<SharedString>) -> Self {
        self.group_name = Some(name.into());
        self
    }

    /// Override the default `Dense` spacing on the inner `ListItem`.
    pub fn spacing(mut self, spacing: ListItemSpacing) -> Self {
        self.spacing = spacing;
        self
    }

    /// Build the card element tree.
    pub fn render(self, cx: &App) -> impl IntoElement {
        let hover_bg = cx.theme().colors().ghost_element_hover;

        // -- header row via ListItem ------------------------------------------
        let label_el = Label::new(self.label.clone());

        let content = {
            let mut col = v_flex().flex_1().overflow_hidden().child(label_el);
            if let Some(desc) = self.description {
                col = col.child(
                    Label::new(desc)
                        .color(Color::Muted)
                        .size(LabelSize::Small)
                        .truncate(),
                );
            }
            col
        };

        let icon_el = self.icon.into_element(cx);

        // Wrap end-slot in a propagation guard so button clicks don't fire the
        // card's on_click handler.
        let end_slot_el = self.end_slot.map(|slot| {
            h_flex()
                .flex_shrink_0()
                .gap(DynamicSpacing::Base04.rems(cx))
                .items_center()
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .child(slot)
        });

        let header_row = {
            let mut row = h_flex()
                .flex_1()
                .p(DynamicSpacing::Base08.rems(cx))
                .gap(DynamicSpacing::Base12.rems(cx))
                .items_center()
                .child(icon_el)
                .child(content);

            if let Some(end) = end_slot_el {
                row = row.child(end);
            }
            row
        };

        // -- outer container --------------------------------------------------
        let inner = {
            let mut col = v_flex().flex_1().child(header_row);
            if let Some(expanded) = self.expanded_content {
                col = col.child(expanded);
            }
            col
        };

        let body = if let Some(accent_color) = self.accent {
            h_flex()
                .w_full()
                .child(accent_strip(accent_color))
                .child(inner)
        } else {
            h_flex().w_full().child(inner)
        };

        let mut card = div()
            .id(self.id.clone())
            .w_full()
            .elevation_2(cx)
            .overflow_hidden()
            .cursor_pointer()
            .hover(move |style| style.bg(hover_bg))
            .child(body);

        if let Some(accent_color) = self.accent {
            card = card.border_color(accent_color.opacity(0.5));
        }

        if let Some(group) = self.group_name {
            card = card.group(group);
        }

        if let Some(handler) = self.on_click {
            card = card.on_click(handler);
        }

        card
    }
}
