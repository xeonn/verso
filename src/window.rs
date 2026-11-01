use std::{
    cell::Cell,
    collections::HashMap,
    rc::{Rc, Weak},
};

use euclid::Scale;
use keyboard_types::{Code, KeyState, KeyboardEvent, Modifiers};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use muda::{MenuEvent, MenuEventReceiver};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use serde::{Deserialize, Serialize};
use servo::{
    AllowOrDenyRequest, AuthenticationRequest, Cursor, DeviceIndependentPixel, DeviceIntRect,
    DevicePixel, DevicePoint, EmbedderControl, EmbedderControlId, FilePicker, InputEvent,
    LoadStatus, MouseButton, MouseButtonAction, MouseButtonEvent, MouseLeftViewportEvent,
    MouseMoveEvent, NavigationRequest, Notification, OffscreenRenderingContext, PermissionRequest,
    PromptDialog, RenderingContext, SimpleDialog, WebResourceLoad, WebView, WebViewBuilder,
    WebViewDelegate, WebViewId, WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};
use versoview_messages::ToControllerMessage;
#[cfg(any(linux, target_os = "windows"))]
use winit::window::ResizeDirection;
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::ModifiersState,
    window::{CursorIcon, Window as WinitWindow, WindowAttributes, WindowId},
};

use crate::{
    Verso,
    keyboard::keyboard_event_from_winit,
    tab::TabManager,
    verso::{DownloadCheckFollowUp, EventLoopProxyMessage},
    webview::{
        Panel,
        history_menu::OpenHistoryMenuRequest,
        prompt::{PROMPT_LOGICAL_SIZE, PendingPrompt, Prompt},
    },
};

const PANEL_HEIGHT: f64 = 50.0;
const TAB_HEIGHT: f64 = 30.0;
const BOOKMARK_HEIGHT: f64 = 30.0;
const PANEL_PADDING: f64 = 4.0;

#[derive(Default)]
pub(crate) struct EventListeners {
    /// This is `true` if the controller wants to get and handle OnNavigationStarting/AllowNavigationRequest
    pub(crate) on_navigation_starting: bool,
    /// Navigation requests waiting for a controller response.
    pub(crate) pending_navigations: HashMap<uuid::Uuid, NavigationRequest>,
    /// Pending web resource loads waiting for a controller response.
    pub(crate) on_web_resource_requested: Option<HashMap<uuid::Uuid, WebResourceLoad>>,
    /// This is `true` if the controller wants to get and handle WindowEvent::CloseRequested
    pub(crate) on_close_requested: bool,
}

#[derive(Debug, Default)]
struct CursorState {
    current_cursor: CursorIcon,
    #[cfg(any(linux, target_os = "windows"))]
    cursor_resizing: bool,
}

/// The tab id format `panel.html` compares: `{ namespace_id, index }`.
///
/// Servo 0.7.0 serializes [`WebViewId`] as `[painter_id, { namespace_id, index }]`, which the
/// panel cannot match, so Verso hands the panel its own ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct PanelTabId {
    namespace_id: u32,
    index: u32,
}

impl PanelTabId {
    fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }
}

/// A region of the window that Servo paints into its own framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    Panel,
    Content,
    Prompt,
}

/// A Verso window is a Winit window containing several web views.
pub struct Window {
    cursor_state: CursorState,
    pub(crate) rendering_context: Rc<WindowRenderingContext>,
    /// Framebuffer of the panel webview.
    panel_context: Option<Rc<OffscreenRenderingContext>>,
    /// Framebuffer shared by every tab. Servo uses one painter per rendering context, and
    /// `WebView::show` / `hide` select the tab it draws.
    content_context: Option<Rc<OffscreenRenderingContext>>,
    /// Regions with a new Servo frame that has not been painted yet.
    panel_dirty: Cell<bool>,
    content_dirty: Cell<bool>,
    prompt_dirty: Cell<bool>,
    /// The main panel of this window.
    pub(crate) panel: Option<Panel>,
    /// The prompt page shown over the content area, if any.
    prompt: Option<Prompt>,
    /// Ids handed to the panel for each tab.
    panel_tab_ids: HashMap<WebViewId, PanelTabId>,
    next_panel_tab_index: u32,
    /// The input method control that currently owns the IME.
    ime_control: Option<EmbedderControlId>,
    /// Event listeners registered from the webview controller
    pub(crate) event_listeners: EventListeners,
    /// The mouse physical position in the web view.
    pub(crate) mouse_position: Cell<Option<PhysicalPosition<f64>>>,
    /// The webview that the mouse is currenly hovering on.
    hovering_webview: Option<WebViewId>,
    /// Modifiers state of the keyboard.
    modifiers_state: Cell<ModifiersState>,
    /// Proxy to ask the event loop to close this window or open another one.
    proxy: EventLoopProxy<EventLoopProxyMessage>,
    /// Global menu event receiver for muda crate
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) menu_event_receiver: MenuEventReceiver,
    /// Window tabs manager
    pub(crate) tab_manager: TabManager,
    pub(crate) focused_webview_id: Option<WebViewId>,
    /// Show the bookmark bar or not
    pub show_bookmark: bool,
    /// Access to Winit window.
    ///
    /// Declared last so it is dropped after the GL surfaces that render into it.
    pub(crate) window: WinitWindow,
}

/// Delegate for the tabs of a window.
///
/// It holds a [`Weak`] reference: [`Verso`] owns the windows, the windows own the webviews, and
/// the webviews own their delegates. A strong reference would keep `Servo` alive forever, and
/// Servo 0.7.0 only shuts down when its last handle is dropped.
struct WindowWebViewDelegate {
    verso: Weak<Verso>,
    window_id: WindowId,
}

impl WindowWebViewDelegate {
    fn new(verso: &Rc<Verso>, window_id: WindowId) -> Self {
        Self {
            verso: Rc::downgrade(verso),
            window_id,
        }
    }

    /// Run `callback` on this delegate's window. Does nothing once Verso or the window is gone.
    fn with_window<R>(&self, callback: impl FnOnce(&mut Window, &Rc<Verso>) -> R) -> Option<R> {
        let verso = self.verso.upgrade()?;
        let Ok(mut windows) = verso.windows.try_borrow_mut() else {
            log::warn!("Verso windows are busy; dropping a webview delegate callback");
            return None;
        };
        let window = windows.get_mut(&self.window_id)?;
        Some(callback(window, &verso))
    }

    fn forward_web_resource(&self, load: WebResourceLoad) {
        let Some(verso) = self.verso.upgrade() else {
            return;
        };
        let Some(sender) = verso.controller_sender() else {
            return;
        };
        let mut windows = verso.windows.borrow_mut();
        let Some(window) = windows.get_mut(&self.window_id) else {
            return;
        };
        let Some(pending) = window.event_listeners.on_web_resource_requested.as_mut() else {
            return;
        };

        let id = uuid::Uuid::new_v4();
        let resource = load.request();
        let mut builder = http::request::Builder::new()
            .uri(resource.url.as_str())
            .method(resource.method.clone());
        for (key, value) in resource.headers.iter() {
            builder = builder.header(key, value);
        }
        let http_request = match builder.body(Vec::new()) {
            Ok(request) => request,
            Err(error) => {
                log::debug!("Skipping a web resource the controller cannot see: {error}");
                return;
            }
        };
        if sender
            .send(ToControllerMessage::OnWebResourceRequested(
                versoview_messages::WebResourceRequest {
                    id,
                    request: http_request,
                },
            ))
            .is_ok()
        {
            pending.insert(id, load);
        }
    }
}

