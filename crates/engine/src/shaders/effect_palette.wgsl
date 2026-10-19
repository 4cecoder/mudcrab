// Skyrim effect shaders with Greyscale_To_PaletteColor / Greyscale_To_PaletteAlpha: the source
// texture's grey level picks a colour (and alpha) from a palette texture instead of being drawn.
// The lookup follows the retail effect pixel shader:
//   colour = base colour scale × palette(texture.g, base colour.r × vertex.r).rgb
//   alpha  = palette(texture.a, base colour.a × vertex.a).a
// Effect shaders are unlit, so the result is drawn as it is, as Bevy draws an unlit material.
#import bevy_pbr::{
    pbr_bindings,
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, main_pass_post_lighting_processing},
    pbr_types::STANDARD_MATERIAL_FLAGS_BASE_COLOR_TEXTURE_BIT,
    forward_io::{VertexOutput, FragmentOutput},
    mesh_view_bindings::view,
}
#import bevy_render::color_operations::linear_to_srgb

struct EffectPaletteSettings {
    // x = 1 when the palette gives the colour, y = 1 when it gives the alpha,
    // z = the shader's base colour scale. w unused.
    lookup: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> effect: EffectPaletteSettings;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var palette: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var palette_sampler: sampler;

fn palette_sample(x: f32, y: f32) -> vec4<f32> {
    // The game clamps the lookup; clamping to the outer texel centres also keeps a wrapping
    // sampler from blending in the opposite edge of the palette.
    let half_texel = 0.5 / vec2<f32>(textureDimensions(palette));
    let uv = clamp(vec2<f32>(x, y), half_texel, vec2<f32>(1.0) - half_texel);
    return textureSampleLevel(palette, palette_sampler, uv, 0.0);
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    var texel = vec4<f32>(1.0);
#ifdef VERTEX_UVS_A
    if (pbr_bindings::material.flags & STANDARD_MATERIAL_FLAGS_BASE_COLOR_TEXTURE_BIT) != 0u {
        let uv = (pbr_bindings::material.uv_transform * vec3<f32>(in.uv, 1.0)).xy;
        texel = textureSampleBias(
            pbr_bindings::base_color_texture,
            pbr_bindings::base_color_sampler,
            uv,
            view.mip_bias,
        );
    }
#endif
    var tint = pbr_bindings::material.base_color;
#ifdef VERTEX_COLORS
    tint *= in.color;
#endif

    // Without a palette colour the effect draws its texture times its tint, as the standard
    // material already computed.
    var color = pbr_input.material.base_color;
    if effect.lookup.y > 0.5 {
        color.a = palette_sample(texel.a, tint.a).a;
    }
    if effect.lookup.x > 0.5 {
        // The source texture is decoded from sRGB on sampling, but the game looks up the stored
        // value. The palette itself is decoded once on sampling, like any colour texture.
        let grey = linear_to_srgb(texel.rgb).g;
        color = vec4<f32>(palette_sample(grey, tint.r).rgb, color.a);
    }
    color = vec4<f32>(color.rgb * effect.lookup.z, color.a);

    pbr_input.material.base_color = alpha_discard(pbr_input.material, color);
    var out: FragmentOutput;
    out.color = main_pass_post_lighting_processing(pbr_input, pbr_input.material.base_color);
    return out;
}
