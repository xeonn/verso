use std::{cell::RefCell, collections::HashMap, rc::Rc};

use ipc_channel::ipc::{self, IpcSender};
use ipc_channel::router::ROUTER;
use serde::{Deserialize, Serialize};
use servo::{
    AllowOrDenyRequest, EventLoopWaker, Servo, ServoBuilder, ServoDelegate, ServoError,
    UserContentManager, WebResourceResponse,
};
use versoview_messages::{PositionType, SizeType, ToControllerMessage, ToVersoMessage};
use winit::{
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    window::WindowId,
};

use crate::{
    bookmark::BookmarkManager,
    config::{Config, parse_cli_args, to_winit_theme, to_winit_window_level},
    download::{DownloadId, DownloadItem, UpdateDownloadState},
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
    ///
    /// Following threads will be created while initializing Verso based on configurations:
    /// - Time Profiler: Enabled
    /// - Memory Profiler: Enabled
    /// - DevTools: `pref!(devtools_server_enabled)`
    /// - Webrender: Enabled
    /// - WebGL: Disabled
    /// - WebXR: Disabled
    /// - Bluetooth: Enabled
    /// - Resource: Enabled
    /// - Storage: Enabled
    /// - Font Cache: Enabled
    /// - Canvas: Enabled
    /// - Constellation: Enabled
    /// - Image Cache: Enabled
    pub fn new(evl: &ActiveEventLoop, proxy: EventLoopProxy<EventLoopProxyMessage>) -> Rc<Self> {
        let (config, to_controller_sender) = try_connect_ipc_and_get_config(&proxy);

        // Initialize configurations and Verso window
        let protocols = config.create_protocols();
        let initial_url = config.url.clone();
        let with_panel = config.with_panel;
        let window_settings = config.window_attributes.clone();
        let user_scripts = config.user_scripts.clone();

        let mut window = Window::new(evl, window_settings);
        let event_loop_waker = Box::new(Waker(proxy));

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
        #[cfg(linux)]
        if let WindowEvent::Resized(_) = event {
            self.handle_winit_window_event(event_loop, window_id, event);
        } else {
            self.handle_winit_window_event(event_loop, window_id, event);
            self.handle_servo_messages(event_loop);
        }

        #[cfg(apple)]
        if let WindowEvent::RedrawRequested = event {
            let resizing = self.handle_winit_window_event(event_loop, window_id, event);
            if !resizing {
                self.handle_servo_messages(event_loop);
            }
        } else {
            self.handle_winit_window_event(event_loop, window_id, event);
            self.handle_servo_messages(event_loop);
        }

        #[cfg(windows)]
        {
            self.handle_winit_window_event(event_loop, window_id, event);
            self.handle_servo_messages(event_loop);
        }

        // self.handle_webdriver_messages();
    }

    /// Handle Winit window events
    fn handle_winit_window_event(
        self: &mut Rc<Self>,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) -> bool {
        log::trace!("Verso is handling Winit event: {event:?}");

        if let WindowEvent::CloseRequested = &event {
            let defer_to_controller = {
                let windows = self.windows.borrow();
                let Some(window) = windows.get(&window_id) else {
                    return false;
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
                self.close_all_windows();
                event_loop.exit();
            }
            return false;
        }

        let mut windows = self.windows.borrow_mut();
        let Some(window) = windows.get_mut(&window_id) else {
            return false;
        };
        window.handle_winit_window_event(self, &event);
        window.resizing
    }

    /// Handle message came from Servo.
    pub fn handle_servo_messages(self: &mut Rc<Self>, event_loop: &ActiveEventLoop) {
        self.servo.spin_event_loop();

        if self.is_animating() {
            event_loop.set_control_flow(ControlFlow::Poll);
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    // TODO: Implement this
    /// Handle webdriver messages
    // pub fn handle_webdriver_messages(&self) {
    //     let Some(webdriver_receiver) = &self.webdriver_receiver else {
    //         return;
    //     };

    //     while let Ok(msg) = webdriver_receiver.try_recv() {
    //         match msg {
    //             WebDriverCommandMsg::KeyboardAction(..)
    //             | WebDriverCommandMsg::MouseButtonAction(..)
    //             | WebDriverCommandMsg::MouseMoveAction(..)
    //             | WebDriverCommandMsg::WheelScrollAction(..)
    //             | WebDriverCommandMsg::ScriptCommand(..)
    //             | WebDriverCommandMsg::TakeScreenshot(..) => {
    //                 log::warn!(
    //                     "WebDriverCommand {msg:?} is still not moved from constellation to embedder"
    //                 );
    //             }
    //             _ => log::warn!("WebDriverCommand {msg:?} is not supported yet"),
    //         };
    //     }
    // }

    /// Request Verso to redraw. It will queue a redraw event on current focused window.
    pub fn request_redraw(self: &mut Rc<Self>, event_loop: &ActiveEventLoop) {
        // let Some(compositor) = &mut self.compositor else {
        //     return;
        // };

        // if let Some(window) = self.windows.get(&compositor.current_window) {
        //     // evl.set_control_flow(ControlFlow::Poll);
        //     window.request_redraw();

        //     // Wait for `request_redraw` to trigger the next event loop if the window is visible
        //     if window.window.is_visible().unwrap_or(true) {
        //         return;
        //     }
        // }

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
            ToVersoMessage::OnNavigationStartingResponse(id, allow) => {
                window.answer_navigation(&id, allow);
            }
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
                let tab = window.tab_manager.current_tab().unwrap();
                let url = tab
                    .webview()
                    .url()
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

    /// Return true if one of the Verso windows is animating.
    pub fn is_animating(&self) -> bool {
        self.windows
            .borrow()
            .values()
            .any(|window| window.is_animating())
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

    /// Handle a message sent from a Verso worker thread.
    pub fn handle_verso_internal_message(&self, message: VersoInternalMsg) {
        match message {
            VersoInternalMsg::CreateDownload(download) => {
                self.downloads
                    .borrow_mut()
                    .insert(download.id().clone(), download);
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
}

/// Message sent from Verso's own threads back to the event loop.
#[derive(Debug, Deserialize, Serialize)]
pub enum VersoInternalMsg {
    /// A download has been created.
    CreateDownload(DownloadItem),
    /// Progress or status for an existing download.
    UpdateDownload(DownloadId, UpdateDownloadState),
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