impl WebViewDelegate for WindowWebViewDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        self.with_window(|window, _| window.mark_dirty(webview.id()));
    }

    fn notify_closed(&self, webview: WebView) {
        self.with_window(|window, _| window.close_tab(webview.id(), false));
    }

    fn notify_history_changed(&self, webview: WebView, entries: Vec<url::Url>, current: usize) {
        self.with_window(|window, _| {
            window
                .tab_manager
                .set_history(webview.id(), entries, current);
            window.update_navbar(webview.id());
        });
    }

    fn show_notification(&self, _webview: WebView, notification: Notification) {
        self.with_window(|window, _| window.show_notification(&notification));
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        self.with_window(|window, _| window.set_tab_title(webview.id(), title));
    }

    fn notify_cursor_changed(&self, _webview: WebView, cursor: Cursor) {
        self.with_window(|window, _| window.set_cursor_icon(cursor));
    }

    fn request_navigation(&self, _webview: WebView, request: NavigationRequest) {
        let request =
            self.with_window(|window, verso| window.handle_navigation_request(verso, request));
        // Allow when there is no window, no controller listening, and no download check.
        if let Some(Some(request)) = request {
            request.allow();
        }
    }

    fn request_unload(&self, _webview: WebView, request: AllowOrDenyRequest) {
        request.allow();
    }

    fn request_permission(&self, webview: WebView, request: PermissionRequest) {
        self.with_window(|window, verso| {
            window.show_prompt(
                verso,
                webview.id(),
                None,
                PendingPrompt::Permission(request),
            )
        });
    }

    fn request_authentication(&self, webview: WebView, request: AuthenticationRequest) {
        self.with_window(|window, verso| {
            window.show_prompt(
                verso,
                webview.id(),
                None,
                PendingPrompt::Authentication(request),
            )
        });
    }

    fn show_embedder_control(&self, webview: WebView, control: EmbedderControl) {
        // Verso's own pages (`verso://`) talk to Verso through `window.prompt("VERSO::...")`.
        // Pages from the web must not reach that channel.
        let control = match control {
            EmbedderControl::SimpleDialog(SimpleDialog::Prompt(dialog))
                if dialog.message().starts_with("VERSO::")
                    && webview.url().is_some_and(|url| url.scheme() == "verso") =>
            {
                self.with_window(|window, verso| {
                    window.handle_verso_page_prompt(verso, webview.id(), dialog)
                });
                return;
            }
            other => other,
        };
        self.with_window(|window, verso| {
            window.present_embedder_control(verso, webview.id(), control)
        });
    }

    fn hide_embedder_control(&self, _webview: WebView, control_id: EmbedderControlId) {
        self.with_window(|window, _| window.hide_embedder_control(control_id));
    }

    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        self.forward_web_resource(load);
    }
}

/// Delegate for the panel webview.
struct PanelWebViewDelegate(WindowWebViewDelegate);

impl WebViewDelegate for PanelWebViewDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        self.0
            .with_window(|window, _| window.mark_dirty(webview.id()));
    }

    fn notify_cursor_changed(&self, _webview: WebView, cursor: Cursor) {
        self.0
            .with_window(|window, _| window.set_cursor_icon(cursor));
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        if status == LoadStatus::Complete {
            self.0
                .with_window(|window, verso| window.finish_panel_load(verso));
        }
    }

    /// The panel's close button calls `window.close()`.
    fn notify_closed(&self, _webview: WebView) {
        self.0.with_window(|window, _| window.request_close());
    }

    fn show_embedder_control(&self, webview: WebView, control: EmbedderControl) {
        self.0.with_window(|window, verso| {
            window.present_panel_control(verso, webview.id(), control)
        });
    }

    fn hide_embedder_control(&self, _webview: WebView, control_id: EmbedderControlId) {
        self.0
            .with_window(|window, _| window.hide_embedder_control(control_id));
    }

    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        self.0.forward_web_resource(load);
    }
}

/// Delegate for a prompt page shown over the content area.
struct PromptWebViewDelegate(WindowWebViewDelegate);

impl WebViewDelegate for PromptWebViewDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        self.0
            .with_window(|window, _| window.mark_dirty(webview.id()));
    }

    fn notify_cursor_changed(&self, _webview: WebView, cursor: Cursor) {
        self.0
            .with_window(|window, _| window.set_cursor_icon(cursor));
    }

    /// The prompt pages send their result with `window.alert(result)`.
    fn show_embedder_control(&self, webview: WebView, control: EmbedderControl) {
        match control {
            EmbedderControl::SimpleDialog(SimpleDialog::Alert(alert)) => {
                let result = alert.message().to_owned();
                alert.confirm();
                self.0
                    .with_window(|window, _| window.finish_prompt(webview.id(), Some(&result)));
            }
            EmbedderControl::InputMethod(input) => {
                self.0.with_window(|window, verso| {
                    window.present_embedder_control(
                        verso,
                        webview.id(),
                        EmbedderControl::InputMethod(input),
                    )
                });
            }
            other => log::debug!("Ignoring {:?} from a prompt page", other.id()),
        }
    }

    fn hide_embedder_control(&self, _webview: WebView, control_id: EmbedderControlId) {
        self.0
            .with_window(|window, _| window.hide_embedder_control(control_id));
    }

    /// The prompt pages call `window.close()` after sending their result.
    fn notify_closed(&self, webview: WebView) {
        self.0
            .with_window(|window, _| window.finish_prompt(webview.id(), None));
    }
}

impl Window {
    /// Create a Verso window from Winit window and return the rendering context.
    pub fn new(
        event_loop: &ActiveEventLoop,
        window_attributes: WindowAttributes,
        proxy: EventLoopProxy<EventLoopProxyMessage>,
    ) -> Self {
        let display_handle = event_loop
            .display_handle()
            .expect("Failed to get display handle");
        let window = event_loop
            .create_window(window_attributes)
            .expect("Failed to create winit Window");
        let window_handle = window.window_handle().expect("Failed to get window handle");
        let rendering_context = Rc::new(
            WindowRenderingContext::new(display_handle, window_handle, window.inner_size())
                .expect("Could not create RenderingContext for window."),
        );

        // Setup for GL accelerated media handling. Servo only accepts this on desktop Linux.
        // It must run once, before the first `WebView`, so later windows skip it.
        static SETUP_GL_ACCELERATED_MEDIA: std::sync::Once = std::sync::Once::new();
        SETUP_GL_ACCELERATED_MEDIA.call_once(|| {
            let details = rendering_context.surfman_details();
            crate::accelerated_gl_media::setup_gl_accelerated_media(details.0, details.1);
        });

        let _ = rendering_context.make_current();

        #[cfg(macos)]
        unsafe {
            let rwh = window.window_handle().expect("Failed to get window handle");
            if let RawWindowHandle::AppKit(AppKitWindowHandle { ns_view, .. }) = rwh.as_ref() {
                decorate_window(
                    ns_view.as_ptr() as *mut AnyObject,
                    LogicalPosition::new(8.0, 40.0),
                );
            }
        }
        log::trace!("Created rendering context for window {:?}", window);

        Self {
            window,
            cursor_state: CursorState::default(),
            rendering_context,
            panel_context: None,
            content_context: None,
            panel_dirty: Cell::new(true),
            content_dirty: Cell::new(true),
            prompt_dirty: Cell::new(true),
            panel: None,
            prompt: None,
            panel_tab_ids: HashMap::new(),
            next_panel_tab_index: 0,
            ime_control: None,
            event_listeners: Default::default(),
            mouse_position: Default::default(),
            hovering_webview: None,
            modifiers_state: Cell::new(ModifiersState::default()),
            proxy,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            menu_event_receiver: MenuEvent::receiver().clone(),
            tab_manager: TabManager::new(),
            focused_webview_id: None,
            show_bookmark: false,
        }
    }

    /// Create the panel webview and load the panel UI.
    pub fn create_panel(&mut self, verso: &Rc<Verso>, initial_url: url::Url) {
        let size = self.panel_size();
        let context = Rc::new(self.rendering_context.offscreen_context(size));
        let delegate = PanelWebViewDelegate(WindowWebViewDelegate::new(verso, self.id()));
        // User scripts from the controller are meant for web content, not for the panel UI.
        let webview = WebViewBuilder::new(&verso.servo, context.clone())
            .url(url::Url::parse("verso://resources/components/panel.html").unwrap())
            .hidpi_scale_factor(self.hidpi_scale())
            .delegate(Rc::new(delegate))
            .build();
        webview.resize(size);
        webview.notify_theme_change(self.servo_theme());
        self.panel_context = Some(context);
        self.panel = Some(Panel {
            webview,
            initial_url,
        });
    }

