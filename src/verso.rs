use std::{cell::RefCell, collections::HashMap, rc::Rc, str::FromStr};

use ipc_channel::ipc::{self, IpcSender};
use ipc_channel::router::ROUTER;
use serde::{Deserialize, Serialize};
use servo::{
    AllowOrDenyRequest, EventLoopWaker, Servo, ServoBuilder, ServoDelegate, ServoError,
    UserContentManager, WebResourceResponse, WebViewId,
};
use versoview_messages::{PositionType, SizeType, ToControllerMessage, ToVersoMessage};
use winit::{
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    window::WindowId,
};

use crate::{
    bookmark::{BookmarkId, BookmarkManager},
    config::{Config, parse_cli_args, to_winit_theme, to_winit_window_level},
    download::{DownloadId, DownloadItem, UpdateDownloadState, check_should_download, download_body},
    storage::Storage,
    window::Window,
};

/// Main entry point of Verso browser.
pub struct Verso {
    pub(crate) servo: Servo,
    pub(crate) user_content_manager: Rc<UserContentManager>,
    pub(crate) windows: RefCell<HashMap<WindowId, Window>>,
    to_controller_sender: Option<IpcSender<ToControllerMessage>>,
    pub(crate) config: Config,
    bookmark_manager: RefCell<BookmarkManager>,
    storage: Storage,
    downloads: RefCell<HashMap<DownloadId, DownloadItem>>,
    /// Proxy to post [`EventLoopProxyMessage`]s to the winit event loop
    proxy: EventLoopProxy<EventLoopProxyMessage>,
    /// Channel for Verso's worker tasks, such as downloads, to reach the event loop
    internal_sender: IpcSender<VersoInternalMsg>,
    /// HTTP client used to check whether a navigation is a download
    http_client: reqwest::Client,
}

struct VersoServoDelegate;
impl ServoDelegate for VersoServoDelegate {
    fn notify_devtools_server_started(&self, port: u16, _token: String) {
        log::info!("Devtools Server running on port {port}");
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        request.allow();
    }

    fn notify_error(&self, error: ServoError) {
        log::error!("Saw Servo error: {error:?}!");
    }
}

impl Verso {
    /// Create a Verso instance from Winit's window and event loop proxy.
    pub fn new(evl: &ActiveEventLoop, proxy: EventLoopProxy<EventLoopProxyMessage>) -> Rc<Self> {
        let (config, to_controller_sender) = try_connect_ipc_and_get_config(&proxy);

        // Initialize configurations and Verso window
        let protocols = config.create_protocols();
        let initial_url = config.url.clone();
        let with_panel = config.with_panel;
        let window_settings = config.window_attributes.clone();
        let user_scripts = config.user_scripts.clone();

        let mut window = Window::new(evl, window_settings, proxy.clone());
        let event_loop_waker = Box::new(Waker(proxy.clone()));
        let internal_sender = route_internal_messages(&proxy);

        let (opts, preferences) = config.init();
        let servo = ServoBuilder::default()
            .opts(opts)
            .preferences(preferences)
            .protocol_registry(protocols)
            .event_loop_waker(event_loop_waker)
            .build();
        servo.setup_logging();
        servo.set_delegate(Rc::new(VersoServoDelegate));

        let user_content_manager = Rc::new(UserContentManager::new(&servo));
        for script in user_scripts {
            user_content_manager.add_script(Rc::new(script));
        }

        let windows = HashMap::new();

        // Create Verso instance
        let verso = Rc::new(Verso {
            servo,
            user_content_manager,
            windows: windows.into(),
            to_controller_sender,
            config,
            bookmark_manager: RefCell::new(BookmarkManager::new()),
            storage: Storage::new(),
            downloads: RefCell::new(HashMap::new()),
            proxy,
            internal_sender,
            http_client: reqwest::Client::new(),
        });

        if with_panel {
            window.create_panel(&verso, initial_url);
        } else {
            window.create_tab(&verso, initial_url);
        }

        verso.windows.borrow_mut().insert(window.id(), window);

        verso
    }

