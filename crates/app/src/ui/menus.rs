//! Row actions shown both in "⋮" dropdowns and right-click context menus.

use std::rc::Rc;

use gpui_kit::component::Icon;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{App, SharedString, Window};

pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone)]
pub struct MenuAction {
    pub label: SharedString,
    pub icon: Icon,
    pub disabled: bool,
    pub handler: Handler,
}

impl MenuAction {
    pub fn new(
        label: impl Into<SharedString>,
        icon: impl Into<Icon>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            icon: icon.into(),
            disabled: false,
            handler: Rc::new(handler),
        }
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Adds `actions` to `menu`, with a separator before index `separator_at`.
pub fn build_menu(
    mut menu: PopupMenu,
    actions: &[MenuAction],
    separator_at: Option<usize>,
) -> PopupMenu {
    for (index, action) in actions.iter().enumerate() {
        if Some(index) == separator_at && index > 0 {
            menu = menu.separator();
        }
        let handler = action.handler.clone();
        menu = menu.item(
            PopupMenuItem::new(action.label.clone())
                .icon(action.icon.clone())
                .disabled(action.disabled)
                .on_click(move |_, window, cx| handler(window, cx)),
        );
    }
    menu
}