    /// Create a new tab webview and load the initial URL.
    pub fn create_tab(&mut self, verso: &Rc<Verso>, initial_url: url::Url) {
        let size = self.content_size();
        let context = self
            .content_context
            .get_or_insert_with(|| Rc::new(self.rendering_context.offscreen_context(size)))
            .clone();
        let delegate = WindowWebViewDelegate::new(verso, self.id());
        let webview = WebViewBuilder::new(&verso.servo, context)
            .url(initial_url)
            .hidpi_scale_factor(self.hidpi_scale())
            .user_content_manager(verso.user_content_manager.clone())
            .delegate(Rc::new(delegate))
            .build();
        webview.resize(size);
        webview.notify_theme_change(self.servo_theme());
        if let Some(zoom) = verso.config.zoom_level {
            webview.set_page_zoom(zoom);
        }

        let id = webview.id();
        let panel_tab_id = PanelTabId {
            namespace_id: 0,
            index: self.next_panel_tab_index,
        };
        self.next_panel_tab_index += 1;
        self.panel_tab_ids.insert(id, panel_tab_id);
        if let Some(panel) = &self.panel {
            let cmd: String = format!(
                "window.navbar.addTab('{}', {})",
                panel_tab_id.to_json(),
                true,
            );
            panel.webview.evaluate_javascript(cmd, |_| {});
        }

        log::debug!("Verso Window {:?} adds webview {}", self.id(), id);
        if let Some(previous) = self.tab_manager.current_tab_id() {
            if let Some(tab) = self.tab_manager.tab(previous) {
                tab.webview().hide();
            }
        }
        self.tab_manager.append_tab(webview, true);
        if let Some(tab) = self.tab_manager.tab(id) {
            tab.webview().show();
            tab.webview().set_focused(true);
        }
        self.focused_webview_id = Some(id);
        self.resize_webviews();
        self.mark_region_dirty(Region::Content);
        self.mark_region_dirty(Region::Panel);
    }

