//! The state of the window, which is shared with the event-loop.

use std::sync::{Arc, Mutex, Weak};

use dpi::Position;
use sctk::compositor::{CompositorState, Region, SurfaceData, SurfaceDataExt};
use sctk::reexports::client::protocol::wl_shm::WlShm;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use sctk::reexports::csd_frame::{
    DecorationsFrame, FrameAction, FrameClick, ResizeEdge, WindowState as XdgWindowState,
};
use sctk::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use sctk::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use sctk::seat::pointer::{PointerDataExt, ThemedPointer};
use sctk::shell::xdg::window::WindowConfigure;
use sctk::shm::slot::SlotPool;
use tracing::warn;
use wayland_client::protocol::wl_subsurface::WlSubsurface;

use crate::cursor::CustomCursor as RootCustomCursor;
use crate::dpi::{LogicalPosition, LogicalSize, PhysicalSize, Size};
use crate::error::{NotSupportedError, RequestError};
use crate::platform_impl::wayland::logical_to_physical_rounded;
use crate::platform_impl::wayland::seat::{
    PointerConstraintsState, WinitPointerData, WinitPointerDataExt,
};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::types::cursor::{CustomCursor, SelectedCursor};
use crate::platform_impl::wayland::window::state::{FrameCallbackState, GrabState};
use crate::platform_impl::PlatformCustomCursor;
use crate::window::{CursorGrabMode, CursorIcon};

// Minimum window surface size.
const MIN_WINDOW_SIZE: LogicalSize<u32> = LogicalSize::new(2, 1);

/// The state of the window which is being updated from the [`WinitState`].
pub struct SubsurfaceState {
    /// The connection to Wayland server.
    pub connection: Connection,

    /// The `Shm` to set cursor.
    pub shm: WlShm,

    // A shared pool where to allocate custom cursors.
    custom_cursor_pool: Arc<Mutex<SlotPool>>,

    /// The last received configure.
    pub last_configure: Option<WindowConfigure>,

    /// The pointers observed on the window.
    pub pointers: Vec<Weak<ThemedPointer<WinitPointerData>>>,

    selected_cursor: SelectedCursor,

    /// Whether the cursor is visible.
    pub cursor_visible: bool,

    /// Pointer constraints to lock/confine pointer.
    pub pointer_constraints: Option<Arc<PointerConstraintsState>>,

    /// Queue handle.
    pub queue_handle: QueueHandle<WinitState>,

    /// Whether the window is transparent.
    transparent: bool,

    /// The state of the compositor to create WlRegions.
    compositor: Arc<CompositorState>,

    /// The current cursor grabbing mode.
    cursor_grab_mode: GrabState,

    /// The position of the window.
    position: LogicalPosition<i32>,

    /// The surface size of the window.
    size: LogicalSize<u32>,

    /// The scale factor of the window.
    scale_factor: f64,

    /// The state of the frame callback.
    frame_callback_state: FrameCallbackState,

    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,

    /// The underlying SCTK window.
    pub surface: WlSurface,
    pub subsurface: WlSubsurface,
}

impl SubsurfaceState {
    pub fn new(
        connection: Connection,
        queue_handle: &QueueHandle<WinitState>,
        winit_state: &WinitState,
        initial_position: Position,
        initial_size: Size,
        surface: WlSurface,
        subsurface: WlSubsurface
    ) -> Self {
        let compositor = winit_state.compositor_state.clone();
        let pointer_constraints = winit_state.pointer_constraints.clone();
        let viewport = winit_state
            .viewporter_state
            .as_ref()
            .map(|state| state.get_viewport(&surface, queue_handle));
        let fractional_scale = winit_state
            .fractional_scaling_manager
            .as_ref()
            .map(|fsm| fsm.fractional_scaling(&surface, queue_handle));

            Self {
                compositor,
                connection,
                cursor_grab_mode: GrabState::new(),
                selected_cursor: Default::default(),
                cursor_visible: true,
                fractional_scale,
                frame_callback_state: FrameCallbackState::None,
                last_configure: None,
                pointer_constraints,
                pointers: Default::default(),
                queue_handle: queue_handle.clone(),
                scale_factor: 1.,
                shm: winit_state.shm.wl_shm().clone(),
                custom_cursor_pool: winit_state.custom_cursor_pool.clone(),
                position: initial_position.to_logical(1.),
                size: initial_size.to_logical(1.),
                transparent: false,
                surface,
                subsurface,
                viewport
            }
    }

