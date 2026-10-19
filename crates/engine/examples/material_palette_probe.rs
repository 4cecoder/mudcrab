//! Converter → Bevy glTF loader → GPU effect palette check. No Skyrim assets required.
//! --output <directory> [--interior] [--standard-material] (negative control).
use bevy::{
    camera::{RenderTarget, ScalingMode},
    core_pipeline::tonemapping::DebandDither,
    prelude::*,
    render::{
        RenderPlugin,
        settings::{RenderCreation, WgpuSettings, WgpuSettingsPriority},
        view::screenshot::{Screenshot, ScreenshotCaptured},
    },
    window::{ExitCondition, WindowPlugin},
};
use converter::{
    material::{
        NifAlphaMode, NifMaterialDisposition, NifShaderFamily, NifShapeMaterial,
        NifTextureSemantic, NifTextureSlot, ValidatedNifMaterial, publish_gltf_materials,
    },
    texture::{TextureConverter, TextureEncoding},
};
use ddsfile::{AlphaMode, D3D10ResourceDimension, Dds, DxgiFormat, NewDxgiParams};
use engine::{
    color_pipeline::SceneColorPipeline,
    nif_material::{EffectPaletteMaterial, EffectPaletteMaterialPlugin, NifMaterialPlugin},
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

const PALETTE_COLOR: u32 = 1 << 4;
const PALETTE_ALPHA: u32 = 1 << 5;
/// 4×4 palette. Red and alpha rise along U, green along V, so a wrong coordinate shows in a
/// channel of its own; blue marks the palette apart from the source texture.
const PALETTE_SIZE: usize = 4;

fn palette_texel(x: usize, y: usize) -> [u8; 4] {
    [40 + 60 * x as u8, 30 + 70 * y as u8, 128, 20 + 70 * x as u8]
}

struct Case {
    name: &'static str,
    flags: u32,
    blend: bool,
    /// The source texture's stored grey level and alpha.
    texel: [u8; 2],
    /// Base colour, the second palette coordinate's source.
    base: [f32; 4],
    /// Base colour scale.
    scale: f32,
}

const CASES: [Case; 5] = [
    Case {
        name: "colour_mid",
        flags: PALETTE_COLOR,
        blend: false,
        texel: [128, 255],
        base: [0.5, 0.5, 0.5, 1.0],
        scale: 1.0,
    },
    Case {
        // The row comes from the base colour's red channel only.
        name: "colour_hdr_low_row",
        flags: PALETTE_COLOR,
        blend: false,
        texel: [200, 255],
        base: [0.1, 0.4, 0.9, 1.0],
        scale: 1.6,
    },
    Case {
        name: "colour_clamped_corner",
        flags: PALETTE_COLOR,
        blend: false,
        texel: [255, 255],
        base: [1.0, 1.0, 1.0, 1.0],
        scale: 0.75,
    },
    Case {
        // The forge's Flames: both channels from the palette, alpha blended.
        name: "colour_and_alpha_blend",
        flags: PALETTE_COLOR | PALETTE_ALPHA,
        blend: true,
        texel: [60, 180],
        base: [0.8, 0.8, 0.8, 0.3],
        scale: 1.2,
    },
    Case {
        name: "alpha_only_blend",
        flags: PALETTE_ALPHA,
        blend: true,
        texel: [150, 100],
        base: [0.6, 0.3, 0.9, 0.7],
        scale: 1.0,
    },
];

fn srgb_decode(byte: u8) -> f32 {
    Color::srgb_u8(byte, 0, 0).to_linear().red
}

/// Bilinear filtering of the decoded palette, the way the GPU filters an sRGB view, with the
/// coordinates clamped to the outer texel centres as the shader clamps them.
fn palette_lookup(u: f32, v: f32) -> [f32; 4] {
    let axis = |coordinate: f32| {
        let half = 0.5 / PALETTE_SIZE as f32;
        let x = coordinate.clamp(half, 1.0 - half) * PALETTE_SIZE as f32 - 0.5;
        let low = (x.floor() as usize).min(PALETTE_SIZE - 1);
        (low, (low + 1).min(PALETTE_SIZE - 1), x - low as f32)
    };
    let (x0, x1, tx) = axis(u);
    let (y0, y1, ty) = axis(v);
    let texel = |x, y| {
        let [r, g, b, a] = palette_texel(x, y);
        [
            srgb_decode(r),
            srgb_decode(g),
            srgb_decode(b),
            f32::from(a) / 255.0,
        ]
    };
    let lerp = |a: [f32; 4], b: [f32; 4], t: f32| std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t);
    lerp(
        lerp(texel(x0, y0), texel(x1, y0), tx),
        lerp(texel(x0, y1), texel(x1, y1), tx),
        ty,
    )
}

