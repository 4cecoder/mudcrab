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
    forward_io::{VertexOutput, FragmentOutput},
}
#import bevy_render::color_operations::linear_to_srgb
#import engine::effect_palette::{effect, effect_alpha, palette_sample, source_texel}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    var texel = vec4<f32>(1.0);
#ifdef VERTEX_UVS_A
    texel = source_texel(in.uv);
#endif
    var tint = pbr_bindings::material.base_color;
#ifdef VERTEX_COLORS
    tint *= in.color;
#endif

    // Without a palette colour the effect draws its texture times its tint, as the standard
    // material already computed.
    var color = vec4<f32>(pbr_input.material.base_color.rgb, effect_alpha(texel, tint));
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
