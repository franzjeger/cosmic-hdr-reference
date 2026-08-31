// SPDX-License-Identifier: GPL-3.0-only

use std::borrow::BorrowMut;

use smithay::{
    backend::renderer::{
        ImportAll, Renderer,
        element::{
            Element, Id, Kind, RenderElement, UnderlyingStorage,
            surface::WaylandSurfaceRenderElement,
        },
        gles::{GlesFrame, UniformValue},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Transform, user_data::UserDataMap},
};

use crate::backend::render::element::AsGlowRenderer;

/// How a color-managed client surface reaches an HDR frame.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HdrSurfaceContent {
    /// The buffer carries PQ/BT.2020 electrical values. Luminance is rescaled
    /// so the content's reference white (203 cd/m² for windows_bt2100) lands
    /// on the output's reference white, like KWin's reference mapping;
    /// without it, PQ games look dim next to a brighter desktop.
    PqPassthrough { content_reference: f32 },
    /// A Windows-scRGB buffer: linear light, sRGB primaries, extended range.
    /// The factor rescales the frame's reference white so that the encoding's
    /// own reference (203 cd/m² per BT.2408) lands on the output's reference.
    ScrgbLinear { reference_scale: f32 },
}

/// A client surface with an HDR image description attached.
///
/// Ordinary compositor textures inherit an SDR-to-PQ shader during fullscreen
/// HDR presentation. This wrapper suspends that shader for PQ content, and
/// re-parameterizes it for scRGB content (no sRGB decode, no gamut stretch,
/// rescaled reference white).
#[derive(Debug)]
pub struct HdrSurfaceRenderElement<R: Renderer> {
    inner: WaylandSurfaceRenderElement<R>,
    content: HdrSurfaceContent,
}

impl<R: Renderer> HdrSurfaceRenderElement<R> {
    pub fn new(inner: WaylandSurfaceRenderElement<R>, content_reference: f32) -> Self {
        Self {
            inner,
            content: HdrSurfaceContent::PqPassthrough { content_reference },
        }
    }

    pub fn new_scrgb(inner: WaylandSurfaceRenderElement<R>, reference_scale: f32) -> Self {
        Self {
            inner,
            content: HdrSurfaceContent::ScrgbLinear { reference_scale },
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
        // Outside HDR presentation there is no override; all kinds then draw
        // plain, which composites HDR content as if it were sRGB. Acceptable
        // until windowed color management gets a dedicated path.
        let replacement = match self.content {
            HdrSurfaceContent::PqPassthrough { content_reference } => {
                saved.as_ref().map(|(program, uniforms)| {
                    let mut uniforms = uniforms.clone();
                    for uniform in &mut uniforms {
                        match uniform.name.as_ref() {
                            "hdr_input_pq" => uniform.value = UniformValue::_1f(1.0),
                            "hdr_content_reference" => {
                                uniform.value = UniformValue::_1f(content_reference)
                            }
                            _ => {}
                        }
                    }
                    (program.clone(), uniforms)
                })
            }
            HdrSurfaceContent::ScrgbLinear { reference_scale } => {
                saved.as_ref().map(|(program, uniforms)| {
                    let mut uniforms = uniforms.clone();
                    for uniform in &mut uniforms {
                        match uniform.name.as_ref() {
                            // The buffer is already linear light.
                            "hdr_sdr_gamma" => uniform.value = UniformValue::_1f(1.0),
                            // scRGB escapes the sRGB gamut numerically; the
                            // "vivid" stretch must not distort it further.
                            "hdr_gamut_stretch" => uniform.value = UniformValue::_1f(0.0),
                            "hdr_reference_white" => {
                                if let UniformValue::_1f(white) = &mut uniform.value {
                                    *white *= reference_scale;
                                }
                            }
                            _ => {}
                        }
                    }
                    (program.clone(), uniforms)
                })
            }
        };
        {
            let gles = BorrowMut::<GlesFrame>::borrow_mut(R::glow_frame_mut(frame));
            gles.set_tex_program_override(replacement);
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
