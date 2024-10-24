use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use sctk::compositor::{CompositorState, Region, SurfaceData};
use sctk::reexports::client::protocol::wl_display::WlDisplay;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use sctk::shell::xdg::window::{self, Window as SctkWindow, WindowDecorations};
use sctk::shell::WaylandSurface;
use tracing::warn;
use wayland_client::protocol::wl_subsurface::WlSubsurface;

use crate::platform_impl::wayland::{self as winit_wayland, Window};

use super::{ActiveEventLoop, WindowRequests};
use crate::dpi::{LogicalSize, PhysicalPosition, PhysicalSize, Position, Size};
use crate::error::{NotSupportedError, OsError, RequestError};
use crate::event::{Ime, SurfaceEvent};
use crate::event_loop::AsyncRequestSerial;
use crate::monitor::MonitorHandle as CoreMonitorHandle;
use crate::platform_impl::{Fullscreen, MonitorHandle as PlatformMonitorHandle};
use crate::window::{
    Cursor, CursorGrabMode, Fullscreen as CoreFullscreen, ImePurpose, ResizeDirection,
    SubsurfaceAttributes, Surface as CoreSurface, SurfaceId, Theme, UserAttentionType,
    Window as CoreWindow, WindowAttributes, WindowButtons, WindowLevel,
};
use winit_wayland::event_loop::sink::EventSink;
use winit_wayland::output::MonitorHandle;
use winit_wayland::state::WinitState;

pub(crate) mod state;

pub use state::SubsurfaceState;

/// A subsurface.
pub struct Subsurface {
    /// Reference to the underlying subsurface.
    subsurface: WlSubsurface,
    surface: WlSurface,

    /// Window id.
    surface_id: SurfaceId,

    /// The state of the window.
    subsurface_state: Arc<Mutex<SubsurfaceState>>,

    /// Compositor to handle WlRegion stuff.
    compositor: Arc<CompositorState>,

    /// The wayland display used solely for raw window handle.
    #[allow(dead_code)]
    display: WlDisplay,

    /// Handle to the main queue to perform requests.
    _queue_handle: QueueHandle<WinitState>,

    /// Window requests to the event loop.
    window_requests: Arc<WindowRequests>,

    /// Observed monitors.
    monitors: Arc<Mutex<Vec<MonitorHandle>>>,

    /// Source to wake-up the event-loop for window requests.
    event_loop_awakener: calloop::ping::Ping,

    /// The event sink to deliver synthetic events.
    _window_events_sink: Arc<Mutex<EventSink>>,
}

impl Subsurface {
    pub(crate) fn new(
        event_loop: &ActiveEventLoop,
        parent: &dyn CoreSurface,
        attributes: SubsurfaceAttributes,
    ) -> Result<Self, RequestError> {
        let queue_handle = event_loop.queue_handle.clone();
        let mut state = event_loop.state.borrow_mut();

        let monitors = state.monitors.clone();

        let compositor = state.compositor_state.clone();
        let subcompositor = state
            .subcompositor_state
            .as_ref()
            .ok_or(os_error!("wl_subcompositor not available"))?;

        let display = event_loop.connection.display();

        let size: Size = attributes.surface_size.unwrap_or(LogicalSize::new(200., 200.).into());

        let parent_surface: WlSurface = {
            let any: &dyn Any = parent.as_any();

            if let Some(window) = any.downcast_ref::<Window>() {
                window.surface().clone()
            } else if let Some(subsurface) = any.downcast_ref::<Subsurface>() {
                subsurface.surface().clone()
            } else {
                unreachable!()
            }
        };

        let (subsurface, surface) = subcompositor.create_subsurface(parent_surface, &queue_handle);

        let mut subsurface_state = SubsurfaceState::new(
            event_loop.connection.clone(),
            &event_loop.queue_handle,
            &state,
            size,
            surface.clone(),
            subsurface.clone(),
        );

        subsurface_state.set_transparent(attributes.transparent);

        match attributes.cursor {
            Cursor::Icon(icon) => subsurface_state.set_cursor(icon),
            Cursor::Custom(custom) => subsurface_state.set_custom_cursor(custom),
        }

        surface.commit();

        let subsurface_state = Arc::new(Mutex::new(subsurface_state));
        let surface_id = winit_wayland::make_wid(&surface);

        state.subsurfaces.get_mut().insert(surface_id, subsurface_state.clone());

        let window_requests = Arc::new(WindowRequests {
            closed: AtomicBool::new(false),
            redraw_requested: AtomicBool::new(true),
        });
        state.window_requests.get_mut().insert(surface_id, window_requests.clone());

        let window_events_sink = state.window_events_sink.clone();

        let mut wayland_source = event_loop.wayland_dispatcher.as_source_mut();
        let event_queue = wayland_source.queue();

        event_queue.roundtrip(&mut state).map_err(|err| os_error!(err))?;

        let event_loop_awakener = event_loop.event_loop_awakener.clone();
        event_loop_awakener.ping();

        Ok(Self {
            subsurface,
            surface,
            surface_id,
            subsurface_state,
            compositor,
            display,
            _queue_handle: queue_handle,
            window_requests,
            monitors,
            event_loop_awakener,
            _window_events_sink: window_events_sink,
        })
    }

