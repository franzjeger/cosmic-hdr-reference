// SPDX-License-Identifier: GPL-3.0-only

use std::borrow::{Borrow, BorrowMut};

use smithay::{
    backend::renderer::{
        ImportAll, Renderer,
        element::{
            Element, Id, Kind, RenderElement, UnderlyingStorage,
            surface::WaylandSurfaceRenderElement,
        },
        gles::{
            GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
            UniformValue,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Transform, user_data::UserDataMap},
    wayland::color::management::ImageDescription,
};

use crate::backend::render::element::AsGlowRenderer;

pub static HDR_CONTENT_SHADER: &str = include_str!("../shaders/hdr_content.frag");

/// Linear BT.2020 to linear Rec.709, row-major: the inverse of the
/// `rec709_to_bt2020` matrix in the HDR shaders.
const BT2020_TO_REC709: [[f32; 3]; 3] = [
    [1.660_490_3, -0.587_639_1, -0.072_851_6],
    [-0.124_55, 1.132_899_9, -0.008_348],
    [-0.018_151_1, -0.100_578_7, 1.118_73],
];

/// `hdr_content.frag`: decodes an HDR client buffer into the SDR encoding the
/// compositor blends in. `clipped` adds the rounded-corner clip of
/// `clipped_surface.frag`.
pub struct HdrContentShader {
    pub plain: GlesTexProgram,
    pub clipped: GlesTexProgram,
}

impl HdrContentShader {
    pub fn compile(renderer: &mut GlesRenderer) -> Result<Self, GlesError> {
        let clipped_source =
            HDR_CONTENT_SHADER.replace("//_DEFINES_", "#define ROUNDED_CORNERS\n//_DEFINES_");
        Ok(Self {
            plain: renderer
                .compile_custom_texture_shader(HDR_CONTENT_SHADER, &uniform_names(false))?,
            clipped: renderer
                .compile_custom_texture_shader(clipped_source, &uniform_names(true))?,
        })
    }

    pub fn get<R: AsGlowRenderer>(renderer: &R) -> &Self {
        Borrow::<GlesRenderer>::borrow(renderer.glow_renderer())
            .egl_context()
            .user_data()
            .get::<HdrContentShader>()
            .expect("Custom Shaders not initialized")
    }
}

fn uniform_names(clipped: bool) -> Vec<UniformName<'static>> {
    let mut names = vec![
        UniformName::new("hdr_sdr_gamma", UniformType::_1f),
        UniformName::new("hdr_input_pq", UniformType::_1f),
        UniformName::new("hdr_content_to_sdr", UniformType::Matrix3x3),
    ];
    if clipped {
        names.extend([
            UniformName::new("geo_size", UniformType::_2f),
            UniformName::new("corner_radius", UniformType::_4f),
            UniformName::new("input_to_geo", UniformType::Matrix3x3),
        ]);
    }
    names
}

/// How a color-managed client buffer encodes light. PQ/BT.2020 and Windows
/// scRGB are the only HDR encodings the compositor advertises.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HdrSurfaceContent {
    /// The buffer carries PQ/BT.2020 electrical values. Luminance is rescaled
    /// so the content's reference white (203 cd/m² for windows_bt2100) lands
    /// on the output's reference white, like KWin's reference mapping;
    /// without it, PQ games look dim next to a brighter desktop.
    Pq { content_reference: f32 },
    /// A Windows-scRGB buffer: linear light, sRGB primaries, extended range.
    /// The factor rescales the frame's reference white so that the encoding's
    /// own reference (203 cd/m² per BT.2408) lands on the output's reference.
    ScrgbLinear { reference_scale: f32 },
}

impl HdrSurfaceContent {
    pub fn from_description(description: &ImageDescription) -> Option<Self> {
        if description.is_pq_bt2020() {
            let content_reference = description
                .luminances
                .map(|(_min, _max, reference)| reference.max(80) as f32)
                .unwrap_or(203.0);
            Some(Self::Pq { content_reference })
        } else if description.windows_scrgb {
            Some(Self::ScrgbLinear {
                reference_scale: scrgb_reference_scale(description),
            })
        } else {
            None
        }
    }

