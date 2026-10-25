//! CPU reference vectors for the constants mirrored in `offscreen.frag`.

use smithay::backend::allocator::Fourcc;

use super::wayland::hdr_surface::HdrSurfaceContent;

#[test]
fn hdr_postprocess_uses_half_float_intermediate_when_renderable() {
    for output_format in [Fourcc::Abgr2101010, Fourcc::Argb2101010, Fourcc::Abgr8888] {
        assert_eq!(
            super::postprocess_intermediate_format(output_format, true, true),
            Fourcc::Abgr16161616f
        );
    }
}

#[test]
fn hdr_postprocess_falls_back_to_sdr_intermediate_matching_channel_order() {
    assert_eq!(
        super::postprocess_intermediate_format(Fourcc::Abgr2101010, true, false),
        Fourcc::Abgr8888
    );
    assert_eq!(
        super::postprocess_intermediate_format(Fourcc::Argb2101010, true, false),
        Fourcc::Argb8888
    );
}

#[test]
fn non_ten_bit_postprocess_format_is_unchanged() {
    for half_float in [false, true] {
        assert_eq!(
            super::postprocess_intermediate_format(Fourcc::Abgr2101010, false, half_float),
            Fourcc::Abgr2101010
        );
    }
    assert_eq!(
        super::postprocess_intermediate_format(Fourcc::Abgr8888, true, false),
        Fourcc::Abgr8888
    );
}

#[test]
fn half_float_render_targets_need_gles_3_2_or_an_extension() {
    let supported = super::gles_supports_half_float_render_target;
    assert!(supported("OpenGL ES 3.2 NVIDIA 580.95.05", ""));
    assert!(supported("OpenGL ES 3.2 Mesa 25.2.4", ""));
    assert!(supported(
        "OpenGL ES 3.0 Mesa 25.2.4",
        "GL_OES_rgb8_rgba8 GL_EXT_color_buffer_float GL_KHR_debug"
    ));
    assert!(supported(
        "OpenGL ES 3.1 v1.r38p1",
        "GL_EXT_color_buffer_half_float"
    ));
    assert!(!supported(
        "OpenGL ES 3.1 v1.r38p1",
        "GL_EXT_color_buffer_float_rgb"
    ));
    // smithay allocates no half-float textures on GLES 2.
    assert!(!supported(
        "OpenGL ES 2.0 Mesa",
        "GL_EXT_color_buffer_half_float"
    ));
    assert!(!supported("4.6 (Core Profile) Mesa 25.2.4", ""));
    assert!(!supported("", ""));
}

fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn rec709_to_bt2020(rgb: [f64; 3]) -> [f64; 3] {
    [
        0.6274040 * rgb[0] + 0.3292820 * rgb[1] + 0.0433136 * rgb[2],
        0.0690970 * rgb[0] + 0.9195400 * rgb[1] + 0.0113612 * rgb[2],
        0.0163916 * rgb[0] + 0.0880132 * rgb[1] + 0.8955950 * rgb[2],
    ]
}

/// Mirror of `encode_sdr` in the HDR shaders (the inverse of `decode_sdr`).
fn encode_sdr(value: f32, gamma: f32) -> f32 {
    let magnitude = value.abs();
    let encoded = if gamma > 0.0 {
        magnitude.powf(1.0 / gamma)
    } else if magnitude <= 0.0031308 {
        magnitude * 12.92
    } else {
        1.055 * magnitude.powf(1.0 / 2.4) - 0.055
    };
    encoded.copysign(value)
}

/// Mirror of `pq_to_linear` in the HDR shaders: normalized to 10000 cd/m².
fn st2084_decode(code: f64) -> f64 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = code.max(0.0).powf(1.0 / m2);
    ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
}

fn st2084_encode(luminance_nits: f64) -> f64 {
    let y = (luminance_nits / 10_000.0).max(0.0);
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = y.powf(m1);
    ((c1 + c2 * p) / (1.0 + c3 * p)).powf(m2)
}

#[test]
fn srgb_endpoints_decode_to_linear_endpoints() {
    assert_eq!(srgb_to_linear(0.0), 0.0);
    assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-12);
}

#[test]
fn rec709_to_bt2020_preserves_neutral_white() {
    let white = rec709_to_bt2020([1.0, 1.0, 1.0]);
    assert!(
        white
            .into_iter()
            .all(|channel| (channel - 1.0).abs() < 2e-6)
    );
}