    /// Apply closure on the given pointer.
    fn apply_on_pointer<F: Fn(&ThemedPointer<WinitPointerData>, &WinitPointerData)>(
        &self,
        callback: F,
    ) {
        self.pointers.iter().filter_map(Weak::upgrade).for_each(|pointer| {
            let data = pointer.pointer().winit_data();
            callback(pointer.as_ref(), data);
        })
    }

    /// Get the current state of the frame callback.
    pub(crate) fn frame_callback_state(&self) -> FrameCallbackState {
        self.frame_callback_state
    }

    /// The frame callback was received, but not yet sent to the user.
    pub(crate) fn frame_callback_received(&mut self) {
        self.frame_callback_state = FrameCallbackState::Received;
    }

    /// Reset the frame callbacks state.
    pub(crate) fn frame_callback_reset(&mut self) {
        self.frame_callback_state = FrameCallbackState::None;
    }

    /// Request a frame callback if we don't have one for this window in flight.
    pub(crate) fn request_frame_callback(&mut self) {
        match self.frame_callback_state {
            FrameCallbackState::None | FrameCallbackState::Received => {
                self.frame_callback_state = FrameCallbackState::Requested;
                self.surface.frame(&self.queue_handle, self.surface.clone());
            },
            FrameCallbackState::Requested => (),
        }
    }
    
    /// Get the size of the window.
    #[inline]
    pub fn surface_size(&self) -> LogicalSize<u32> {
        self.size
    }

    /// Try to resize the window when the user can do so.
    pub fn request_surface_size(&mut self, surface_size: Size) -> PhysicalSize<u32> {
        self.resize(surface_size.to_logical(self.scale_factor()));
        logical_to_physical_rounded(self.surface_size(), self.scale_factor())
    }

    pub fn position(&self) -> LogicalPosition<i32> {
        self.position
    }

    pub fn set_position(&self, position: Position) {
        let pos = position.to_physical(self.scale_factor());
        self.subsurface.set_position(pos.x, pos.y);
    }

    
    /// Reissue the transparency hint to the compositor.
    pub fn reload_transparency_hint(&self) {
        let surface = &self.surface;

        if self.transparent {
            surface.set_opaque_region(None);
        } else if let Ok(region) = Region::new(&*self.compositor) {
            region.add(0, 0, i32::MAX, i32::MAX);
            surface.set_opaque_region(Some(region.wl_region()));
        } else {
            warn!("Failed to mark window opaque.");
        }
    }

    /// Resize the window to the new surface size.
    fn resize(&mut self, surface_size: LogicalSize<u32>) {
        self.size = surface_size;

        // Reload the hint.
        self.reload_transparency_hint();

        // Update the target viewport, this is used if and only if fractional scaling is in use.
        if let Some(viewport) = self.viewport.as_ref() {
            // Set surface size without the borders.
            viewport.set_destination(self.size.width as _, self.size.height as _);
        }
    }

