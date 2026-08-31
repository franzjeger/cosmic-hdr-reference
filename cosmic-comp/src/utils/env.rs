// SPDX-License-Identifier: GPL-3.0-only

use smithay::backend::drm::{DrmNode, NodeType};
use std::{
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};
use tracing::{info, warn};

fn parse_bool_token(value: &str) -> bool {
    ["1", "true", "yes", "y"].contains(&value.to_lowercase().as_str())
}

pub fn bool_var(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    Some(parse_bool_token(&value))
}

#[derive(Debug, Clone, PartialEq)]
pub struct HdrPolicy {
    pub experiment_enabled: bool,
    pub require_active: bool,
    pub isolate_output: bool,
    pub output: Option<String>,
    pub reference_white: u16,
    /// Transfer function used to decode SDR content before HDR encoding.
    /// `0.0` selects the piecewise sRGB curve; any other value is a pure power
    /// gamma. Displays show SDR with ~gamma 2.2, and decoding with the sRGB
    /// curve instead lifts shadows noticeably ("washed out" HDR desktop).
    pub sdr_gamma: f32,
    /// 0.0 sends colorimetrically converted BT.2020 (same appearance as
    /// calibrated SDR); 1.0 skips the 709->2020 matrix so the panel's native
    /// wide gamut stretches colors like an uncalibrated SDR mode does.
    pub gamut_stretch: f32,
    /// Send the panel's EDID luminance values in HDR_OUTPUT_METADATA instead
    /// of zeros. Zero ("unknown") keeps panels from tone-mapping the desktop
    /// into a matte look when MaxCLL equals their own peak.
    pub metadata_luminance_from_panel: bool,
    pub safe_exit_grace: Duration,
    pub teardown_timeout: Duration,
}

impl HdrPolicy {
    /// Connectors requested via COSMIC_HDR_OUTPUT (comma-separated list).
    pub fn outputs(&self) -> impl Iterator<Item = &str> {
        self.output
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    pub fn output_requested(&self, name: &str) -> bool {
        self.outputs().any(|requested| requested == name)
    }

    /// The first requested connector. Strict mode fails closed only when this
    /// one cannot activate; additional connectors fall back to SDR with a
    /// warning so one weaker panel cannot kill the whole session.
    pub fn primary_output(&self) -> Option<&str> {
        self.outputs().next()
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        let enabled = |value: Option<String>| value.is_some_and(|value| parse_bool_token(&value));
        let bounded_ms = |value: Option<String>, default, min, max| {
            value
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(default)
                .clamp(min, max)
        };

        Self {
            // Opt-out: HDR support (color-management advertising, per-output
            // toggles) is a normal feature; COSMIC_HDR_EXPERIMENT=0 disables
            // it for debugging.
            experiment_enabled: lookup("COSMIC_HDR_EXPERIMENT")
                .is_none_or(|value| parse_bool_token(&value)),
            require_active: enabled(lookup("COSMIC_HDR_REQUIRE_ACTIVE")),
            isolate_output: enabled(lookup("COSMIC_HDR_ISOLATE_OUTPUT")),
            output: lookup("COSMIC_HDR_OUTPUT"),
            reference_white: lookup("COSMIC_HDR_REFERENCE_WHITE")
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(203)
                .clamp(
                    cosmic_comp_config::HDR_REFERENCE_WHITE_MIN,
                    cosmic_comp_config::HDR_REFERENCE_WHITE_MAX,
                ),
            sdr_gamma: match lookup("COSMIC_HDR_SDR_GAMMA").as_deref() {
                None => 2.2,
                Some(value) if value.eq_ignore_ascii_case("srgb") => 0.0,
                Some(value) => value
                    .trim()
                    .parse::<f32>()
                    .ok()
                    .filter(|gamma| gamma.is_finite())
                    .map(|gamma| {
                        if gamma == 0.0 {
                            0.0
                        } else {
                            gamma.clamp(1.0, 3.0)
                        }
                    })
                    .unwrap_or(2.2),
            },
            gamut_stretch: lookup("COSMIC_HDR_GAMUT_STRETCH")
                .and_then(|value| value.trim().parse::<f32>().ok())
                .filter(|stretch| stretch.is_finite())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0),
            metadata_luminance_from_panel: lookup("COSMIC_HDR_METADATA_LUMINANCE")
                .is_some_and(|value| value.eq_ignore_ascii_case("panel")),
            safe_exit_grace: Duration::from_millis(bounded_ms(
                lookup("COSMIC_HDR_SAFE_EXIT_GRACE_MS"),
                5_000,
                1_000,
                15_000,
            )),
            teardown_timeout: Duration::from_millis(bounded_ms(
                lookup("COSMIC_HDR_TEARDOWN_TIMEOUT_MS"),
                5_000,
                100,
                15_000,
            )),
        }
    }
}