    /// Close a tab. `from_panel` is true when the panel already removed the tab from its strip.
    ///
    /// The tab strip and [`TabManager`] pick the same neighbour when the active tab closes, so
    /// neither side has to wait for the other. Closing the last tab closes the window.
    pub fn close_tab(&mut self, tab_id: WebViewId, from_panel: bool) {
        if self.tab_manager.tab(tab_id).is_none() {
            return;
        }
        if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.parent == tab_id)
        {
            self.close_prompt();
        }
        let panel_tab_id = self.panel_tab_ids.remove(&tab_id);
        if !from_panel
            && let Some(panel) = &self.panel
            && let Some(panel_tab_id) = panel_tab_id
        {
            panel.webview.evaluate_javascript(
                format!("window.navbar.closeTab('{}')", panel_tab_id.to_json()),
                |_| {},
            );
        }
        let previous = self.tab_manager.current_tab_id();
        if self.focused_webview_id == Some(tab_id) {
            self.focused_webview_id = None;
        }
        // Dropping the tab drops its `WebView`, which closes it in Servo.
        drop(self.tab_manager.close_tab(tab_id));
        if self.tab_manager.count() == 0 {
            self.request_close();
            return;
        }
        if let Some(next) = self.tab_manager.current_tab_id()
            && previous != Some(next)
            && let Some(tab) = self.tab_manager.tab(next)
        {
            tab.webview().show();
            self.focus_webview(next);
            self.update_navbar(next);
        }
        self.resize_webviews();
        self.mark_region_dirty(Region::Content);
    }

    /// Ask the event loop to close this window. The controller may handle the request instead.
    pub(crate) fn request_close(&self) {
        if let Err(error) = self
            .proxy
            .send_event(EventLoopProxyMessage::CloseWindow(self.id()))
        {
            log::error!("Failed to request closing window {:?}: {error}", self.id());
        }
    }

    /// Activate a tab
    pub fn activate_tab(&mut self, tab_id: WebViewId, _show_tab: bool) {
        let Some(old_id) = self.tab_manager.current_tab_id() else {
            return;
        };
        if self.tab_manager.activate_tab(tab_id).is_none() {
            return;
        }
        if old_id != tab_id
            && let Some(old) = self.tab_manager.tab(old_id)
        {
            old.webview().hide();
        }
        if let Some(tab) = self.tab_manager.tab(tab_id) {
            tab.webview().show();
            tab.webview().set_focused(true);
        }
        self.focused_webview_id = Some(tab_id);
        self.resize_webviews();
        self.update_navbar(tab_id);
        if let Some(panel) = &self.panel
            && let Some(panel_tab_id) = self.panel_tab_ids.get(&tab_id)
        {
            let cmd = format!("window.navbar.activateTab('{}')", panel_tab_id.to_json());
            panel.webview.evaluate_javascript(cmd, |_| {});
        }
        self.mark_region_dirty(Region::Content);
    }

    /// Drop every webview of this window. Servo can only shut down once all of them are gone.
    pub(crate) fn close_webviews(&mut self) {
        self.prompt = None;
        self.panel = None;
        for id in self.tab_manager.tab_ids() {
            let _ = self.tab_manager.close_tab(id);
        }
        self.panel_tab_ids.clear();
        self.focused_webview_id = None;
        self.hovering_webview = None;
        self.panel_context = None;
        self.content_context = None;
    }

    /// Handle a Winit window event for this window.
    pub fn handle_winit_window_event(
        &mut self,
        verso: &Rc<Verso>,
        event: &winit::event::WindowEvent,
    ) {
        match event {
            WindowEvent::RedrawRequested => {
                self.repaint();
            }
            WindowEvent::Resized(size) => {
                let size = PhysicalSize::new(size.width.max(1), size.height.max(1));
                self.rendering_context.resize(size);
                self.resize_webviews();
                self.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let scale = self.scale_for(*scale_factor);
                for webview in self.all_webviews() {
                    webview.set_hidpi_scale_factor(scale);
                }
                self.resize_webviews();
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(id) = self.hovering_webview.take()
                    && let Some(webview) = self.webview_cloned(id)
                {
                    webview.notify_input_event(InputEvent::MouseLeftViewport(
                        MouseLeftViewportEvent::default(),
                    ));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_position.set(Some(*position));
                let point = DevicePoint::new(position.x as f32, position.y as f32);
                let target = self.hit_test(point);
                let target_id = target.as_ref().map(|(webview, _)| webview.id());
                let last_hovering_webview = self.hovering_webview.take();
                self.hovering_webview = target_id;
                if last_hovering_webview != target_id
                    && let Some(last_id) = last_hovering_webview
                    && let Some(webview) = self.webview_cloned(last_id)
                {
                    webview.notify_input_event(InputEvent::MouseLeftViewport(
                        MouseLeftViewportEvent::default(),
                    ));
                }
                if let Some((webview, local)) = target {
                    webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                        local.into(),
                    )));
                }

                #[cfg(any(linux, target_os = "windows"))]
                {
                    if self.should_use_client_region_drag() {
                        let direction = self.get_drag_resize_direction();
                        self.set_drag_resize_cursor(direction);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(position) = self.mouse_position.get() else {
                    log::trace!("Mouse position is None, skipping MouseInput event.");
                    return;
                };
                let point = DevicePoint::new(position.x as f32, position.y as f32);

                #[cfg(any(linux, target_os = "windows"))]
                {
                    if *state == ElementState::Pressed
                        && *button == winit::event::MouseButton::Left
                        && self.should_use_client_region_drag()
                    {
                        self.drag_resize_window();
                    }
                }

                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Primary,
                    winit::event::MouseButton::Right => MouseButton::Secondary,
                    winit::event::MouseButton::Middle => MouseButton::Auxiliary,
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                    winit::event::MouseButton::Other(value) => MouseButton::Other(*value),
                };
                let action = match state {
                    ElementState::Pressed => MouseButtonAction::Down,
                    ElementState::Released => MouseButtonAction::Up,
                };
                let Some((webview, local)) = self.hit_test(point) else {
                    return;
                };
                if *state == ElementState::Pressed {
                    self.focus_webview(webview.id());
                }
                webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                    action,
                    button,
                    local.into(),
                )));
            }
            WindowEvent::PinchGesture { delta, .. } => {
                let center = self.mouse_position.get().unwrap_or_default();
                let point = DevicePoint::new(center.x as f32, center.y as f32);
                if let Some((webview, local)) = self.hit_test(point) {
                    webview.adjust_pinch_zoom(*delta as f32, local);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(position) = self.mouse_position.get() else {
                    log::trace!("Mouse position is None, skipping MouseWheel event.");
                    return;
                };
                // Pixels per line, in CSS pixels.
                const LINE_HEIGHT: f64 = 38.0;
                // Servo fires a DOM `wheel` event with this delta and, unless the page cancels
                // it, scrolls by the inverse of the delta in device pixels.
                let (mut dx, mut dy, mode) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        let line = LINE_HEIGHT * self.scale_factor();
                        (*x as f64 * line, *y as f64 * line, WheelMode::DeltaLine)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(position) => {
                        (position.x, position.y, WheelMode::DeltaPixel)
                    }
                };
                // Scroll one axis at a time.
                if dy.abs() >= dx.abs() {
                    dx = 0.0;
                } else {
                    dy = 0.0;
                }
                let point = DevicePoint::new(position.x as f32, position.y as f32);
                if let Some((webview, local)) = self.hit_test(point) {
                    webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                        WheelDelta {
                            x: dx,
                            y: dy,
                            z: 0.0,
                            mode,
                        },
                        local.into(),
                    )));
                }
            }
            WindowEvent::ModifiersChanged(modifier) => self.modifiers_state.set(modifier.state()),
            WindowEvent::KeyboardInput { event, .. } => {
                let event = keyboard_event_from_winit(event, self.modifiers_state.get());
                log::trace!("Verso is handling {event:?}");
                if self.prompt.is_none() && self.handle_keyboard_shortcut(verso, &event.event) {
                    return;
                }
                if let Some(webview) = self.focused_or_current() {
                    webview.notify_input_event(InputEvent::Keyboard(event));
                }
            }
            WindowEvent::ThemeChanged(theme) => {
                let theme = to_servo_theme(Some(theme));
                for webview in self.all_webviews() {
                    webview.notify_theme_change(theme);
                }
            }
            _ => log::trace!("Verso Window isn't supporting this window event yet: {event:?}"),
        }
    }

    /// Handle Window keyboard shortcut
    ///
    /// - Returns `true` if the event is handled, then we should skip sending it to the webview
    fn handle_keyboard_shortcut(&mut self, verso: &Rc<Verso>, event: &KeyboardEvent) -> bool {
        let control_or_meta = if cfg!(target_os = "macos") {
            Modifiers::META
        } else {
            Modifiers::CONTROL
        };

        if event.state == KeyState::Down {
            match (event.modifiers, event.code) {
                (modifiers, Code::KeyT) if modifiers == control_or_meta => {
                    self.create_tab(verso, url::Url::parse("https://example.com").unwrap());
                    return true;
                }
                (modifiers, Code::KeyW) if modifiers == control_or_meta => {
                    if let Some(tab_id) = self.tab_manager.current_tab_id() {
                        self.close_tab(tab_id, false);
                    }
                    return true;
                }
                _ => (),
            }
        }

        false
    }

    fn finish_panel_load(&mut self, verso: &Rc<Verso>) {
        let initial_url = self.panel.as_ref().map(|panel| panel.initial_url.clone());
        if self.tab_manager.count() == 0
            && let Some(initial_url) = initial_url
        {
            self.create_tab(verso, initial_url);
        }
        self.mark_region_dirty(Region::Panel);
    }

    /// Height of the panel chrome, in device pixels.
    fn chrome_height(&self) -> u32 {
        if self.panel.is_none() {
            return 0;
        }
        let mut height = PANEL_HEIGHT + PANEL_PADDING;
        if self.tab_manager.count() > 1 {
            height += TAB_HEIGHT;
        }
        if self.show_bookmark {
            height += BOOKMARK_HEIGHT;
        }
        (height * self.scale_factor()) as u32
    }

    fn panel_size(&self) -> PhysicalSize<u32> {
        PhysicalSize::new(self.size().width.max(1), self.chrome_height().max(1))
    }

    fn content_size(&self) -> PhysicalSize<u32> {
        let size = self.size();
        PhysicalSize::new(
            size.width.max(1),
            size.height.saturating_sub(self.chrome_height()).max(1),
        )
    }

    fn prompt_size(&self) -> PhysicalSize<u32> {
        let scale = self.scale_factor();
        let content = self.content_size();
        PhysicalSize::new(
            ((PROMPT_LOGICAL_SIZE.0 * scale) as u32).clamp(1, content.width),
            ((PROMPT_LOGICAL_SIZE.1 * scale) as u32).clamp(1, content.height),
        )
    }

    /// Rectangle of a region in device pixels, with the origin at the top left of the window.
    fn region_rect(&self, region: Region) -> euclid::default::Rect<i32> {
        let window_w = self.size().width.max(1) as i32;
        let window_h = self.size().height.max(1) as i32;
        let chrome = self.chrome_height() as i32;
        let (origin, size) = match region {
            Region::Panel => ((0, 0), (window_w, chrome.max(1))),
            Region::Content => ((0, chrome), (window_w, (window_h - chrome).max(1))),
            Region::Prompt => {
                let content = self.content_size();
                let prompt = self.prompt_size();
                (
                    (
                        (content.width as i32 - prompt.width as i32) / 2,
                        chrome + (content.height as i32 - prompt.height as i32) / 2,
                    ),
                    (prompt.width as i32, prompt.height as i32),
                )
            }
        };
        euclid::default::Rect::new(origin.into(), size.into())
    }

    /// Rectangle of a region in GL coordinates, with the origin at the bottom left.
    fn gl_target_rect(&self, region: Region) -> euclid::default::Rect<i32> {
        let window_h = self.size().height.max(1) as i32;
        let rect = self.region_rect(region);
        euclid::default::Rect::new(
            euclid::default::Point2D::new(rect.origin.x, window_h - rect.origin.y - rect.height()),
            rect.size,
        )
    }

    fn hidpi_scale(&self) -> Scale<f32, DeviceIndependentPixel, DevicePixel> {
        Scale::new(self.scale_factor() as f32)
    }

    fn scale_for(&self, scale_factor: f64) -> Scale<f32, DeviceIndependentPixel, DevicePixel> {
        Scale::new(scale_factor as f32)
    }

    fn servo_theme(&self) -> servo::Theme {
        to_servo_theme(self.window.theme().as_ref())
    }

    /// Record that Servo has a new frame for `webview_id` and ask winit for a redraw.
    pub(crate) fn mark_dirty(&self, webview_id: WebViewId) {
        self.mark_region_dirty(self.region_of(webview_id));
    }

    /// The region a webview is drawn in.
    fn region_of(&self, webview_id: WebViewId) -> Region {
        if self
            .panel
            .as_ref()
            .is_some_and(|panel| panel.webview.id() == webview_id)
        {
            Region::Panel
        } else if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.webview.id() == webview_id)
        {
            Region::Prompt
        } else {
            Region::Content
        }
    }

    /// Top-left corner of a webview's region, in device pixels from the top left of the window.
    pub(crate) fn webview_origin(&self, webview_id: WebViewId) -> euclid::default::Point2D<i32> {
        self.region_rect(self.region_of(webview_id)).origin
    }

    fn mark_region_dirty(&self, region: Region) {
        match region {
            Region::Panel => self.panel_dirty.set(true),
            Region::Content => self.content_dirty.set(true),
            Region::Prompt => self.prompt_dirty.set(true),
        }
        self.window.request_redraw();
    }

    /// Paint the regions that have a new frame, blit every region into the window, and present
    /// once. Called on `WindowEvent::RedrawRequested`.
    pub(crate) fn repaint(&self) {
        let _ = self.rendering_context.make_current();
        if let Some(panel) = &self.panel {
            self.blit_region(Region::Panel, &panel.webview, &self.panel_context);
        }
        if let Some(tab) = self.tab_manager.current_tab() {
            self.blit_region(Region::Content, tab.webview(), &self.content_context);
        }
        if let Some(prompt) = &self.prompt {
            self.blit_region(
                Region::Prompt,
                &prompt.webview,
                &Some(prompt.rendering_context.clone()),
            );
        }
        self.window.pre_present_notify();
        self.rendering_context.present();
    }

    fn blit_region(
        &self,
        region: Region,
        webview: &WebView,
        context: &Option<Rc<OffscreenRenderingContext>>,
    ) {
        let dirty = match region {
            Region::Panel => &self.panel_dirty,
            Region::Content => &self.content_dirty,
            Region::Prompt => &self.prompt_dirty,
        };
        if dirty.take() {
            // A new tab changes the chrome height, and Servo replaces the offscreen
            // framebuffer. Its destructor binds framebuffer 0 and the resize blit can
            // leave GL_INVALID_OPERATION. WebRender reports that on its next query and
            // aborts the debug build. Bind this region's framebuffer and drop the error
            // first.
            if let Some(context) = context.as_ref() {
                let _ = context.make_current();
                context.prepare_for_rendering();
            }
            self.discard_gl_errors();
            webview.paint();
        }
        let Some(blit) = context
            .as_ref()
            .and_then(|context| context.render_to_parent_callback())
        else {
            return;
        };
        // Servo's blit clears the framebuffer that is currently bound, and only then
        // switches the draw target to the window. `paint` leaves the offscreen
        // framebuffer bound. The content rectangle covers that framebuffer, so the
        // clear erases the page. Bind the window first and the clear lands there.
        self.rendering_context.prepare_for_rendering();
        blit(
            self.rendering_context.glow_gl_api().as_ref(),
            self.gl_target_rect(region),
        );
    }

    /// Size every webview to its region. Resizing one tab resizes the shared content context,
    /// and with it every other tab.
    fn resize_webviews(&mut self) {
        let _ = self.rendering_context.make_current();
        // Servo blits out of whichever framebuffer is current. WebRender may have left the
        // read buffer unset for that framebuffer, which makes the resize blit fail.
        self.prepare_offscreen_for_copy(self.panel_context.as_deref());
        self.prepare_offscreen_for_copy(self.content_context.as_deref());
        self.discard_gl_errors();
        if let Some(panel) = &self.panel {
            panel.webview.resize(self.panel_size());
        }
        if let Some(tab) = self.tab_manager.current_tab() {
            tab.webview().resize(self.content_size());
        }
        if let Some(prompt) = &self.prompt {
            self.prepare_offscreen_for_copy(Some(prompt.rendering_context.as_ref()));
            prompt.webview.resize(self.prompt_size());
        }
        self.panel_dirty.set(true);
        self.content_dirty.set(true);
        self.prompt_dirty.set(true);
        self.discard_gl_errors();
        self.window.request_redraw();
    }

    /// Bind `context` and select its color attachment, so a later framebuffer blit can read it.
    fn prepare_offscreen_for_copy(&self, context: Option<&OffscreenRenderingContext>) {
        let Some(context) = context else {
            return;
        };
        use glow::HasContext as _;
        context.prepare_for_rendering();
        let gl = context.glow_gl_api();
        unsafe {
            gl.read_buffer(glow::COLOR_ATTACHMENT0);
            gl.draw_buffer(glow::COLOR_ATTACHMENT0);
        }
    }

    /// Drop a GL error left by Servo's framebuffer resize so WebRender does not abort on it.
    fn discard_gl_errors(&self) {
        use glow::HasContext as _;
        let gl = self.rendering_context.glow_gl_api();
        unsafe {
            for _ in 0..8 {
                if gl.get_error() == glow::NO_ERROR {
                    break;
                }
            }
        }
    }

    /// Find the webview under `point` and translate the point into that webview.
    fn hit_test(&self, point: DevicePoint) -> Option<(WebView, DevicePoint)> {
        // A prompt is modal: only the prompt itself receives input.
        if let Some(prompt) = &self.prompt {
            let rect = self.region_rect(Region::Prompt).to_f32();
            if !rect.contains(euclid::default::Point2D::new(point.x, point.y)) {
                return None;
            }
            return Some((
                prompt.webview.clone(),
                DevicePoint::new(point.x - rect.origin.x, point.y - rect.origin.y),
            ));
        }
        let chrome = self.chrome_height() as f32;
        if self.panel.is_some() && point.y < chrome {
            let panel = self.panel.as_ref()?;
            return Some((panel.webview.clone(), point));
        }
        let tab = self.tab_manager.current_tab()?;
        Some((
            tab.webview().clone(),
            DevicePoint::new(point.x, (point.y - chrome).max(0.0)),
        ))
    }

    fn all_webviews(&self) -> Vec<WebView> {
        self.panel
            .iter()
            .map(|panel| panel.webview.clone())
            .chain(self.tab_manager.tabs().map(|tab| tab.webview().clone()))
            .chain(self.prompt.iter().map(|prompt| prompt.webview.clone()))
            .collect()
    }

    fn webview_cloned(&self, id: WebViewId) -> Option<WebView> {
        if let Some(panel) = &self.panel
            && panel.webview.id() == id
        {
            return Some(panel.webview.clone());
        }
        if let Some(prompt) = &self.prompt
            && prompt.webview.id() == id
        {
            return Some(prompt.webview.clone());
        }
        self.tab_manager.tab(id).map(|tab| tab.webview().clone())
    }

    fn focused_or_current(&self) -> Option<WebView> {
        self.focused_webview_id
            .and_then(|id| self.webview_cloned(id))
            .or_else(|| {
                self.tab_manager
                    .current_tab()
                    .map(|tab| tab.webview().clone())
            })
            .or_else(|| self.panel.as_ref().map(|panel| panel.webview.clone()))
    }

    fn focus_webview(&mut self, id: WebViewId) {
        if self.focused_webview_id == Some(id) {
            return;
        }
        if let Some(old) = self
            .focused_webview_id
            .and_then(|old| self.webview_cloned(old))
        {
            old.set_focused(false);
        }
        if let Some(webview) = self.webview_cloned(id) {
            webview.set_focused(true);
            self.focused_webview_id = Some(id);
        }
    }

    /// Update the navbar buttons and URL from the active tab's `WebView` state.
    fn update_navbar(&self, tab_id: WebViewId) {
        if self.tab_manager.current_tab_id() != Some(tab_id) {
            return;
        }
        let (Some(panel), Some(tab)) = (&self.panel, self.tab_manager.tab(tab_id)) else {
            return;
        };
        let webview = tab.webview();
        panel.webview.evaluate_javascript(
            format!(
                "window.navbar.setNavBtnEnabled({}, {})",
                webview.can_go_back(),
                webview.can_go_forward()
            ),
            |_| {},
        );
        if let Some(url) = webview.url() {
            let url = serde_json::to_string(url.as_str()).unwrap_or_default();
            panel
                .webview
                .evaluate_javascript(format!("window.navbar.setNavbarUrl({url})"), |_| {});
        }
    }

    fn set_tab_title(&mut self, tab_id: WebViewId, title: Option<String>) {
        let title = title.unwrap_or_else(|| "null".to_string());
        self.tab_manager.set_title(tab_id, title.clone());
        let (Some(panel), Some(panel_tab_id)) = (&self.panel, self.panel_tab_ids.get(&tab_id))
        else {
            return;
        };
        let script = format!(
            "window.navbar.setTabTitle('{}', {})",
            panel_tab_id.to_json(),
            serde_json::to_string(&title).unwrap_or_else(|_| "null".to_string())
        );
        panel.webview.evaluate_javascript(script, |_| {});
    }

    /// Decide a navigation request. The controller decides when it listens for navigations. In
    /// the browser with a panel, Verso first checks whether the URL is a download. Returns the
    /// request back when it should be allowed right away.
    fn handle_navigation_request(
        &mut self,
        verso: &Rc<Verso>,
        request: NavigationRequest,
    ) -> Option<NavigationRequest> {
        let controller = verso
            .controller_sender()
            .filter(|_| self.event_listeners.on_navigation_starting);
        let Some(sender) = controller else {
            if self.panel.is_some() && is_http(&request.url) {
                let id = uuid::Uuid::new_v4();
                verso.check_download(request.url.clone(), DownloadCheckFollowUp::Navigation(id));
                self.event_listeners.pending_navigations.insert(id, request);
                return None;
            }
            return Some(request);
        };
        let id = uuid::Uuid::new_v4();
        if let Err(error) = sender.send(ToControllerMessage::OnNavigationStarting(
            id.as_bytes().to_vec(),
            request.url.clone(),
        )) {
            log::error!("Verso failed to send OnNavigationStarting to controller: {error}");
            return Some(request);
        }
        self.event_listeners.pending_navigations.insert(id, request);
        None
    }

    /// Answer a navigation request held for the controller or for a download check.
    pub(crate) fn answer_navigation(&mut self, id: uuid::Uuid, allow: bool) {
        match self.event_listeners.pending_navigations.remove(&id) {
            Some(request) if allow => request.allow(),
            Some(request) => request.deny(),
            None => log::warn!("No pending navigation for response {id}"),
        }
    }

    /// Continue after [`Verso::check_download`]. `proceed` is false when the URL turned out to
    /// be a download, which then runs instead of the navigation.
    pub(crate) fn finish_download_check(
        &mut self,
        follow_up: &DownloadCheckFollowUp,
        url: &url::Url,
        proceed: bool,
    ) {
        match follow_up {
            DownloadCheckFollowUp::Navigation(id) => {
                if self.event_listeners.pending_navigations.contains_key(id) {
                    self.answer_navigation(*id, proceed);
                }
            }
            DownloadCheckFollowUp::Load(tab_id) => {
                if proceed && let Some(tab) = self.tab_manager.tab(*tab_id) {
                    tab.webview().load(url.clone());
                }
            }
        }
    }

    /// Show the panel's download button once a download exists.
    pub(crate) fn show_download_button(&self) {
        if let Some(panel) = &self.panel {
            panel
                .webview
                .evaluate_javascript("window.navbar.showDownloadBtn(true)", |_| {});
        }
    }

    /// Show the bookmark bar when there are bookmarks, and resize the regions when it changes.
    fn update_bookmark_bar(&mut self, verso: &Rc<Verso>) {
        let show = verso.has_bookmarks();
        if show != self.show_bookmark {
            self.show_bookmark = show;
            self.resize_webviews();
            self.mark_region_dirty(Region::Panel);
            self.mark_region_dirty(Region::Content);
        }
    }

    /// Answer a `window.prompt("VERSO::...")` from Verso's bookmark and downloads pages.
    fn handle_verso_page_prompt(
        &mut self,
        verso: &Rc<Verso>,
        tab_id: WebViewId,
        mut dialog: PromptDialog,
    ) {
        let message = dialog.message().trim_start_matches("VERSO::").to_owned();
        if message == "DOWNLOAD_STATUS_GET" {
            dialog.set_current_value(&verso.downloads_json());
            dialog.confirm();
            return;
        }
        if message == "BOOKMARK_LIST_GET" {
            dialog.set_current_value(&verso.bookmarks_json());
            dialog.confirm();
            return;
        }
        if let Some(id) = message.strip_prefix("ABORT_DOWNLOAD::") {
            verso.abort_download(id);
        } else if let Some(id) = message.strip_prefix("BOOKMARK_REMOVE::") {
            verso.remove_bookmark(id);
        } else if let Some(params) = message.strip_prefix("BOOKMARK_RENAME::") {
            match params.split("::").collect::<Vec<_>>()[..] {
                [id, name] => verso.rename_bookmark(id, name.to_owned()),
                _ => log::error!("Invalid parameters for BOOKMARK_RENAME"),
            }
        } else if let Some(url) = message.strip_prefix("NAVIGATE_TO::") {
            if let Some(url) = parse_navigation_url(url)
                && let Some(tab) = self.tab_manager.tab(tab_id)
            {
                tab.webview().load(url);
            }
        } else {
            log::debug!("Ignoring Verso page prompt: {message}");
        }
        dialog.dismiss();
    }

    fn present_panel_control(
        &mut self,
        verso: &Rc<Verso>,
        panel_id: WebViewId,
        control: EmbedderControl,
    ) {
        match control {
            EmbedderControl::SimpleDialog(SimpleDialog::Prompt(mut dialog)) => {
                let message = dialog.message().to_owned();
                if let Some(value) = self.handle_panel_prompt(verso, &message) {
                    dialog.set_current_value(&value);
                }
                dialog.confirm();
            }
            EmbedderControl::SimpleDialog(dialog) => dialog.confirm(),
            other => self.present_embedder_control(verso, panel_id, other),
        }
    }

    fn present_embedder_control(
        &mut self,
        verso: &Rc<Verso>,
        webview_id: WebViewId,
        control: EmbedderControl,
    ) {
        let control_id = control.id();
        match control {
            EmbedderControl::FilePicker(picker) => present_file_picker(picker),
            EmbedderControl::ContextMenu(menu) => self.show_context_menu(menu),
            EmbedderControl::SelectElement(select) => self.show_select_element(webview_id, select),
            EmbedderControl::SimpleDialog(dialog) => {
                let pending = match dialog {
                    SimpleDialog::Alert(dialog) => PendingPrompt::Alert(dialog),
                    SimpleDialog::Confirm(dialog) => PendingPrompt::Confirm(dialog),
                    SimpleDialog::Prompt(dialog) => PendingPrompt::Input(dialog),
                };
                self.show_prompt(verso, webview_id, Some(control_id), pending);
            }
            EmbedderControl::InputMethod(input) => {
                if input.allow_virtual_keyboard() {
                    self.ime_control = Some(input.id());
                    self.show_ime(webview_id, input.position());
                }
            }
            EmbedderControl::ColorPicker(_) => {
                // No native color picker is available. Dropping the control keeps the
                // current color.
                log::debug!("Color pickers are not supported yet");
            }
        }
    }

    fn hide_embedder_control(&mut self, control_id: EmbedderControlId) {
        if self.ime_control == Some(control_id) {
            self.ime_control = None;
            self.hide_ime();
        }
        if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.control_id == Some(control_id))
        {
            self.close_prompt();
        }
    }

    /// Show a prompt page over the content area for a request from the tab `parent`.
    fn show_prompt(
        &mut self,
        verso: &Rc<Verso>,
        parent: WebViewId,
        control_id: Option<EmbedderControlId>,
        pending: PendingPrompt,
    ) {
        if self.tab_manager.tab(parent).is_none() {
            log::debug!("Ignoring a prompt from a webview that is not a tab");
            return;
        }
        if self.prompt.is_some() {
            // Dropping the request gives Servo its default answer.
            log::warn!("A prompt is already open; answering the new one with its default");
            return;
        }
        if self.tab_manager.current_tab_id() != Some(parent) {
            self.activate_tab(parent, true);
        }

        let size = self.prompt_size();
        let context = Rc::new(self.rendering_context.offscreen_context(size));
        let delegate = PromptWebViewDelegate(WindowWebViewDelegate::new(verso, self.id()));
        let webview = WebViewBuilder::new(&verso.servo, context.clone())
            .url(pending.resource_url())
            .hidpi_scale_factor(self.hidpi_scale())
            .delegate(Rc::new(delegate))
            .build();
        webview.resize(size);
        webview.notify_theme_change(self.servo_theme());
        let id = webview.id();
        log::debug!(
            "Verso Window {:?} shows prompt {} for tab {}",
            self.id(),
            id,
            parent
        );
        self.prompt = Some(Prompt {
            webview,
            rendering_context: context,
            parent,
            control_id,
            pending: Some(pending),
        });
        self.focus_webview(id);
        self.mark_region_dirty(Region::Prompt);
    }

    /// Finish the prompt shown by `prompt_webview`, answering its request with `result`.
    fn finish_prompt(&mut self, prompt_webview: WebViewId, result: Option<&str>) {
        let Some(prompt) = self
            .prompt
            .as_mut()
            .filter(|prompt| prompt.webview.id() == prompt_webview)
        else {
            return;
        };
        if let Some(result) = result {
            prompt.respond(result);
        }
        self.close_prompt();
    }

    /// Remove the prompt overlay. An unanswered request gets Servo's default answer.
    fn close_prompt(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        if self.focused_webview_id == Some(prompt.webview.id()) {
            self.focused_webview_id = None;
            self.focus_webview(prompt.parent);
        }
        self.mark_region_dirty(Region::Content);
    }

    /// Handle a `window.prompt(...)` command from the panel. Returns the value the panel reads
    /// back, for the commands that have one.
    fn handle_panel_prompt(&mut self, verso: &Rc<Verso>, message: &str) -> Option<String> {
        let current = self
            .tab_manager
            .current_tab()
            .map(|tab| tab.webview().clone());
        match message {
            "PREV" => {
                if let Some(webview) = current {
                    let _ = webview.go_back(1);
                }
            }
            "FORWARD" => {
                if let Some(webview) = current {
                    let _ = webview.go_forward(1);
                }
            }
            "REFRESH" => {
                if let Some(webview) = current {
                    webview.reload();
                }
            }
            "MINIMIZE" => {
                let _ = self.window.set_minimized(true);
            }
            "MAXIMIZE" | "DBCLICK_PANEL" => self.window.set_maximized(!self.window.is_maximized()),
            "DRAG_WINDOW" => {
                if let Err(error) = self.window.drag_window() {
                    log::warn!("Failed to drag the window: {error}");
                }
            }
            "NEW_WINDOW" => {
                if let Err(error) = self.proxy.send_event(EventLoopProxyMessage::NewWindow) {
                    log::error!("Failed to request a new window: {error}");
                }
            }
            "BOOKMARK" => {
                let tab = self.tab_manager.current_tab()?;
                let url = tab.webview().url()?;
                // Verso's own pages are not bookmarked.
                if url.scheme() != "verso" {
                    verso.toggle_bookmark(tab.title(), url.to_string());
                }
                self.update_bookmark_bar(verso);
                return Some(verso.bookmarks_json());
            }
            "UPDATE_BOOKMARK" => {
                self.update_bookmark_bar(verso);
                return Some(verso.bookmarks_json());
            }
            "OPEN_BOOKMARK_MANAGER" => self.create_tab(
                verso,
                url::Url::parse("verso://resources/components/bookmark.html").unwrap(),
            ),
            "DOWNLOAD" => self.create_tab(
                verso,
                url::Url::parse("verso://resources/components/downloads.html").unwrap(),
            ),
            other if other.starts_with("NAVIGATE_TO:") => {
                let url = parse_navigation_url(other.trim_start_matches("NAVIGATE_TO:"))?;
                match self.tab_manager.current_tab_id() {
                    Some(tab_id) if is_http(&url) => {
                        verso.check_download(url, DownloadCheckFollowUp::Load(tab_id));
                    }
                    Some(_) => current?.load(url),
                    None => self.create_tab(verso, url),
                }
            }
            other if other.starts_with("ACTIVATE_TAB:") => {
                let payload = other.trim_start_matches("ACTIVATE_TAB:");
                if let Some(tab_id) = self.parse_panel_tab(payload) {
                    self.activate_tab(tab_id, self.tab_manager.count() > 1);
                }
            }
            other if other.starts_with("CLOSE_TAB:") => {
                let payload = other.trim_start_matches("CLOSE_TAB:");
                if let Some(tab_id) = self.parse_panel_tab(payload) {
                    self.close_tab(tab_id, true);
                }
            }
            other if other.starts_with("OPEN_HISTORY_MENU:") => {
                let payload = other.trim_start_matches("OPEN_HISTORY_MENU:");
                match serde_json::from_str::<OpenHistoryMenuRequest>(payload) {
                    Ok(request) => self.show_history_menu(request),
                    Err(error) => log::error!("Invalid history menu request {payload:?}: {error}"),
                }
            }
            _ => log::debug!("Ignoring panel prompt: {message}"),
        }
        None
    }

    /// Read the tab id from an `ACTIVATE_TAB:` or `CLOSE_TAB:` panel message, `{"id": ...}`.
    fn parse_panel_tab(&self, payload: &str) -> Option<WebViewId> {
        let value: serde_json::Value = serde_json::from_str(payload).ok()?;
        let id = &value["id"];
        let panel_tab_id: PanelTabId = serde_json::from_value(id.clone())
            .ok()
            .or_else(|| id.as_str().and_then(|text| serde_json::from_str(text).ok()))?;
        self.panel_tab_ids
            .iter()
            .find(|(_, id)| **id == panel_tab_id)
            .map(|(webview_id, _)| *webview_id)
    }

    /// Queues a Winit `WindowEvent::RedrawRequested` event to be emitted that aligns with the windowing system drawing loop.
    pub fn request_redraw(&self) {
        self.window.request_redraw()
    }

    /// Size of the window that's used by webrender.
    pub fn size(&self) -> dpi::PhysicalSize<u32> {
        self.window.inner_size()
    }

    /// Size of the window, including the window decorations.
    pub fn outer_size(&self) -> PhysicalSize<u32> {
        self.window.outer_size()
    }

    /// Get Winit window ID of the window.
    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// Scale factor of the window. This is also known as HIDPI.
    pub fn scale_factor(&self) -> f64 {
        self.window.scale_factor()
    }

    /// Check if the window has such webview.
    pub fn has_webview(&self, id: WebViewId) -> bool {
        self.webview_cloned(id).is_some()
    }

    /// Set cursor icon of the window.
    pub fn set_cursor_icon(&mut self, cursor: Cursor) {
        let winit_cursor = match cursor {
            Cursor::Default => CursorIcon::Default,
            Cursor::Pointer => CursorIcon::Pointer,
            Cursor::ContextMenu => CursorIcon::ContextMenu,
            Cursor::Help => CursorIcon::Help,
            Cursor::Progress => CursorIcon::Progress,
            Cursor::Wait => CursorIcon::Wait,
            Cursor::Cell => CursorIcon::Cell,
            Cursor::Crosshair => CursorIcon::Crosshair,
            Cursor::Text => CursorIcon::Text,
            Cursor::VerticalText => CursorIcon::VerticalText,
            Cursor::Alias => CursorIcon::Alias,
            Cursor::Copy => CursorIcon::Copy,
            Cursor::Move => CursorIcon::Move,
            Cursor::NoDrop => CursorIcon::NoDrop,
            Cursor::NotAllowed => CursorIcon::NotAllowed,
            Cursor::Grab => CursorIcon::Grab,
            Cursor::Grabbing => CursorIcon::Grabbing,
            Cursor::EResize => CursorIcon::EResize,
            Cursor::NResize => CursorIcon::NResize,
            Cursor::NeResize => CursorIcon::NeResize,
            Cursor::NwResize => CursorIcon::NwResize,
            Cursor::SResize => CursorIcon::SResize,
            Cursor::SeResize => CursorIcon::SeResize,
            Cursor::SwResize => CursorIcon::SwResize,
            Cursor::WResize => CursorIcon::WResize,
            Cursor::EwResize => CursorIcon::EwResize,
            Cursor::NsResize => CursorIcon::NsResize,
            Cursor::NeswResize => CursorIcon::NeswResize,
            Cursor::NwseResize => CursorIcon::NwseResize,
            Cursor::ColResize => CursorIcon::ColResize,
            Cursor::RowResize => CursorIcon::RowResize,
            Cursor::AllScroll => CursorIcon::AllScroll,
            Cursor::ZoomIn => CursorIcon::ZoomIn,
            Cursor::ZoomOut => CursorIcon::ZoomOut,
            Cursor::None => {
                self.window.set_cursor_visible(false);
                return;
            }
        };
        self.cursor_state.current_cursor = winit_cursor;
        self.window.set_cursor(winit_cursor);
        self.window.set_cursor_visible(true);
    }

    /// This method enables IME and set the IME cursor area of the window.
    /// `position` is in the webview's device pixels.
    pub fn show_ime(&self, webview_id: WebViewId, position: DeviceIntRect) {
        self.window.set_ime_allowed(true);
        let origin = self.webview_origin(webview_id);
        self.window.set_ime_cursor_area(
            PhysicalPosition::new(position.min.x + origin.x, position.min.y + origin.y),
            PhysicalSize::new(position.width(), position.height()),
        );
    }

    /// This method disables IME of the window.
    pub fn hide_ime(&self) {
        self.window.set_ime_allowed(false);
    }

    /// Show notification
    pub fn show_notification(&self, notification: &Notification) {
        let mut display_notification = notify_rust::Notification::new();
        display_notification
            .summary(&notification.title)
            .body(&notification.body);

        #[cfg(linux)]
        std::thread::spawn(move || {
            if let Ok(handle) = display_notification.show() {
                // prevent handler dropped immediately which will close the notification as well
                handle.on_close(|| {});
            }
        });
        #[cfg(not(linux))]
        let _ = display_notification.show();
    }
}

