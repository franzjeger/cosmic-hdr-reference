//! Implementation of the wp-color-management-v1 protocol (the stabilized staging version
//! shipped in wayland-protocols).
//!
//! Clients use this protocol to describe the colorimetry of their surface contents (e.g.
//! BT.2020 primaries with the ST 2084 PQ transfer function for HDR video) by creating
//! parametric image descriptions and attaching them to a `wl_surface`. The attached
//! description is double-buffered surface state; the committed value can be read with
//! [`get_surface_description`]. What the compositor *does* with that information (HDR
//! signalling, color conversion, tone mapping) is entirely up to the compositor — see e.g.
//! [`ConnectorColorState`](crate::backend::drm::ConnectorColorState) for signalling HDR on a
//! DRM connector.
//!
//! Only *parametric* image descriptions with *named* transfer functions are supported;
//! ICC-file descriptions are rejected with `unsupported_feature`. The pre-defined
//! Windows-scRGB and Windows-BT.2100 descriptions are available via
//! [`Feature::WindowsScrgb`] / [`Feature::WindowsBt2100`] and are flagged as
//! [`windows_scrgb`](ImageDescription::windows_scrgb) /
//! [`windows_bt2100`](ImageDescription::windows_bt2100), exempting them from any tone mapping
//! the compositor performs. The compositor chooses which transfer functions, primaries, features
//! and rendering intents to advertise when creating the
//! [`ColorManagementState`]. Mastering display metadata
//! (target color volume) is supported via [`Feature::SetMasteringDisplayPrimaries`], including
//! target volumes exceeding the primary color volume via [`Feature::ExtendedTargetVolume`];
//! without the latter, such descriptions fail gracefully as the protocol recommends.
//! Source image description primaries may also be set to arbitrary values via
//! [`Feature::SetPrimaries`].
//!
//! ## Usage
//!
//! Implement [`ColorManagementHandler`], create a [`ColorManagementState`] and route the
//! protocol objects with the crate-wide [`delegate_dispatch2!`](crate::delegate_dispatch2)
//! macro. In your rendering/output logic, read the committed description of relevant
//! surfaces with [`get_surface_description`].

use std::sync::Mutex;

use tracing::{debug, trace};
use wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};
use wayland_server::protocol::wl_output::WlOutput;
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak};

use crate::output::Output;
use crate::wayland::compositor::{self, Cacheable};
use crate::wayland::{Dispatch2, GlobalDispatch2};

pub use wp_color_manager_v1::{Feature, Primaries, RenderIntent, TransferFunction};

const VERSION: u32 = 3;

/// Whether a transfer function may be advertised to a client that bound the given version:
/// deprecated entries must not be sent to v2+ clients, entries added in v2 don't exist for v1
/// clients.
fn tf_visible(tf: TransferFunction, version: u32) -> bool {
    match tf {
        // Deprecated since version 2 (ambiguous sRGB curves).
        TransferFunction::Srgb | TransferFunction::ExtSrgb => version < 2,
        // Added in version 2.
        TransferFunction::CompoundPower24 => version >= 2,
        _ => true,
    }
}

/// Whether a feature may be advertised to a client that bound the given version.
fn feature_visible(feature: Feature, version: u32) -> bool {
    match feature {
        // create_windows_bt2100 was added in version 3.
        Feature::WindowsBt2100 => version >= 3,
        _ => true,
    }
}

/// Whether a rendering intent may be advertised to a client that bound the given version.
fn intent_visible(intent: RenderIntent, version: u32) -> bool {
    match intent {
        // Added in version 2.
        RenderIntent::AbsoluteNoAdaptation => version >= 2,
        _ => true,
    }
}

/// CIE 1931 xy chromaticities of a set of primaries and their white point, in protocol wire
/// units (coordinate multiplied by 1,000,000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chromaticities {
    /// The red primary as (x, y).
    pub red: (i32, i32),
    /// The green primary as (x, y).
    pub green: (i32, i32),
    /// The blue primary as (x, y).
    pub blue: (i32, i32),
    /// The white point as (x, y).
    pub white: (i32, i32),
}

/// The primary color volume of an image description: either a named set of primaries
/// (`set_primaries_named`) or raw chromaticity coordinates (`set_primaries`). Exactly one is
/// set on a created description; both `None` only occurs mid-build.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PrimariesOption {
    /// The named primaries last set.
    pub named: Option<Primaries>,
    /// The actual raw primaries.
    pub values: Option<Chromaticities>,
}

impl Chromaticities {
    /// The chromaticities of a named set of primaries, per ITU-T H.273 (and the respective
    /// defining standards referenced by the protocol).
    pub const fn from_named(primaries: Primaries) -> Self {
        const D65: (i32, i32) = (312_700, 329_000);
        const C: (i32, i32) = (310_000, 316_000);
        match primaries {
            Primaries::Srgb => Self {
                red: (640_000, 330_000),
                green: (300_000, 600_000),
                blue: (150_000, 60_000),
                white: D65,
            },
            Primaries::PalM => Self {
                red: (670_000, 330_000),
                green: (210_000, 710_000),
                blue: (140_000, 80_000),
                white: C,
            },
            Primaries::Pal => Self {
                red: (640_000, 330_000),
                green: (290_000, 600_000),
                blue: (150_000, 60_000),
                white: D65,
            },
            Primaries::Ntsc => Self {
                red: (630_000, 340_000),
                green: (310_000, 595_000),
                blue: (155_000, 70_000),
                white: D65,
            },
            Primaries::GenericFilm => Self {
                red: (681_000, 319_000),
                green: (243_000, 692_000),
                blue: (145_000, 49_000),
                white: C,
            },
            Primaries::Bt2020 => Self {
                red: (708_000, 292_000),
                green: (170_000, 797_000),
                blue: (131_000, 46_000),
                white: D65,
            },
            Primaries::Cie1931Xyz => Self {
                red: (1_000_000, 0),
                green: (0, 1_000_000),
                blue: (0, 0),
                white: (333_333, 333_333),
            },
            Primaries::DciP3 => Self {
                red: (680_000, 320_000),
                green: (265_000, 690_000),
                blue: (150_000, 60_000),
                white: (314_000, 351_000),
            },
            Primaries::DisplayP3 => Self {
                red: (680_000, 320_000),
                green: (265_000, 690_000),
                blue: (150_000, 60_000),
                white: D65,
            },
            Primaries::AdobeRgb => Self {
                red: (640_000, 330_000),
                green: (210_000, 710_000),
                blue: (150_000, 60_000),
                white: D65,
            },
            // Named primaries added by future protocol versions; they cannot be advertised or
            // accepted by this implementation, so this is unreachable for stored descriptions.
            _ => Self {
                red: (640_000, 330_000),
                green: (300_000, 600_000),
                blue: (150_000, 60_000),
                white: D65,
            },
        }
    }

    /// Whether the chromaticity gamut of `other` (its primaries and white point) lies entirely
    /// inside (or on the boundary of) the RGB triangle of `self`.
    ///
    /// This is the 2D gamut-containment check used to decide whether a target color volume
    /// stays within the primary color volume.
    pub fn gamut_contains(&self, other: &Chromaticities) -> bool {
        [other.red, other.green, other.blue, other.white]
            .into_iter()
            .all(|p| point_in_triangle(p, self.red, self.green, self.blue))
    }