/// What the retail effect shader draws for `case`, as linear RGBA.
fn expected(case: &Case) -> [f32; 4] {
    let grey = f32::from(case.texel[0]) / 255.0;
    let texel_alpha = f32::from(case.texel[1]) / 255.0;
    let tint = case.base;
    let mut color = [
        tint[0] * srgb_decode(case.texel[0]),
        tint[1] * srgb_decode(case.texel[0]),
        tint[2] * srgb_decode(case.texel[0]),
        tint[3] * texel_alpha,
    ];
    if case.flags & PALETTE_ALPHA != 0 {
        color[3] = palette_lookup(texel_alpha, tint[3])[3];
    }
    if case.flags & PALETTE_COLOR != 0 {
        let palette = palette_lookup(grey, tint[0]);
        color[..3].copy_from_slice(&palette[..3]);
    }
    [
        color[0] * case.scale,
        color[1] * case.scale,
        color[2] * case.scale,
        if case.blend { color[3] } else { 1.0 },
    ]
}

#[derive(Resource)]
struct Probe {
    output: PathBuf,
    interior: bool,
    standard: bool,
    started: Instant,
    palettes: Vec<Handle<EffectPaletteMaterial>>,
    standards: Vec<Handle<StandardMaterial>>,
    target: Handle<Image>,
    spawned: bool,
    frames: u32,
    requested: bool,
    loaded: Vec<serde_json::Value>,
    loader_passed: bool,
}

fn write_texture(
    path: PathBuf,
    width: usize,
    height: usize,
    data: Vec<u8>,
    encoding: TextureEncoding,
) {
    let mut dds = Dds::new_dxgi(NewDxgiParams {
        height: height as u32,
        width: width as u32,
        depth: None,
        format: DxgiFormat::R8G8B8A8_UNorm,
        mipmap_levels: None,
        array_layers: None,
        caps2: None,
        is_cubemap: false,
        resource_dimension: D3D10ResourceDimension::Texture2D,
        alpha_mode: AlphaMode::Straight,
    })
    .unwrap();
    dds.data = data;
    let mut bytes = Vec::new();
    dds.write(&mut bytes).unwrap();
    std::fs::write(
        path,
        TextureConverter::convert_uncompressed(&bytes, encoding).unwrap(),
    )
    .unwrap();
}

