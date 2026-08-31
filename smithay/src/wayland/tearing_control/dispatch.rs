use wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
    wp_tearing_control_v1::{self, WpTearingControlV1},
};

use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, New, Resource, WEnum, backend::ClientId};

use super::{TearingControlSurfaceCachedState, TearingControlSurfaceData, TearingControlUserData};
use crate::wayland::{Dispatch2, GlobalData, GlobalDispatch2, compositor};

impl<D> GlobalDispatch2<WpTearingControlManagerV1, D> for GlobalData
where
    D: Dispatch<WpTearingControlManagerV1, GlobalData>,
    D: 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpTearingControlManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl<D> Dispatch2<WpTearingControlManagerV1, D> for GlobalData
where
    D: Dispatch<WpTearingControlV1, TearingControlUserData>,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _: &Client,
        manager: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } => {
                let already_taken = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(TearingControlSurfaceData::new);
                    let data = states.data_map.get::<TearingControlSurfaceData>().unwrap();

                    let already_taken = data.is_resource_attached();

                    if !already_taken {
                        data.set_is_resource_attached(true);
                    }

                    already_taken
                });

                if already_taken {
                    manager.post_error(
                        wp_tearing_control_manager_v1::Error::TearingControlExists,
                        "wl_surface already has a tearing control object attached",
                    )
                } else {
                    data_init.init(id, TearingControlUserData::new(surface));
                }
            }

            wp_tearing_control_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<WpTearingControlV1, D> for TearingControlUserData {
    fn request(
        &self,
        _state: &mut D,
        _: &Client,
        obj: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        _dh: &DisplayHandle,
        _: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_tearing_control_v1::Request::SetPresentationHint { hint } => {
                let Some(surface) = self.wl_surface() else {
                    // The surface is gone; the hint has nothing to apply to.
                    return;
                };

                let prefer_async =
                    matches!(hint, WEnum::Value(wp_tearing_control_v1::PresentationHint::Async));
                compositor::with_states(&surface, |states| {
                    states
                        .cached_state
                        .get::<TearingControlSurfaceCachedState>()
                        .pending()
                        .prefer_async = prefer_async;
                });
                let _ = obj;
            }
            // "Switch back to vsync" per the protocol: destroying the object
            // resets the double-buffered hint.
            wp_tearing_control_v1::Request::Destroy => {
                let Some(surface) = self.wl_surface() else {
                    return;
                };

                compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<TearingControlSurfaceData>()
                        .unwrap()
                        .set_is_resource_attached(false);

                    states
                        .cached_state
                        .get::<TearingControlSurfaceCachedState>()
                        .pending()
                        .prefer_async = false;
                });
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, _state: &mut D, _client: ClientId, _object: &WpTearingControlV1) {
        // Graceful destroy is handled above; client teardown cleans up the
        // surface data with the surface itself.
    }
}
