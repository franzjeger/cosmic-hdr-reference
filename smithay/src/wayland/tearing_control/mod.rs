//! Implementation of the `wp_tearing_control_v1` protocol.
//!
//! Clients use it to declare, per surface, whether presentation may tear
//! (asynchronous page flips) instead of waiting for vertical sync. The hint is
//! double-buffered surface state; honoring it is entirely up to the
//! compositor, which typically requires the surface to be the fullscreen
//! scanout candidate and the backend to support async flips.

use std::sync::{
    Mutex,
    atomic::{self, AtomicBool},
};

use wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::WpTearingControlManagerV1, wp_tearing_control_v1::WpTearingControlV1,
};
use wayland_server::{
    Dispatch, DisplayHandle, GlobalDispatch, Resource, Weak, backend::GlobalId,
    protocol::wl_surface::WlSurface,
};

use super::compositor::Cacheable;

use crate::wayland::GlobalData;

mod dispatch;

/// Double-buffered tearing preference of a surface.
#[derive(Debug, Clone, Copy, Default)]
pub struct TearingControlSurfaceCachedState {
    prefer_async: bool,
}

impl TearingControlSurfaceCachedState {
    /// Whether the client prefers tearing (async) presentation for this
    /// surface. `false` is the protocol's `vsync` hint and the default.
    pub fn prefer_async(&self) -> bool {
        self.prefer_async
    }
}

impl Cacheable for TearingControlSurfaceCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Reads the committed tearing preference from already borrowed surface
/// state, e.g. inside surface-tree traversal callbacks where locking the
/// surface again would deadlock.
pub fn prefer_async_from_states(states: &super::compositor::SurfaceData) -> bool {
    states
        .cached_state
        .get::<TearingControlSurfaceCachedState>()
        .current()
        .prefer_async()
}

#[derive(Debug)]
struct TearingControlSurfaceData {
    is_resource_attached: AtomicBool,
}

impl TearingControlSurfaceData {
    fn new() -> Self {
        Self {
            is_resource_attached: AtomicBool::new(false),
        }
    }

    fn set_is_resource_attached(&self, is_attached: bool) {
        self.is_resource_attached
            .store(is_attached, atomic::Ordering::Release)
    }

    fn is_resource_attached(&self) -> bool {
        self.is_resource_attached.load(atomic::Ordering::Acquire)
    }
}

/// User data of [`WpTearingControlV1`] objects.
#[derive(Debug)]
pub struct TearingControlUserData(Mutex<Weak<WlSurface>>);

impl TearingControlUserData {
    fn new(surface: WlSurface) -> Self {
        Self(Mutex::new(surface.downgrade()))
    }

    fn wl_surface(&self) -> Option<WlSurface> {
        self.0.lock().unwrap().upgrade().ok()
    }
}

/// Delegate type for the [`WpTearingControlManagerV1`] global.
#[derive(Debug)]
pub struct TearingControlState {
    global: GlobalId,
}

impl TearingControlState {
    /// Registers a new [`WpTearingControlManagerV1`] global.
    pub fn new<D>(display: &DisplayHandle) -> TearingControlState
    where
        D: GlobalDispatch<WpTearingControlManagerV1, GlobalData>
            + Dispatch<WpTearingControlManagerV1, GlobalData>
            + Dispatch<WpTearingControlV1, TearingControlUserData>
            + 'static,
    {
        let global = display.create_global::<D, WpTearingControlManagerV1, _>(1, GlobalData);

        TearingControlState { global }
    }

    /// Returns the [`WpTearingControlManagerV1`] global id.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}