    /// Handle Winit window events. The strategy to handle event are different between platforms
    /// because the order of events might be different.
    pub fn handle_window_event(
        self: &mut Rc<Self>,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let spin = !cfg!(linux) || !matches!(event, WindowEvent::Resized(_));
        self.handle_winit_window_event(event_loop, window_id, event);
        if spin {
            self.handle_servo_messages(event_loop);
        }
    }

    /// Handle Winit window events
    fn handle_winit_window_event(
        self: &mut Rc<Self>,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        log::trace!("Verso is handling Winit event: {event:?}");

        if let WindowEvent::CloseRequested = &event {
            self.request_close_window(event_loop, window_id);
            return;
        }

        let mut windows = self.windows.borrow_mut();
        let Some(window) = windows.get_mut(&window_id) else {
            return;
        };
        window.handle_winit_window_event(self, &event);
    }

    /// Close a window, unless the controller asked to handle close requests itself.
    pub fn request_close_window(&self, event_loop: &ActiveEventLoop, window_id: WindowId) {
        let defer_to_controller = {
            let windows = self.windows.borrow();
            let Some(window) = windows.get(&window_id) else {
                return;
            };
            self.to_controller_sender
                .as_ref()
                .is_some_and(|sender| window.event_listeners.on_close_requested && {
                    if let Err(error) = sender.send(ToControllerMessage::OnCloseRequested) {
                        log::error!("Verso failed to send close request to controller: {error}");
                        false
                    } else {
                        true
                    }
                })
        };
        if !defer_to_controller {
            self.close_window(event_loop, window_id);
        }
    }

    /// Close one window and drop its webviews. Exit once no window is left.
    pub fn close_window(&self, event_loop: &ActiveEventLoop, window_id: WindowId) {
        let window = self.windows.borrow_mut().remove(&window_id);
        if let Some(mut window) = window {
            window.close_webviews();
        }
        if self.windows.borrow().is_empty() {
            event_loop.exit();
        }
    }

    /// Open a new window with the panel, as the panel's new-window button does.
    pub fn create_window(self: &Rc<Self>, event_loop: &ActiveEventLoop) {
        let mut window = Window::new(
            event_loop,
            self.config.window_attributes.clone(),
            self.proxy.clone(),
        );
        if self.config.with_panel {
            window.create_panel(self, self.config.url.clone());
        } else {
            window.create_tab(self, self.config.url.clone());
        }
        self.windows.borrow_mut().insert(window.id(), window);
    }

