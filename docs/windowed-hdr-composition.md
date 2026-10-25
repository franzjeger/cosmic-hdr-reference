# Windowed HDR composition

Fixes franzjeger/cosmic-hdr-reference#1: windowed scRGB and PQ/BT.2020
clients were composited as if they were sRGB unless they took the fullscreen
passthrough path.

## Root cause

An HDR output renders in one of two modes:

- **Passthrough** (fullscreen or output-covering HDR client): elements are
  drawn straight into the 10-bit PQ framebuffer. The frame carries a default
  texture-program override (`hdr_sdr_texture.frag`) that encodes SDR textures
  to PQ, and `HdrSurfaceRenderElement` re-parameterizes it for PQ and scRGB
  buffers.
- **Desktop** (everything else): elements are drawn into an offscreen
  intermediate that `offscreen.frag` decodes as SDR (gamma 2.2), converts to
  BT.2020, scales to the output reference white and PQ-encodes.

In desktop mode no override exists, so `HdrSurfaceRenderElement` drew HDR
buffers with the plain texture shader. PQ code values and linear scRGB values
were written into an eight-bit intermediate and then decoded as gamma 2.2:
linear values read as gamma-encoded crush shadows and oversaturate, and
everything above 1.0 clipped. Rounded windows took the
`ClippedSurfaceRenderElement` branch, which had no HDR handling in either
mode, so reordering the branches could not help on its own.

## Composition space

The intermediate's encoding is now defined explicitly:

> Rec.709 primaries, the SDR transfer of the HDR policy (gamma 2.2 by default,
> piecewise sRGB optionally), **extended by odd symmetry**, with 1.0 at the
> output's reference white. Premultiplied alpha.

SDR content already lives in this space, so SDR rendering and blending are
bit-for-bit what they were; only the storage changes. On HDR outputs the
intermediate is `Abgr16161616f` when the renderer can render to and blend into
half-float textures (GLES 3.2, `EXT_color_buffer_half_float` or
`EXT_color_buffer_float`) and the output is rendered on its own scanout GPU.
Otherwise it stays eight-bit.

HDR buffers are converted into this space as they are drawn
(`hdr_content.frag`):

| Content | Decode | To composition space |
| --- | --- | --- |
| PQ / BT.2020 | ST 2084 EOTF | × 10000 / content reference white, BT.2020 → Rec.709 |
| Windows scRGB | none (linear) | × 80 / 203 (BT.2408 reference white) |

followed by the extended SDR encode. Highlights become values above 1.0 and
wide-gamut colors become negative components. The postprocess pass decodes
with the matching extended curve, so both survive to the PQ encode. This is
the same reference-white mapping that passthrough applies, so a window and its
fullscreen version have the same tone.

With an eight-bit intermediate (or any SDR target: SDR outputs, screen capture)
the same conversion runs and the target clamps it. HDR content then clips at
reference white and the sRGB gamut, but shadows and mid-tones are correct.

## Element selection

`HdrSurfaceContent::from_description` turns a surface's image description into
the decode parameters (PQ/BT.2020 and Windows scRGB are the only HDR encodings
the compositor advertises). Both wrappers carry them:

- `HdrSurfaceRenderElement` for unclipped surfaces.
- `ClippedSurfaceRenderElement` for rounded windows, using the clipped variant
  of `hdr_content.frag` (same rounding code as `clipped_surface.frag`, checked
  by a test).

When drawing, a wrapper decides by the frame's texture-program override:
present means the passthrough PQ framebuffer (behavior unchanged), absent means
an SDR-encoded target (the new conversion). The clipped and blur elements now
restore the override they replace instead of clearing it, so the override
reliably describes the frame for every element drawn after them.

## Why not a linear intermediate

The long-term design is a linear half-float intermediate with every element
decoded to linear and blended there, like KWin. That changes how antialiased
text, shadows, blur and translucency look on the SDR desktop, and it needs
every shader (outline, shadow, blur, clipping, solid colors, cursor) to output
linear light. The extended-SDR space gets correct HDR windows now with no
change to SDR blending, and it is a direct step toward that design: swapping
the encoding to linear is then a change to `encode_sdr`/`decode_sdr` plus the
SDR-element shaders, not to the HDR surface path.

## Known limits

- With `COSMIC_HDR_GAMUT_STRETCH` > 0 ("vivid"), the postprocess stretch also
  applies to windowed HDR content. The default (0) is colorimetric.
- Blending between HDR content and the desktop (window edges, translucency)
  happens in the gamma-2.2 domain, like all desktop blending today.
- Direct scanout of an HDR buffer to a plane of an SDR output bypasses the
  conversion, as it did before this change.
- Hybrid-GPU outputs (rendered on another GPU than the one scanning out) keep
  an eight-bit intermediate.

## Behavior changes in passthrough

`decode_sdr` is shared with the passthrough shader. Extending it by odd
symmetry means negative scRGB components (colors outside the sRGB gamut) are
now converted to BT.2020 instead of clamped to zero first. SDR inputs in
[0, 1] are unaffected.

## Hardware test checklist

1. SDR desktop on an HDR output looks identical (text, shadows, blur, panel).
2. Blender 5.3, Vulkan/Wayland, normal rounded window: correct shadows and
   saturation, viewport highlights brighter than UI white.
3. A PQ client in a window (e.g. `mpv --vo=gpu-next` with an HDR10 file): tone
   matches the same client fullscreen.
4. Fullscreen scRGB and PQ games: passthrough log line still fires and output
   matches the previous build.
5. Screenshot/screencast of an HDR window: plausible SDR image, no garbage.
6. An HDR client on an SDR monitor: SDR image clipped at white, not washed out.
