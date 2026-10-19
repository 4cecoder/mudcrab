// Prepass and shadow fragment shader of the effect palette material. Bevy's standard prepass
// tests the source texture's alpha, but a palette-alpha effect draws the palette's alpha, so an
// alpha-tested effect would write depth and cast shadow where its colour pass discards (or the
// reverse). This tests the alpha the main pass draws; the outputs follow Bevy's `pbr_prepass.wgsl`
// for an unlit material, which these effects are.
#import bevy_pbr::{
    pbr_bindings,
    pbr_functions,
    pbr_prepass_functions,
    pbr_types,
    prepass_io,
}
#import engine::effect_palette::{effect_alpha, source_texel}

fn palette_alpha_discard(in: prepass_io::VertexOutput) {
#ifdef MAY_DISCARD
    let alpha_mode = pbr_bindings::material.flags & pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_RESERVED_BITS;
    if alpha_mode != pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_MASK {
        return;
    }
    var texel = vec4<f32>(1.0);
#ifdef VERTEX_UVS_A
    texel = source_texel(in.uv);
#endif
    var tint = pbr_bindings::material.base_color;
#ifdef VERTEX_COLORS
    tint *= in.color;
#endif
    if effect_alpha(texel, tint) < pbr_bindings::material.alpha_cutoff {
        discard;
    }
#endif
}

#ifdef PREPASS_FRAGMENT
@fragment
fn fragment(in: prepass_io::VertexOutput) -> prepass_io::FragmentOutput {
#ifdef VISIBILITY_RANGE_DITHER
    pbr_functions::visibility_range_dither(in.position, in.visibility_range_dither);
#endif
    palette_alpha_discard(in);

    var out: prepass_io::FragmentOutput;
#ifdef UNCLIPPED_DEPTH_ORTHO_EMULATION
    out.frag_depth = in.unclipped_depth;
#endif
#ifdef NORMAL_PREPASS
    out.normal = vec4<f32>(in.world_normal * 0.5 + vec3<f32>(0.5), 1.0);
#endif
#ifdef MOTION_VECTOR_PREPASS
    out.motion_vector = pbr_prepass_functions::calculate_motion_vector(in.world_position, in.previous_world_position);
#endif
    return out;
}
#else
@fragment
fn fragment(in: prepass_io::VertexOutput) {
    palette_alpha_discard(in);
}
#endif
