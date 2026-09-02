// SPDX-License-Identifier: GPL-3.0-only

use anyhow::{Context, Result, anyhow};
use libdisplay_info::{
    edid::DisplayDescriptorTag,
    info::{HdrStaticMetadata, Info, SupportedSignalColorimetry},
};
use smithay::{
    backend::drm::{DrmDevice, color::HdrOutputMetadata},
    reexports::drm::control::{
        AtomicCommitFlags, Device as ControlDevice, Mode, ModeFlags, PlaneType, ResourceHandle,
        atomic::AtomicModeReq,
        connector::{self, State as ConnectorState},
        crtc,
        dumbbuffer::DumbBuffer,
        property,
    },
    utils::Transform,
};
use std::{
    collections::HashMap,
    ops::{Range, RangeInclusive},
    sync::Mutex,
};

pub fn display_configuration(
    device: &mut DrmDevice,
) -> Result<HashMap<connector::Handle, Option<crtc::Handle>>> {
    let res_handles = device.resource_handles()?;
    let connectors = res_handles.connectors();

    let mut map = HashMap::new();
    let mut cleanup = Vec::new();

    // We expect the previous running drm master (likely the login mananger)
    // to leave the drm device in a sensible state.
    // That means, to reduce flickering, we try to keep an established mapping.
    for conn in connectors
        .iter()
        .flat_map(|conn| device.get_connector(*conn, true).ok())
    {
        if let Some(enc) = conn.current_encoder()
            && let Some(crtc) = device.get_encoder(enc)?.crtc()
        {
            // If is is connected we found a mapping
            if conn.state() == ConnectorState::Connected {
                map.insert(conn.handle(), Some(crtc));
            // If not, the user just unplugged something,
            // or the drm master did not cleanup?
            // Well, I guess we cleanup after them.
            } else {
                cleanup.push(crtc);
            }
        }
    }

    // But just in case we try to match all remaining connectors.
    for conn in connectors
        .iter()
        .flat_map(|conn| device.get_connector(*conn, false).ok())
        .filter(|conn| conn.state() == ConnectorState::Connected)
        .filter(|conn| !map.contains_key(&conn.handle()))
        .collect::<Vec<_>>()
        .iter()
    {
        'outer: for encoder_info in conn
            .encoders()
            .iter()
            .flat_map(|encoder_handle| device.get_encoder(*encoder_handle))
        {
            for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                if !map.values().any(|v| *v == Some(crtc)) {
                    map.insert(conn.handle(), Some(crtc));
                    break 'outer;
                }
            }
        }

        map.entry(conn.handle()).or_insert(None);
    }

    // And then cleanup
    if device.is_atomic() {
        let mut req = AtomicModeReq::new();
        let mut has_changes = false;
        let plane_handles = device.plane_handles()?;

        // We cannot just shortcut and use the legacy api for all cleanups because of this.
        // (Technically a device does not need to be atomic for planes to be used, but nobody does this otherwise.)
        for plane in plane_handles {
            let info = device.get_plane(plane)?;
            if let Some(crtc) = info.crtc() {
                let is_primary = get_property_val(device, plane, "type").map(
                    |(val_type, val)| match val_type.convert_value(val) {
                        property::Value::Enum(Some(val)) => {
                            val.value() == PlaneType::Primary as u64
                        }
                        _ => false,
                    },
                )?;
                if !is_primary && !cleanup.contains(&crtc) {
                    let crtc_id = get_prop(device, plane, "CRTC_ID")?;
                    let fb_id = get_prop(device, plane, "FB_ID")?;
                    req.add_property(plane, crtc_id, property::Value::CRTC(None));
                    req.add_property(plane, fb_id, property::Value::Framebuffer(None));
                    has_changes = true;
                }
            }
        }
        // Skip an empty commit: a no-op modeset that also fails with EPERM
        // without DRM master (e.g. a render-only secondary GPU).
        if has_changes {
            device.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)?;
        }
    } else {
        for crtc in res_handles.crtcs() {
            #[allow(deprecated)]
            let _ = device.set_cursor(*crtc, Option::<&DumbBuffer>::None);
        }
    }
    disable_crtcs(device, &cleanup)?;

    Ok(map)
}

