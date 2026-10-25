#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
// BEGIN HDR COMMON
// Shared HDR math for every shader in this pipeline. The block must stay
// byte-identical across the hdr shaders; hdr_test_vectors enforces that, and
// srgb_color_to_pq mirrors it on the CPU for solid colors.
uniform float hdr_reference_white;
// SDR decode: 0.0 = piecewise sRGB, otherwise a pure power gamma (2.2 matches
// how displays actually show SDR; sRGB's linear toe lifts shadows in HDR).
uniform float hdr_sdr_gamma;
// 0.0 = colorimetric 709->2020; 1.0 = keep native primaries ("vivid").
uniform float hdr_gamut_stretch;

// Linear Rec.709 to linear BT.2020. GLSL constructors are column-major.
const mat3 rec709_to_bt2020 = mat3(
    0.6274040, 0.0690970, 0.0163916,
    0.3292820, 0.9195400, 0.0880132,
    0.0433136, 0.0113612, 0.8955950
);

// Both curves are extended by odd symmetry: the desktop intermediate of an HDR
// output stores windowed HDR content as values above 1.0 (highlights) and
// below 0.0 (colors outside Rec.709), and those must survive a round trip.
// Values in [0, 1] are unaffected.
float decode_sdr(float value) {
    float magnitude = abs(value);
    float linear;
    if (hdr_sdr_gamma > 0.0) {
        linear = pow(magnitude, hdr_sdr_gamma);
    } else {
        linear = magnitude <= 0.04045
            ? magnitude / 12.92
            : pow((magnitude + 0.055) / 1.055, 2.4);
    }
    return sign(value) * linear;
}

// Inverse of decode_sdr.
float encode_sdr(float value) {
    float magnitude = abs(value);
    float encoded;
    if (hdr_sdr_gamma > 0.0) {
        encoded = pow(magnitude, 1.0 / hdr_sdr_gamma);
    } else {
        encoded = magnitude <= 0.0031308
            ? magnitude * 12.92
            : 1.055 * pow(magnitude, 1.0 / 2.4) - 0.055;
    }
    return sign(value) * encoded;
}

// ST 2084 inverse EOTF: absolute luminance in the normalized 10000 cd/m^2
// domain to a PQ code value.
float encode_pq(float value) {
    const float m1 = 0.1593017578125; // 2610 / 16384
    const float m2 = 78.84375;        // 2523 / 4096 * 128
    const float c1 = 0.8359375;       // 3424 / 4096
    const float c2 = 18.8515625;      // 2413 / 4096 * 32
    const float c3 = 18.6875;         // 2392 / 4096 * 32
    float p = pow(max(value, 0.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

// SDR (premultiplied handling done by callers) to PQ/BT.2020, with the
// reference white defining where linear SDR 1.0 lands in absolute luminance.
vec3 sdr_to_pq(vec3 rgb) {
    vec3 linear_709 = vec3(
        decode_sdr(rgb.r),
        decode_sdr(rgb.g),
        decode_sdr(rgb.b)
    );
    vec3 absolute = max(
            mix(
                rec709_to_bt2020 * linear_709,
                linear_709,
                clamp(hdr_gamut_stretch, 0.0, 1.0)
            ),
            0.0
        )
        * (clamp(hdr_reference_white, 80.0, 10000.0) / 10000.0);
    return vec3(
        encode_pq(absolute.r),
        encode_pq(absolute.g),
        encode_pq(absolute.b)
    );
}
// PQ content re-referencing: passthrough content carries its own reference
// white (203 cd/m² for windows_bt2100 per BT.2408). Scaling decoded luminance
// by hdr_reference_white / hdr_content_reference aligns the content's SDR
// level with the desktop's, like KWin's reference-white mapping. Values above
// the panel's peak are left for the panel to clip.
uniform float hdr_input_pq;          // 1.0 = buffer holds PQ code values
uniform float hdr_content_reference; // content reference white in cd/m²

// ST 2084 EOTF: PQ code value to luminance in the normalized 10000 cd/m^2
// domain (inverse of encode_pq).
float pq_to_linear(float code) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(max(code, 0.0), 1.0 / m2);
    return pow(max(p - c1, 0.0) / (c2 - c3 * p), 1.0 / m1);
}

vec3 pq_rescale(vec3 code) {
    float gain = clamp(hdr_reference_white, 80.0, 10000.0)
        / max(hdr_content_reference, 80.0);
    return vec3(
        encode_pq(pq_to_linear(code.r) * gain),
        encode_pq(pq_to_linear(code.g) * gain),
        encode_pq(pq_to_linear(code.b) * gain)
    );
}
// END HDR COMMON

varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color.a = 1.0;
#endif

    vec3 rgb = color.a > 0.00001 ? color.rgb / color.a : vec3(0.0);
    color.rgb = (hdr_input_pq > 0.5 ? pq_rescale(rgb) : sdr_to_pq(rgb)) * color.a;
    color *= alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