    /// Cast dynamically from PrimariesOption
    pub const fn from_option(primaries: PrimariesOption) -> Option<Chromaticities> {
        match (primaries.values, primaries.named) {
            (Some(val), None) => Some(val),
            (None, Some(val)) => Some(Chromaticities::from_named(val)),
            (None, None) => None,
            (Some(_val), Some(_otherval)) => None, // Should not happen
        }
    }
}

/// Whether `p` lies inside (or on an edge of) the triangle `(a, b, c)`, in exact integer
/// arithmetic and independent of winding order.
fn point_in_triangle(p: (i32, i32), a: (i32, i32), b: (i32, i32), c: (i32, i32)) -> bool {
    fn cross(o: (i32, i32), a: (i32, i32), b: (i32, i32)) -> i64 {
        let (ox, oy) = (o.0 as i64, o.1 as i64);
        let (ax, ay) = (a.0 as i64, a.1 as i64);
        let (bx, by) = (b.0 as i64, b.1 as i64);
        (ax - ox) * (by - oy) - (ay - oy) * (bx - ox)
    }
    let d1 = cross(a, b, p);
    let d2 = cross(b, c, p);
    let d3 = cross(c, a, p);
    let has_neg = d1 < 0 || d2 < 0 || d3 < 0;
    let has_pos = d1 > 0 || d2 > 0 || d3 > 0;
    !(has_neg && has_pos)
}

/// A parsed, immutable parametric image description.
///
/// Only named transfer functions and primaries are representable, since those are the only
/// ones this implementation advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDescription {
    /// The transfer characteristics of the content.
    pub transfer: TransferFunction,
    /// The color primaries of the content.
    pub primaries: PrimariesOption,
    /// Maximum content light level in cd/m², if the client provided it.
    pub max_cll: Option<u32>,
    /// Maximum frame-average light level in cd/m², if provided.
    pub max_fall: Option<u32>,
    /// Mastering display luminance as (min in 0.0001 cd/m², max in cd/m²), if provided.
    pub mastering_luminance: Option<(u32, u32)>,
    /// Mastering display primaries and white point (SMPTE ST 2086), if provided. Together with
    /// [`mastering_luminance`](Self::mastering_luminance) these define the target color volume.
    ///
    /// Coordinates are 1e6-scaled CIE 1931 xy; for DRM HDR metadata they can be bridged with
    /// `CtaCoordinate::from_xy(x as f64 / 1e6, y as f64 / 1e6)`.
    pub mastering_primaries: Option<Chromaticities>,
    /// Content luminances as (min in 0.0001 cd/m², max in cd/m², reference white in cd/m²),
    /// if provided via `set_luminances`.
    pub luminances: Option<(u32, u32, u32)>,
    /// Whether this is the pre-defined Windows-scRGB stimulus encoding (created via
    /// `create_windows_scrgb`) rather than a client-authored parametric description.
    ///
    /// Windows-scRGB content is display-referred for a BT.2100/PQ-mode screen: compositors
    /// implementing tone mapping must pass it through unmapped (clamping to the output volume
    /// at most), unlike parametric descriptions with the same extended-linear transfer and
    /// sRGB primaries, which describe content that may be tone mapped.
    pub windows_scrgb: bool,
    /// Whether this is the pre-defined Windows-BT.2100 stimulus encoding (created via
    /// `create_windows_bt2100`) rather than a client-authored parametric description.
    ///
    /// Like Windows-scRGB it is display-referred for a BT.2100/PQ-mode screen and exempt from
    /// tone mapping; the content itself is regular PQ/BT.2020 and composites like other PQ
    /// content.
    pub windows_bt2100: bool,
}

impl ImageDescription {
    /// sRGB / sRGB — the default SDR description, also used for surfaces without an attached
    /// description.
    pub const SRGB: Self = Self {
        transfer: TransferFunction::Srgb,
        primaries: PrimariesOption {
            named: Some(Primaries::Srgb),
            values: None,
        },
        max_cll: None,
        max_fall: None,
        mastering_luminance: None,
        mastering_primaries: None,
        luminances: None,
        windows_scrgb: false,
        windows_bt2100: false,
    };

    /// The pre-defined Windows-scRGB stimulus encoding: sRGB primaries and white point with an
    /// extended-linear transfer where R=G=B=1.0 corresponds to 80 cd/m² (up to 125.0 for
    /// 10,000 cd/m²).
    pub const WINDOWS_SCRGB: Self = Self {
        transfer: TransferFunction::ExtLinear,
        primaries: PrimariesOption {
            named: Some(Primaries::Srgb),
            values: None,
        },
        max_cll: None,
        max_fall: None,
        mastering_luminance: None,
        // The protocol leaves the target color volume unknown ("anything between sRGB and
        // BT.2100"); assume BT.2020 like KWin so consumers don't clip wide-gamut content.
        mastering_primaries: Some(Chromaticities::from_named(Primaries::Bt2020)),
        // 1.0 = 80 cd/m²; the protocol suggests assuming a 203 cd/m² (BT.2408) reference
        // white. Reference above maximum is the extended-target-volume shape inherent to
        // scRGB's escape-the-gamut encoding.
        luminances: Some((0, 80, 203)),
        windows_scrgb: true,
        windows_bt2100: false,
    };

    /// The pre-defined Windows-BT.2100 stimulus encoding: BT.2020 primaries and white point
    /// with the ST 2084 (PQ) transfer characteristic, as produced by Windows 10 driving an HDR
    /// screen in BT.2100/PQ signalling mode.
    pub const WINDOWS_BT2100: Self = Self {
        transfer: TransferFunction::St2084Pq,
        primaries: PrimariesOption {
            named: Some(Primaries::Bt2020),
            values: None,
        },
        max_cll: None,
        max_fall: None,
        mastering_luminance: None,
        // The target color volume is unknown ("anything up to BT.2100"); it defaults to the
        // container primaries.
        mastering_primaries: None,
        // PQ luminances with the protocol-suggested 203 cd/m² (BT.2408) reference white.
        luminances: Some((50, 10_000, 203)),
        windows_scrgb: false,
        windows_bt2100: true,
    };

    /// Whether this description denotes HDR/wide-gamut content: an HDR or extended-range
    /// transfer function (PQ, HLG or extended linear), BT.2020 primaries, or the
    /// Windows-scRGB encoding.
    pub fn is_hdr(&self) -> bool {
        matches!(
            self.transfer,
            TransferFunction::St2084Pq | TransferFunction::Hlg | TransferFunction::ExtLinear
        ) || self
            .primaries
            .named
            .is_some_and(|named| named == Primaries::Bt2020)
            || self.windows_scrgb
    }

    /// Whether the image's electrical values can be presented unchanged on a
    /// BT.2100/PQ output.
    ///
    /// This is intentionally narrower than [`Self::is_hdr`]. scRGB, HLG and
    /// extended-range or wide-gamut SDR all require a color conversion before
    /// presentation on a PQ/BT.2020 scanout buffer.
    pub fn is_pq_bt2020(&self) -> bool {
        self.transfer == TransferFunction::St2084Pq
            && self.primaries.named == Some(Primaries::Bt2020)
            && self.primaries.values.is_none()
    }

