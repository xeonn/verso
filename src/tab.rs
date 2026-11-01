use servo::{WebView, WebViewId};
use url::Url;

/// Tab state
pub struct Tab {
    /// Tab WebView
    webview: WebView,
    /// Title
    title: String,
    /// Session history entries, as reported by `notify_history_changed`
    history: Vec<Url>,
    /// Index of the current entry in `history`
    history_index: usize,
}

impl Tab {
    /// Create a new tab state.
    pub fn new(webview: WebView) -> Self {
        Self {
            webview,
            title: "null".to_string(),
            history: Vec::new(),
            history_index: 0,
        }
    }

    /// Get tab WebView id.
    pub fn id(&self) -> WebViewId {
        self.webview().id()
    }

    /// Get tab WebView.
    pub fn webview(&self) -> &WebView {
        &self.webview
    }

    /// Set tab title.
    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    /// Get tab title.
    pub fn title(&self) -> String {
        self.title.clone()
    }

    /// Session history entries before the current one, nearest first.
    pub fn back_entries(&self) -> Vec<Url> {
        self.history[..self.history_index.min(self.history.len())]
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// Session history entries after the current one, nearest first.
    pub fn forward_entries(&self) -> Vec<Url> {
        self.history
            .get(self.history_index + 1..)
            .map(<[Url]>::to_vec)
            .unwrap_or_default()
    }
}

/// Tab manager to handle multiple tab in a window.
///
/// Tabs are kept in the order the panel shows them, so both sides pick the same tab when the
/// active one closes.
#[derive(Default)]
pub struct TabManager {
    /// Current active tab id
    active_tab_id: Option<WebViewId>,
    /// Tabs in display order
    tabs: Vec<Tab>,
}

impl TabManager {
    /// Create a new tab manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get tab count.
    pub fn count(&self) -> usize {
        self.tabs.len()
    }

    /// Get current active tab id.
    pub fn current_tab_id(&self) -> Option<WebViewId> {
        self.active_tab_id
    }

    /// Get current active tab.
    pub fn current_tab(&self) -> Option<&Tab> {
        self.active_tab_id.and_then(|id| self.tab(id))
    }

    /// Get all tab ids, in display order.
    pub fn tab_ids(&self) -> Vec<WebViewId> {
        self.tabs.iter().map(Tab::id).collect()
    }

    /// Get all tabs, in display order.
    pub fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    /// Activate the tab by tab id.
    pub fn activate_tab(&mut self, tab_id: WebViewId) -> Option<&Tab> {
        let index = self.index_of(tab_id)?;
        self.active_tab_id = Some(tab_id);
        self.tabs.get(index)
    }

    /// Get tab by tab id.
    pub fn tab(&self, id: WebViewId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id() == id)
    }

    /// Get mutable tab by tab id.
    pub fn tab_mut(&mut self, id: WebViewId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id() == id)
    }

    /// Append a tab at the end.
    pub fn append_tab(&mut self, webview: WebView, active: bool) {
        let id = webview.id();
        self.tabs.push(Tab::new(webview));
        if active {
            self.active_tab_id = Some(id);
        }
    }

    /// Close a tab. When it was the active tab, its neighbour becomes active.
    pub fn close_tab(&mut self, id: WebViewId) -> Option<Tab> {
        let closed = self.index_of(id)?;
        let active = self.active_tab_id.and_then(|active| self.index_of(active));
        let tab = self.tabs.remove(closed);
        self.active_tab_id = active
            .and_then(|active| next_active_index(closed, active, self.tabs.len()))
            .map(|index| self.tabs[index].id());
        Some(tab)
    }

    /// Set the title of a tab.
    pub fn set_title(&mut self, tab_id: WebViewId, title: String) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.set_title(title);
        }
    }

    /// Record the session history of a tab.
    pub fn set_history(&mut self, tab_id: WebViewId, history: Vec<Url>, index: usize) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.history = history;
            tab.history_index = index;
        }
    }

    fn index_of(&self, id: WebViewId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id() == id)
    }
}

/// Index of the active tab after the tab at `closed` is removed from a strip whose active tab was
/// at `active`. `remaining` is the number of tabs left. This mirrors `panel.html`, which picks the
/// tab that slides into the closed tab's place, or the previous one when the last tab closes.
pub fn next_active_index(closed: usize, active: usize, remaining: usize) -> Option<usize> {
    if remaining == 0 {
        return None;
    }
    Some(if closed < active {
        active - 1
    } else if closed == active {
        closed.min(remaining - 1)
    } else {
        active
    })
}