    fn surface(&self) -> &WlSurface {
        &self.surface
    }
}

impl Drop for Subsurface {
    fn drop(&mut self) {
        self.window_requests.closed.store(true, Ordering::Relaxed);
        self.event_loop_awakener.ping();
    }
}

impl CoreSurface for Subsurface {
    fn id(&self) -> SurfaceId {
        self.surface_id
    }

    fn scale_factor(&self) -> f64 {
        self.subsurface_state.lock().unwrap().scale_factor()
    }

    fn request_redraw(&self) {
        // NOTE: try to not wake up the loop when the event was already scheduled and not yet
        // processed by the loop, because if at this point the value was `true` it could only
        // mean that the loop still haven't dispatched the value to the client and will do
        // eventually, resetting it to `false`.
        if self
            .window_requests
            .redraw_requested
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.event_loop_awakener.ping();
        }
    }

    fn pre_present_notify(&self) {
        self.subsurface_state.lock().unwrap().request_frame_callback();
    }

    fn surface_size(&self) -> PhysicalSize<u32> {
        let subsurface_state = self.subsurface_state.lock().unwrap();
        let scale_factor = subsurface_state.scale_factor();
        winit_wayland::logical_to_physical_rounded(subsurface_state.surface_size(), scale_factor)
    }

    fn request_surface_size(&self, size: Size) -> Option<PhysicalSize<u32>> {
        let new_size = self.subsurface_state.lock().unwrap().request_surface_size(size);
        self.request_redraw();

        Some(new_size)
    }

    fn set_transparent(&self, transparent: bool) {
        self.subsurface_state.lock().unwrap().set_transparent(transparent);
    }

    fn set_cursor(&self, cursor: Cursor) {
        let subsurface_state = &mut self.subsurface_state.lock().unwrap();

        match cursor {
            Cursor::Icon(icon) => subsurface_state.set_cursor(icon),
            Cursor::Custom(cursor) => subsurface_state.set_custom_cursor(cursor),
        }
    }

    fn set_cursor_position(&self, position: Position) -> Result<(), RequestError> {
        let scale_factor = self.scale_factor();
        let position = position.to_logical(scale_factor);
        self.subsurface_state
            .lock()
            .unwrap()
            .set_cursor_position(position)
            // Request redraw on success, since the state is double buffered.
            .map(|_| self.request_redraw())
    }

    fn set_cursor_grab(&self, mode: CursorGrabMode) -> Result<(), RequestError> {
        self.subsurface_state.lock().unwrap().set_cursor_grab(mode)
    }

    fn set_cursor_visible(&self, visible: bool) {
        self.subsurface_state.lock().unwrap().set_cursor_visible(visible);
    }

    fn set_cursor_hittest(&self, hittest: bool) -> Result<(), RequestError> {
        let surface = &self.surface;

        if hittest {
            surface.set_input_region(None);
            Ok(())
        } else {
            let region = Region::new(&*self.compositor).map_err(|err| os_error!(err))?;
            region.add(0, 0, 0, 0);
            surface.set_input_region(Some(region.wl_region()));
            Ok(())
        }
    }

    fn current_monitor(&self) -> Option<CoreMonitorHandle> {
        let data = self.surface.data::<SurfaceData>()?;
        data.outputs()
            .next()
            .map(MonitorHandle::new)
            .map(crate::platform_impl::MonitorHandle::Wayland)
            .map(|inner| CoreMonitorHandle { inner })
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(
            self.monitors
                .lock()
                .unwrap()
                .clone()
                .into_iter()
                .map(crate::platform_impl::MonitorHandle::Wayland)
                .map(|inner| CoreMonitorHandle { inner }),
        )
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        // NOTE: There's no such concept on Wayland.
        None
    }

    /// Get the raw-window-handle v0.6 display handle.
    #[cfg(feature = "rwh_06")]
    fn rwh_06_display_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }

    /// Get the raw-window-handle v0.6 window handle.
    #[cfg(feature = "rwh_06")]
    fn rwh_06_window_handle(&self) -> &dyn rwh_06::HasWindowHandle {
        self
    }
}


#[cfg(feature = "rwh_06")]
impl rwh_06::HasWindowHandle for Subsurface {
    fn window_handle(&self) -> Result<rwh_06::WindowHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::WaylandWindowHandle::new({
            let ptr = self.surface.id().as_ptr();
            std::ptr::NonNull::new(ptr as *mut _).expect("wl_surface will never be null")
        });

        unsafe { Ok(rwh_06::WindowHandle::borrow_raw(raw.into())) }
    }
}

#[cfg(feature = "rwh_06")]
impl rwh_06::HasDisplayHandle for Subsurface {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::WaylandDisplayHandle::new({
            let ptr = self.display.id().as_ptr();
            std::ptr::NonNull::new(ptr as *mut _).expect("wl_proxy should never be null")
        });

        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw.into())) }
    }
}