/// Disables the given CRTCs and detaches their connectors/planes.
pub fn disable_crtcs(device: &mut DrmDevice, crtcs: &[crtc::Handle]) -> Result<()> {
    if crtcs.is_empty() {
        return Ok(());
    }

    let res_handles = device.resource_handles()?;

    if device.is_atomic() {
        let mut req = AtomicModeReq::new();

        for conn in res_handles
            .connectors()
            .iter()
            .flat_map(|conn| device.get_connector(*conn, false).ok())
            .filter(|conn| {
                if let Some(enc) = conn.current_encoder()
                    && let Ok(enc) = device.get_encoder(enc)
                    && let Some(crtc) = enc.crtc()
                {
                    return crtcs.contains(&crtc);
                }
                false
            })
            .map(|info| info.handle())
        {
            let crtc_id = get_prop(device, conn, "CRTC_ID")?;
            req.add_property(conn, crtc_id, property::Value::CRTC(None));
        }

        for plane in device.plane_handles()? {
            let info = device.get_plane(plane)?;
            if let Some(crtc) = info.crtc()
                && crtcs.contains(&crtc)
            {
                let crtc_id = get_prop(device, plane, "CRTC_ID")?;
                let fb_id = get_prop(device, plane, "FB_ID")?;
                req.add_property(plane, crtc_id, property::Value::CRTC(None));
                req.add_property(plane, fb_id, property::Value::Framebuffer(None));
            }
        }

        for crtc in crtcs {
            let mode_id = get_prop(device, *crtc, "MODE_ID")?;
            let active = get_prop(device, *crtc, "ACTIVE")?;
            req.add_property(*crtc, active, property::Value::Boolean(false));
            req.add_property(*crtc, mode_id, property::Value::Unknown(0));
        }

        device.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)?;
    } else {
        for crtc in crtcs {
            // null commit (necessary to trigger removal on the kernel side with the legacy api.)
            let _ = device.set_crtc(*crtc, None, (0, 0), &[], None);
        }
    }

    Ok(())
}

pub fn interface_name(device: &impl ControlDevice, connector: connector::Handle) -> Result<String> {
    let conn_info = device.get_connector(connector, false)?;

    let other_short_name;
    let interface_short_name = match conn_info.interface() {
        connector::Interface::DVII => "DVI-I",
        connector::Interface::DVID => "DVI-D",
        connector::Interface::DVIA => "DVI-A",
        connector::Interface::SVideo => "S-VIDEO",
        connector::Interface::DisplayPort => "DP",
        connector::Interface::HDMIA => "HDMI-A",
        connector::Interface::HDMIB => "HDMI-B",
        connector::Interface::EmbeddedDisplayPort => "eDP",
        other => {
            other_short_name = format!("{:?}", other);
            &other_short_name
        }
    };

    Ok(format!(
        "{}-{}",
        interface_short_name,
        conn_info.interface_id()
    ))
}

pub fn edid_info(device: &impl ControlDevice, connector: connector::Handle) -> Result<Info> {
    let edid_prop = get_prop(device, connector, "EDID")?;
    let edid_info = device.get_property(edid_prop)?;

    let mut edid = None;
    let props = device.get_properties(connector)?;
    let (ids, vals) = props.as_props_and_values();
    for (&id, &val) in ids.iter().zip(vals.iter()) {
        if id == edid_prop {
            if let property::Value::Blob(edid_blob) = edid_info.value_type().convert_value(val) {
                let blob = device.get_property_blob(edid_blob)?;
                edid = Some(Info::parse_edid(&blob).context("Unable to parse edid")?);
            }
            break;
        }
    }

    edid.ok_or(anyhow!("No EDID found"))
}

/// HDR capabilities derived from the sink's EDID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrSinkCapabilities {
    /// Desired peak luminance in cd/m².
    pub max_luminance: u16,
    /// Desired minimum luminance in units of 0.0001 cd/m².
    pub min_luminance: u16,
    /// Desired maximum frame-average luminance in cd/m².
    pub max_frame_average_luminance: u16,
}