#[test]
fn st2084_matches_reference_code_values() {
    // Published implementations commonly use these rounded code values.
    assert!((st2084_encode(100.0) - 0.5081).abs() < 0.0002);
    assert!((st2084_encode(203.0) - 0.5807).abs() < 0.0002);
    assert!((st2084_encode(1_000.0) - 0.7518).abs() < 0.0002);
    assert!((st2084_encode(10_000.0) - 1.0).abs() < 1e-12);
}

#[test]
fn solid_color_hdr_path_matches_shader_white_point() {
    let white = super::srgb_color_to_pq(
        smithay::backend::renderer::Color32F::new(1.0, 1.0, 1.0, 1.0),
        203.0,
    );
    assert!((white.r() - 0.5807).abs() < 0.0003);
    assert!((white.r() - white.g()).abs() < 0.0001);
    assert!((white.g() - white.b()).abs() < 0.0001);

    let transparent = super::srgb_color_to_pq(
        smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 0.0),
        203.0,
    );
    assert_eq!(transparent.a(), 0.0);
    assert!(transparent.r().is_finite());
}

#[test]
fn gamma22_decode_keeps_shadows_darker_than_srgb_toe() {
    // sRGB's linear toe lifts near-black values well above what a gamma-2.2
    // display would show; that lift is the "washed out" HDR desktop look.
    let srgb = super::decode_sdr(0.03, 0.0);
    let gamma = super::decode_sdr(0.03, 2.2);
    assert!((srgb - 0.03 / 12.92).abs() < 1e-6);
    assert!(gamma < srgb / 2.0, "gamma22={gamma} srgb={srgb}");

    // The end points agree, so reference white is unaffected by the choice.
    assert_eq!(super::decode_sdr(0.0, 2.2), 0.0);
    assert!((super::decode_sdr(1.0, 2.2) - 1.0).abs() < 1e-6);
    let white = super::sdr_color_to_pq(
        smithay::backend::renderer::Color32F::new(1.0, 1.0, 1.0, 1.0),
        203.0,
        2.2,
        0.0,
    );
    assert!((white.r() - 0.5807).abs() < 0.0003);
}

#[test]
fn extended_sdr_transfer_round_trips() {
    for gamma in [2.2, 0.0] {
        for linear in [-0.6, -0.003, 0.0, 0.002, 0.03, 0.5, 1.0, 4.9, 49.0] {
            let encoded = encode_sdr(linear, gamma);
            let decoded = super::decode_sdr(encoded, gamma);
            assert!(
                (decoded - linear).abs() <= 1e-5 * linear.abs().max(1.0),
                "gamma={gamma} linear={linear} decoded={decoded}"
            );
            assert_eq!(super::decode_sdr(-encoded, gamma), -decoded);
        }
    }
}

#[test]
fn extended_decode_matches_sdr_decode_in_range() {
    // SDR content never leaves [0, 1]; extending the curve must not move it.
    for step in 0..=255 {
        let value = step as f32 / 255.0;
        let srgb = if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        };
        assert_eq!(super::decode_sdr(value, 0.0), srgb);
        assert_eq!(super::decode_sdr(value, 2.2), value.powf(2.2));
    }
}

/// The desktop path for one gray channel: decode the buffer, map it into the
/// intermediate, then run the postprocess decode and PQ encode.
fn windowed_pq_code(content: HdrSurfaceContent, value: f64, output_reference: f64) -> f64 {
    let gamma = 2.2;
    let decoded = match content {
        HdrSurfaceContent::Pq { .. } => st2084_decode(value),
        HdrSurfaceContent::ScrgbLinear { .. } => value,
    };
    let matrix = content.content_to_sdr();
    // A neutral input stays neutral; any row of the matrix gives the gain.
    let relative = decoded as f32 * (matrix[0] + matrix[3] + matrix[6]);
    let intermediate = encode_sdr(relative, gamma);
    let linear = super::decode_sdr(intermediate, gamma) as f64;
    st2084_encode(linear * output_reference)
}

#[test]
fn windowed_pq_matches_passthrough_reference_mapping() {
    // Passthrough maps 1000 cd/m² content mastered at a 203 cd/m² reference
    // to 1000 * 300 / 203 cd/m² on a 300 cd/m² desktop; so must a window.
    let content = HdrSurfaceContent::Pq {
        content_reference: 203.0,
    };
    let code = st2084_encode(1_000.0);
    let expected = st2084_encode(1_000.0 * 300.0 / 203.0);
    let windowed = windowed_pq_code(content, code, 300.0);
    assert!(
        (windowed - expected).abs() < 2e-4,
        "windowed={windowed} expected={expected}"
    );

    // Content reference white lands exactly on the desktop's.
    let white = windowed_pq_code(content, st2084_encode(203.0), 300.0);
    assert!((white - st2084_encode(300.0)).abs() < 2e-4);
}

