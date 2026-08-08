use smithay::backend::renderer::gles::{GlesError, GlesRenderer, GlesTexProgram};

/// GLSL ES fragment-shader body compiled via `compile_custom_texture_shader`.
/// The `//_DEFINES` line is mandatory (smithay substitutes it with
/// `#define EXTERNAL`/`#define NO_ALPHA`/`#define DEBUG_FLAGS` as
/// needed) — see this module's doc comment for the exact contract.
const HDR_TONEMAP_FRAGMENT_SHADER: &str = r#"
precision mediump float;
//_DEFINES

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

varying vec2 v_coords;
uniform float alpha;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// ST 2084 (PQ) EOTF constants.
const float pq_m1 = 0.1593017578125;
const float pq_m2 = 78.84375;
const float pq_c1 = 0.8359375;
const float pq_c2 = 18.8515625;
const float pq_c3 = 18.6875;

// Inverse PQ EOTF: PQ-encoded [0,1] signal -> linear light, normalized so
// 1.0 == 10,000 cd/m^2 (the PQ reference range).
vec3 pq_eotf(vec3 e) {
    vec3 ep = pow(max(e, vec3(0.0)), vec3(1.0 / pq_m2));
    vec3 num = max(ep - pq_c1, vec3(0.0));
    vec3 den = pq_c2 - pq_c3 * ep;
    return pow(num / max(den, vec3(1e-6)), vec3(1.0 / pq_m1));
}

// BT.2020 -> BT.709/sRGB primaries (standard fixed 3x3, ITU-R BT.2087).
const mat3 BT2020_TO_BT709 = mat3(
     1.6605, -0.1246, -0.0182,
    -0.5876,  1.1329, -0.1006,
    -0.0728, -0.0083,  1.1187
);

// sRGB OETF (linear -> display-encoded).
vec3 srgb_oetf(vec3 c) {
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(max(c, vec3(0.0)), vec3(1.0 / 2.4)) - 0.055;
    return mix(lo, hi, step(vec3(0.0031308), c));
}

// Reference-white scale: how many PQ-linear units (out of the 10,000
// cd/m^2 PQ range) map to sRGB's 1.0. 100 cd/m^2 is the conventional SDR
// reference white used when no mastering-display metadata overrides it
// (see this file's "NOTE ON VERIFICATION" — real content-aware scaling
// is part of the not-yet-done render-loop integration, not this shader).
const float REFERENCE_WHITE_SCALE = 100.0 / 10000.0;

void main() {
    vec4 texel = texture2D(tex, v_coords);
    vec3 linear_pq = pq_eotf(texel.rgb) / REFERENCE_WHITE_SCALE;
    vec3 linear_709 = BT2020_TO_BT709 * linear_pq;
    vec3 mapped = srgb_oetf(clamp(linear_709, 0.0, 1.0));

#if defined(NO_ALPHA)
    gl_FragColor = vec4(mapped, 1.0) * alpha;
#else
    gl_FragColor = vec4(mapped, texel.a) * alpha;
#endif
#if defined(DEBUG_FLAGS)
    gl_FragColor = mix(gl_FragColor, vec4(1.0, 0.0, 1.0, 1.0), tint);
#endif
}
"#;

/// Compile the HDR tone-mapping shader once, at renderer-init time
/// (alongside `protocols::dmabuf::init_dmabuf`/`color_management::
/// init_color_management` — same call sites in render/mod.rs). Cheap to
/// call once and hold onto; expensive-ish (a real GL shader compile) to
/// call per frame, which is exactly why this returns a reusable
/// `GlesTexProgram` rather than being inlined into the render loop.
///
/// No `additional_uniforms` needed: `tex`/`alpha`/`v_coords` (and `tint`
/// under `DEBUG_FLAGS`) are provided automatically by smithay for every
/// custom texture shader per the contract documented in
/// `compile_custom_texture_shader`'s doc comment — this shader doesn't
/// need anything beyond those.
pub fn compile_hdr_tonemap_shader(renderer: &mut GlesRenderer) -> Result<GlesTexProgram, GlesError> {
    renderer.compile_custom_texture_shader(HDR_TONEMAP_FRAGMENT_SHADER, &[])
}