/// The HDR presentation state actually accepted by KMS for an output.
///
/// This lives in the Smithay `Output` user-data map so Wayland color
/// management reports the hardware-validated state rather than merely the
/// user's requested configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveHdrOutput {
    pub capabilities: HdrSinkCapabilities,
    pub reference_white: u16,
}

#[derive(Debug, Default)]
struct HdrOutputStateInner {
    staged: Option<ActiveHdrOutput>,
    committed: bool,
}

#[derive(Debug, Default)]
pub struct HdrOutputState(Mutex<HdrOutputStateInner>);

impl HdrOutputState {
    pub fn get(&self) -> Option<ActiveHdrOutput> {
        let state = self.0.lock().unwrap();
        state.committed.then_some(state.staged).flatten()
    }

    /// Set an already committed state, primarily for protocol tests.
    #[cfg(test)]
    pub fn set(&self, state: Option<ActiveHdrOutput>) {
        *self.0.lock().unwrap() = HdrOutputStateInner {
            staged: state,
            committed: true,
        };
    }

    pub fn staged(&self) -> Option<ActiveHdrOutput> {
        self.0.lock().unwrap().staged
    }

    pub fn stage(&self, state: Option<ActiveHdrOutput>) {
        *self.0.lock().unwrap() = HdrOutputStateInner {
            staged: state,
            committed: false,
        };
    }

    pub fn commit(&self) {
        self.0.lock().unwrap().committed = true;
    }
}

impl HdrSinkCapabilities {
    /// Build conservative HDR10 output metadata for compositor-generated
    /// content. EDID describes the sink rather than a mastering display, so
    /// these values are an output policy, not a calibration claim.
    pub fn output_metadata(self, luminance_from_panel: bool) -> HdrOutputMetadata {
        if luminance_from_panel {
            HdrOutputMetadata::pq_bt2020(
                self.max_luminance,
                self.min_luminance,
                self.max_luminance,
                self.max_frame_average_luminance,
            )
        } else {
            // All-zero luminance means "unknown" (CTA-861-G). Panels that
            // tone-map against MaxCLL then leave the signal alone instead of
            // flattening the desktop (observed as a matte, greyish picture).
            HdrOutputMetadata::pq_bt2020(0, 0, 0, 0)
        }
    }
}

/// Selects the connector `max bpc` value for an HDR10 modeset.
///
/// Some drivers, notably NVIDIA's DRM KMS implementation, do not expose a
/// `max bpc` property. In that case the 10-bit framebuffer format and atomic
/// validation are the authoritative negotiation, so the connector value is
/// left driver-managed. When the property exists it must explicitly allow 10.
pub fn hdr_max_bpc_value(range: Option<&RangeInclusive<u32>>) -> Option<Option<u32>> {
    match range {
        None => Some(None),
        Some(range) if range.contains(&10) => Some(Some(10)),
        Some(_) => None,
    }
}

/// Returns usable HDR10 capabilities when the EDID advertises Static Metadata
/// Type 1, PQ and BT.2020 RGB signaling. CTA permits sinks to omit the
/// optional luminance bytes; use a conservative internal ceiling in that case.
pub fn hdr_sink_capabilities(info: &Info) -> Option<HdrSinkCapabilities> {
    hdr_sink_capabilities_from_edid(
        info.hdr_static_metadata(),
        info.supported_signal_colorimetry(),
    )
}

fn hdr_sink_capabilities_from_edid(
    metadata: HdrStaticMetadata,
    colorimetry: SupportedSignalColorimetry,
) -> Option<HdrSinkCapabilities> {
    if !metadata.type1 || !metadata.pq || !colorimetry.bt2020_rgb {
        return None;
    }

    fn rounded_u16(value: f32) -> u16 {
        value.round().clamp(0.0, u16::MAX as f32) as u16
    }

    let reported_max_luminance = rounded_u16(metadata.desired_content_max_luminance);
    let max_luminance = if reported_max_luminance == 0 {
        1_000
    } else {
        reported_max_luminance
    };
    let reported_frame_average = rounded_u16(metadata.desired_content_max_frame_avg_luminance);
    let max_frame_average_luminance = if reported_frame_average == 0 {
        max_luminance
    } else {
        reported_frame_average.min(max_luminance)
    };
    let min_luminance = rounded_u16(metadata.desired_content_min_luminance * 10_000.0);

    Some(HdrSinkCapabilities {
        max_luminance,
        min_luminance,
        max_frame_average_luminance,
    })
}