    /// Get the scale factor of the window.
    #[inline]
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }


    /// Set the scale factor for the given window.
    #[inline]
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.scale_factor = scale_factor;

        // NOTE: When fractional scaling is not used update the buffer scale.
        if self.fractional_scale.is_none() {
            let _ = self.surface.set_buffer_scale(self.scale_factor as _);
        }

        
    }

    /// Mark the window as transparent.
    #[inline]
    pub fn set_transparent(&mut self, transparent: bool) {
        self.transparent = transparent;
        self.reload_transparency_hint();
    }

    pub fn set_cursor(&mut self, cursor_icon: CursorIcon) {
        self.selected_cursor = SelectedCursor::Named(cursor_icon);

        if !self.cursor_visible {
            return;
        }

        self.apply_on_pointer(|pointer, _| {
            if pointer.set_cursor(&self.connection, cursor_icon).is_err() {
                warn!("Failed to set cursor to {:?}", cursor_icon);
            }
        })
    }

    /// Set the custom cursor icon.
    pub(crate) fn set_custom_cursor(&mut self, cursor: RootCustomCursor) {
        let cursor = match cursor {
            RootCustomCursor { inner: PlatformCustomCursor::Wayland(cursor) } => cursor.0,
            #[cfg(x11_platform)]
            RootCustomCursor { inner: PlatformCustomCursor::X(_) } => {
                tracing::error!("passed a X11 cursor to Wayland backend");
                return;
            },
        };

        let cursor = {
            let mut pool = self.custom_cursor_pool.lock().unwrap();
            CustomCursor::new(&mut pool, &cursor)
        };

        if self.cursor_visible {
            self.apply_custom_cursor(&cursor);
        }

        self.selected_cursor = SelectedCursor::Custom(cursor);
    }

    fn apply_custom_cursor(&self, cursor: &CustomCursor) {
        self.apply_on_pointer(|pointer, _| {
            let surface = pointer.surface();

            let scale = surface.data::<SurfaceData>().unwrap().surface_data().scale_factor();

            surface.set_buffer_scale(scale);
            surface.attach(Some(cursor.buffer.wl_buffer()), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, cursor.w, cursor.h);
            } else {
                surface.damage(0, 0, cursor.w / scale, cursor.h / scale);
            }
            surface.commit();

            let serial = pointer
                .pointer()
                .data::<WinitPointerData>()
                .and_then(|data| data.pointer_data().latest_enter_serial())
                .unwrap();

            pointer.pointer().set_cursor(
                serial,
                Some(surface),
                cursor.hotspot_x / scale,
                cursor.hotspot_y / scale,
            );
        });
    }

    /// Set the cursor grabbing state on the top-level.
    pub fn set_cursor_grab(&mut self, mode: CursorGrabMode) -> Result<(), RequestError> {
        if self.cursor_grab_mode.user_grab_mode == mode {
            return Ok(());
        }

        self.set_cursor_grab_inner(mode)?;
        // Update user grab on success.
        self.cursor_grab_mode.user_grab_mode = mode;
        Ok(())
    }

    /// Set the grabbing state on the surface.
    fn set_cursor_grab_inner(&mut self, mode: CursorGrabMode) -> Result<(), RequestError> {
        let pointer_constraints = match self.pointer_constraints.as_ref() {
            Some(pointer_constraints) => pointer_constraints,
            None if mode == CursorGrabMode::None => return Ok(()),
            None => {
                return Err(
                    NotSupportedError::new("zwp_pointer_constraints is not available").into()
                )
            },
        };

        // Replace the current mode.
        let old_mode = std::mem::replace(&mut self.cursor_grab_mode.current_grab_mode, mode);

        match old_mode {
            CursorGrabMode::None => (),
            CursorGrabMode::Confined => self.apply_on_pointer(|_, data| {
                data.unconfine_pointer();
            }),
            CursorGrabMode::Locked => {
                self.apply_on_pointer(|_, data| data.unlock_pointer());
            },
        }

        let surface = &self.surface;
        match mode {
            CursorGrabMode::Locked => self.apply_on_pointer(|pointer, data| {
                let pointer = pointer.pointer();
                data.lock_pointer(pointer_constraints, surface, pointer, &self.queue_handle)
            }),
            CursorGrabMode::Confined => self.apply_on_pointer(|pointer, data| {
                let pointer = pointer.pointer();
                data.confine_pointer(pointer_constraints, surface, pointer, &self.queue_handle)
            }),
            CursorGrabMode::None => {
                // Current lock/confine was already removed.
            },
        }

        Ok(())
    }

    /// Set the visibility state of the cursor.
    pub fn set_cursor_visible(&mut self, cursor_visible: bool) {
        self.cursor_visible = cursor_visible;

        if self.cursor_visible {
            match &self.selected_cursor {
                SelectedCursor::Named(icon) => self.set_cursor(*icon),
                SelectedCursor::Custom(cursor) => self.apply_custom_cursor(cursor),
            }
        } else {
            for pointer in self.pointers.iter().filter_map(|pointer| pointer.upgrade()) {
                let latest_enter_serial = pointer.pointer().winit_data().latest_enter_serial();

                pointer.pointer().set_cursor(latest_enter_serial, None, 0, 0);
            }
        }
    }

    /// Set the position of the cursor.
    pub fn set_cursor_position(&self, position: LogicalPosition<f64>) -> Result<(), RequestError> {
        if self.pointer_constraints.is_none() {
            return Err(NotSupportedError::new("zwp_pointer_constraints is not available").into());
        }

        // Position can be set only for locked cursor.
        if self.cursor_grab_mode.current_grab_mode != CursorGrabMode::Locked {
            return Err(NotSupportedError::new(
                "cursor position could only be changed for locked pointer",
            )
            .into());
        }

        self.apply_on_pointer(|_, data| {
            data.set_locked_cursor_position(position.x, position.y);
        });

        Ok(())
    }
}