/// Live `allow_tearing` cosmic-config value: whether clients' async
/// presentation hints are honored with real async page flips.
static ALLOW_TEARING: AtomicBool = AtomicBool::new(true);

pub fn set_allow_tearing(allow: bool) {
    ALLOW_TEARING.store(allow, Ordering::Relaxed);
}

pub fn allow_tearing() -> bool {
    ALLOW_TEARING.load(Ordering::Relaxed)
}

/// Per-output tearing overrides from the live `allow_tearing_outputs` key.
static ALLOW_TEARING_OUTPUTS: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

pub fn set_allow_tearing_outputs(map: HashMap<String, bool>) {
    *ALLOW_TEARING_OUTPUTS
        .get_or_init(Default::default)
        .lock()
        .unwrap() = map;
}

pub fn tearing_allowed_for(connector: &str) -> bool {
    ALLOW_TEARING_OUTPUTS
        .get()
        .and_then(|map| map.lock().unwrap().get(connector).copied())
        .unwrap_or_else(allow_tearing)
}

/// Per-output HDR toggles from the live `hdr_enabled_outputs` key. `None`
/// falls back to the output config file / environment request.
static HDR_ENABLED_OUTPUTS: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

pub fn set_hdr_enabled_outputs(map: HashMap<String, bool>) {
    *HDR_ENABLED_OUTPUTS
        .get_or_init(Default::default)
        .lock()
        .unwrap() = map;
}

pub fn hdr_enabled_override(connector: &str) -> Option<bool> {
    HDR_ENABLED_OUTPUTS
        .get()
        .and_then(|map| map.lock().unwrap().get(connector).copied())
}

/// Runtime override for the HDR reference white, fed by the live
/// `hdr_reference_white` cosmic-config key ("SDR brightness"). Zero means
/// unset; values are clamped to the shared reference-white range.
static HDR_REFERENCE_WHITE_OVERRIDE: AtomicU16 = AtomicU16::new(0);

pub fn set_hdr_reference_white_override(value: Option<u16>) {
    HDR_REFERENCE_WHITE_OVERRIDE.store(
        value.map_or(0, |white| {
            white.clamp(
                cosmic_comp_config::HDR_REFERENCE_WHITE_MIN,
                cosmic_comp_config::HDR_REFERENCE_WHITE_MAX,
            )
        }),
        Ordering::Relaxed,
    );
}

pub fn hdr_reference_white_override() -> Option<u16> {
    match HDR_REFERENCE_WHITE_OVERRIDE.load(Ordering::Relaxed) {
        0 => None,
        white => Some(white),
    }
}

/// Per-output reference-white overrides from the live
/// `hdr_reference_white_outputs` cosmic-config key, keyed by connector name.
static HDR_REFERENCE_WHITE_OUTPUTS: OnceLock<Mutex<HashMap<String, u16>>> = OnceLock::new();

pub fn set_hdr_reference_white_outputs(map: HashMap<String, u16>) {
    *HDR_REFERENCE_WHITE_OUTPUTS
        .get_or_init(Default::default)
        .lock()
        .unwrap() = map
        .into_iter()
        .map(|(connector, white)| {
            (
                connector,
                white.clamp(
                    cosmic_comp_config::HDR_REFERENCE_WHITE_MIN,
                    cosmic_comp_config::HDR_REFERENCE_WHITE_MAX,
                ),
            )
        })
        .collect();
}

