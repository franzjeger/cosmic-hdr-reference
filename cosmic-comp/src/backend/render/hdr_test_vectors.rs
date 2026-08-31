//! CPU reference vectors for the constants mirrored in `offscreen.frag`.

use smithay::backend::allocator::Fourcc;

#[test]
fn hdr_postprocess_uses_sdr_intermediate_matching_channel_order() {
    assert_eq!(
        super::postprocess_intermediate_format(Fourcc::Abgr2101010, true),
        Fourcc::Abgr8888
    );
    assert_eq!(
        super::postprocess_intermediate_format(Fourcc::Argb2101010, true),
        Fourcc::Argb8888
    );
}

#[test]
fn non_ten_bit_postprocess_format_is_unchanged() {
    assert_eq!(
        super::postprocess_intermediate_format(Fourcc::Abgr2101010, false),
        Fourcc::Abgr2101010
    );
    assert_eq!(
        super::postprocess_intermediate_format(Fourcc::Abgr8888, true),
        Fourcc::Abgr8888
    );
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
    assert_eq!(
        common_block(offscreen),
        common_block(texture),
        "HDR shader math drifted between offscreen.frag and hdr_sdr_texture.frag"
    );
}
