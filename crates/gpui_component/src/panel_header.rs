use gpui::{
    AnyElement, App, ClickEvent, ElementId, FontWeight, IntoElement, ParentElement, RenderOnce,
    SharedString, Styled, Window, div, px,
};

use crate::{
    ActiveTheme, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    separator::Separator,
    v_flex,
};

/// The header button a panel that docks at the bottom uses to fill the whole
/// window and to go back, as the stock bottom panels (terminal, debugger) do.
/// It only draws the button and passes the click on: the panel keeps the
/// `zoomed` flag, implements `Panel::is_zoomed`/`set_zoomed`, and emits
/// `PanelEvent::ZoomIn` or `ZoomOut` from `on_click`, which is what makes the
/// workspace zoom its dock.
pub fn zoom_button(
    id: impl Into<ElementId>,
    zoomed: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(if zoomed {
            IconName::Minimize
        } else {
            IconName::Maximize
        })
        .tooltip(if zoomed { "Restore" } else { "Full screen" })
        .on_click(on_click)
}

/// The standard tool-panel header: `[icon] [title]  [actions...]` over a
/// full-width divider. Every ported `forge_shell` panel (Cockpit, Processes,
/// Python, Node, ...) starts with this same row — this widget replaces each
/// panel's own hand-rolled copy of it with one shared implementation built
/// from real `gpui_component` parts (`Separator`, not a raw colored `div`).
///
/// ```ignore
/// PanelHeader::new("Python")
///     .icon(Icon::new(IconName::SquareTerminal))
///     .action(Button::new("refresh").icon(IconName::RotateCw))
/// ```
#[derive(IntoElement)]
pub struct PanelHeader {
    title: SharedString,
    icon: Option<AnyElement>,
    actions: Vec<AnyElement>,
}

impl PanelHeader {
    /// Creates a header with the given title and no icon or actions.
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            icon: None,
            actions: Vec::new(),
        }
    }

    /// Sets the leading icon. Accepts anything renderable — `Icon::new(..)`
    /// for panels on `IconName`, or a raw `svg().path(..)` for panels with
    /// their own icon assets.
    pub fn icon(mut self, icon: impl IntoElement) -> Self {
        self.icon = Some(icon.into_any_element());
        self
    }

    /// Appends one trailing action element (right-aligned, after the
    /// title). Call multiple times to add several.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }

    /// Appends several trailing action elements at once.
    pub fn actions(mut self, actions: impl IntoIterator<Item = AnyElement>) -> Self {
        self.actions.extend(actions);
        self
    }
}

impl RenderOnce for PanelHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        v_flex()
            .flex_shrink_0()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .children(self.icon)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_lg()
                            .text_color(cx.theme().foreground)
                            .child(self.title),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .justify_end()
                            .gap_2()
                            .children(self.actions),
                    ),
            )
            .child(Separator::horizontal().h(px(1.)))
    }
}
