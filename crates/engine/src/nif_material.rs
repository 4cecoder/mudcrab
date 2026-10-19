//! Native construction of converted NIF materials using Bevy's glTF extension hooks.
use bevy::{
    asset::{LoadContext, embedded_asset},
    gltf::{
        GltfMaterial,
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
        gltf,
    },
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
};

/// Register after `DefaultPlugins`; applies converter-tagged native material semantics.
pub struct NifMaterialPlugin;

impl Plugin for NifMaterialPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<NifSourceMaterial>()
            .register_type::<ExternalEmittance>();
        app.world_mut()
            .resource_mut::<GltfExtensionHandlers>()
            .0
            .write_blocking()
            .push(Box::new(NativeMaterialHandler::default()));
    }
}

/// Registers the effect palette material and its shader. Separate from [`NifMaterialPlugin`]
/// because it needs the asset and render plugins, which the material handler's tests leave out.
pub struct EffectPaletteMaterialPlugin;

impl Plugin for EffectPaletteMaterialPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/effect_palette.wgsl");
        app.add_plugins(MaterialPlugin::<EffectPaletteMaterial>::default());
    }
}

/// A converted effect surface coloured through its greyscale palette.
pub type EffectPaletteMaterial = ExtendedMaterial<StandardMaterial, EffectPalette>;

/// The palette half of an [`EffectPaletteMaterial`]; the standard half carries the source
/// texture, base colour, UV transform and blending as for every other converted material.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct EffectPalette {
    #[uniform(100)]
    settings: EffectPaletteSettings,
    #[texture(101)]
    #[sampler(102)]
    #[dependency]
    palette: Handle<Image>,
}

#[derive(ShaderType, Reflect, Debug, Clone, Copy, PartialEq)]
struct EffectPaletteSettings {
    /// x = 1 when the palette gives the colour, y = 1 when it gives the alpha,
    /// z = the shader's base colour scale. w unused.
    lookup: Vec4,
}

impl EffectPalette {
    /// `color` and `alpha` say which channels the palette supplies; `scale` is the shader's base
    /// colour scale.
    pub fn new(palette: Handle<Image>, color: bool, alpha: bool, scale: f32) -> Self {
        Self {
            settings: EffectPaletteSettings {
                lookup: Vec4::new(
                    f32::from(u8::from(color)),
                    f32::from(u8::from(alpha)),
                    scale,
                    0.0,
                ),
            },
            palette,
        }
    }

    pub fn palette(&self) -> &Handle<Image> {
        &self.palette
    }
}

impl MaterialExtension for EffectPalette {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/effect_palette.wgsl".into()
    }
}

/// `BSEffectShaderProperty` flags 1 bits 4 and 5 (`Greyscale_To_PaletteColor` and
/// `Greyscale_To_PaletteAlpha`). The same bits mean other things on lighting shaders.
const EFFECT_PALETTE_COLOR: u64 = 1 << 4;
const EFFECT_PALETTE_ALPHA: u64 = 1 << 5;

/// What an effect material reads from its palette: the glTF texture index of the palette and
/// which channels it supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaletteLookup {
    texture: usize,
    color: bool,
    alpha: bool,
}

/// The palette lookup of a converted effect material, if its flags enable one and the converter
/// published the palette as the `greyscale` slot of `OPEN_SKYRIM_material`.
fn palette_lookup(material: &gltf::Material<'_>) -> Option<PaletteLookup> {
    let extension = material.extensions()?.get("OPEN_SKYRIM_material")?;
    if extension.get("shaderFamily")?.as_str()? != "effect" {
        return None;
    }
    let flags = extension.get("shaderFlags1")?.as_u64()?;
    let color = flags & EFFECT_PALETTE_COLOR != 0;
    let alpha = flags & EFFECT_PALETTE_ALPHA != 0;
    if !color && !alpha {
        return None;
    }
    let texture = extension
        .get("textureSlots")?
        .as_array()?
        .iter()
        .find(|slot| slot.get("semantic").and_then(|value| value.as_str()) == Some("greyscale"))?
        .get("texture")?
        .as_u64()?;
    Some(PaletteLookup {
        texture: usize::try_from(texture).ok()?,
        color,
        alpha,
    })
}