pub fn get_prop(
    device: &impl ControlDevice,
    handle: impl ResourceHandle,
    name: &str,
) -> Result<property::Handle> {
    let props = device.get_properties(handle)?;
    let (prop_handles, _) = props.as_props_and_values();
    for prop in prop_handles {
        let info = device.get_property(*prop)?;
        if Some(name) == info.name().to_str().ok() {
            return Ok(*prop);
        }
    }
    anyhow::bail!("No prop found for {}", name)
}

pub fn get_property_val(
    device: &impl ControlDevice,
    handle: impl ResourceHandle,
    name: &str,
) -> Result<(property::ValueType, property::RawValue)> {
    let props = device.get_properties(handle)?;
    let (prop_handles, values) = props.as_props_and_values();
    for (&prop, &val) in prop_handles.iter().zip(values.iter()) {
        let info = device.get_property(prop)?;
        if Some(name) == info.name().to_str().ok() {
            let val_type = info.value_type();
            return Ok((val_type, val));
        }
    }
    anyhow::bail!("No prop found for {}", name)
}

// Returns refresh rate in milliherz
pub fn calculate_refresh_rate(mode: Mode) -> u32 {
    let htotal = mode.hsync().2 as u32;
    let vtotal = mode.vsync().2 as u32;
    let mut refresh =
        (mode.clock() as u64 * 1000000_u64 / htotal as u64 + vtotal as u64 / 2) / vtotal as u64;

    if mode.flags().contains(ModeFlags::INTERLACE) {
        refresh *= 2;
    }
    if mode.flags().contains(ModeFlags::DBLSCAN) {
        refresh /= 2;
    }
    if mode.vscan() > 1 {
        refresh /= mode.vscan() as u64;
    }

    refresh as u32
}

pub fn get_minimum_refresh_rate(
    device: &impl ControlDevice,
    connector: connector::Handle,
) -> Result<Option<u32>> {
    let info = edid_info(device, connector)?;
    let edid = info.edid().context("EDID lacking into")?;
    for descriptor in edid.display_descriptors() {
        if descriptor.tag() == DisplayDescriptorTag::RangeLimits {
            return Ok(Some(
                descriptor
                    .range_limits()
                    .context("Invalid range limits descriptor")?
                    .min_vert_rate_hz as u32,
            ));
        }
    }

    Ok(None)
}

pub fn get_max_bpc(
    dev: &impl ControlDevice,
    conn: connector::Handle,
) -> Result<Option<(u32, Range<u32>)>> {
    let Some(handle) = get_prop(dev, conn, "max bpc").ok() else {
        return Ok(None);
    };

    let info = dev.get_property(handle)?;
    let range = match info.value_type() {
        property::ValueType::UnsignedRange(x, y) => (x as u32)..(y as u32),
        _ => return Err(anyhow!("max bpc has wrong value type")),
    };

    let value = get_property_val(dev, conn, "max bpc").map(|(val_type, val)| {
        match val_type.convert_value(val) {
            property::Value::UnsignedRange(res) => res as u32,
            _ => unreachable!(),
        }
    })?;

    Ok(Some((value, range)))
}

pub fn set_max_bpc(dev: &impl ControlDevice, conn: connector::Handle, bpc: u32) -> Result<u32> {
    let (_, range) =
        get_max_bpc(dev, conn)?.ok_or(anyhow!("max bpc does not exist for connector"))?;
    dev.set_property(
        conn,
        get_prop(dev, conn, "max bpc")?,
        property::Value::UnsignedRange(bpc.clamp(range.start, range.end) as u64).into(),
    )
    .map_err(Into::<anyhow::Error>::into)
    .and_then(|_| get_property_val(dev, conn, "max bpc"))
    .map(|(val_type, val)| match val_type.convert_value(val) {
        property::Value::UnsignedRange(val) => val as u32,
        _ => unreachable!(),
    })
}

