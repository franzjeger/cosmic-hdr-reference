use crate::wayland::compositor;
use crate::wayland::{Dispatch2, GlobalDispatch2};

use super::{
    ColorRepresentationHandler, ColorRepresentationSurfaceCachedState, ColorRepresentationSurfaceData,
    wp_color_representation_manager_v1, wp_color_representation_v1,
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, New, Resource, Weak, protocol::wl_surface::WlSurface,
};

impl<D> GlobalDispatch2<wp_color_representation_manager_v1::WpColorRepresentationManagerV1, D> for ()
where
    D: Dispatch<wp_color_representation_manager_v1::WpColorRepresentationManagerV1, ()>
        + ColorRepresentationHandler
        + 'static,
{
    fn bind(
        &self,
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<wp_color_representation_manager_v1::WpColorRepresentationManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let state = state.color_representation_state();
        let instance = data_init.init(resource, ());

        for code_point in &state.coefficients {
            instance.coefficients(*code_point);
        }
        for code_point in &state.chroma_locations {
            instance.chroma_location(*code_point);
        }

        state.known_instances.push(instance);
    }
}

impl<D> Dispatch2<wp_color_representation_manager_v1::WpColorRepresentationManagerV1, D> for ()
where
    D: Dispatch<wp_color_representation_v1::WpColorRepresentationV1, Weak<WlSurface>>
        + ColorRepresentationHandler
        + 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        resource: &wp_color_representation_manager_v1::WpColorRepresentationManagerV1,
        request: wp_color_representation_manager_v1::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        if let wp_color_representation_manager_v1::Request::Create {
            color_representation,
            surface,
        } = request
        {
            compositor::with_states(&surface, |states| {
                let data = states
                    .data_map
                    .get_or_insert_threadsafe(ColorRepresentationSurfaceData::new);

                if data.is_resource_attached() {
                    resource.post_error(
                        wp_color_representation_manager_v1::Error::AlreadyConstructed,
                        "Surface already has ColorRepresentation attached",
                    );
                    return;
                }

                // TODO: add pre_commit_hook to verify chroma_location / coefficient are valid for buffer pixel format

                let instance = data_init.init(color_representation, surface.downgrade());
                *data.instance.lock().unwrap() = Some(instance);
            });
        }
    }

    fn destroyed(
        &self,
        state: &mut D,
        _client: wayland_backend::server::ClientId,
        resource: &wp_color_representation_manager_v1::WpColorRepresentationManagerV1,
    ) {
        let state = state.color_representation_state();
        state.known_instances.retain(|i| i != resource);
    }
}

impl<D> Dispatch2<wp_color_representation_v1::WpColorRepresentationV1, D> for Weak<WlSurface>
where
    D: ColorRepresentationHandler + 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &wp_color_representation_v1::WpColorRepresentationV1,
        request: wp_color_representation_v1::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_color_representation_v1::Request::SetAlphaMode { alpha_mode } => {
                let wayland_server::WEnum::Value(alpha_mode) = alpha_mode else {
                    resource.post_error(
                        wp_color_representation_v1::Error::InvalidAlphaMode,
                        "Unknown alpha mode",
                    );
                    return;
                };

                let Ok(surface) = self.upgrade() else {
                    return;
                };

                compositor::with_states(&surface, |states| {
                    let mut guard = states
                        .cached_state
                        .get::<Option<ColorRepresentationSurfaceCachedState>>();
                    let representation = guard.pending();
                    if representation.is_none() {
                        *representation = Some(ColorRepresentationSurfaceCachedState::default());
                    }

                    representation.as_mut().unwrap().alpha_mode = Some(alpha_mode);
                });
            }
            wp_color_representation_v1::Request::SetChromaLocation { code_point } => {
                let state = state.color_representation_state();
                if !state.chroma_locations.contains(&code_point) {
                    resource.post_error(
                        wp_color_representation_v1::Error::UnsupportedChromaLocation,
                        "client send chroma location not advertised",
                    );
                    return;
                }

                let Ok(surface) = self.upgrade() else {
                    return;
                };

                compositor::with_states(&surface, |states| {
                    let mut guard = states
                        .cached_state
                        .get::<Option<ColorRepresentationSurfaceCachedState>>();
                    let representation = guard.pending();
                    if representation.is_none() {
                        *representation = Some(ColorRepresentationSurfaceCachedState::default());
                    }

                    representation.as_mut().unwrap().chroma_location = Some(code_point);
                });
            }
            wp_color_representation_v1::Request::SetCoefficients { code_point } => {
                let state = state.color_representation_state();
                if !state.coefficients.contains(&code_point) {
                    resource.post_error(
                        wp_color_representation_v1::Error::UnsupportedCoefficients,
                        "client send coefficient not advertised",
                    );
                    return;
                }

                let Ok(surface) = self.upgrade() else {
                    return;
                };

                compositor::with_states(&surface, |states| {
                    let mut guard = states
                        .cached_state
                        .get::<Option<ColorRepresentationSurfaceCachedState>>();
                    let representation = guard.pending();
                    if representation.is_none() {
                        *representation = Some(ColorRepresentationSurfaceCachedState::default());
                    }

                    representation.as_mut().unwrap().coefficient = Some(code_point);
                });
            }
            _ => {}
        }
    }

    fn destroyed(
        &self,
        _state: &mut D,
        _client: wayland_backend::server::ClientId,
        _resource: &wp_color_representation_v1::WpColorRepresentationV1,
    ) {
        let Ok(surface) = self.upgrade() else {
            return;
        };

        compositor::with_states(&surface, |states| {
            if let Some(data) = states.data_map.get::<ColorRepresentationSurfaceData>() {
                data.instance.lock().unwrap().take();
            }

            *states
                .cached_state
                .get::<Option<ColorRepresentationSurfaceCachedState>>()
                .pending() = None;
        });
    }
}