/// Effective runtime override for one output: the per-output map wins over the
/// global key. The per-panel EDID ceiling is applied by the KMS layer.
pub fn hdr_reference_white_for(connector: &str) -> Option<u16> {
    HDR_REFERENCE_WHITE_OUTPUTS
        .get()
        .and_then(|map| map.lock().unwrap().get(connector).copied())
        .or_else(hdr_reference_white_override)
}

pub fn hdr_policy() -> &'static HdrPolicy {
    static POLICY: OnceLock<HdrPolicy> = OnceLock::new();
    POLICY.get_or_init(|| HdrPolicy::from_lookup(|name| std::env::var(name).ok()))
}

#[derive(Debug, Clone, Copy)]
pub enum DeviceIdentifier {
    Id { vendor: u32, device: u32 },
    Node(DrmNode),
}

impl DeviceIdentifier {
    pub fn matches(&self, dev_node: &DrmNode) -> bool {
        match self {
            DeviceIdentifier::Node(id_node) => id_node == dev_node,
            DeviceIdentifier::Id { vendor, device } => {
                let (major, minor) = (dev_node.major(), dev_node.minor());
                let Some(dev_vendor) = std::fs::read_to_string(format!(
                    "/sys/dev/char/{}:{}/device/vendor",
                    major, minor
                ))
                .ok()
                .and_then(|ven| u32::from_str_radix(ven[2..].trim(), 16).ok()) else {
                    return false;
                };
                let Some(dev_device) = std::fs::read_to_string(format!(
                    "/sys/dev/char/{}:{}/device/device",
                    major, minor
                ))
                .ok()
                .and_then(|dev| u32::from_str_radix(dev[2..].trim(), 16).ok()) else {
                    return false;
                };
                info!(
                    "{:x}:{:x} == {:x}:{:x}",
                    *vendor, *device, dev_vendor, dev_device
                );
                dev_vendor == *vendor && dev_device == *device
            }
        }
    }
}

pub fn dev_var(name: &str) -> Option<DeviceIdentifier> {
    let value = std::env::var(name).ok()?;
    try_parse_dev_from_str(&value)
}

pub fn dev_list_var(name: &str) -> Option<Vec<DeviceIdentifier>> {
    let value = std::env::var(name).ok()?;
    Some(value.split(',').flat_map(try_parse_dev_from_str).collect())
}

fn try_parse_dev_from_str(val: &str) -> Option<DeviceIdentifier> {
    let val = val.trim();
    if val.starts_with("0x") && val.contains(':') {
        let (vendor, device) = val.split_once(':').unwrap();
        if !device.starts_with("0x") {
            warn!(
                "Failed to parse device entry {}, device id doesn't start with '0x'. Skipping",
                val
            );
            return None;
        }
        let vendor = u32::from_str_radix(&vendor[2..], 16)
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, vendor_id is no hex integer: {}. Skipping",
                    val, err
                );
            })
            .ok()?;
        let device = u32::from_str_radix(&device[2..], 16)
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, device_id is no hex integer: {}. Skipping",
                    val, err
                );
            })
            .ok()?;
        Some(DeviceIdentifier::Id { vendor, device })
    } else if val.starts_with("pci-") {
        let path = std::fs::read_link(format!("/dev/dri/by-path/{}-render", val))
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, no known pci path: {}. Skipping",
                    val, err
                );
            })
            .ok()?;
        let node = DrmNode::from_path(&path)
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, failed to get node from path {}: {}",
                    val,
                    path.display(),
                    err
                )
            })
            .ok()?;

        let node = node
            .node_with_type(NodeType::Render)
            .and_then(|res| res.ok())
            .unwrap_or(node);
        Some(DeviceIdentifier::Node(node))
    } else if val.contains(':') {
        let (major, minor) = val.split_once(':').unwrap();
        let major = str::parse::<u32>(major)
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, major is no integer: {}. Skipping",
                    val, err
                )
            })
            .ok()?;
        let minor = str::parse::<u32>(minor)
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, minor is no integer: {}. Skipping",
                    val, err
                )
            })
            .ok()?;
        let dev = rustix::fs::makedev(major, minor);
        let node = DrmNode::from_dev_id(dev)
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, failed to get node from dev_t {}: {}",
                    val, dev, err
                );
            })
            .ok()?;

        let node = node
            .node_with_type(NodeType::Render)
            .and_then(|res| res.ok())
            .unwrap_or(node);
        Some(DeviceIdentifier::Node(node))
    } else {
        // try to parse as device path

        let path = format!("/dev/dri/{}", val);
        let node = DrmNode::from_path(&path)
            .inspect_err(|err| {
                warn!(
                    "Failed to parse device entry {}, failed to get node from path {}: {}",
                    val, path, err
                );
            })
            .ok()?;

        let node = node
            .node_with_type(NodeType::Render)
            .and_then(|res| res.ok())
            .unwrap_or(node);
        Some(DeviceIdentifier::Node(node))
    }
}