    /// The primary color volume luminances as (min in 0.0001 cd/m², max in cd/m², reference
    /// white in cd/m²): the explicit `set_luminances` values if the client provided them,
    /// otherwise the defaults implied by the transfer function per the protocol
    /// (PQ: 0.005/10,000/203; HLG: 0.005/1,000/203; BT.1886: 0.01/100/100; anything else the
    /// sRGB defaults 0.2/80/80).
    pub const fn luminances_or_default(&self) -> (u32, u32, u32) {
        match self.luminances {
            Some(lum) => lum,
            None => match self.transfer {
                TransferFunction::St2084Pq => (50, 10_000, 203),
                TransferFunction::Hlg => (50, 1_000, 203),
                TransferFunction::Bt1886 => (100, 100, 100),
                _ => (2_000, 80, 80),
            },
        }
    }
}

/// Double-buffered per-surface color management state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorManagementSurfaceCachedState {
    /// The image description attached to the surface, if any.
    pub description: Option<ImageDescription>,
    /// The rendering intent the client prefers for mapping the surface to outputs.
    pub render_intent: RenderIntent,
}

impl Default for ColorManagementSurfaceCachedState {
    fn default() -> Self {
        Self {
            description: None,
            render_intent: RenderIntent::Perceptual,
        }
    }
}

impl Cacheable for ColorManagementSurfaceCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Returns the committed image description and rendering intent of a surface.
/// Returns the committed image description and render intent of a surface.
///
/// This locks the surface's compositor state. It must **not** be called from
/// inside a surface-tree traversal callback (`with_surface_tree_downward`,
/// `with_surfaces_surface_tree`, ...), because those callbacks already run
/// with the surface lock held; doing so deadlocks the calling thread. Use
/// [`surface_description_from_states`] with the `&SurfaceData` the traversal
/// hands you instead.
pub fn get_surface_description(surface: &WlSurface) -> (Option<ImageDescription>, RenderIntent) {
    compositor::with_states(surface, surface_description_from_states)
}

/// Lock-free variant of [`get_surface_description`] for use where the surface
/// state is already borrowed, e.g. inside surface-tree traversal callbacks.
pub fn surface_description_from_states(
    states: &compositor::SurfaceData,
) -> (Option<ImageDescription>, RenderIntent) {
    let state = *states
        .cached_state
        .get::<ColorManagementSurfaceCachedState>()
        .current();
    (state.description, state.render_intent)
}

/// Per-surface color-management bookkeeping in the surface's data map: enforces the
/// one-`wp_color_management_surface_v1`-per-surface rule and tracks the surface's feedback
/// objects for `preferred_changed`.
#[derive(Debug, Default)]
struct ColorManagementSurfaceData {
    attached: Mutex<bool>,
    feedbacks: Mutex<Vec<WpColorManagementSurfaceFeedbackV1>>,
    /// Identity of the last preferred description notified for this surface, for dedupe.
    last_preferred: Mutex<Option<u32>>,
}

/// User data of a `wp_image_description_v1`: the parsed description it represents.
#[derive(Debug)]
pub struct ImageDescriptionData {
    /// `None` for objects that got the `failed` event instead of `ready`; those can only be
    /// destroyed.
    desc: Option<ImageDescription>,
}

impl ImageDescriptionData {
    /// The description this object represents, or `None` if creating it failed (the object
    /// received the `failed` event and never became ready).
    pub fn description(&self) -> Option<ImageDescription> {
        self.desc
    }
}

/// Accumulated parameters of a `wp_image_description_creator_params_v1`, validated on
/// `create`.
#[derive(Debug, Default)]
pub struct ImageDescriptionBuilder {
    transfer: Option<TransferFunction>,
    primaries: PrimariesOption,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
    mastering_luminance: Option<(u32, u32)>,
    mastering_primaries: Option<Chromaticities>,
    luminances: Option<(u32, u32, u32)>,
}

/// Global data of `wp_color_manager_v1`, carrying the client visibility filter.
pub struct ColorManagementGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for ColorManagementGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColorManagementGlobalData")
            .finish_non_exhaustive()
    }
}

/// Handler trait for wp-color-management-v1.
pub trait ColorManagementHandler {
    /// Returns the [`ColorManagementState`].
    fn color_management_state(&mut self) -> &mut ColorManagementState;

    /// Called when a surface's *pending* image description changed (set or unset). The
    /// committed value becomes visible via [`get_surface_description`] after the next
    /// `wl_surface.commit`; compositors typically re-evaluate color handling for the
    /// surface's output on the next redraw.
    fn image_description_changed(&mut self, _surface: &WlSurface) {}

    /// The image description describing how the compositor presents the given output.
    ///
    /// Defaults to sRGB.
    fn description_for_output(&mut self, _output: &Output) -> ImageDescription {
        ImageDescription::SRGB
    }

    /// The image description the compositor would prefer the given surface to use, reported
    /// via the surface feedback object.
    ///
    /// Defaults to sRGB.
    fn preferred_description_for_surface(&mut self, _surface: &WlSurface) -> ImageDescription {
        ImageDescription::SRGB
    }

    /// Schedules sending the information events for `info` (describing `desc`), to run
    /// *after* the current request dispatch returns — see [`send_image_description_info`].
    /// This MUST be deferred (e.g. via an event loop idle callback):
    /// `wp_image_description_info_v1.done` is a destructor event, and destroying the object
    /// inside the very callback that created it corrupts wayland-backend's bookkeeping (it
    /// writes the new object's data after the callback returns, which would then be a
    /// use-after-free).
    fn schedule_image_description_info(&mut self, info: WpImageDescriptionInfoV1, desc: ImageDescription);
}

/// Sends the information events describing `desc` on `info`, terminating with the destructor
/// `done` event. Must be called *outside* the request callback that created `info` (e.g. from
/// an event-loop idle), via [`ColorManagementHandler::schedule_image_description_info`].
pub fn send_image_description_info(info: &WpImageDescriptionInfoV1, desc: &ImageDescription) {
    if !info.is_alive() {
        return;
    }
    let container = Chromaticities::from_option(desc.primaries);
    if let Some(primaries) = container {
        info.primaries(
            primaries.red.0,
            primaries.red.1,
            primaries.green.0,
            primaries.green.1,
            primaries.blue.0,
            primaries.blue.1,
            primaries.white.0,
            primaries.white.1,
        );
    }
    if let Some(named) = desc.primaries.named {
        info.primaries_named(named);
    }
    info.tf_named(desc.transfer);
    // The primary color volume luminances must always be sent; clients (e.g. winewayland's
    // HDR display detection) read the reference white from here.
    let (min_lum, max_lum, reference_lum) = desc.luminances_or_default();
    info.luminances(min_lum, max_lum, reference_lum);
    // The target color volume defaults to the primary color volume when no mastering display
    // primaries were given, or fall back to sRGB.
    let target = desc
        .mastering_primaries
        .unwrap_or(container.unwrap_or(Chromaticities::from_named(Primaries::Srgb)));
    info.target_primaries(
        target.red.0,
        target.red.1,
        target.green.0,
        target.green.1,
        target.blue.0,
        target.blue.1,
        target.white.0,
        target.white.1,
    );
    // Likewise always sent: without mastering luminances the target volume has the primary
    // color volume's luminance range.
    let (target_min, target_max) = desc.mastering_luminance.unwrap_or((min_lum, max_lum));
    info.target_luminance(target_min, target_max);
    if let Some(max_cll) = desc.max_cll {
        info.target_max_cll(max_cll);
    }
    if let Some(max_fall) = desc.max_fall {
        info.target_max_fall(max_fall);
    }
    info.done();
}

