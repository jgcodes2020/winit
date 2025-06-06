use std::cell::RefCell;
use std::{fmt, mem};

use winit_core::application::ApplicationHandler;

/// A helper type for storing a reference to `ApplicationHandler`, allowing interior mutable access
/// to it within the execution of a closure.
#[derive(Default)]
pub struct EventHandler {
    /// This can be in the following states:
    /// - Not registered by the event loop, or terminated (None).
    /// - Present (Some(handler)).
    /// - Currently executing the handler / in use (RefCell borrowed).
    inner: RefCell<Option<Box<dyn ApplicationHandler + 'static>>>,
}

impl fmt::Debug for EventHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.inner.try_borrow().as_deref() {
            Ok(Some(_)) => "<available>",
            Ok(None) => "<not set>",
            Err(_) => "<in use>",
        };
        f.debug_struct("EventHandler").field("state", &state).finish_non_exhaustive()
    }
}

impl EventHandler {
    pub fn new() -> Self {
        Self { inner: RefCell::new(None) }
    }

    /// Set the event loop handler for the duration of the given closure.
    ///
    /// This is similar to using the `scoped-tls` or `scoped-tls-hkt` crates
    /// to store the handler in a thread local, such that it can be accessed
    /// from within the closure.
    pub fn set<'handler, R>(
        &self,
        app: Box<dyn ApplicationHandler + 'handler>,
        closure: impl FnOnce() -> R,
    ) -> R {
        // SAFETY: We extend the lifetime of the handler here so that we can
        // store it in `EventHandler`'s `RefCell`.
        //
        // This is sound, since we make sure to unset the handler again at the
        // end of this function, and as such the lifetime isn't actually
        // extended beyond `'handler`.
        let handler = unsafe {
            mem::transmute::<
                Box<dyn ApplicationHandler + 'handler>,
                Box<dyn ApplicationHandler + 'static>,
            >(app)
        };

        match self.inner.try_borrow_mut().as_deref_mut() {
            Ok(Some(_)) => {
                unreachable!("tried to set handler while another was already set");
            },
            Ok(data @ None) => {
                *data = Some(handler);
            },
            Err(_) => {
                unreachable!("tried to set handler that is currently in use");
            },
        }

        struct ClearOnDrop<'a>(&'a EventHandler);

        impl Drop for ClearOnDrop<'_> {
            fn drop(&mut self) {
                match self.0.inner.try_borrow_mut().as_deref_mut() {
                    Ok(data @ Some(_)) => {
                        let handler = data.take();
                        // Explicitly `Drop` the application handler.
                        drop(handler);
                    },
                    Ok(None) => {
                        // Allowed, happens if the handler was cleared manually
                        // elsewhere (such as in `applicationWillTerminate:`).
                    },
                    Err(_) => {
                        // Note: This is not expected to ever happen, this
                        // module generally controls the `RefCell`, and
                        // prevents it from ever being borrowed outside of it.
                        //
                        // But if it _does_ happen, it is a serious error, and
                        // we must abort the process, it'd be unsound if we
                        // weren't able to unset the handler.
                        eprintln!("tried to clear handler that is currently in use");
                        std::process::abort();
                    },
                }
            }
        }

        let _clear_on_drop = ClearOnDrop(self);

        // Note: The RefCell should not be borrowed while executing the
        // closure, that'd defeat the whole point.
        closure()

        // `_clear_on_drop` will be dropped here, or when unwinding, ensuring
        // soundness.
    }

    pub fn in_use(&self) -> bool {
        self.inner.try_borrow().is_err()
    }

    pub fn ready(&self) -> bool {
        matches!(self.inner.try_borrow().as_deref(), Ok(Some(_)))
    }

    pub fn handle(&self, callback: impl FnOnce(&mut (dyn ApplicationHandler + '_))) {
        match self.inner.try_borrow_mut().as_deref_mut() {
            Ok(Some(ref mut user_app)) => {
                // It is important that we keep the reference borrowed here,
                // so that `in_use` can properly detect that the handler is
                // still in use.
                //
                // If the handler unwinds, the `RefMut` will ensure that the
                // handler is no longer borrowed.
                callback(&mut **user_app);
            },
            Ok(None) => {
                // `NSApplication`, our app state and this handler are all
                // global state and so it's not impossible that we could get
                // an event after the application has exited the `EventLoop`.
                tracing::error!("tried to run event handler, but no handler was set");
            },
            Err(_) => {
                // Prevent re-entrancy.
                panic!("tried to handle event while another event is currently being handled");
            },
        }
    }

    pub fn terminate(&self) {
        match self.inner.try_borrow_mut().as_deref_mut() {
            Ok(data @ Some(_)) => {
                let handler = data.take();
                // Explicitly `Drop` the application handler.
                drop(handler);
            },
            Ok(None) => {
                // When terminating, we expect the application handler to still be registered.
                tracing::error!("tried to clear handler, but no handler was set");
            },
            Err(_) => {
                panic!("tried to clear handler while an event is currently being handled");
            },
        }
    }
}