// Non-decorated window resizing for Windows and Linux.
#[cfg(any(linux, target_os = "windows"))]
impl Window {
    /// Check current window state is allowed to apply drag-resize in client region.
    fn should_use_client_region_drag(&self) -> bool {
        !self.window.is_decorated()
            && !self.window.is_maximized()
            && self.window.is_resizable()
            && self.window.fullscreen().is_none()
    }

    /// Drag resize the window.
    fn drag_resize_window(&self) {
        if let Some(direction) = self.get_drag_resize_direction() {
            if let Err(err) = self.window.drag_resize_window(direction) {
                log::error!("Failed to drag-resize window: {:?}", err);
            }
        }
    }

    /// Get drag-resize direction.
    fn get_drag_resize_direction(&self) -> Option<ResizeDirection> {
        let mouse_position = match self.mouse_position.get() {
            Some(position) => position,
            None => {
                return None;
            }
        };

        let window_size = self.window.outer_size();
        let border_size = 5.0 * self.window.scale_factor();

        let x_direction = if mouse_position.x < border_size {
            Some(ResizeDirection::West)
        } else if mouse_position.x > (window_size.width as f64 - border_size) {
            Some(ResizeDirection::East)
        } else {
            None
        };

        let y_direction = if mouse_position.y < border_size {
            Some(ResizeDirection::North)
        } else if mouse_position.y > (window_size.height as f64 - border_size) {
            Some(ResizeDirection::South)
        } else {
            None
        };

        let direction = match (x_direction, y_direction) {
            (Some(ResizeDirection::East), None) => ResizeDirection::East,
            (Some(ResizeDirection::West), None) => ResizeDirection::West,
            (None, Some(ResizeDirection::South)) => ResizeDirection::South,
            (None, Some(ResizeDirection::North)) => ResizeDirection::North,
            (Some(ResizeDirection::East), Some(ResizeDirection::North)) => {
                ResizeDirection::NorthEast
            }
            (Some(ResizeDirection::West), Some(ResizeDirection::North)) => {
                ResizeDirection::NorthWest
            }
            (Some(ResizeDirection::East), Some(ResizeDirection::South)) => {
                ResizeDirection::SouthEast
            }
            (Some(ResizeDirection::West), Some(ResizeDirection::South)) => {
                ResizeDirection::SouthWest
            }
            _ => return None,
        };

        Some(direction)
    }