/// State of the wp-color-management-v1 global.
#[derive(Debug)]
pub struct ColorManagementState {
    supported_tfs: Vec<TransferFunction>,
    supported_primaries: Vec<Primaries>,
    supported_features: Vec<Feature>,
    supported_intents: Vec<RenderIntent>,
    /// Known distinct image descriptions; a description's identity is its index + 1.
    ///
    /// Identities must be stable so that the identity sent in `preferred_changed` matches the
    /// identity a subsequent `get_preferred` delivers via `ready`. The table grows
    /// monotonically with distinct descriptions, which is bounded in practice (clients create
    /// the same few descriptions).
    identities: Vec<ImageDescription>,
    /// Live `wp_color_management_output_v1` objects per output, for
    /// [`output_description_changed`](Self::output_description_changed).
    output_objects: Vec<WpColorManagementOutputV1>,
}

impl ColorManagementState {
    /// Creates a new wp-color-management-v1 global.
    ///
    /// The supported transfer functions, primaries, features and rendering intents are
    /// advertised to clients and validated in requests. [`Feature::Parametric`] is always
    /// advertised (this implementation is parametric-only); [`RenderIntent::Perceptual`]
    /// is always advertised as required by the protocol. If
    /// [`Feature::ExtendedTargetVolume`] is requested, [`Feature::SetMasteringDisplayPrimaries`]
    /// is advertised as well, since the protocol only allows the former alongside the latter.
    ///
    /// Without [`Feature::ExtendedTargetVolume`], image descriptions whose target color volume
    /// extends outside the primary color volume are failed gracefully (`failed` with cause
    /// `unsupported`), as recommended by the protocol.
    ///
    /// The global is only visible to clients for which `filter` returns `true`.
    pub fn new<D, F>(
        display: &DisplayHandle,
        supported_tfs: impl IntoIterator<Item = TransferFunction>,
        supported_primaries: impl IntoIterator<Item = Primaries>,
        supported_features: impl IntoIterator<Item = Feature>,
        supported_intents: impl IntoIterator<Item = RenderIntent>,
        filter: F,
    ) -> Self
    where
        D: GlobalDispatch<WpColorManagerV1, ColorManagementGlobalData>,
        D: Dispatch<WpColorManagerV1, ()>,
        D: ColorManagementHandler,
        D: 'static,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let data = ColorManagementGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, WpColorManagerV1, _>(VERSION, data);

        let mut supported_features: Vec<Feature> = supported_features.into_iter().collect();
        if !supported_features.contains(&Feature::Parametric) {
            supported_features.push(Feature::Parametric);
        }
        if supported_features.contains(&Feature::ExtendedTargetVolume)
            && !supported_features.contains(&Feature::SetMasteringDisplayPrimaries)
        {
            supported_features.push(Feature::SetMasteringDisplayPrimaries);
        }
        let mut supported_intents: Vec<RenderIntent> = supported_intents.into_iter().collect();
        if !supported_intents.contains(&RenderIntent::Perceptual) {
            supported_intents.push(RenderIntent::Perceptual);
        }

        Self {
            supported_tfs: supported_tfs.into_iter().collect(),
            supported_primaries: supported_primaries.into_iter().collect(),
            supported_features,
            supported_intents,
            identities: Vec::new(),
            output_objects: Vec::new(),
        }
    }

    /// Returns the stable identity for a description, assigning a new one if it is not known
    /// yet.
    fn identity_for(&mut self, desc: ImageDescription) -> u32 {
        let index = match self.identities.iter().position(|d| *d == desc) {
            Some(index) => index,
            None => {
                self.identities.push(desc);
                self.identities.len() - 1
            }
        };
        index as u32 + 1
    }

    /// Notifies the given surface's feedback objects that the compositor's preferred image
    /// description for it changed.
    ///
    /// Deduplicated per surface: notifying the same description again is a no-op, so this is
    /// safe to call from a periodic refresh. Clients react by calling `get_preferred`, which
    /// routes through
    /// [`ColorManagementHandler::preferred_description_for_surface`] — that must already
    /// return the new description when this is called.
    pub fn preferred_changed(&mut self, surface: &WlSurface, desc: ImageDescription) {
        let identity = self.identity_for(desc);
        compositor::with_states(surface, |states| {
            let Some(data) = states.data_map.get::<ColorManagementSurfaceData>() else {
                return;
            };
            let mut last_preferred = data.last_preferred.lock().unwrap();
            if *last_preferred == Some(identity) {
                return;
            }
            *last_preferred = Some(identity);

            let mut feedbacks = data.feedbacks.lock().unwrap();
            feedbacks.retain(|feedback| feedback.is_alive());
            for feedback in feedbacks.iter() {
                if feedback.version() >= 2 {
                    feedback.preferred_changed2(0, identity);
                } else {
                    feedback.preferred_changed(identity);
                }
            }
        });
    }

    /// Notifies all `wp_color_management_output_v1` objects of the given output that its
    /// image description changed.
    ///
    /// Clients react by calling `get_image_description`, which routes through
    /// [`ColorManagementHandler::description_for_output`] — that must already return the new
    /// description when this is called.
    pub fn output_description_changed(&mut self, output: &Output) {
        self.output_objects.retain(|obj| obj.is_alive());
        for obj in &self.output_objects {
            let same_output = obj
                .data::<WlOutput>()
                .and_then(Output::from_resource)
                .is_some_and(|o| o == *output);
            if same_output {
                obj.image_description_changed();
            }
        }
    }
}

