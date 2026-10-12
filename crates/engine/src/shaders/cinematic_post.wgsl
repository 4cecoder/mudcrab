// Post-tonemap cinematic grade: the brightness, contrast and tint of the weather's
// image space (IMGS CNAM/TNAM). Vanilla's ISHDRTonemapBlendCinematic applies these
// on display-range values after its tone curve: tint toward the tinted luminance,
// then brightness/contrast around the adapted luminance (here the fixed
// CINEMATIC_PIVOT until auto-exposure metering feeds it).
//
// Bevy's intermediate target at this point still holds linear values (the sRGB
// write conversion happens at the output), so the shader converts to display
// range, grades there, and converts back. Grading in linear range crushes most
// shadow values to black with these record values. Saturation is not here:
// Bevy applies it inside the tonemapping pass (post_saturation), immediately
// before this pass, as vanilla applies it first.

#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput

@group(0) @binding(0) var screen_texture: texture_2d<f32>;
@group(0) @binding(1) var texture_sampler: sampler;

struct SkyrimCinematicPost {
    brightness: f32,
    contrast: f32,
    tint_amount: f32,
    pivot: f32,
    tint_rgb: vec3<f32>,
    enabled: f32,
}

@group(0) @binding(2) var<uniform> settings: SkyrimCinematicPost;

// The luminance weights of the shipped image-space shaders.
const LUMA: vec3<f32> = vec3<f32>(0.2125, 0.7154, 0.0721);

fn linear_to_srgb(color: vec3<f32>) -> vec3<f32> {
    let c = max(color, vec3<f32>(0.0));
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

fn srgb_to_linear(color: vec3<f32>) -> vec3<f32> {
    let lo = color / 12.92;
    let hi = pow((color + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, color <= vec3<f32>(0.04045));
}

@fragment
fn fragment(in: FullscreenVertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(screen_texture, texture_sampler, in.uv);
    if settings.enabled < 0.5 {
        return color;
    }
    var rgb = linear_to_srgb(color.rgb);
    let luma = dot(rgb, LUMA);
    rgb = mix(rgb, vec3<f32>(luma) * settings.tint_rgb, settings.tint_amount);
    rgb = settings.contrast * (settings.brightness * rgb - vec3<f32>(settings.pivot))
        + vec3<f32>(settings.pivot);
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(srgb_to_linear(rgb), color.a);
}
