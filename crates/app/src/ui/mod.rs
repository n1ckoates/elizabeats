//! The Elizabeats window: header, Queue (left), Search / Log (right).

mod app_view;
mod drag;
mod header;
mod log_panel;
mod menus;
mod queue_panel;
mod row;
mod search_panel;
mod selection;
mod settings;

use std::sync::Arc;

use elizabeats_core::Track;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, StyledExt, h_flex, v_flex};
use gpui_kit::{
    App, Div, FontFeatures, Image, KeyBinding, ObjectFit, ParentElement, SharedString, Styled,
    StyledImage, div, img, px,
};

pub use app_view::AppView;
pub use header::traffic_light_position;
pub use row::{RowSize, row};

gpui_kit::actions!(
    elizabeats,
    [
        Quit,
        Minimize,
        Zoom,
        SkipActive,
        InsertBreak,
        FocusSearch,
        RemoveSelected,
        SelectPrevious,
        SelectNext,
        ExtendSelectionUp,
        ExtendSelectionDown,
        SelectAll,
        ClearSelection,
        MoveSelectedUp,
        MoveSelectedDown
    ]
);

/// Height of the Queue and Search/Log panel headers: the height of gpui-kit's
/// default (medium) tabs, so the tabs fill their header exactly.
pub const PANEL_HEADER_HEIGHT: f32 = 32.;

/// Key context for the main window and for focused Queue rows.
pub const APP_CONTEXT: &str = "Elizabeats";
pub const QUEUE_ROW_CONTEXT: &str = "QueueRow";

/// The macOS menu bar. gpui doesn't install the standard menus itself, so
/// without these the system shortcuts (⌘M, ⌘Q, ⌘C/⌘V…) have nothing to trigger.
pub fn set_menus(cx: &mut App) {
    use gpui_kit::{Menu, MenuItem, OsAction};
    cx.set_menus([
        Menu::new("Elizabeats").items([MenuItem::action("Quit Elizabeats", Quit)]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", gpui_kit::component::input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", gpui_kit::component::input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", gpui_kit::component::input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", gpui_kit::component::input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", gpui_kit::component::input::Paste, OsAction::Paste),
            MenuItem::os_action(
                "Select All",
                gpui_kit::component::input::SelectAll,
                OsAction::SelectAll,
            ),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
    ]);
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("secondary-right", SkipActive, Some(APP_CONTEXT)),
        KeyBinding::new("secondary-b", InsertBreak, Some(APP_CONTEXT)),
        KeyBinding::new("secondary-k", FocusSearch, Some(APP_CONTEXT)),
        KeyBinding::new("backspace", RemoveSelected, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("delete", RemoveSelected, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("up", SelectPrevious, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("shift-up", ExtendSelectionUp, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("shift-down", ExtendSelectionDown, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("secondary-a", SelectAll, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("escape", ClearSelection, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("secondary-up", MoveSelectedUp, Some(QUEUE_ROW_CONTEXT)),
        KeyBinding::new("secondary-down", MoveSelectedDown, Some(QUEUE_ROW_CONTEXT)),
    ]);
}

/// Tabular figures so clocks and durations don't jitter.
pub fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// The Electron app's uppercase section label.
pub fn section_label(text: &str, cx: &App) -> Div {
    div()
        .text_xs()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(text.to_uppercase()))
}

/// A Track's square artwork (or a placeholder) at `size` pixels.
pub fn track_artwork(image: Option<Arc<Image>>, size: f32, cx: &App) -> Div {
    let frame = div()
        .relative()
        .flex_none()
        .size(px(size))
        .rounded(px(8.))
        .overflow_hidden()
        .bg(cx.theme().primary);
    match image {
        Some(image) => frame.child(
            img(image)
                .size_full()
                .rounded(px(8.))
                .object_fit(ObjectFit::Cover),
        ),
        None => frame.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(gpui_kit::assets::IconName::Music)
                        .small()
                        .text_color(cx.theme().primary_foreground.opacity(0.7)),
                ),
        ),
    }
}

/// The square tile used for Breaks.
pub fn break_tile(size: f32, cx: &App) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(8.))
        .bg(cx.theme().muted)
        .flex()
        .items_center()
        .justify_center()
        .child(
            Icon::new(gpui_kit::assets::IconName::MicVocal)
                .small()
                .text_color(cx.theme().muted_foreground),
        )
}

/// Title (with optional trailing badges) over the artist.
pub fn track_details(track: &Track, badges: Vec<gpui_kit::AnyElement>, cx: &App) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .child(
            h_flex()
                .min_w_0()
                .gap(px(6.))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_medium()
                        .child(SharedString::from(track.title.clone())),
                )
                .children(badges),
        )
        .child(
            div()
                .truncate()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(track.artist.clone())),
        )
}

pub fn file_manager_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Finder"
    } else if cfg!(target_os = "windows") {
        "File Explorer"
    } else {
        "file manager"
    }
}

pub fn alert_icon(cx: &App) -> Icon {
    Icon::new(IconName::CircleAlert)
        .small()
        .text_color(cx.theme().danger)
}

#[cfg(test)]
mod tests;
