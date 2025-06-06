//! Window startup notification to handle window raising.
//!
//! The [`ActivationToken`] is essential to ensure that your newly
//! created window will obtain the focus, otherwise the user could
//! be requered to click on the window.
//!
//! Such token is usually delivered via the environment variable and
//! could be read from it with the [`EventLoopExtStartupNotify::read_token_from_env`].
//!
//! Such token must also be reset after reading it from your environment with
//! [`reset_activation_token_env`] otherwise child processes could inherit it.
//!
//! When starting a new child process with a newly obtained [`ActivationToken`] from
//! [`WindowExtStartupNotify::request_activation_token`] the [`set_activation_token_env`]
//! must be used to propagate it to the child
//!
//! To ensure the delivery of such token by other processes to you, the user should
//! set `StartupNotify=true` inside the `.desktop` file of their application.
//!
//! The specification could be found [`here`].
//!
//! [`here`]: https://specifications.freedesktop.org/startup-notification-spec/startup-notification-latest.txt

use std::env;

use crate::error::{NotSupportedError, RequestError};
use crate::event_loop::{ActiveEventLoop, AsyncRequestSerial};
#[cfg(wayland_platform)]
use crate::platform::wayland::ActiveEventLoopExtWayland;
use crate::window::{ActivationToken, Window};

/// The variable which is used mostly on X11.
const X11_VAR: &str = "DESKTOP_STARTUP_ID";

/// The variable which is used mostly on Wayland.
const WAYLAND_VAR: &str = "XDG_ACTIVATION_TOKEN";

pub trait EventLoopExtStartupNotify {
    /// Read the token from the environment.
    ///
    /// It's recommended **to unset** this environment variable for child processes.
    fn read_token_from_env(&self) -> Option<ActivationToken>;
}

pub trait WindowExtStartupNotify {
    /// Request a new activation token.
    ///
    /// The token will be delivered inside
    fn request_activation_token(&self) -> Result<AsyncRequestSerial, RequestError>;
}

pub trait WindowAttributesExtStartupNotify {
    /// Use this [`ActivationToken`] during window creation.
    ///
    /// Not using such a token upon a window could make your window not gaining
    /// focus until the user clicks on the window.
    fn with_activation_token(self, token: ActivationToken) -> Self;
}

impl EventLoopExtStartupNotify for dyn ActiveEventLoop + '_ {
    fn read_token_from_env(&self) -> Option<ActivationToken> {
        #[cfg(x11_platform)]
        let _is_wayland = false;
        #[cfg(wayland_platform)]
        let _is_wayland = self.is_wayland();

        if _is_wayland {
            env::var(WAYLAND_VAR).ok().map(ActivationToken::from_raw)
        } else {
            env::var(X11_VAR).ok().map(ActivationToken::from_raw)
        }
    }
}

impl WindowExtStartupNotify for dyn Window + '_ {
    fn request_activation_token(&self) -> Result<AsyncRequestSerial, RequestError> {
        #[cfg(wayland_platform)]
        if let Some(window) = self.cast_ref::<crate::platform_impl::wayland::Window>() {
            return window.request_activation_token();
        }

        #[cfg(x11_platform)]
        if let Some(window) = self.cast_ref::<crate::platform_impl::x11::Window>() {
            return window.request_activation_token();
        }

        Err(NotSupportedError::new("startup notify is not supported").into())
    }
}

/// Remove the activation environment variables from the current process.
///
/// This is wise to do before running child processes,
/// which may not to support the activation token.
pub fn reset_activation_token_env() {
    env::remove_var(X11_VAR);
    env::remove_var(WAYLAND_VAR);
}

/// Set environment variables responsible for activation token.
///
/// This could be used before running daemon processes.
pub fn set_activation_token_env(token: ActivationToken) {
    let token = token.into_raw();
    env::set_var(X11_VAR, &token);
    env::set_var(WAYLAND_VAR, token);
}