    /// Set drag-resize cursor when mouse is hover on the border of the window.
    fn set_drag_resize_cursor(&mut self, direction: Option<ResizeDirection>) {
        if let Some(direction) = direction {
            let cursor = match direction {
                ResizeDirection::East => CursorIcon::EResize,
                ResizeDirection::West => CursorIcon::WResize,
                ResizeDirection::South => CursorIcon::SResize,
                ResizeDirection::North => CursorIcon::NResize,
                ResizeDirection::NorthEast => CursorIcon::NeResize,
                ResizeDirection::NorthWest => CursorIcon::NwResize,
                ResizeDirection::SouthEast => CursorIcon::SeResize,
                ResizeDirection::SouthWest => CursorIcon::SwResize,
            };
            self.cursor_state.cursor_resizing = true;
            self.window.set_cursor(cursor);
        } else if self.cursor_state.cursor_resizing {
            self.cursor_state.cursor_resizing = false;
            self.window.set_cursor(self.cursor_state.current_cursor);
        }
    }
}

/* window decoration */
#[cfg(macos)]
use objc2::runtime::AnyObject;
#[cfg(macos)]
use raw_window_handle::{AppKitWindowHandle, RawWindowHandle};
#[cfg(macos)]
use winit::dpi::LogicalPosition;