    /// Handle message came from Servo.
    ///
    /// Servo's refresh driver wakes the event loop through [`EventLoopWaker`] when an animation
    /// needs a new frame, so the loop always waits for the next event.
    pub fn handle_servo_messages(self: &mut Rc<Self>, event_loop: &ActiveEventLoop) {
        self.servo.spin_event_loop();
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    /// Handle a wake-up from Servo by spinning its event loop. Windows request their own
    /// redraws when Servo has a new frame.
    pub fn request_redraw(self: &mut Rc<Self>, event_loop: &ActiveEventLoop) {
        self.handle_servo_messages(event_loop);
    }

    /// Handle message came from webview controller.
    pub fn handle_incoming_webview_message(
        self: &mut Rc<Self>,
        event_loop: &ActiveEventLoop,
        message: ToVersoMessage,
    ) {
        if let ToVersoMessage::Exit = message {
            self.close_all_windows();
            event_loop.exit();
            return;
        }

        // We only use the first window for now
        let mut windows = self.windows.borrow_mut();
        let Some(window) = windows.values_mut().next().map(|window| window) else {
            return;
        };
        // We only use the first webview for now
        let webview = window.tab_manager.current_tab().map(|tab| tab.webview());

        match message {
            ToVersoMessage::Exit => unreachable!("handled above"),
            ToVersoMessage::ListenToOnCloseRequested => {
                window.event_listeners.on_close_requested = true;
            }
            ToVersoMessage::NavigateTo(to_url) => {
                if let Some(webview) = webview {
                    webview.load(to_url);
                }
            }
            ToVersoMessage::Reload => {
                if let Some(webview) = webview {
                    webview.reload();
                }
            }
            ToVersoMessage::ListenToOnNavigationStarting => {
                window.event_listeners.on_navigation_starting = true;
            }
            ToVersoMessage::OnNavigationStartingResponse(id, allow) => match uuid::Uuid::from_slice(&id) {
                Ok(id) => window.answer_navigation(id, allow),
                Err(_) => log::error!("Controller answered an unknown navigation id"),
            },
            ToVersoMessage::ExecuteScript(js) => {
                if let Some(webview) = webview {
                    webview.evaluate_javascript(js, |_| {});
                }
            }
            ToVersoMessage::ListenToWebResourceRequests => {
                window
                    .event_listeners
                    .on_web_resource_requested
                    .replace(HashMap::new());
            }
            ToVersoMessage::WebResourceRequestResponse(response) => {
                if let Some(load) = window
                    .event_listeners
                    .on_web_resource_requested
                    .as_mut()
                    .and_then(|loads| loads.remove(&response.id))
                {
                    if let Some(http_response) = response.response {
                        let url = load.request().url.clone();
                        let mut intercepted = load.intercept(
                            WebResourceResponse::new(url)
                                .headers(http_response.headers().clone())
                                .status_code(http_response.status()),
                        );
                        intercepted.send_body_data(http_response.into_body());
                        intercepted.finish();
                    }
                }
            }
            ToVersoMessage::SetTitle(title) => {
                window.window.set_title(&title);
            }
            ToVersoMessage::SetSize(size) => {
                let _ = window.window.request_inner_size(size);
            }
            ToVersoMessage::SetPosition(position) => {
                window.window.set_outer_position(position);
            }
            ToVersoMessage::SetMaximized(maximized) => {
                window.window.set_maximized(maximized);
            }
            ToVersoMessage::SetMinimized(minimized) => {
                window.window.set_minimized(minimized);
            }
            ToVersoMessage::SetFullscreen(fullscreen) => {
                window.window.set_fullscreen(if fullscreen {
                    Some(winit::window::Fullscreen::Borderless(None))
                } else {
                    None
                });
            }
            ToVersoMessage::SetVisible(visible) => {
                window.window.set_visible(visible);
            }
            ToVersoMessage::SetWindowLevel(window_level) => {
                window
                    .window
                    .set_window_level(to_winit_window_level(window_level));
            }
            ToVersoMessage::SetTheme(theme) => {
                window.window.set_theme(to_winit_theme(&theme));
            }
            ToVersoMessage::StartDragging => {
                let _ = window.window.drag_window();
            }
            ToVersoMessage::Focus => {
                window.window.focus_window();
            }
            ToVersoMessage::GetTitle(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetTitleResponse(id, window.window.title()),
                ) {
                    log::error!("Verso failed to send GetTitleReponse to controller: {error}")
                }
            }
            ToVersoMessage::GetSize(id, size_type) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetSizeResponse(
                        id,
                        match size_type {
                            SizeType::Inner => window.window.inner_size(),
                            SizeType::Outer => window.window.outer_size(),
                        },
                    ),
                ) {
                    log::error!("Verso failed to send GetSizeReponse to controller: {error}")
                }
            }
            ToVersoMessage::GetPosition(id, position_type) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetPositionResponse(
                        id,
                        match position_type {
                            PositionType::Inner => window.window.inner_position(),
                            PositionType::Outer => window.window.outer_position(),
                        }
                        .ok(),
                    ),
                ) {
                    log::error!("Verso failed to send GetPositionResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetMinimized(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetMinimizedResponse(
                        id,
                        window.window.is_minimized().unwrap_or_default(),
                    ),
                ) {
                    log::error!("Verso failed to send GetMinimizedResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetMaximized(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetMaximizedResponse(id, window.window.is_maximized()),
                ) {
                    log::error!("Verso failed to send GetMaximizedResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetFullscreen(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetFullscreenResponse(
                        id,
                        window.window.fullscreen().is_some(),
                    ),
                ) {
                    log::error!("Verso failed to send GetFullscreenResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetVisible(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetVisibleResponse(
                        id,
                        window.window.is_visible().unwrap_or(true),
                    ),
                ) {
                    log::error!("Verso failed to send GetVisibleResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetScaleFactor(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetScaleFactorResponse(id, window.window.scale_factor()),
                ) {
                    log::error!(
                        "Verso failed to send GetScaleFactorResponse to controller: {error}"
                    )
                }
            }
            ToVersoMessage::GetTheme(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetThemeResponse(
                        id,
                        match window.window.theme() {
                            Some(winit::window::Theme::Dark) => versoview_messages::Theme::Dark,
                            _ => versoview_messages::Theme::Light,
                        },
                    ),
                ) {
                    log::error!("Verso failed to send GetThemeResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetCurrentUrl(id) => {
                // Before the first tab exists, or before it commits a URL, report `about:blank`.
                let url = window
                    .tab_manager
                    .current_tab()
                    .and_then(|tab| tab.webview().url())
                    .unwrap_or_else(|| url::Url::parse("about:blank").unwrap());
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetCurrentUrlResponse(id, url),
                ) {
                    log::error!(
                        "Verso failed to send GetScaleFactorResponse to controller: {error}"
                    )
                }
            }
            ToVersoMessage::SetConfig(..) => {
                log::error!(
                    "`ToVersoMessage::SetConfig` should not be received after the initial setup"
                )
            }
        }
    }

    /// Drop every window and its webviews.
    ///
    /// Servo 0.7.0 shuts down when the last `Servo` handle is dropped, and every `WebView` holds
    /// one. Dropping the webviews here lets `Servo` shut down when [`Verso`] itself is dropped.
    pub fn close_all_windows(&self) {
        let mut windows = std::mem::take(&mut *self.windows.borrow_mut());
        for window in windows.values_mut() {
            window.close_webviews();
        }
        drop(windows);
    }

    /// Load bookmarks saved by a previous session.
    pub fn init(&self) {
        let Some(storage) = self.storage.bookmark_storage() else {
            return;
        };
        match storage.load_from_file() {
            Ok(bookmarks) => self.bookmark_manager.borrow_mut().set_bookmarks(bookmarks),
            Err(error) => log::debug!("No bookmarks loaded: {error}"),
        }
    }

    /// Persist bookmarks before the process exits.
    pub fn before_shutdown(&self) {
        let Some(storage) = self.storage.bookmark_storage() else {
            return;
        };
        let bookmarks = self.bookmark_manager.borrow().bookmarks().clone();
        if let Err(error) = storage.save_to_file(&bookmarks) {
            log::error!("Failed to save bookmarks: {error}");
        }
    }

    /// IPC channel used to talk to a versoview controller, when one is connected.
    pub(crate) fn controller_sender(&self) -> Option<IpcSender<ToControllerMessage>> {
        self.to_controller_sender.clone()
    }

    /// Handle a message posted to the event loop by Verso itself.
    pub fn handle_event_loop_message(
        self: &Rc<Self>,
        event_loop: &ActiveEventLoop,
        message: EventLoopProxyMessage,
    ) {
        match message {
            EventLoopProxyMessage::CloseWindow(window_id) => {
                self.request_close_window(event_loop, window_id)
            },
            EventLoopProxyMessage::NewWindow => self.create_window(event_loop),
            _ => log::error!("Unexpected event loop message {message:?}"),
        }
    }

    /// Bookmarks as the JSON array `panel.html` and `bookmark.html` read.
    pub(crate) fn bookmarks_json(&self) -> String {
        serde_json::to_string(self.bookmark_manager.borrow().bookmarks()).unwrap_or_default()
    }

    /// Whether there is at least one bookmark, which shows the bookmark bar.
    pub(crate) fn has_bookmarks(&self) -> bool {
        !self.bookmark_manager.borrow().bookmarks().is_empty()
    }

    /// Bookmark `url`, or remove its bookmark when it already has one.
    pub(crate) fn toggle_bookmark(&self, name: String, url: String) {
        let mut manager = self.bookmark_manager.borrow_mut();
        if manager.bookmarks().iter().any(|bookmark| bookmark.url == url) {
            if let Ok(id) = BookmarkId::from_str(&url) {
                let _ = manager.remove_bookmark(id);
            }
        } else {
            manager.append_bookmark(name, url);
        }
    }

    /// Remove a bookmark by id.
    pub(crate) fn remove_bookmark(&self, id: &str) {
        let removed = BookmarkId::from_str(id)
            .map_err(|_| format!("Invalid bookmark ID: {id}"))
            .and_then(|id| self.bookmark_manager.borrow_mut().remove_bookmark(id));
        if let Err(error) = removed {
            log::error!("Failed to remove bookmark: {error}");
        }
    }

    /// Rename a bookmark by id.
    pub(crate) fn rename_bookmark(&self, id: &str, name: String) {
        let renamed = BookmarkId::from_str(id)
            .map_err(|_| format!("Invalid bookmark ID: {id}"))
            .and_then(|id| self.bookmark_manager.borrow_mut().rename_bookmark(id, name));
        if let Err(error) = renamed {
            log::error!("Failed to rename bookmark: {error}");
        }
    }

    /// Download states as the JSON object `downloads.html` reads.
    pub(crate) fn downloads_json(&self) -> String {
        serde_json::to_string(&*self.downloads.borrow()).unwrap_or_default()
    }

    /// Abort a running download.
    pub(crate) fn abort_download(&self, id: &str) {
        let Ok(id) = DownloadId::from_str(id) else {
            log::error!("Invalid download ID: {id}");
            return;
        };
        if let Some(download) = self.downloads.borrow_mut().get_mut(&id) {
            download.abort();
        }
    }

    /// Check in the background whether `url` is a file to download rather than a page.
    ///
    /// A download is started with the user's confirmation. Otherwise `follow_up` runs: the
    /// pending navigation is allowed, or the URL is loaded into the tab.
    pub(crate) fn check_download(&self, url: url::Url, follow_up: DownloadCheckFollowUp) {
        let client = self.http_client.clone();
        let sender = self.internal_sender.clone();
        tokio::spawn(async move {
            let (should_download, response) = check_should_download(&client, &url).await;
            let download = should_download.then_some(response).flatten();
            let _ = sender.send(VersoInternalMsg::DownloadChecked(
                follow_up,
                url.clone(),
                download.is_none(),
            ));
            if let Some(response) = download {
                download_body(url, response, sender).await;
            }
        });
    }

    /// Handle a message sent from a Verso worker thread.
    pub fn handle_verso_internal_message(&self, message: VersoInternalMsg) {
        match message {
            VersoInternalMsg::DownloadChecked(follow_up, url, proceed) => {
                for window in self.windows.borrow_mut().values_mut() {
                    window.finish_download_check(&follow_up, &url, proceed);
                }
            },
            VersoInternalMsg::CreateDownload(download) => {
                self.downloads
                    .borrow_mut()
                    .insert(download.id().clone(), download);
                for window in self.windows.borrow().values() {
                    window.show_download_button();
                }
            }
            VersoInternalMsg::UpdateDownload(id, state) => {
                let mut downloads = self.downloads.borrow_mut();
                if let Some(download) = downloads.get_mut(&id) {
                    if let Some(status) = state.status {
                        download.status = status;
                    }
                    if let Some(progress) = state.progress {
                        download.progress = progress;
                    }
                    if let Some(stopped) = state.stopped {
                        download.stopped = stopped;
                    }
                }
            }
        }
    }
}