fn fixtures(output: &std::path::Path) -> PathBuf {
    let assets = output.join("fixtures");
    let textures = assets.join("textures");
    std::fs::create_dir_all(&textures).unwrap();
    let palette = (0..PALETTE_SIZE)
        .flat_map(|y| (0..PALETTE_SIZE).flat_map(move |x| palette_texel(x, y)))
        .collect();
    write_texture(
        textures.join("palette.ktx2"),
        PALETTE_SIZE,
        PALETTE_SIZE,
        palette,
        TextureEncoding::DataLinear,
    );
    for (index, case) in CASES.iter().enumerate() {
        let [grey, alpha] = case.texel;
        write_texture(
            textures.join(format!("source{index}.opensky-srgb.ktx2")),
            4,
            4,
            [grey, grey, grey, alpha].repeat(16),
            TextureEncoding::ColorSrgb,
        );
    }
    let contract: Vec<_> = CASES
        .iter()
        .enumerate()
        .map(|(index, case)| {
            let material = ValidatedNifMaterial {
                uv_offset: [0.0; 2],
                uv_scale: [1.0; 2],
                texture_clamp_mode: 3,
                shader_family: NifShaderFamily::Effect,
                lighting_shader_type: None,
                shader_block: index as u32,
                texture_set_block: None,
                alpha_property_block: None,
                shader_flags_1: case.flags,
                shader_flags_2: 0,
                base_color: case.base,
                alpha: case.base[3],
                alpha_mode: if case.blend {
                    NifAlphaMode::Blend
                } else {
                    NifAlphaMode::Opaque
                },
                alpha_threshold: None,
                glossiness: 0.0,
                specular_color: [0.0; 3],
                specular_strength: 0.0,
                emissive_color: [case.base[0], case.base[1], case.base[2]],
                emissive_multiple: case.scale,
                double_sided: false,
                textures: vec![
                    NifTextureSlot {
                        slot: 0,
                        semantic: NifTextureSemantic::Diffuse,
                        path: format!("textures/source{index}.dds"),
                        required: true,
                    },
                    NifTextureSlot {
                        slot: 1,
                        semantic: NifTextureSemantic::Greyscale,
                        path: "textures/palette.dds".into(),
                        required: false,
                    },
                ],
            };
            NifShapeMaterial {
                shape_block: index as u32,
                shape_name: Some(case.name.into()),
                shader_property_block: Some(index as u32),
                alpha_property_block: None,
                disposition: NifMaterialDisposition::Validated { material },
            }
        })
        .collect();
    let mut document = serde_json::json!({"asset":{"version":"2.0"}, "meshes": CASES.iter().map(|_| serde_json::json!({"primitives":[{}]})).collect::<Vec<_>>()});
    publish_gltf_materials(
        &mut document,
        &contract,
        &(0..CASES.len() as u32).collect::<Vec<_>>(),
        std::path::Path::new("materials.glb"),
    )
    .unwrap();
    // Load the published materials on diagnostic rectangles; geometry is not under test.
    document.as_object_mut().unwrap().remove("meshes");
    std::fs::write(
        assets.join("materials.gltf"),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    assets
}

fn main() {
    let mut output = PathBuf::from("material-palette-probe");
    let mut interior = false;
    let mut standard = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = args.next().expect("--output requires a directory").into(),
            "--interior" => interior = true,
            "--standard-material" => standard = true,
            _ => panic!("unknown option {arg}"),
        }
    }
    std::fs::create_dir_all(&output).unwrap();
    let output = output.canonicalize().unwrap();
    for name in ["probe.json", "probe.png"] {
        let path = output.join(name);
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let assets = fixtures(&output);
    let mut app = App::new();
    app.insert_resource(GlobalAmbientLight {
        brightness: 0.0,
        ..default()
    })
    .add_plugins(
        DefaultPlugins
            .set(AssetPlugin {
                file_path: assets.to_string_lossy().into(),
                ..default()
            })
            .set(RenderPlugin {
                render_creation: RenderCreation::Automatic(Box::new(WgpuSettings {
                    priority: WgpuSettingsPriority::WebGPU,
                    ..default()
                })),
                ..default()
            })
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .disable::<bevy::winit::WinitPlugin>()
            .disable::<bevy::audio::AudioPlugin>(),
    )
    .add_plugins((NifMaterialPlugin, EffectPaletteMaterialPlugin))
    .add_plugins(bevy::app::ScheduleRunnerPlugin::run_loop(
        Duration::from_millis(16),
    ));
    // The production handler publishes the palette material under the /nif_palette label; the
    // negative control draws the stock /std material the engine drew before.
    let server = app.world().resource::<AssetServer>().clone();
    let palettes = (0..CASES.len())
        .map(|index| server.load(format!("materials.gltf#Material{index}/nif_palette")))
        .collect();
    let standards = (0..CASES.len())
        .map(|index| server.load(format!("materials.gltf#Material{index}/std")))
        .collect();
    let target = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new_target_texture(
            800,
            CASES.len() as u32 * 100,
            bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
            None,
        ));
    let result = app
        .insert_resource(Probe {
            output,
            interior,
            standard,
            started: Instant::now(),
            palettes,
            standards,
            target,
            spawned: false,
            frames: 0,
            requested: false,
            loaded: vec![],
            loader_passed: true,
        })
        .add_systems(Update, render_and_capture)
        .run();
    if !result.is_success() {
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_and_capture(
    mut commands: Commands,
    mut probe: ResMut<Probe>,
    palettes: Res<Assets<EffectPaletteMaterial>>,
    mut standards: ResMut<Assets<StandardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    images: Res<Assets<Image>>,
    mut exit: MessageWriter<AppExit>,
) {
    if probe.started.elapsed() > Duration::from_secs(90) {
        eprintln!("material load/render timed out");
        exit.write(AppExit::error());
        return;
    }
    if !probe.spawned {
        let Some(loaded) = probe
            .palettes
            .iter()
            .map(|handle| palettes.get(handle))
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        if probe.standards.iter().any(|h| standards.get(h).is_none()) {
            return;
        }
        let dependencies = loaded.iter().flat_map(|material| {
            [
                material.base.base_color_texture.clone(),
                Some(material.extension.palette().clone()),
            ]
        });
        if dependencies.flatten().any(|h| images.get(&h).is_none()) {
            return;
        }
        for (case, material) in CASES.iter().zip(&loaded) {
            let format = format!(
                "{:?}",
                images
                    .get(material.extension.palette())
                    .unwrap()
                    .texture_descriptor
                    .format
            );
            // The shader takes the palette colour as decoded on sampling.
            let passed = format == "Rgba8UnormSrgb";
            probe.loader_passed &= passed;
            probe.loaded.push(
                serde_json::json!({"case":case.name,"palette_format":format,"passed":passed}),
            );
        }
        let quad = meshes.add(Rectangle::new(3.0, 0.8));
        for (index, case) in CASES.iter().enumerate() {
            let [r, g, b, a] = expected(case);
            let alpha_mode = if case.blend {
                bevy::prelude::AlphaMode::Blend
            } else {
                bevy::prelude::AlphaMode::Opaque
            };
            let reference = standards.add(StandardMaterial {
                base_color: LinearRgba::new(r, g, b, a).into(),
                unlit: true,
                alpha_mode,
                ..default()
            });
            let y = (CASES.len() as f32 - 1.0) * 0.5 - index as f32;
            if probe.standard {
                commands.spawn((
                    Mesh3d(quad.clone()),
                    MeshMaterial3d(probe.standards[index].clone()),
                    Transform::from_xyz(-2.0, y, 0.0),
                ));
            } else {
                commands.spawn((
                    Mesh3d(quad.clone()),
                    MeshMaterial3d(probe.palettes[index].clone()),
                    Transform::from_xyz(-2.0, y, 0.0),
                ));
            }
            commands.spawn((
                Mesh3d(quad.clone()),
                MeshMaterial3d(reference),
                Transform::from_xyz(2.0, y, 0.0),
            ));
        }
        commands.spawn((
            Camera3d::default(),
            RenderTarget::Image(probe.target.clone().into()),
            SceneColorPipeline::default(),
            DebandDither::Disabled,
            Msaa::Off,
            Camera {
                clear_color: ClearColorConfig::Custom(if probe.interior {
                    Color::BLACK
                } else {
                    Color::srgb(0.12, 0.18, 0.25)
                }),
                ..default()
            },
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::FixedVertical {
                    viewport_height: CASES.len() as f32,
                },
                ..OrthographicProjection::default_3d()
            }),
            Transform::from_xyz(0.0, 0.0, 10.0),
        ));
        probe.spawned = true;
    }
    probe.frames += 1;
    if probe.frames >= 90 && !probe.requested {
        probe.requested = true;
        commands
            .spawn(Screenshot::image(probe.target.clone()))
            .observe(evaluate);
    }
}