impl<D> GlobalDispatch2<WpColorManagerV1, D> for ColorManagementGlobalData
where
    D: Dispatch<WpColorManagerV1, ()>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn bind(
        &self,
        state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<WpColorManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(manager, ());

        // Enum entries that are deprecated in — or newer than — the bound version must not be
        // advertised.
        let version = manager.version();
        let cm_state = state.color_management_state();
        for intent in &cm_state.supported_intents {
            if intent_visible(*intent, version) {
                manager.supported_intent(*intent);
            }
        }
        for feature in &cm_state.supported_features {
            if feature_visible(*feature, version) {
                manager.supported_feature(*feature);
            }
        }
        for tf in &cm_state.supported_tfs {
            if tf_visible(*tf, version) {
                manager.supported_tf_named(*tf);
            }
        }
        for primaries in &cm_state.supported_primaries {
            manager.supported_primaries_named(*primaries);
        }
        manager.done();
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<WpColorManagerV1, D> for ()
where
    D: Dispatch<WpColorManagementOutputV1, WlOutput>,
    D: Dispatch<WpColorManagementSurfaceV1, Weak<WlSurface>>,
    D: Dispatch<WpColorManagementSurfaceFeedbackV1, Weak<WlSurface>>,
    D: Dispatch<WpImageDescriptionCreatorParamsV1, Mutex<ImageDescriptionBuilder>>,
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpColorManagerV1,
        request: <WpColorManagerV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_manager_v1::Request;
        match request {
            Request::GetOutput { id, output } => {
                let obj = data_init.init(id, output);
                state.color_management_state().output_objects.push(obj);
            }
            Request::GetSurface { id, surface } => {
                let already_attached = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing(ColorManagementSurfaceData::default);
                    let data = states.data_map.get::<ColorManagementSurfaceData>().unwrap();
                    let mut attached = data.attached.lock().unwrap();
                    std::mem::replace(&mut *attached, true)
                });
                if already_attached {
                    resource.post_error(
                        wp_color_manager_v1::Error::SurfaceExists,
                        "surface already has a wp_color_management_surface_v1",
                    );
                    return;
                }
                data_init.init(id, surface.downgrade());
            }
            Request::GetSurfaceFeedback { id, surface } => {
                let feedback = data_init.init(id, surface.downgrade());
                // Track the feedback object so `preferred_changed` can reach it.
                compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing(ColorManagementSurfaceData::default);
                    let data = states.data_map.get::<ColorManagementSurfaceData>().unwrap();
                    data.feedbacks.lock().unwrap().push(feedback);
                });
            }
            Request::CreateParametricCreator { obj } => {
                data_init.init(obj, Mutex::new(ImageDescriptionBuilder::default()));
            }
            Request::CreateIccCreator { .. } => {
                resource.post_error(
                    wp_color_manager_v1::Error::UnsupportedFeature,
                    "ICC image descriptions are not supported",
                );
            }
            Request::CreateWindowsScrgb { image_description } => {
                if !state
                    .color_management_state()
                    .supported_features
                    .contains(&Feature::WindowsScrgb)
                {
                    resource.post_error(
                        wp_color_manager_v1::Error::UnsupportedFeature,
                        "Windows scRGB image descriptions are not supported",
                    );
                    return;
                }
                make_ready_description(
                    state,
                    image_description,
                    ImageDescription::WINDOWS_SCRGB,
                    data_init,
                );
            }
            Request::CreateWindowsBt2100 { image_description } => {
                if !state
                    .color_management_state()
                    .supported_features
                    .contains(&Feature::WindowsBt2100)
                {
                    resource.post_error(
                        wp_color_manager_v1::Error::UnsupportedFeature,
                        "Windows BT.2100 image descriptions are not supported",
                    );
                    return;
                }
                make_ready_description(
                    state,
                    image_description,
                    ImageDescription::WINDOWS_BT2100,
                    data_init,
                );
            }
            Request::GetImageDescription {
                image_description, ..
            } => {
                // This implementation never creates wp_image_description_reference_v1
                // objects, so any reference necessarily comes from a protocol it doesn't
                // know about.
                make_failed_description(
                    image_description,
                    "image description references are not supported",
                    data_init,
                );
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D> Dispatch2<WpColorManagementOutputV1, D> for WlOutput
where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: <WpColorManagementOutputV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_output_v1::Request;
        match request {
            Request::GetImageDescription { image_description } => {
                let desc = Output::from_resource(self)
                    .map(|output| state.description_for_output(&output))
                    .unwrap_or(ImageDescription::SRGB);
                make_ready_description(state, image_description, desc, data_init);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D> Dispatch2<WpColorManagementSurfaceV1, D> for Weak<WlSurface>
where
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpColorManagementSurfaceV1,
        request: <WpColorManagementSurfaceV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_surface_v1::Request;
        match request {
            Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                let Ok(surface) = self.upgrade() else {
                    resource.post_error(
                        wp_color_management_surface_v1::Error::Inert,
                        "the underlying wl_surface was destroyed",
                    );
                    return;
                };

                let render_intent = match render_intent.into_result() {
                    Ok(intent) if state.color_management_state().supported_intents.contains(&intent) => {
                        intent
                    }
                    _ => {
                        resource.post_error(
                            wp_color_management_surface_v1::Error::RenderIntent,
                            "unsupported rendering intent",
                        );
                        return;
                    }
                };

                let Some(desc) = image_description
                    .data::<ImageDescriptionData>()
                    .and_then(|d| d.desc)
                else {
                    resource.post_error(
                        wp_color_management_surface_v1::Error::ImageDescription,
                        "image description is not ready",
                    );
                    return;
                };

                if set_pending_description(&surface, Some(desc), render_intent) {
                    if desc.is_hdr() {
                        debug!(surface = ?surface.id(), ?desc, "client attached an HDR image description");
                    } else {
                        trace!(surface = ?surface.id(), ?desc, "client attached an image description");
                    }
                    state.image_description_changed(&surface);
                }
            }
            Request::UnsetImageDescription => {
                let Ok(surface) = self.upgrade() else {
                    resource.post_error(
                        wp_color_management_surface_v1::Error::Inert,
                        "the underlying wl_surface was destroyed",
                    );
                    return;
                };
                if set_pending_description(&surface, None, RenderIntent::Perceptual) {
                    state.image_description_changed(&surface);
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut D,
        _client: wayland_server::backend::ClientId,
        _resource: &WpColorManagementSurfaceV1,
    ) {
        // Destroying the object does the same as unset_image_description, and allows
        // attaching a new wp_color_management_surface_v1 to the surface.
        if let Ok(surface) = self.upgrade() {
            let changed = compositor::with_states(&surface, |states| {
                if let Some(data) = states.data_map.get::<ColorManagementSurfaceData>() {
                    *data.attached.lock().unwrap() = false;
                }
                let mut guard = states.cached_state.get::<ColorManagementSurfaceCachedState>();
                let pending = guard.pending();
                let changed = pending.description.is_some();
                *pending = ColorManagementSurfaceCachedState::default();
                changed
            });
            if changed {
                state.image_description_changed(&surface);
            }
        }
    }
}

/// Stores a new pending image description on the surface. Returns whether the pending value
/// actually changed — clients (e.g. mpv) re-attach the same description every frame, and
/// callers only want to react/log on real changes.
fn set_pending_description(
    surface: &WlSurface,
    description: Option<ImageDescription>,
    render_intent: RenderIntent,
) -> bool {
    compositor::with_states(surface, |states| {
        let mut guard = states.cached_state.get::<ColorManagementSurfaceCachedState>();
        let pending = guard.pending();
        let new = ColorManagementSurfaceCachedState {
            description,
            render_intent,
        };
        if *pending == new {
            false
        } else {
            *pending = new;
            true
        }
    })
}

impl<D> Dispatch2<WpColorManagementSurfaceFeedbackV1, D> for Weak<WlSurface>
where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpColorManagementSurfaceFeedbackV1,
        request: <WpColorManagementSurfaceFeedbackV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_surface_feedback_v1::Request;
        match request {
            Request::GetPreferred { image_description }
            | Request::GetPreferredParametric { image_description } => {
                let Ok(surface) = self.upgrade() else {
                    resource.post_error(
                        wp_color_management_surface_feedback_v1::Error::Inert,
                        "the underlying wl_surface was destroyed",
                    );
                    return;
                };
                let desc = state.preferred_description_for_surface(&surface);
                make_ready_description(state, image_description, desc, data_init);
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        &self,
        _state: &mut D,
        _client: wayland_server::backend::ClientId,
        resource: &WpColorManagementSurfaceFeedbackV1,
    ) {
        if let Ok(surface) = self.upgrade() {
            compositor::with_states(&surface, |states| {
                if let Some(data) = states.data_map.get::<ColorManagementSurfaceData>() {
                    data.feedbacks.lock().unwrap().retain(|f| f != resource);
                }
            });
        }
    }
}

impl<D> Dispatch2<WpImageDescriptionCreatorParamsV1, D> for Mutex<ImageDescriptionBuilder>
where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpImageDescriptionCreatorParamsV1,
        request: <WpImageDescriptionCreatorParamsV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_image_description_creator_params_v1::{Error, Request};
        match request {
            Request::SetTfNamed { tf } => {
                let mut params = self.lock().unwrap();
                if params.transfer.is_some() {
                    resource.post_error(Error::AlreadySet, "transfer function already set");
                    return;
                }
                match tf
                    .into_result()
                    .ok()
                    .filter(|tf| state.color_management_state().supported_tfs.contains(tf))
                    // Only names advertised to this client are allowed; deprecated/newer
                    // entries were filtered out at bind time.
                    .filter(|tf| tf_visible(*tf, resource.version()))
                {
                    Some(tf) => params.transfer = Some(tf),
                    None => resource.post_error(Error::InvalidTf, "unsupported transfer function"),
                }
            }
            Request::SetPrimariesNamed { primaries } => {
                let mut params = self.lock().unwrap();
                if params.primaries.named.is_some() || params.primaries.values.is_some() {
                    resource.post_error(Error::AlreadySet, "primaries already set");
                    return;
                }
                match primaries
                    .into_result()
                    .ok()
                    .filter(|p| state.color_management_state().supported_primaries.contains(p))
                {
                    Some(p) => params.primaries.named = Some(p),
                    None => resource.post_error(Error::InvalidPrimariesNamed, "unsupported primaries"),
                }
            }
            Request::SetMasteringLuminance { min_lum, max_lum } => {
                if !state
                    .color_management_state()
                    .supported_features
                    .contains(&Feature::SetMasteringDisplayPrimaries)
                {
                    resource.post_error(
                        Error::UnsupportedFeature,
                        "set_mastering_luminance is not supported",
                    );
                    return;
                }
                // min_lum is in 0.0001 cd/m² units, max_lum in cd/m².
                if u64::from(max_lum) * 10000 <= u64::from(min_lum) {
                    resource.post_error(Error::InvalidLuminance, "max L must be greater than min L");
                    return;
                }
                self.lock().unwrap().mastering_luminance = Some((min_lum, max_lum));
            }
            Request::SetMaxCll { max_cll } => {
                self.lock().unwrap().max_cll = Some(max_cll);
            }
            Request::SetMaxFall { max_fall } => {
                self.lock().unwrap().max_fall = Some(max_fall);
            }
            Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                if !state
                    .color_management_state()
                    .supported_features
                    .contains(&Feature::SetLuminances)
                {
                    resource.post_error(Error::UnsupportedFeature, "set_luminances is not supported");
                    return;
                }
                let mut params = self.lock().unwrap();
                if params.luminances.is_some() {
                    resource.post_error(Error::AlreadySet, "luminances already set");
                    return;
                }
                // min_lum is in 0.0001 cd/m² units, max_lum and reference_lum in cd/m².
                if u64::from(max_lum) * 10000 <= u64::from(min_lum)
                    || u64::from(reference_lum) * 10000 <= u64::from(min_lum)
                {
                    resource.post_error(
                        Error::InvalidLuminance,
                        "max_lum and reference_lum must be greater than min_lum",
                    );
                    return;
                }
                params.luminances = Some((min_lum, max_lum, reference_lum));
            }
            Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if !state
                    .color_management_state()
                    .supported_features
                    .contains(&Feature::SetMasteringDisplayPrimaries)
                {
                    resource.post_error(
                        Error::UnsupportedFeature,
                        "set_mastering_display_primaries is not supported",
                    );
                    return;
                }
                let mut params = self.lock().unwrap();
                if params.mastering_primaries.is_some() {
                    resource.post_error(Error::AlreadySet, "mastering display primaries already set");
                    return;
                }
                params.mastering_primaries = Some(Chromaticities {
                    red: (r_x, r_y),
                    green: (g_x, g_y),
                    blue: (b_x, b_y),
                    white: (w_x, w_y),
                });
            }
            Request::SetTfPower { .. } => {
                resource.post_error(Error::UnsupportedFeature, "set_tf_power is not supported");
            }
            Request::SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if !state
                    .color_management_state()
                    .supported_features
                    .contains(&Feature::SetPrimaries)
                {
                    resource.post_error(Error::UnsupportedFeature, "set_primaries is not supported");
                    return;
                }
                let mut params = self.lock().unwrap();
                if params.primaries.values.is_some() || params.primaries.named.is_some() {
                    resource.post_error(Error::AlreadySet, "primaries already set");
                    return;
                }
                params.primaries.values = Some(Chromaticities {
                    red: (r_x, r_y),
                    green: (g_x, g_y),
                    blue: (b_x, b_y),
                    white: (w_x, w_y),
                });
            }
            Request::Create { image_description } => {
                let params = self.lock().unwrap();
                let Some(transfer) = params.transfer else {
                    resource.post_error(Error::IncompleteSet, "transfer function is required");
                    return;
                };
                let primaries = params.primaries;
                // Exactly one of named/raw primaries must be set (the setters enforce mutual
                // exclusion, so "both" cannot happen).
                if primaries.named.is_some() == primaries.values.is_some() {
                    resource.post_error(Error::IncompleteSet, "primaries are required");
                    return;
                }
                if let (Some(max_cll), Some(max_fall)) = (params.max_cll, params.max_fall) {
                    if max_fall > max_cll {
                        resource.post_error(
                            Error::InvalidLuminance,
                            "max_fall must be less or equal to max_cll",
                        );
                        return;
                    }
                }
                let desc = ImageDescription {
                    transfer,
                    primaries,
                    max_cll: params.max_cll,
                    max_fall: params.max_fall,
                    mastering_luminance: params.mastering_luminance,
                    mastering_primaries: params.mastering_primaries,
                    luminances: params.luminances,
                    windows_scrgb: false,
                    windows_bt2100: false,
                };
                drop(params);

                // Without extended_target_volume, the target color volume must stay inside the
                // primary color volume; the protocol recommends detecting violations and
                // failing the image description gracefully.
                if !state
                    .color_management_state()
                    .supported_features
                    .contains(&Feature::ExtendedTargetVolume)
                {
                    let exceeds_gamut = desc.mastering_primaries.is_some_and(|mastering| {
                        !Chromaticities::from_option(desc.primaries)
                            .unwrap()
                            .gamut_contains(&mastering)
                    });
                    // A reference white above the maximum luminance encodes signal levels only
                    // reachable with an extended target volume (see set_luminances).
                    let exceeds_luminance = desc
                        .luminances
                        .is_some_and(|(_, max_lum, reference_lum)| reference_lum > max_lum);
                    if exceeds_gamut || exceeds_luminance {
                        make_failed_description(
                            image_description,
                            "target color volume exceeds the primary color volume \
                             (extended_target_volume is not supported)",
                            data_init,
                        );
                        return;
                    }
                }

                make_ready_description(state, image_description, desc, data_init);
            }
            _ => {}
        }
    }
}