    /// Maps decoded buffer values (PQ: normalized to 10000 cd/m²) to linear
    /// Rec.709 with 1.0 at the content's reference white. Column-major.
    pub(in crate::backend::render) fn content_to_sdr(&self) -> [f32; 9] {
        let (scale, matrix) = match *self {
            Self::Pq { content_reference } => (10_000.0 / content_reference, BT2020_TO_REC709),
            Self::ScrgbLinear { reference_scale } => (
                reference_scale,
                [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            ),
        };
        std::array::from_fn(|i| matrix[i % 3][i / 3] * scale)
    }

    /// Uniforms of `hdr_content.frag` for drawing into an SDR-encoded target.
    pub fn sdr_uniforms(&self) -> Vec<Uniform<'static>> {
        vec![
            Uniform::new("hdr_sdr_gamma", crate::utils::env::hdr_policy().sdr_gamma),
            Uniform::new(
                "hdr_input_pq",
                if matches!(self, Self::Pq { .. }) {
                    1.0
                } else {
                    0.0
                },
            ),
            Uniform::new(
                "hdr_content_to_sdr",
                UniformValue::Matrix3x3 {
                    matrices: vec![self.content_to_sdr()],
                    transpose: false,
                },
            ),
        ]
    }

    /// Re-parameterizes the frame's SDR-to-PQ shader of HDR client
    /// passthrough for this buffer: PQ is rescaled instead of decoded as SDR;
    /// scRGB skips the sRGB decode and the gamut stretch and rescales the
    /// reference white.
    fn passthrough_uniforms(&self, uniforms: &[Uniform<'static>]) -> Vec<Uniform<'static>> {
        let mut uniforms = uniforms.to_vec();
        for uniform in &mut uniforms {
            match (*self, uniform.name.as_ref()) {
                (Self::Pq { .. }, "hdr_input_pq") => uniform.value = UniformValue::_1f(1.0),
                (Self::Pq { content_reference }, "hdr_content_reference") => {
                    uniform.value = UniformValue::_1f(content_reference)
                }
                // The buffer is already linear light.
                (Self::ScrgbLinear { .. }, "hdr_sdr_gamma") => {
                    uniform.value = UniformValue::_1f(1.0)
                }
                // scRGB escapes the sRGB gamut numerically; the "vivid"
                // stretch must not distort it further.
                (Self::ScrgbLinear { .. }, "hdr_gamut_stretch") => {
                    uniform.value = UniformValue::_1f(0.0)
                }
                (Self::ScrgbLinear { reference_scale }, "hdr_reference_white") => {
                    if let UniformValue::_1f(white) = &mut uniform.value {
                        *white *= reference_scale;
                    }
                }
                _ => {}
            }
        }
        uniforms
    }
}

/// Rescales an output's reference white for Windows-scRGB content: the
/// encoding's 1.0 is its `max` luminance (80 cd/m²) and its SDR white is the
/// `reference` (203 cd/m² per BT.2408), so mapping that reference onto the
/// output's reference keeps SDR-in-scRGB at the same brightness as native SDR.
fn scrgb_reference_scale(description: &ImageDescription) -> f32 {
    description
        .luminances
        .map(|(_min, max, reference)| {
            if reference == 0 {
                1.0
            } else {
                max as f32 / reference as f32
            }
        })
        .unwrap_or(80.0 / 203.0)
}

/// A client surface with an HDR image description attached.
///
/// How it is drawn depends on the frame. During HDR client passthrough the
/// frame carries a texture-program override that encodes SDR textures to PQ;
/// this element re-parameterizes it for its buffer. Every other frame
/// composites in SDR encoding (the desktop intermediate of an HDR output, an
/// SDR output, a capture buffer), and the buffer is decoded into it.
#[derive(Debug)]
pub struct HdrSurfaceRenderElement<R: Renderer> {
    inner: WaylandSurfaceRenderElement<R>,
    content: HdrSurfaceContent,
    sdr_program: (GlesTexProgram, Vec<Uniform<'static>>),
}

impl<R: Renderer> HdrSurfaceRenderElement<R> {
    pub fn new(
        renderer: &R,
        inner: WaylandSurfaceRenderElement<R>,
        content: HdrSurfaceContent,
    ) -> Self
    where
        R: AsGlowRenderer,
    {
        Self {
            inner,
            content,
            sdr_program: (
                HdrContentShader::get(renderer).plain.clone(),
                content.sdr_uniforms(),
            ),
        }
    }
}

impl<R> Element for HdrSurfaceRenderElement<R>
where
    R: Renderer + ImportAll,
    R::TextureId: 'static,
{
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<R> RenderElement<R> for HdrSurfaceRenderElement<R>
where
    R: Renderer + ImportAll + AsGlowRenderer,
    R::TextureId: 'static,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let saved = {
            let gles = BorrowMut::<GlesFrame>::borrow_mut(R::glow_frame_mut(frame));
            gles.take_tex_program_override()
        };
        let replacement = match &saved {
            Some((program, uniforms)) => {
                (program.clone(), self.content.passthrough_uniforms(uniforms))
            }
            None => self.sdr_program.clone(),
        };
        {
            let gles = BorrowMut::<GlesFrame>::borrow_mut(R::glow_frame_mut(frame));
            gles.set_tex_program_override(Some(replacement));
        }
        let result = self
            .inner
            .draw(frame, src, dst, damage, opaque_regions, cache);
        BorrowMut::<GlesFrame>::borrow_mut(R::glow_frame_mut(frame))
            .set_tex_program_override(saved);
        result
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

#[cfg(test)]
mod tests {
    use super::{HDR_CONTENT_SHADER, HdrSurfaceContent, uniform_names};
    use smithay::wayland::color::management::ImageDescription;

    #[test]
    fn scrgb_reference_scale_matches_windows_conventions() {
        let scale = super::scrgb_reference_scale(&ImageDescription::WINDOWS_SCRGB);
        assert!((scale - 80.0 / 203.0).abs() < 1e-6);
    }

    #[test]
    fn descriptions_select_their_decode() {
        assert_eq!(
            HdrSurfaceContent::from_description(&ImageDescription::WINDOWS_BT2100),
            Some(HdrSurfaceContent::Pq {
                content_reference: 203.0
            })
        );
        assert_eq!(
            HdrSurfaceContent::from_description(&ImageDescription::WINDOWS_SCRGB),
            Some(HdrSurfaceContent::ScrgbLinear {
                reference_scale: 80.0 / 203.0
            })
        );
        assert_eq!(
            HdrSurfaceContent::from_description(&ImageDescription::SRGB),
            None
        );
    }

    #[test]
    fn bt2020_to_rec709_inverts_shader_matrix() {
        // `rec709_to_bt2020` from the HDR shaders, row-major.
        let forward: [[f64; 3]; 3] = [
            [0.6274040, 0.3292820, 0.0433136],
            [0.0690970, 0.9195400, 0.0113612],
            [0.0163916, 0.0880132, 0.8955950],
        ];
        for (row, inverse_row) in super::BT2020_TO_REC709.iter().enumerate() {
            for column in 0..3 {
                let product: f64 = inverse_row
                    .iter()
                    .zip(&forward)
                    .map(|(inverse, forward_row)| f64::from(*inverse) * forward_row[column])
                    .sum();
                let expected = if row == column { 1.0 } else { 0.0 };
                assert!(
                    (product - expected).abs() < 1e-5,
                    "({row}, {column}) = {product}"
                );
            }
        }
    }

    #[test]
    fn content_to_sdr_is_column_major_and_scaled() {
        let matrix = HdrSurfaceContent::Pq {
            content_reference: 100.0,
        }
        .content_to_sdr();
        // Column 0 holds the BT.2020 red primary in Rec.709.
        assert!((matrix[0] - 1.660_490_3 * 100.0).abs() < 1e-3);
        assert!((matrix[1] + 0.124_55 * 100.0).abs() < 1e-3);
        assert!((matrix[3] + 0.587_639_1 * 100.0).abs() < 1e-3);

        let scrgb = HdrSurfaceContent::ScrgbLinear {
            reference_scale: 0.5,
        }
        .content_to_sdr();
        assert_eq!(scrgb, [0.5, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn sdr_uniforms_are_compiled_and_declared() {
        // Smithay rejects a draw that sets a uniform the program was not
        // compiled with, so every uniform passed must be listed and declared.
        let uniforms = HdrSurfaceContent::Pq {
            content_reference: 203.0,
        }
        .sdr_uniforms();
        for clipped in [false, true] {
            let names = uniform_names(clipped);
            for uniform in &uniforms {
                assert!(names.iter().any(|name| name.name == uniform.name));
            }
            for name in names {
                assert!(
                    HDR_CONTENT_SHADER.contains(&format!(" {};", name.name)),
                    "{} not declared",
                    name.name
                );
            }
        }
    }
}