/// The effect shader's base colour scale. The converter publishes it as emission:
/// `emissiveFactor × emissiveStrength` is the base colour times the scale, so dividing by the
/// base colour's brightest channel recovers it. A black base colour publishes no emission, and
/// the scale then cannot be recovered; 1 stands in.
fn base_color_scale(material: &StandardMaterial) -> f32 {
    let base = material.base_color.to_linear();
    let peak = base.red.max(base.green).max(base.blue);
    let emissive = material.emissive;
    let scale = emissive.red.max(emissive.green).max(emissive.blue) / peak;
    if peak > 0.0 && scale.is_finite() {
        scale
    } else {
        1.0
    }
}

fn effect_palette_material(
    lookup: PaletteLookup,
    base: StandardMaterial,
    palette: Handle<Image>,
) -> EffectPaletteMaterial {
    let scale = base_color_scale(&base);
    EffectPaletteMaterial {
        base,
        extension: EffectPalette::new(palette, lookup.color, lookup.alpha, scale),
    }
}

/// Per load, Bevy clones the registered handler, so `textures` holds this glTF's images by
/// glTF texture index: Bevy reports every texture before any material. A slot Bevy never
/// reported holds a default handle, which spawn validation reports as not loaded.
#[derive(Clone, Default)]
struct NativeMaterialHandler {
    textures: Vec<Handle<Image>>,
}

impl NativeMaterialHandler {
    fn palette(&self, material: &gltf::Material<'_>) -> Option<(PaletteLookup, Handle<Image>)> {
        let lookup = palette_lookup(material)?;
        Some((lookup, self.textures.get(lookup.texture)?.clone()))
    }
}

/// The stock hook records this dependency before our hook replaces its render binding.
/// Keep its strong handle in the scene so scene-only loads remain fully loaded.
#[derive(Component, Clone, Reflect)]
#[reflect(Component)]
struct NifSourceMaterial(Handle<StandardMaterial>);

/// `SLSF1_External_Emittance` (`vendor/project-wormhole-nif/src/nif_flags.rs`, bit 29).
const SLSF1_EXTERNAL_EMITTANCE: u64 = 1 << 29;

/// Marks a primitive whose NIF shader takes its emission colour from outside the model: from the
/// placing reference's `XEMI` light or region (`crate::emittance`). Reflected so the marker is
/// cloned with the scene into every world instance.
#[derive(Component, Clone, Copy, Default, Reflect)]
#[reflect(Component, Default)]
pub struct ExternalEmittance;

/// Whether the converter published `SLSF1_External_Emittance` in the material's
/// `extras.openSkyrim.shaderFlags1`.
fn has_external_emittance(material: &gltf::Material<'_>) -> bool {
    material
        .extras()
        .as_ref()
        .and_then(|extras| serde_json::from_str::<serde_json::Value>(extras.get()).ok())
        .and_then(|extras| extras["openSkyrim"]["shaderFlags1"].as_u64())
        .is_some_and(|flags| flags & SLSF1_EXTERNAL_EMITTANCE != 0)
}

fn native_tag(material: &gltf::Material<'_>, name: &str, value: &str) -> bool {
    material
        .extras()
        .as_ref()
        .and_then(|extras| serde_json::from_str::<serde_json::Value>(extras.get()).ok())
        .is_some_and(|extras| extras["openSkyrim"][name].as_str() == Some(value))
}

fn has_normal_alpha_mask(material: &gltf::Material<'_>) -> bool {
    native_tag(material, "specularMask", "normal_alpha")
}

fn native_uv_transform(material: &gltf::Material<'_>) -> Option<bevy::math::Affine2> {
    let extras: serde_json::Value = serde_json::from_str(material.extras().as_ref()?.get()).ok()?;
    let transform = extras.pointer("/openSkyrim/uvTransform")?;
    let offset: [f32; 2] = serde_json::from_value(transform.get("offset")?.clone()).ok()?;
    let scale: [f32; 2] = serde_json::from_value(transform.get("scale")?.clone()).ok()?;
    if !offset.into_iter().chain(scale).all(f32::is_finite) {
        return None;
    }
    Some(bevy::math::Affine2::from_scale_angle_translation(
        scale.into(),
        0.0,
        offset.into(),
    ))
}

fn needs_native_material(material: &gltf::Material<'_>) -> bool {
    has_normal_alpha_mask(material)
        || native_tag(material, "normalConvention", "directx")
        || native_uv_transform(material).is_some()
}