impl<D> Dispatch2<WpImageDescriptionV1, D> for ImageDescriptionData
where
    D: Dispatch<WpImageDescriptionInfoV1, ()>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpImageDescriptionV1,
        request: <WpImageDescriptionV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_image_description_v1::Request;
        match request {
            Request::GetInformation { information } => {
                let Some(desc) = self.desc else {
                    resource.post_error(
                        wp_image_description_v1::Error::NotReady,
                        "the image description failed and never became ready",
                    );
                    return;
                };
                // The protocol forbids get_information on descriptions from
                // create_windows_scrgb and create_windows_bt2100.
                if desc.windows_scrgb || desc.windows_bt2100 {
                    resource.post_error(
                        wp_image_description_v1::Error::NoInformation,
                        "this image description does not allow get_information",
                    );
                    return;
                }
                // The actual events (ending in the destructor `done`) are sent deferred —
                // see the handler doc.
                let info = data_init.init(information, ());
                state.schedule_image_description_info(info, desc);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D> Dispatch2<WpImageDescriptionInfoV1, D> for ()
where
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &WpImageDescriptionInfoV1,
        _request: <WpImageDescriptionInfoV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // wp_image_description_info_v1 has no requests.
    }
}

/// Initializes a `wp_image_description_v1` carrying `desc` and immediately marks it ready.
fn make_ready_description<D>(
    state: &mut D,
    image_description: New<WpImageDescriptionV1>,
    desc: ImageDescription,
    data_init: &mut DataInit<'_, D>,
) where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData> + ColorManagementHandler + 'static,
{
    let identity = state.color_management_state().identity_for(desc);
    let image = data_init.init(image_description, ImageDescriptionData { desc: Some(desc) });
    if image.version() >= 2 {
        // Identities are never recycled (the table grows monotonically), as ready2 requires.
        image.ready2(0, identity);
    } else {
        image.ready(identity);
    }
}