/// Parse the command line arguments,
/// if `ipc_channel` is set, we try to connect to it and set up routing to the event loop proxy
/// then return the config from [`ToVersoMessage::SetConfig`] or fallback to from the command line arguments
fn try_connect_ipc_and_get_config(
    proxy: &EventLoopProxy<EventLoopProxyMessage>,
) -> (Config, Option<IpcSender<ToControllerMessage>>) {
    let cli_args = parse_cli_args().unwrap_or_default();
    let (to_controller_sender, initial_settings) = if let Some(ipc_channel) = &cli_args.ipc_channel
    {
        let sender = IpcSender::<ToControllerMessage>::connect(ipc_channel.to_string()).unwrap();
        let (to_verso_sender, receiver) = ipc::channel::<ToVersoMessage>().unwrap();
        sender
            .send(ToControllerMessage::SetToVersoSender(to_verso_sender))
            .unwrap();
        let ToVersoMessage::SetConfig(initial_settings) = receiver
            .recv()
            .expect("Failed to recieve the initial settings from controller")
        else {
            panic!("The initial message sent from versoview is not a `ToVersoMessage::SetConfig`")
        };
        let proxy_clone = EventLoopProxyDropGuard(proxy.clone());
        ROUTER.add_typed_route(
            receiver,
            Box::new(move |message| match message {
                Ok(message) => {
                    if let Err(e) = proxy_clone
                        .0
                        .send_event(EventLoopProxyMessage::IpcMessage(Box::new(message)))
                    {
                        log::error!("Failed to send controller message to Verso: {e}");
                    }
                }
                Err(e) => log::error!("Failed to receive controller message: {e}"),
            }),
        );
        (Some(sender), Some(initial_settings))
    } else {
        (None, None)
    };
    let config = if let Some(initial_settings) = initial_settings {
        Config::from_controller_config(initial_settings)
    } else {
        Config::from_cli_args(cli_args)
    };
    (config, to_controller_sender)
}