pub fn panel_orientation(dev: &impl ControlDevice, conn: connector::Handle) -> Result<Transform> {
    let (val_type, val) = get_property_val(dev, conn, "panel orientation")?;
    match val_type.convert_value(val) {
        property::Value::Enum(Some(val)) => match val.value() {
            // "Normal"
            0 => Ok(Transform::Normal),
            // "Upside Down"
            1 => Ok(Transform::_180),
            // "Left Side Up"
            2 => Ok(Transform::_90),
            // "Right Side Up"
            3 => Ok(Transform::_270),
            _ => Err(anyhow!("panel orientation has invalid value '{:?}'", val)),
        },
        _ => Err(anyhow!("panel orientation has wrong value type")),
    }
}

#[cfg(test)]
mod hdr_tests {
    use super::*;

    fn metadata() -> HdrStaticMetadata {
        HdrStaticMetadata {
            desired_content_max_luminance: 993.486,
            desired_content_max_frame_avg_luminance: 0.0,
            desired_content_min_luminance: 0.001,
            type1: true,
            traditional_sdr: true,
            traditional_hdr: false,
            pq: true,
            hlg: false,
        }
    }

    fn colorimetry() -> SupportedSignalColorimetry {
        SupportedSignalColorimetry {
            bt2020_cycc: false,
            bt2020_ycc: true,
            bt2020_rgb: true,
            st2113_rgb: false,
            ictcp: false,
        }
    }

    #[test]
    fn converts_real_edid_luminance_units() {
        let caps = hdr_sink_capabilities_from_edid(metadata(), colorimetry()).unwrap();
        assert_eq!(caps.max_luminance, 993);
        assert_eq!(caps.min_luminance, 10);
        // Unknown MaxFALL uses the sink peak rather than claiming a 1-nit limit.
        assert_eq!(caps.max_frame_average_luminance, 993);
    }

    #[test]
    fn rejects_missing_required_hdr10_signals() {
        let mut no_pq = metadata();
        no_pq.pq = false;
        assert!(hdr_sink_capabilities_from_edid(no_pq, colorimetry()).is_none());

        let mut no_bt2020 = colorimetry();
        no_bt2020.bt2020_rgb = false;
        assert!(hdr_sink_capabilities_from_edid(metadata(), no_bt2020).is_none());
    }

    #[test]
    fn selects_max_bpc_when_available_and_allows_driver_management() {
        assert_eq!(hdr_max_bpc_value(None), Some(None));
        assert_eq!(hdr_max_bpc_value(Some(&(8..=12))), Some(Some(10)));
        assert_eq!(hdr_max_bpc_value(Some(&(8..=8))), None);
    }

    #[test]
    fn zeroed_metadata_luminance_avoids_panel_tonemapping_hints() {
        let caps = HdrSinkCapabilities {
            max_luminance: 993,
            min_luminance: 6,
            max_frame_average_luminance: 277,
        };
        let zeroed = caps.output_metadata(false);
        assert_eq!(zeroed.max_display_mastering_luminance, 0);
        assert_eq!(zeroed.min_display_mastering_luminance, 0);
        assert_eq!(zeroed.max_cll, 0);
        assert_eq!(zeroed.max_fall, 0);

        let panel = caps.output_metadata(true);
        assert_eq!(panel.max_display_mastering_luminance, 993);
        assert_eq!(panel.max_cll, 993);
        assert_eq!(panel.max_fall, 277);
    }

    #[test]
    fn hdr_output_is_invisible_until_the_real_commit() {
        let active = ActiveHdrOutput {
            capabilities: HdrSinkCapabilities {
                max_luminance: 1_000,
                min_luminance: 5,
                max_frame_average_luminance: 400,
            },
            reference_white: 203,
        };
        let state = HdrOutputState::default();

        state.stage(Some(active));
        assert_eq!(state.staged(), Some(active));
        assert_eq!(state.get(), None);

        state.commit();
        assert_eq!(state.get(), Some(active));
    }
}