/// Window decoration for macOS.
#[cfg(macos)]
pub unsafe fn decorate_window(view: *mut AnyObject, _position: LogicalPosition<f64>) {
    use objc2::rc::Id;
    use objc2_app_kit::{NSView, NSWindowStyleMask, NSWindowTitleVisibility};

    let ns_view: Id<NSView> = unsafe { Id::retain(view.cast()) }.unwrap();
    let window = ns_view
        .window()
        .expect("view was not installed in a window");
    window.setMovable(false); // let panel UI handle window moving
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::NSWindowTitleHidden);
    window.setStyleMask(
        NSWindowStyleMask::Titled
            | NSWindowStyleMask::FullSizeContentView
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::Miniaturizable,
    );
}

fn to_servo_theme(theme: Option<&winit::window::Theme>) -> servo::Theme {
    match theme {
        Some(winit::window::Theme::Dark) => servo::Theme::Dark,
        Some(winit::window::Theme::Light) | None => servo::Theme::Light,
    }
}

fn present_file_picker(mut picker: FilePicker) {
    let mut dialog = rfd::FileDialog::new();
    for pattern in picker.filter_patterns() {
        dialog = dialog.add_filter(&pattern.0, &[&pattern.0]);
    }
    let paths = if picker.allow_select_multiple() {
        dialog.pick_files()
    } else {
        dialog.pick_file().map(|path| vec![path])
    };
    if let Some(paths) = paths {
        picker.select(&paths);
        picker.submit();
    } else {
        picker.dismiss();
    }
}

/// Parse a URL typed in the panel. A bare host such as `example.com` becomes `https://example.com`.
fn parse_navigation_url(text: &str) -> Option<url::Url> {
    match url::Url::parse(text) {
        Ok(url) => Some(url),
        Err(url::ParseError::RelativeUrlWithoutBase) => url::Url::parse(&format!("https://{text}"))
            .inspect_err(|error| log::warn!("Navigation URL {text:?} is invalid: {error}"))
            .ok(),
        Err(error) => {
            log::warn!("Navigation URL {text:?} is invalid: {error}");
            None
        }
    }
}

/// Only HTTP(S) responses can be downloads; other schemes load directly.
fn is_http(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}