/// Initializes a `wp_image_description_v1` that immediately fails gracefully with the
/// `unsupported` cause; the object never becomes ready and can only be destroyed.
fn make_failed_description<D>(
    image_description: New<WpImageDescriptionV1>,
    msg: &str,
    data_init: &mut DataInit<'_, D>,
) where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData> + 'static,
{
    let image = data_init.init(image_description, ImageDescriptionData { desc: None });
    image.failed(wp_image_description_v1::Cause::Unsupported, msg.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_stable_per_description() {
        let mut state = ColorManagementState {
            supported_tfs: Vec::new(),
            supported_primaries: Vec::new(),
            supported_features: Vec::new(),
            supported_intents: Vec::new(),
            identities: Vec::new(),
            output_objects: Vec::new(),
        };

        let srgb = ImageDescription::SRGB;
        let pq = ImageDescription {
            transfer: TransferFunction::St2084Pq,
            primaries: PrimariesOption {
                named: Some(Primaries::Bt2020),
                values: None,
            },
            max_cll: Some(800),
            max_fall: Some(400),
            mastering_luminance: None,
            mastering_primaries: None,
            luminances: None,
            windows_scrgb: false,
            windows_bt2100: false,
        };

        let a = state.identity_for(srgb);
        let b = state.identity_for(pq);
        assert_ne!(a, b);
        assert_ne!(a, 0, "identity 0 is reserved by the protocol");
        assert_ne!(b, 0);
        // The same description always maps to the same identity.
        assert_eq!(state.identity_for(srgb), a);
        assert_eq!(state.identity_for(pq), b);
        // A description differing only in metadata gets its own identity.
        let pq_brighter = ImageDescription {
            max_cll: Some(1000),
            ..pq
        };
        let c = state.identity_for(pq_brighter);
        assert_ne!(c, b);
        assert_eq!(state.identity_for(pq), b);
        // Mastering primaries are part of a description's identity.
        let pq_mastered = ImageDescription {
            mastering_primaries: Some(Chromaticities::from_named(Primaries::DciP3)),
            ..pq
        };
        let d = state.identity_for(pq_mastered);
        assert_ne!(d, b);
        assert_eq!(state.identity_for(pq_mastered), d);

        // Windows scRGB is distinct from a parametric description with identical values, so
        // its tone-mapping exemption survives identity-based deduplication.
        let scrgb = ImageDescription::WINDOWS_SCRGB;
        let parametric_twin = ImageDescription {
            windows_scrgb: false,
            ..scrgb
        };
        let e = state.identity_for(scrgb);
        let f = state.identity_for(parametric_twin);
        assert_ne!(e, f);
        assert_eq!(state.identity_for(scrgb), e);
    }

    #[test]
    fn windows_scrgb_description() {
        let scrgb = ImageDescription::WINDOWS_SCRGB;
        assert!(scrgb.windows_scrgb);
        assert!(scrgb.is_hdr());
        assert_eq!(scrgb.transfer, TransferFunction::ExtLinear);
        assert_eq!(scrgb.primaries.named, Some(Primaries::Srgb));
        // Assumed BT.2020 target volume, exceeding the sRGB primary volume.
        assert_eq!(
            scrgb.mastering_primaries,
            Some(Chromaticities::from_named(Primaries::Bt2020))
        );
        // 1.0 = 80 cd/m² with an assumed 203 cd/m² reference white.
        assert_eq!(scrgb.luminances, Some((0, 80, 203)));
        // A parametric extended-linear twin is extended-range content too (what Mesa's WSI
        // creates for EXTENDED_SRGB_LINEAR swapchains), while a plain sRGB one is not.
        assert!(
            ImageDescription {
                windows_scrgb: false,
                mastering_primaries: None,
                luminances: None,
                ..scrgb
            }
            .is_hdr()
        );
        assert!(!ImageDescription::SRGB.is_hdr());
    }

    #[test]
    fn windows_bt2100_description() {
        let bt2100 = ImageDescription::WINDOWS_BT2100;
        assert!(bt2100.windows_bt2100);
        assert!(!bt2100.windows_scrgb);
        assert!(bt2100.is_hdr());
        assert_eq!(bt2100.transfer, TransferFunction::St2084Pq);
        assert_eq!(bt2100.primaries.named, Some(Primaries::Bt2020));
        // PQ luminances with the assumed 203 cd/m² reference white.
        assert_eq!(bt2100.luminances_or_default(), (50, 10_000, 203));
        assert!(bt2100.is_pq_bt2020());
        assert!(!ImageDescription::WINDOWS_SCRGB.is_pq_bt2020());
        assert!(!ImageDescription::SRGB.is_pq_bt2020());

        let hlg_bt2020 = ImageDescription {
            transfer: TransferFunction::Hlg,
            primaries: PrimariesOption {
                named: Some(Primaries::Bt2020),
                values: None,
            },
            ..ImageDescription::SRGB
        };
        assert!(hlg_bt2020.is_hdr());
        assert!(!hlg_bt2020.is_pq_bt2020());

        // Distinct from a parametric PQ/BT.2020 twin, so the tone-mapping exemption survives
        // identity-based deduplication.
        let mut state = ColorManagementState {
            supported_tfs: Vec::new(),
            supported_primaries: Vec::new(),
            supported_features: Vec::new(),
            supported_intents: Vec::new(),
            identities: Vec::new(),
            output_objects: Vec::new(),
        };
        let twin = ImageDescription {
            windows_bt2100: false,
            ..bt2100
        };
        assert_ne!(state.identity_for(bt2100), state.identity_for(twin));
    }

    #[test]
    fn version_visibility() {
        // Deprecated sRGB TFs are only advertised to v1 clients; compound_power_2_4 only
        // from v2 on.
        assert!(tf_visible(TransferFunction::Srgb, 1));
        assert!(!tf_visible(TransferFunction::Srgb, 2));
        assert!(!tf_visible(TransferFunction::ExtSrgb, 3));
        assert!(!tf_visible(TransferFunction::CompoundPower24, 1));
        assert!(tf_visible(TransferFunction::CompoundPower24, 2));
        assert!(tf_visible(TransferFunction::St2084Pq, 1));
        assert!(tf_visible(TransferFunction::St2084Pq, 3));

        // windows_bt2100 requires the v3 create request.
        assert!(!feature_visible(Feature::WindowsBt2100, 2));
        assert!(feature_visible(Feature::WindowsBt2100, 3));
        assert!(feature_visible(Feature::WindowsScrgb, 1));
        assert!(feature_visible(Feature::ExtendedTargetVolume, 1));

        // absolute_no_adaptation was added in v2.
        assert!(!intent_visible(RenderIntent::AbsoluteNoAdaptation, 1));
        assert!(intent_visible(RenderIntent::AbsoluteNoAdaptation, 2));
        assert!(intent_visible(RenderIntent::Perceptual, 1));
    }

    #[test]
    fn luminance_defaults() {
        // Explicit set_luminances values win.
        let scrgb = ImageDescription::WINDOWS_SCRGB;
        assert_eq!(scrgb.luminances_or_default(), (0, 80, 203));

        // Otherwise the transfer function implies the defaults from the protocol spec.
        let pq = ImageDescription {
            transfer: TransferFunction::St2084Pq,
            luminances: None,
            ..ImageDescription::SRGB
        };
        assert_eq!(pq.luminances_or_default(), (50, 10_000, 203));
        let hlg = ImageDescription {
            transfer: TransferFunction::Hlg,
            ..pq
        };
        assert_eq!(hlg.luminances_or_default(), (50, 1_000, 203));
        let bt1886 = ImageDescription {
            transfer: TransferFunction::Bt1886,
            ..pq
        };
        assert_eq!(bt1886.luminances_or_default(), (100, 100, 100));
        // sRGB defaults for everything else (min is in 0.0001 cd/m² units).
        assert_eq!(ImageDescription::SRGB.luminances_or_default(), (2_000, 80, 80));
    }

    #[test]
    fn custom_primaries() {
        let named_srgb = PrimariesOption {
            named: Some(Primaries::Srgb),
            values: None,
        };
        let raw_srgb = PrimariesOption {
            named: None,
            values: Some(Chromaticities::from_named(Primaries::Srgb)),
        };

        // from_option resolves named primaries through the table and raw values as-is;
        // an unset (mid-build) option resolves to nothing.
        assert_eq!(
            Chromaticities::from_option(named_srgb),
            Some(Chromaticities::from_named(Primaries::Srgb))
        );
        assert_eq!(
            Chromaticities::from_option(raw_srgb),
            Some(Chromaticities::from_named(Primaries::Srgb))
        );
        assert_eq!(Chromaticities::from_option(PrimariesOption::default()), None);

        // A description built from set_primaries with sRGB's coordinates is a different
        // record than one built from set_primaries_named(srgb), even though the resolved
        // chromaticities match: primaries_named must only be sent for the latter.
        let mut state = ColorManagementState {
            supported_tfs: Vec::new(),
            supported_primaries: Vec::new(),
            supported_features: Vec::new(),
            supported_intents: Vec::new(),
            identities: Vec::new(),
            output_objects: Vec::new(),
        };
        let named_desc = ImageDescription::SRGB;
        let raw_desc = ImageDescription {
            primaries: raw_srgb,
            ..named_desc
        };
        let a = state.identity_for(named_desc);
        let b = state.identity_for(raw_desc);
        assert_ne!(a, b);
        // The same raw coordinates dedupe to the same identity.
        assert_eq!(state.identity_for(raw_desc), b);

        // Wide-gamut detection keys off *named* BT.2020 only; raw coordinates that happen
        // to equal BT.2020 don't flag HDR.
        let raw_bt2020 = ImageDescription {
            primaries: PrimariesOption {
                named: None,
                values: Some(Chromaticities::from_named(Primaries::Bt2020)),
            },
            ..ImageDescription::SRGB
        };
        assert!(!raw_bt2020.is_hdr());
        let named_bt2020 = ImageDescription {
            primaries: PrimariesOption {
                named: Some(Primaries::Bt2020),
                values: None,
            },
            ..ImageDescription::SRGB
        };
        assert!(named_bt2020.is_hdr());

        // The extended-target-volume containment check resolves custom containers too:
        // a raw P3 container holds sRGB mastering primaries but not BT.2020 ones (this is
        // the expression the create handler evaluates).
        let raw_p3 = PrimariesOption {
            named: None,
            values: Some(Chromaticities::from_named(Primaries::DisplayP3)),
        };
        let container = Chromaticities::from_option(raw_p3).unwrap();
        assert!(container.gamut_contains(&Chromaticities::from_named(Primaries::Srgb)));
        assert!(!container.gamut_contains(&Chromaticities::from_named(Primaries::Bt2020)));
    }

    #[test]
    fn gamut_containment() {
        let srgb = Chromaticities::from_named(Primaries::Srgb);
        let bt2020 = Chromaticities::from_named(Primaries::Bt2020);
        let dci_p3 = Chromaticities::from_named(Primaries::DciP3);
        let xyz = Chromaticities::from_named(Primaries::Cie1931Xyz);

        // A gamut contains itself (all points on the triangle boundary).
        assert!(srgb.gamut_contains(&srgb));
        assert!(bt2020.gamut_contains(&bt2020));

        // Smaller gamuts are contained in wider ones, but not vice versa.
        assert!(bt2020.gamut_contains(&srgb));
        assert!(!srgb.gamut_contains(&bt2020));
        assert!(bt2020.gamut_contains(&Chromaticities::from_named(Primaries::AdobeRgb)));
        // The DCI-P3 red primary lies marginally outside the BT.2020 triangle, so P3 is not
        // a strict subset of BT.2020 (nor the other way around).
        assert!(!bt2020.gamut_contains(&dci_p3));
        assert!(!dci_p3.gamut_contains(&bt2020));

        // CIE 1931 XYZ spans the entire chromaticity diagram.
        for other in [srgb, bt2020, dci_p3, xyz] {
            assert!(xyz.gamut_contains(&other));
        }
        assert!(!srgb.gamut_contains(&xyz));
    }

    #[test]
    fn points_on_edges_are_contained() {
        let srgb = Chromaticities::from_named(Primaries::Srgb);
        // The midpoint of the red-green edge lies exactly on the triangle boundary.
        let midpoint = ((srgb.red.0 + srgb.green.0) / 2, (srgb.red.1 + srgb.green.1) / 2);
        assert!(point_in_triangle(midpoint, srgb.red, srgb.green, srgb.blue));
        // Nudged outwards (up, past the red-green edge) it is not contained.
        assert!(!point_in_triangle(
            (midpoint.0, midpoint.1 + 100_000),
            srgb.red,
            srgb.green,
            srgb.blue
        ));
        // Winding order does not matter.
        assert!(point_in_triangle(midpoint, srgb.blue, srgb.green, srgb.red));
    }

    #[test]
    fn named_chromaticities_sanity() {
        // Every named gamut contains its own white point.
        for primaries in [
            Primaries::Srgb,
            Primaries::PalM,
            Primaries::Pal,
            Primaries::Ntsc,
            Primaries::GenericFilm,
            Primaries::Bt2020,
            Primaries::Cie1931Xyz,
            Primaries::DciP3,
            Primaries::DisplayP3,
            Primaries::AdobeRgb,
        ] {
            let c = Chromaticities::from_named(primaries);
            assert!(
                point_in_triangle(c.white, c.red, c.green, c.blue),
                "white point of {primaries:?} outside its own gamut"
            );
        }

        // Display P3 shares the DCI-P3 primaries but uses the D65 white point.
        let dci_p3 = Chromaticities::from_named(Primaries::DciP3);
        let display_p3 = Chromaticities::from_named(Primaries::DisplayP3);
        assert_eq!(dci_p3.red, display_p3.red);
        assert_eq!(dci_p3.green, display_p3.green);
        assert_eq!(dci_p3.blue, display_p3.blue);
        assert_eq!(
            display_p3.white,
            Chromaticities::from_named(Primaries::Srgb).white
        );
    }
}