/// Signal the event loop to exit when dropped,
/// this is used for when we lose the connection with our host controller
struct EventLoopProxyDropGuard(EventLoopProxy<EventLoopProxyMessage>);

impl Drop for EventLoopProxyDropGuard {
    fn drop(&mut self) {
        if let Err(error) = self
            .0
            .send_event(EventLoopProxyMessage::IpcMessage(Box::new(
                ToVersoMessage::Exit,
            )))
        {
            log::error!("Failed to send exit message to event loop on IPC disconnect: {error}");
        }
    }
}

/// Message send to the event loop
#[derive(Debug)]
pub enum EventLoopProxyMessage {
    /// Wake
    Wake,
    /// Message coming from the webview controller
    IpcMessage(Box<ToVersoMessage>),
    /// Message coming from a Verso worker, such as the download task
    VersoInternalMessage(VersoInternalMsg),
    /// Close a window, for example from the panel's close button or after its last tab closed
    CloseWindow(WindowId),
    /// Open a new window, from the panel's new-window button
    NewWindow,
}

/// What to do once [`Verso::check_download`] decided a URL is not a download.
#[derive(Debug, Deserialize, Serialize)]
pub enum DownloadCheckFollowUp {
    /// Allow the navigation request held under this id.
    Navigation(uuid::Uuid),
    /// Load the URL into this tab.
    Load(WebViewId),
}

