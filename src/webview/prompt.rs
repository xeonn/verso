use std::rc::Rc;

use serde::Deserialize;
use servo::{
    AlertDialog, AuthenticationRequest, ConfirmDialog, EmbedderControlId,
    OffscreenRenderingContext, PermissionRequest, PromptDialog as ServoPromptDialog, WebView,
    WebViewId,
};
use url::Url;

/// Logical size of the prompt overlay. The prompt pages center a 300px wide dialog.
pub(crate) const PROMPT_LOGICAL_SIZE: (f64, f64) = (320.0, 200.0);

/// A request from web content that waits for the user to answer a prompt page.
pub(crate) enum PendingPrompt {
    /// `window.alert()`
    Alert(AlertDialog),
    /// `window.confirm()`
    Confirm(ConfirmDialog),
    /// `window.prompt()`
    Input(ServoPromptDialog),
    /// A permission request, answered with allow or deny
    Permission(PermissionRequest),
    /// An HTTP authentication request
    Authentication(AuthenticationRequest),
}

impl PendingPrompt {
    /// The prompt page that presents this request.
    pub(crate) fn resource_url(&self) -> Url {
        let (page, message, default_value) = match self {
            PendingPrompt::Alert(dialog) => ("alert.html", Some(dialog.message().to_owned()), None),
            PendingPrompt::Confirm(dialog) => {
                ("ok_cancel.html", Some(dialog.message().to_owned()), None)
            },
            PendingPrompt::Input(dialog) => (
                "prompt.html",
                Some(dialog.message().to_owned()),
                Some(dialog.current_value().to_owned()),
            ),
            PendingPrompt::Permission(request) => (
                "allow_deny.html",
                Some(format!("Allow this site to use {:?}?", request.feature())),
                None,
            ),
            PendingPrompt::Authentication(_) => ("http_basic_auth.html", None, None),
        };
        let mut url = Url::parse("verso://resources/components/prompt/")
            .and_then(|base| base.join(page))
            .expect("prompt page URL is valid");
        {
            let mut query = url.query_pairs_mut();
            if let Some(message) = message {
                query.append_pair("msg", &message);
            }
            if let Some(default_value) = default_value {
                query.append_pair("defaultValue", &default_value);
            }
        }
        url
    }

    /// Answer the request with the result the prompt page sent through `window.alert`.
    pub(crate) fn respond(self, result: &str) {
        match self {
            PendingPrompt::Alert(dialog) => dialog.confirm(),
            PendingPrompt::Confirm(dialog) => match result {
                "ok" => dialog.confirm(),
                _ => dialog.dismiss(),
            },
            PendingPrompt::Input(mut dialog) => match serde_json::from_str::<PromptInputResult>(result)
            {
                Ok(PromptInputResult { action, value }) if action == "ok" => {
                    dialog.set_current_value(&value);
                    dialog.confirm();
                },
                Ok(_) => dialog.dismiss(),
                Err(error) => {
                    log::error!("Invalid prompt result {result:?}: {error}");
                    dialog.dismiss();
                },
            },
            PendingPrompt::Permission(request) => match result {
                "allow" => request.allow(),
                _ => request.deny(),
            },
            PendingPrompt::Authentication(request) => {
                match serde_json::from_str::<HttpBasicAuthInputResult>(result) {
                    Ok(HttpBasicAuthInputResult { action, auth }) if action == "signin" => {
                        request.authenticate(auth.username, auth.password);
                    },
                    Ok(_) => {},
                    Err(error) => log::error!("Invalid authentication result {result:?}: {error}"),
                }
            },
        }
    }
}

/// A prompt page shown over the content area of a window, answering a request from a tab.
///
/// Dropping it without calling [`PendingPrompt::respond`] gives Servo its default answer.
pub(crate) struct Prompt {
    /// The webview that shows the prompt page
    pub(crate) webview: WebView,
    /// Framebuffer the prompt page paints into
    pub(crate) rendering_context: Rc<OffscreenRenderingContext>,
    /// The tab that asked for the prompt
    pub(crate) parent: WebViewId,
    /// The embedder control id of a simple dialog, so Servo can cancel it
    pub(crate) control_id: Option<EmbedderControlId>,
    /// The request to answer
    pub(crate) pending: Option<PendingPrompt>,
}

impl Prompt {
    /// Answer the pending request with the prompt page's result.
    pub(crate) fn respond(&mut self, result: &str) {
        if let Some(pending) = self.pending.take() {
            pending.respond(result);
        }
    }
}

/// Result sent by `prompt.html`: `{ action: "ok" | "cancel", value }`.
#[derive(Debug, Deserialize)]
struct PromptInputResult {
    action: String,
    value: String,
}

/// Result sent by `http_basic_auth.html`: `{ action: "signin" | "cancel", auth }`.
#[derive(Debug, Deserialize)]
struct HttpBasicAuthInputResult {
    action: String,
    auth: Credentials,
}

#[derive(Debug, Deserialize)]
struct Credentials {
    username: String,
    password: String,
}