fn native_material(
    source: &gltf::Material<'_>,
    loaded: &StandardMaterial,
) -> Option<StandardMaterial> {
    if !needs_native_material(source) {
        return None;
    }
    let mut material = loaded.clone();
    if has_normal_alpha_mask(source) && material.specular_texture.is_some() {
        // Bevy 0.19 multiplies sampled mask alpha by 0.5 in pbr_fragment.wgsl.
        // Compensate only on this fresh native material, leaving valid glTF factors
        // and generic glTF assets unchanged. No mask (including pruning) means no doubling.
        material.reflectance *= 2.0;
    }
    if native_tag(source, "normalConvention", "directx") && material.normal_map_texture.is_some() {
        // Skyrim normal Y follows increasing texture V. Bevy's generated
        // bitangent uses the opposite orientation. Keep source RGB/alpha intact.
        material.flip_normal_map_y = true;
    }
    if let Some(transform) = native_uv_transform(source) {
        // NIF transforms every texture use, even with no diffuse texture from
        // which Bevy's stock glTF loader could recover the common transform.
        material.uv_transform = transform;
    }
    Some(material)
}

impl GltfExtensionHandler for NativeMaterialHandler {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::new(self.clone())
    }

    fn on_texture(&mut self, texture: &gltf::Texture<'_>, handle: Handle<Image>) {
        let index = texture.index();
        if self.textures.len() <= index {
            self.textures.resize(index + 1, Handle::default());
        }
        self.textures[index] = handle;
    }

    fn on_material(
        &mut self,
        context: &mut LoadContext<'_>,
        source: &gltf::Material<'_>,
        _handle: Handle<GltfMaterial>,
        _loaded: &GltfMaterial,
        label: &str,
    ) {
        let palette = self.palette(source);
        if !needs_native_material(source) && palette.is_none() {
            return;
        }
        // Reuse Bevy's fully constructed standard material instead of duplicating
        // its field conversion. This handler registers after the stock PBR handler.
        let standard = context
            .get_labeled(format!("{label}/std"))
            .and_then(|asset| asset.get::<StandardMaterial>())
            .expect("NifMaterialPlugin requires the stock PBR glTF handler before it");
        let material = native_material(source, standard).unwrap_or_else(|| standard.clone());
        match palette {
            Some((lookup, image)) => {
                context.add_labeled_asset(
                    format!("{label}/nif_palette"),
                    effect_palette_material(lookup, material, image),
                );
            }
            None => {
                context.add_labeled_asset(format!("{label}/nif"), material);
            }
        }
    }

    fn on_spawn_mesh_and_material(
        &mut self,
        context: &mut LoadContext<'_>,
        _primitive: &gltf::Primitive<'_>,
        _mesh: &gltf::Mesh<'_>,
        material: &gltf::Material<'_>,
        entity: &mut EntityWorldMut,
        label: &str,
    ) {
        if has_external_emittance(material) {
            entity.insert(ExternalEmittance);
        }
        let palette = self.palette(material).is_some();
        if palette || needs_native_material(material) {
            let source = entity
                .get::<MeshMaterial3d<StandardMaterial>>()
                .expect("NifMaterialPlugin requires the stock PBR glTF handler before it")
                .0
                .clone();
            entity.insert(NifSourceMaterial(source));
            if palette {
                // A mesh draws once per material component, so the standard one has to go.
                entity
                    .remove::<MeshMaterial3d<StandardMaterial>>()
                    .insert(MeshMaterial3d(
                        context.get_label_handle::<EffectPaletteMaterial>(format!(
                            "{label}/nif_palette"
                        )),
                    ));
            } else {
                entity.insert(MeshMaterial3d(
                    context.get_label_handle::<StandardMaterial>(format!("{label}/nif")),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v15_native_uv_transform_works_without_a_diffuse_texture() {
        let document = gltf::Gltf::from_slice(
            br#"{"asset":{"version":"2.0"},"materials":[{
            "extras":{"openSkyrim":{"uvTransform":{"offset":[0.25,-0.5],"scale":[2,-3]}}}
        }]}"#,
        )
        .unwrap();
        let source = document.materials().next().unwrap();
        let loaded = StandardMaterial::default();
        let native = native_material(&source, &loaded).unwrap();
        assert_eq!(
            native.uv_transform.transform_point2(Vec2::new(0.5, 0.5)),
            Vec2::new(1.25, -2.0)
        );
        assert_eq!(loaded.uv_transform, bevy::math::Affine2::IDENTITY);
    }

    #[test]
    fn v13_native_normal_convention_is_scoped_and_preserves_mask() {
        for convention in [None, Some("directx"), Some("model_space")] {
            let document = gltf::Gltf::from_slice(
                serde_json::to_string(&serde_json::json!({
                    "asset":{"version":"2.0"}, "materials":[{
                        "extras":{"openSkyrim":{"normalConvention":convention}}
                    }]
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
            let source = document.materials().next().unwrap();
            let loaded = StandardMaterial {
                normal_map_texture: Some(Handle::default()),
                specular_texture: Some(Handle::default()),
                reflectance: 0.4,
                ..default()
            };
            let native = native_material(&source, &loaded);
            if convention == Some("directx") {
                let native = native.expect("tagged tangent normals require native construction");
                assert!(native.flip_normal_map_y);
                assert_eq!(native.reflectance, loaded.reflectance);
                assert_eq!(native.specular_texture, loaded.specular_texture);
                assert_eq!(native.normal_map_texture, loaded.normal_map_texture);
            } else {
                assert!(native.is_none());
            }
            assert!(!loaded.flip_normal_map_y);
        }
    }

    #[test]
    fn v12_scene_cloning_retains_source_and_native_material_handles() {
        let mut app = App::new();
        app.init_resource::<GltfExtensionHandlers>()
            .add_plugins(NifMaterialPlugin)
            .register_type::<MeshMaterial3d<StandardMaterial>>();
        let mut assets = Assets::<StandardMaterial>::default();
        let source = assets.add(StandardMaterial::default());
        let native = assets.add(StandardMaterial::default());
        let mut world = World::new();
        world.spawn((
            NifSourceMaterial(source.clone()),
            MeshMaterial3d(native.clone()),
        ));
        let mut cloned = WorldAsset::new(world)
            .clone_with(
                app.world()
                    .resource::<bevy::ecs::reflect::AppTypeRegistry>(),
            )
            .unwrap();
        let mut query = cloned
            .world
            .query::<(&NifSourceMaterial, &MeshMaterial3d<StandardMaterial>)>();
        let (retained, rendered) = query.single(&cloned.world).unwrap();
        assert_eq!(retained.0, source);
        assert_eq!(rendered.0, native);
    }

    fn first_material(material: serde_json::Value) -> gltf::Gltf {
        gltf::Gltf::from_slice(
            serde_json::to_string(&serde_json::json!({
                "asset": {"version": "2.0"}, "materials": [material]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap()
    }

    /// The forge's `Flames` (both palette channels) and `JetBoost` (palette colour only) from
    /// `furniture\blacksmithforgemarker.nif`, as the converter publishes them.
    #[test]
    fn palette_lookup_reads_effect_flags_and_the_greyscale_slot() {
        let effect = |flags: u64, family: &str, slots: serde_json::Value| {
            serde_json::json!({"extensions": {"OPEN_SKYRIM_material": {
                "shaderFamily": family, "shaderFlags1": flags, "textureSlots": slots
            }}})
        };
        let greyscale = serde_json::json!([{"semantic": "greyscale", "slot": 1, "texture": 2}]);
        for (material, expected) in [
            (
                effect(0xC000_0078, "effect", greyscale.clone()),
                Some(PaletteLookup {
                    texture: 2,
                    color: true,
                    alpha: true,
                }),
            ),
            (
                effect(0x8000_0058, "effect", greyscale.clone()),
                Some(PaletteLookup {
                    texture: 2,
                    color: true,
                    alpha: false,
                }),
            ),
            // The forge glow: an effect with no palette flag.
            (effect(0xC000_0008, "effect", greyscale.clone()), None),
            // Bits 4 and 5 mean other things on a lighting shader.
            (effect(0xC000_0078, "lighting", greyscale.clone()), None),
            (
                effect(
                    0xC000_0078,
                    "effect",
                    serde_json::json!([{"semantic": "detail", "slot": 1, "texture": 2}]),
                ),
                None,
            ),
            (serde_json::json!({}), None),
        ] {
            let document = first_material(material.clone());
            let source = document.materials().next().unwrap();
            assert_eq!(palette_lookup(&source), expected, "{material}");
        }
    }

    #[test]
    fn handler_hands_out_the_palette_by_gltf_texture_index() {
        let document = gltf::Gltf::from_slice(
            serde_json::to_string(&serde_json::json!({
                "asset": {"version": "2.0"},
                "images": [{"uri": "a.ktx2"}, {"uri": "b.ktx2"}, {"uri": "c.ktx2"}],
                "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
                "materials": [{"extensions": {"OPEN_SKYRIM_material": {
                    "shaderFamily": "effect", "shaderFlags1": 0x10,
                    "textureSlots": [{"semantic": "greyscale", "slot": 1, "texture": 2}]
                }}}]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let mut images = Assets::<Image>::default();
        let handles: Vec<_> = (0..3).map(|_| images.add(Image::default())).collect();
        // Reported out of order, the palette still resolves by its own index.
        let mut handler = NativeMaterialHandler::default();
        for index in [2, 0, 1] {
            let texture = document.textures().nth(index).unwrap();
            GltfExtensionHandler::on_texture(&mut handler, &texture, handles[index].clone());
        }
        let source = document.materials().next().unwrap();
        let (lookup, palette) = handler.palette(&source).unwrap();
        assert!(lookup.color && !lookup.alpha);
        assert_eq!(palette, handles[2]);
        // A slot below a reported texture that Bevy never reported stays unloadable.
        let mut sparse = NativeMaterialHandler::default();
        GltfExtensionHandler::on_texture(
            &mut sparse,
            &document.textures().nth(2).unwrap(),
            handles[2].clone(),
        );
        assert_eq!(sparse.textures[0], Handle::default());
        let empty = NativeMaterialHandler::default();
        assert!(empty.palette(&source).is_none());
    }

    #[test]
    fn base_color_scale_is_recovered_from_the_published_emission() {
        for (base, scale) in [
            (LinearRgba::rgb(1.0, 1.0, 1.0), 1.6),
            (LinearRgba::rgb(0.706, 0.706, 0.706), 1.5),
            (LinearRgba::rgb(1.0, 0.56, 0.24), 0.856),
        ] {
            let material = StandardMaterial {
                base_color: base.into(),
                emissive: base * scale,
                ..default()
            };
            assert!((base_color_scale(&material) - scale).abs() < 1e-5);
        }
        let black = StandardMaterial {
            base_color: Color::BLACK,
            emissive: LinearRgba::BLACK,
            ..default()
        };
        assert_eq!(base_color_scale(&black), 1.0);
    }

    /// The flags are those of `architecture\farmhouse\inn01.nif`: its window, then its walls.
    #[test]
    fn external_emittance_needs_bit_29_of_shader_flags_1() {
        for (extras, expected) in [
            (
                serde_json::json!({"openSkyrim": {"shaderFlags1": 0xA240_0301u32}}),
                true,
            ),
            (
                serde_json::json!({"openSkyrim": {"shaderFlags1": 0x8240_0301u32}}),
                false,
            ),
            (
                serde_json::json!({"openSkyrim": {"shaderFlags2": 0x2000_0000u32}}),
                false,
            ),
            (serde_json::json!({}), false),
        ] {
            let document = gltf::Gltf::from_slice(
                serde_json::to_string(&serde_json::json!({
                    "asset": {"version": "2.0"}, "materials": [{"extras": extras}]
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
            let material = document.materials().next().unwrap();
            assert_eq!(has_external_emittance(&material), expected, "{extras}");
        }
    }

    #[test]
    fn scene_cloning_retains_the_external_emittance_marker() {
        let mut app = App::new();
        app.init_resource::<GltfExtensionHandlers>()
            .add_plugins(NifMaterialPlugin);
        let mut world = World::new();
        world.spawn(ExternalEmittance);
        let mut cloned = WorldAsset::new(world)
            .clone_with(
                app.world()
                    .resource::<bevy::ecs::reflect::AppTypeRegistry>(),
            )
            .unwrap();
        let mut query = cloned.world.query::<&ExternalEmittance>();
        assert!(query.single(&cloned.world).is_ok());
    }

    #[test]
    fn v10_native_compensation_requires_tag_and_loaded_mask_and_does_not_compound() {
        for tagged in [false, true] {
            let document = gltf::Gltf::from_slice(serde_json::to_string(&serde_json::json!({
                "asset":{"version":"2.0"},"materials":[{
                    "extras":if tagged { serde_json::json!({"openSkyrim":{"specularMask":"normal_alpha"}}) }
                             else { serde_json::json!({}) }
                }]
            })).unwrap().as_bytes()).unwrap();
            let source = document.materials().next().unwrap();
            for mask in [false, true] {
                let loaded = StandardMaterial {
                    reflectance: 0.4,
                    specular_texture: mask.then(Handle::default),
                    ..default()
                };
                for _ in 0..2 {
                    let material = native_material(&source, &loaded);
                    if tagged {
                        assert!(
                            (material.unwrap().reflectance - if mask { 0.8 } else { 0.4 }).abs()
                                < 1e-6
                        );
                    } else {
                        assert!(material.is_none());
                    }
                    assert_eq!(loaded.reflectance, 0.4);
                }
            }
        }
    }
}