/// Message sent from Verso's own threads back to the event loop.
#[derive(Debug, Deserialize, Serialize)]
pub enum VersoInternalMsg {
    /// The download check for a URL finished. `true` when the URL is not a download and the
    /// follow-up should go ahead.
    DownloadChecked(DownloadCheckFollowUp, url::Url, bool),
    /// A download has been created.
    CreateDownload(DownloadItem),
    /// Progress or status for an existing download.
    UpdateDownload(DownloadId, UpdateDownloadState),
}

/// Route messages from Verso's worker threads to the event loop.
fn route_internal_messages(
    proxy: &EventLoopProxy<EventLoopProxyMessage>,
) -> IpcSender<VersoInternalMsg> {
    let (sender, receiver) = ipc::channel::<VersoInternalMsg>().unwrap();
    let proxy = proxy.clone();
    ROUTER.add_typed_route(
        receiver,
        Box::new(move |message| match message {
            Ok(message) => {
                if let Err(error) =
                    proxy.send_event(EventLoopProxyMessage::VersoInternalMessage(message))
                {
                    log::error!("Failed to send an internal message to Verso: {error}");
                }
            },
            Err(error) => log::error!("Failed to receive an internal message: {error}"),
        }),
    );
    sender
}

#[derive(Debug, Clone)]
struct Waker(pub EventLoopProxy<EventLoopProxyMessage>);

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        if let Err(e) = self.0.send_event(EventLoopProxyMessage::Wake) {
            log::error!("Servo failed to send wake up event to Verso: {e}");
        }
    }
}