#[cfg(test)]
mod tests {
    use super::HdrPolicy;
    use std::{collections::HashMap, time::Duration};

    #[test]
    fn hdr_policy_parses_and_bounds_environment_values() {
        let values = HashMap::from([
            ("COSMIC_HDR_EXPERIMENT", "yes"),
            ("COSMIC_HDR_REQUIRE_ACTIVE", "TRUE"),
            ("COSMIC_HDR_ISOLATE_OUTPUT", "1"),
            ("COSMIC_HDR_OUTPUT", "DP-2"),
            ("COSMIC_HDR_REFERENCE_WHITE", "9999"),
            ("COSMIC_HDR_SDR_GAMMA", "srgb"),
            ("COSMIC_HDR_GAMUT_STRETCH", "1.5"),
            ("COSMIC_HDR_METADATA_LUMINANCE", "Panel"),
            ("COSMIC_HDR_SAFE_EXIT_GRACE_MS", "10"),
            ("COSMIC_HDR_TEARDOWN_TIMEOUT_MS", "30000"),
        ]);
        let policy = HdrPolicy::from_lookup(|name| values.get(name).map(ToString::to_string));

        assert!(policy.experiment_enabled);
        assert!(policy.require_active);
        assert!(policy.isolate_output);
        assert_eq!(policy.output.as_deref(), Some("DP-2"));
        assert!(policy.output_requested("DP-2"));
        assert_eq!(policy.primary_output(), Some("DP-2"));
        let multi = HdrPolicy {
            output: Some("DP-2, DP-3".into()),
            ..policy.clone()
        };
        assert!(multi.output_requested("DP-2") && multi.output_requested("DP-3"));
        assert!(!multi.output_requested("HDMI-A-1"));
        assert_eq!(multi.primary_output(), Some("DP-2"));
        assert_eq!(policy.reference_white, 2000);
        assert_eq!(policy.sdr_gamma, 0.0);
        assert_eq!(policy.gamut_stretch, 1.0);
        assert!(policy.metadata_luminance_from_panel);
        assert_eq!(policy.safe_exit_grace, Duration::from_millis(1_000));
        assert_eq!(policy.teardown_timeout, Duration::from_millis(15_000));
    }

    #[test]
    fn hdr_policy_uses_safe_defaults_for_invalid_values() {
        let values = HashMap::from([
            ("COSMIC_HDR_EXPERIMENT", "no"),
            ("COSMIC_HDR_REFERENCE_WHITE", "invalid"),
            ("COSMIC_HDR_SDR_GAMMA", "invalid"),
            ("COSMIC_HDR_SAFE_EXIT_GRACE_MS", "invalid"),
        ]);
        let policy = HdrPolicy::from_lookup(|name| values.get(name).map(ToString::to_string));

        assert!(!policy.experiment_enabled);
        let default_policy = HdrPolicy::from_lookup(|_| None);
        assert!(default_policy.experiment_enabled, "HDR is on by default");
        assert!(!policy.require_active);
        assert!(!policy.isolate_output);
        assert_eq!(policy.output, None);
        assert_eq!(policy.reference_white, 203);
        assert_eq!(policy.sdr_gamma, 2.2);
        assert_eq!(policy.gamut_stretch, 0.0);
        assert!(!policy.metadata_luminance_from_panel);
        assert_eq!(policy.safe_exit_grace, Duration::from_millis(5_000));
        assert_eq!(policy.teardown_timeout, Duration::from_millis(5_000));
    }
}