#[test]
fn windowed_scrgb_reference_white_matches_sdr_white() {
    // scRGB 2.5375 is 203 cd/m², the encoding's assumed reference white.
    let content = HdrSurfaceContent::ScrgbLinear {
        reference_scale: 80.0 / 203.0,
    };
    let white = windowed_pq_code(content, 2.5375, 203.0);
    assert!((white - 0.5807).abs() < 3e-4);
    // Highlights keep their ratio to reference white.
    let highlight = windowed_pq_code(content, 12.5, 203.0);
    assert!((highlight - st2084_encode(1_000.0)).abs() < 3e-4);
}

#[test]
fn windowed_bt2020_primaries_survive_the_rec709_intermediate() {
    // Pure BT.2020 green lies outside Rec.709. In the intermediate it has
    // negative components; the postprocess must recover it exactly.
    let content = HdrSurfaceContent::Pq {
        content_reference: 203.0,
    };
    let matrix = content.content_to_sdr();
    let linear_2020 = [0.0_f32, 0.0203, 0.0];
    let rec709: [f32; 3] =
        std::array::from_fn(|row| (0..3).map(|k| matrix[k * 3 + row] * linear_2020[k]).sum());
    assert!(rec709[0] < 0.0 && rec709[2] < 0.0);
    let decoded: [f32; 3] =
        std::array::from_fn(|c| super::decode_sdr(encode_sdr(rec709[c], 2.2), 2.2));
    let back = rec709_to_bt2020(decoded.map(f64::from));
    assert!(back[0].abs() < 1e-4 && back[2].abs() < 1e-4, "{back:?}");
    assert!((back[1] - 1.0).abs() < 1e-3, "{back:?}");
}

#[test]
fn gamut_stretch_moves_primaries_toward_native() {
    let red = smithay::backend::renderer::Color32F::new(1.0, 0.0, 0.0, 1.0);
    let colorimetric = super::sdr_color_to_pq(red, 203.0, 2.2, 0.0);
    let native = super::sdr_color_to_pq(red, 203.0, 2.2, 1.0);
    // Colorimetric red carries green/blue energy in the BT.2020 container;
    // native ("vivid") keeps the full red channel and none elsewhere.
    assert!(native.r() > colorimetric.r());
    assert!(colorimetric.g() > native.g());
    assert!(native.g() < 1e-4 && native.b() < 1e-4);
    // Neutral white is unaffected by the stretch.
    let white = smithay::backend::renderer::Color32F::new(1.0, 1.0, 1.0, 1.0);
    let a = super::sdr_color_to_pq(white, 203.0, 2.2, 0.0);
    let b = super::sdr_color_to_pq(white, 203.0, 2.2, 1.0);
    assert!((a.r() - b.r()).abs() < 1e-4);
}

#[test]
fn shader_common_blocks_are_identical() {
    fn common_block(source: &str) -> &str {
        let start = source
            .find("// BEGIN HDR COMMON")
            .expect("shader lacks HDR common block");
        let end = source
            .find("// END HDR COMMON")
            .expect("shader lacks HDR common block terminator");
        &source[start..end]
    }
    let offscreen = include_str!("shaders/offscreen.frag");
    let texture = include_str!("shaders/hdr_sdr_texture.frag");
    let content = include_str!("shaders/hdr_content.frag");
    assert_eq!(
        common_block(offscreen),
        common_block(texture),
        "HDR shader math drifted between offscreen.frag and hdr_sdr_texture.frag"
    );
    assert_eq!(
        common_block(offscreen),
        common_block(content),
        "HDR shader math drifted between offscreen.frag and hdr_content.frag"
    );
}

#[test]
fn rounded_corner_blocks_are_identical() {
    fn rounded_block(source: &str) -> &str {
        let start = source
            .find("// BEGIN ROUNDED CORNERS")
            .expect("shader lacks rounded-corner block");
        let end = source
            .find("// END ROUNDED CORNERS")
            .expect("shader lacks rounded-corner block terminator");
        &source[start..end]
    }
    assert_eq!(
        rounded_block(include_str!("shaders/clipped_surface.frag")),
        rounded_block(include_str!("shaders/hdr_content.frag")),
        "corner rounding drifted between clipped_surface.frag and hdr_content.frag"
    );
}
