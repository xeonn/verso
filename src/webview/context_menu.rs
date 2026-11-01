//! Native menus for Servo's context menu and `<select>` controls.
//!
//! **Platform Specific**
//! - macOS / Windows: a native menu built with the muda crate. Showing it blocks until the user
//!   picks an item or dismisses the menu.
//! - Linux: winit has no popup surface yet, so the control is dismissed and Servo keeps its
//!   current state.

use servo::{ContextMenu, SelectElement, WebViewId};

use crate::window::Window;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use muda::{
    ContextMenu as _, Menu, MenuItem, PredefinedMenuItem,
    dpi::{LogicalPosition, Position},
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use servo::{ContextMenuItem, SelectElementOptionOrOptgroup};

impl Window {
    /// Show Servo's context menu and send the chosen action back to Servo.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn show_context_menu(&self, context_menu: ContextMenu) {
        let menu = Menu::new();
        let mut actions = Vec::new();
        for (index, item) in context_menu.items().iter().enumerate() {
            match item {
                ContextMenuItem::Item {
                    label,
                    action,
                    enabled,
                } => {
                    let id = format!("verso-context-menu-{index}");
                    let _ = menu.append(&MenuItem::with_id(id.clone(), label, *enabled, None));
                    actions.push((id, *action));
                },
                ContextMenuItem::Separator => {
                    let _ = menu.append(&PredefinedMenuItem::separator());
                },
            }
        }

        // Open at the cursor, like a native context menu.
        self.show_native_menu(&menu, None);
        match self.take_menu_selection(|id| actions.iter().find(|(item, _)| item == id)) {
            Some((_, action)) => context_menu.select(*action),
            None => context_menu.dismiss(),
        }
    }

    /// Show the options of a `<select>` element and submit the chosen one to Servo.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn show_select_element(&self, webview_id: WebViewId, mut select: SelectElement) {
        let menu = Menu::new();
        let selected = select.selected_options();
        let mut options = Vec::new();
        let mut append_option = |menu: &Menu, id: usize, label: &str, disabled: bool| {
            let menu_id = format!("verso-select-{id}");
            let label = if selected.contains(&id) {
                format!("✓ {label}")
            } else {
                label.to_owned()
            };
            let _ = menu.append(&MenuItem::with_id(menu_id.clone(), label, !disabled, None));
            options.push((menu_id, id));
        };
        for entry in select.options() {
            match entry {
                SelectElementOptionOrOptgroup::Option(option) => {
                    append_option(&menu, option.id, &option.label, option.is_disabled);
                },
                SelectElementOptionOrOptgroup::Optgroup { label, options } => {
                    let _ = menu.append(&MenuItem::new(label, false, None));
                    for option in options {
                        append_option(&menu, option.id, &option.label, option.is_disabled);
                    }
                },
            }
        }

        // Open below the `<select>` element.
        let rect = select.position();
        let origin = self.webview_origin(webview_id);
        let scale = self.scale_factor();
        let position = Position::Logical(LogicalPosition::new(
            (rect.min.x + origin.x) as f64 / scale,
            (rect.max.y + origin.y) as f64 / scale,
        ));
        self.show_native_menu(&menu, Some(position));
        // Dropping the control without submitting keeps the current selection.
        let Some((_, id)) = self.take_menu_selection(|menu_id| {
            options.iter().find(|(item, _)| item == menu_id)
        }) else {
            return;
        };
        let id = *id;
        let new_selection = if select.allow_select_multiple() {
            let mut current = selected;
            if let Some(index) = current.iter().position(|option| *option == id) {
                current.remove(index);
            } else {
                current.push(id);
            }
            current
        } else {
            vec![id]
        };
        select.select(new_selection);
        select.submit();
    }

    /// Pop up `menu` at `position` in the window, or at the cursor when it is `None`.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn show_native_menu(&self, menu: &Menu, position: Option<Position>) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let Ok(handle) = self.window.window_handle() else {
            return;
        };
        unsafe {
            match handle.as_raw() {
                #[cfg(target_os = "macos")]
                RawWindowHandle::AppKit(handle) => {
                    menu.show_context_menu_for_nsview(handle.ns_view.as_ptr(), position);
                },
                #[cfg(target_os = "windows")]
                RawWindowHandle::Win32(handle) => {
                    menu.show_context_menu_for_hwnd(handle.hwnd.into(), position);
                },
                handle => log::error!("Unsupported window handle for a native menu: {handle:?}"),
            }
        }
    }

    /// Find the menu item the user clicked, if any, among the events queued while the menu was open.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn take_menu_selection<'a, T>(
        &self,
        find: impl Fn(&str) -> Option<&'a T>,
    ) -> Option<&'a T> {
        let mut found = None;
        while let Ok(event) = self.menu_event_receiver.try_recv() {
            if found.is_none() {
                found = find(event.id().0.as_str());
            }
        }
        found
    }

    /// Context menus need a popup surface, which winit does not offer on Linux yet.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub(crate) fn show_context_menu(&self, context_menu: ContextMenu) {
        log::debug!("Context menus are not supported on this platform yet");
        context_menu.dismiss();
    }

    /// `<select>` popups need a popup surface, which winit does not offer on Linux yet.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub(crate) fn show_select_element(&self, _webview_id: WebViewId, _select: SelectElement) {
        log::debug!("Select popups are not supported on this platform yet");
    }
}