fn evaluate(
    event: On<ScreenshotCaptured>,
    probe: Res<Probe>,
    adapter: Res<bevy::render::renderer::RenderAdapterInfo>,
    mut exit: MessageWriter<AppExit>,
) {
    let image = event.image.clone().try_into_dynamic().unwrap().to_rgb8();
    image.save(probe.output.join("probe.png")).unwrap();
    let mut passed = probe.loader_passed;
    let mut samples = Vec::new();
    for (index, case) in CASES.iter().enumerate() {
        let y = 50 + index as u32 * 100;
        let mut error = 0;
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let a = image
                    .get_pixel((200i32 + dx) as u32, (y as i32 + dy) as u32)
                    .0;
                let e = image
                    .get_pixel((600i32 + dx) as u32, (y as i32 + dy) as u32)
                    .0;
                for (a, e) in a.into_iter().zip(e) {
                    error = error.max(a.abs_diff(e));
                }
            }
        }
        let actual = image.get_pixel(200, y).0;
        let reference = image.get_pixel(600, y).0;
        passed &= error <= 2;
        // Guard against a blank render satisfying a pairwise comparison.
        passed &= reference.iter().any(|v| *v > 5 && *v < 250);
        samples.push(serde_json::json!({"case":case.name,"flags1":case.flags,"blend":case.blend,"source_grey_alpha_u8":case.texel,"base_color":case.base,"base_color_scale":case.scale,"expected_linear":expected(case),"pixel_y":y,"converted_rgb":actual,"reference_rgb":reference,"max_error_u8":error}));
    }
    let palette: Vec<_> = (0..PALETTE_SIZE)
        .map(|y| {
            (0..PALETTE_SIZE)
                .map(|x| palette_texel(x, y))
                .collect::<Vec<_>>()
        })
        .collect();
    let report = serde_json::json!({"kind":"converted-effect-palette","retail_parity":false,"space":if probe.interior {"interior"} else {"exterior"},"standard_material":probe.standard,"ev100":9.7,"tonemapping":"TonyMcMapface","palette_rgba_u8_rows":palette,"resolution":[800,CASES.len()*100],"camera":{"position":[0,0,10],"projection":"orthographic","vertical_size":CASES.len()},"ambient_brightness":0,"lights":0,"tolerance_u8":2,"loader":probe.loaded,"samples":samples,"adapter":{"name":adapter.name,"driver":adapter.driver,"driver_info":adapter.driver_info},"passed":passed});
    std::fs::write(
        probe.output.join("probe.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{report}");
    exit.write(if passed {
        AppExit::Success
    } else {
        AppExit::error()
    });
}
