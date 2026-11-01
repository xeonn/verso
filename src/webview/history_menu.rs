//! The back and forward history menus, opened by a long press on the panel's arrows.
//!
//! **Platform Specific**
//! - macOS / Windows: a native menu built with the muda crate.
//! - Linux: winit has no popup surface yet, so the menu is not shown.

use serde::Deserialize;

use crate::window::Window;

/// `OPEN_HISTORY_MENU:{ action, position }` from the panel.
#[derive(Debug, Deserialize)]
pub(crate) struct OpenHistoryMenuRequest {
    /// Which side of the session history to list
    action: HistoryMenuAction,
    /// Where to open the menu, in the panel's CSS pixels
    position: MenuPosition,
}

/// Which side of the session history the menu lists.
#[derive(Debug, Deserialize)]
enum HistoryMenuAction {
    /// Entries before the current one
    Prev,
    /// Entries after the current one
    Next,
}

#[derive(Debug, Deserialize)]
struct MenuPosition {
    x: f64,
    y: f64,
}

impl Window {
    /// Show the back or forward history of the active tab, and traverse to the chosen entry.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn show_history_menu(&self, request: OpenHistoryMenuRequest) {
        use muda::{
            Menu, MenuItem,
            dpi::{LogicalPosition, Position},
        };

        let Some(tab) = self.tab_manager.current_tab() else {
            return;
        };
        let entries = match request.action {
            HistoryMenuAction::Prev => tab.back_entries(),
            HistoryMenuAction::Next => tab.forward_entries(),
        };
        if entries.is_empty() {
            return;
        }

        let menu = Menu::new();
        let mut items = Vec::new();
        for (steps, url) in entries.iter().enumerate() {
            let id = format!("verso-history-{steps}");
            let _ = menu.append(&MenuItem::with_id(id.clone(), url.as_str(), true, None));
            items.push((id, steps + 1));
        }

        // The panel sits at the top left of the window, so its CSS pixels are window logical pixels.
        let position = Position::Logical(LogicalPosition::new(
            request.position.x,
            request.position.y,
        ));
        self.show_native_menu(&menu, Some(position));
        let Some((_, steps)) =
            self.take_menu_selection(|id| items.iter().find(|(item, _)| item == id))
        else {
            return;
        };
        let webview = tab.webview();
        let _ = match request.action {
            HistoryMenuAction::Prev => webview.go_back(*steps),
            HistoryMenuAction::Next => webview.go_forward(*steps),
        };
    }

    /// History menus need a popup surface, which winit does not offer on Linux yet.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub(crate) fn show_history_menu(&self, request: OpenHistoryMenuRequest) {
        log::debug!("History menus are not supported on this platform yet: {request:?}");
    }
}
