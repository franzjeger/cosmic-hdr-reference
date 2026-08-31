// SPDX-License-Identifier: GPL-3.0-only

use crate::{backend::kms::drm_helpers::HdrOutputState, state::State};
use smithay::{
    desktop::utils::surface_primary_scanout_output,
    output::Output,
    reexports::{
        wayland_protocols::wp::color_management::v1::server::wp_image_description_info_v1::WpImageDescriptionInfoV1,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    wayland::{
        color::management::{
            ColorManagementHandler, ColorManagementState, ImageDescription, Primaries,
            PrimariesOption, TransferFunction, send_image_description_info,
        },
        compositor::{get_parent, with_states},
    },
};

fn description_for_output(output: &Output) -> ImageDescription {
    let Some(active) = output
        .user_data()
        .get::<HdrOutputState>()
        .and_then(HdrOutputState::get)
    else {
        return ImageDescription::SRGB;
    };

    let caps = active.capabilities;
    ImageDescription {
        transfer: TransferFunction::St2084Pq,
        primaries: PrimariesOption {
            named: Some(Primaries::Bt2020),
            values: None,
        },
        max_cll: Some(u32::from(caps.max_luminance)),
        max_fall: Some(u32::from(caps.max_frame_average_luminance)),
        mastering_luminance: Some((u32::from(caps.min_luminance), u32::from(caps.max_luminance))),
        mastering_primaries: None,
        luminances: Some((
            u32::from(caps.min_luminance),
            u32::from(caps.max_luminance),
            u32::from(active.reference_white),
        )),
        windows_scrgb: false,
        windows_bt2100: false,
    }
}

impl ColorManagementHandler for State {
    fn color_management_state(&mut self) -> &mut ColorManagementState {
        &mut self.common.color_management_state
    }

    fn description_for_output(&mut self, output: &Output) -> ImageDescription {
        description_for_output(output)
    }

    fn preferred_description_for_surface(&mut self, surface: &WlSurface) -> ImageDescription {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        let output = with_states(&root, |states| {
            surface_primary_scanout_output(&root, states)
        })
        .or_else(|| {
            self.common
                .shell
                .read()
                .visible_output_for_surface(&root)
                .cloned()
        });

        output
            .as_ref()
            .map(description_for_output)
            .unwrap_or(ImageDescription::SRGB)
    }

    fn schedule_image_description_info(
        &mut self,
        info: WpImageDescriptionInfoV1,
        desc: ImageDescription,
    ) {
        self.common.event_loop_handle.insert_idle(move |_state| {
            send_image_description_info(&info, &desc);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kms::drm_helpers::{ActiveHdrOutput, HdrSinkCapabilities};

    #[test]
    fn reports_the_hardware_validated_hdr_description() {
        let output = Output::new(
            "DP-test".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "Test".into(),
                model: "HDR".into(),
                serial_number: "test".into(),
            },
        );
        output
            .user_data()
            .insert_if_missing_threadsafe(HdrOutputState::default);
        output
            .user_data()
            .get::<HdrOutputState>()
            .unwrap()
            .set(Some(ActiveHdrOutput {
                capabilities: HdrSinkCapabilities {
                    max_luminance: 993,
                    min_luminance: 10,
                    max_frame_average_luminance: 993,
                },
                reference_white: 203,
            }));

        let desc = description_for_output(&output);
        assert_eq!(desc.transfer, TransferFunction::St2084Pq);
        assert_eq!(desc.primaries.named, Some(Primaries::Bt2020));
        assert_eq!(desc.luminances, Some((10, 993, 203)));
    }
}
