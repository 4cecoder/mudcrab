//! Shared scene-linear composition for world views and diagnostic fixtures.
//!
//! HDR moves the display transform after sky, fog and transparent composition. The exposure
//! and tone curve below are pinned Bevy baselines. The experimental grade uses reported
//! Skyrim image-space (`IMGS`) values with Bevy saturation and an sRGB-domain post pass.
//! Its appearance was approved in Riverwood; its mapping is not verified retail parity.

use crate::console::{AppConsoleExt, ConsoleCommand};
use bevy::{
    asset::embedded_asset,
    camera::{CompositingSpace, Exposure, Hdr},
    core_pipeline::{
        Core3dSystems,
        fullscreen_material::{FullscreenMaterial, FullscreenMaterialPlugin},
        tonemapping::{Tonemapping, tonemapping as tonemapping_system},
    },
    ecs::{schedule::ScheduleConfigs, system::BoxedSystem},
    prelude::*,
    render::{
        extract_component::ExtractComponent,
        render_resource::ShaderType,
        view::{ColorGrading, ColorGradingGlobal, ColorGradingSection},
    },
    shader::ShaderRef,
};

/// Diagnostic selection; exposure, tone mapping, lighting and fog stay fixed.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SceneGradeMode {
    Off,
    Saturation,
    #[default]
    Full,
}

impl SceneGradeMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "saturation" => Some(Self::Saturation),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Saturation => "saturation",
            Self::Full => "full",
        }
    }
}

/// Identifies shared scene cameras without treating reflections as graded views.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum SceneGradeTarget {
    Scene,
    Reflection,
}

fn scene_grade_command() -> ConsoleCommand {
    ConsoleCommand {
        name: "SceneGrade",
        short: None,
        usage: "SceneGrade [off|saturation|full]",
        help: "inspect or change color grading without changing exposure, lighting or fog",
        handler: Box::new(|world, args| {
            match args {
                [] => {}
                [value] => {
                    let mode = SceneGradeMode::parse(value)
                        .ok_or("usage: SceneGrade [off|saturation|full]")?;
                    *world.resource_mut::<SceneGradeMode>() = mode;
                }
                _ => return Err("usage: SceneGrade [off|saturation|full]".into()),
            }
            Ok(format!(
                "scene grade: {}",
                world.resource::<SceneGradeMode>().name()
            ))
        }),
    }
}

fn apply_scene_grade_mode(
    mode: Res<SceneGradeMode>,
    mut cameras: Query<(
        Ref<SceneGradeTarget>,
        &mut ColorGrading,
        &mut SkyrimCinematicPost,
    )>,
) {
    for (target, mut grading, mut post) in &mut cameras {
        if *target == SceneGradeTarget::Reflection || !(mode.is_changed() || target.is_added()) {
            continue;
        }
        grading.global.post_saturation = if *mode == SceneGradeMode::Off {
            1.0
        } else {
            SkyrimCinematicGrade::SKYRIM_CLEAR_DAY.saturation
        };
        post.enabled = if *mode == SceneGradeMode::Full {
            1.0
        } else {
            0.0
        };
    }
}

/// Pinned Bevy exposure baseline; not a calibrated Skyrim image-space value.
pub const DEFAULT_SCENE_EV100: f32 = 9.7;

/// Artistic sRGB-encoded pivot for the experimental brightness/contrast pass.
/// This is not a measured Skyrim adapted luminance. Auto-exposure integration
/// and the relationship to the record's `HNAM` fields remain unresolved.
pub const CINEMATIC_PIVOT: f32 = 0.18;

/// One weather's cinematic grade: the `CNAM` triple and `TNAM` tint of its image
/// space (`IMGS` reached through the weather's `IMSP` slot), in record order.
///
/// Skyrim splits the image space across subrecords (xEdit `wbDefinitionsTES5`, UESP
/// `Skyrim Mod:Mod File Format/IMGS`): `CNAM` holds Saturation, Brightness and Contrast,
/// `TNAM` holds a tint amount followed by RGB, and `HNAM` holds the nine HDR fields (Eye
/// Adapt Speed, Bloom Blur Radius, Bloom Threshold, Bloom Scale, Receive Bloom Threshold,
/// White, Sunlight Scale, Sky Scale, Eye Adapt Strength). The `HNAM` block is not
/// carried here; see [`SkyrimCinematicGrade::color_grading`] for what maps and what
/// does not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyrimCinematicGrade {
    pub saturation: f32,
    pub brightness: f32,
    pub contrast: f32,
    /// Experimental interpretation of `TNAM`: mix toward luminance tinted by
    /// [`SkyrimCinematicGrade::tint_rgb`].
    pub tint_amount: f32,
    /// `TNAM` tint colour, RGB in 0..1.
    pub tint_rgb: [f32; 3],
}

impl SkyrimCinematicGrade {
    /// Reported grade for `SkyrimClear`'s day slot, read from the winning records in the
    /// local game data (`Skyrim.esm` as overridden by `Update.esm`): the weather's
    /// `IMSP` day slot names `ISSkyrimClearDAY` (`IMGS` 0x00012F88), whose `CNAM`
    /// holds Saturation 1.5, Brightness 1.135 and Contrast 1.4, and whose `TNAM`
    /// holds tint amount 0.42 toward RGB 0.894, 0.839, 0.773.
    /// These constants preserve the earlier local extraction; this module does
    /// not parse records or verify their mapping into retail shader slots.
    pub const SKYRIM_CLEAR_DAY: Self = Self {
        saturation: 1.5,
        brightness: 1.135,
        contrast: 1.4,
        tint_amount: 0.42,
        tint_rgb: [0.894_118, 0.839_216, 0.772_549],
    };

    /// The post-tonemap pass settings for this grade: brightness, contrast and
    /// tint, applied by [`SkyrimCinematicPost`] after the tone curve.
    pub fn post(&self) -> SkyrimCinematicPost {
        SkyrimCinematicPost {
            brightness: self.brightness,
            contrast: self.contrast,
            tint_amount: self.tint_amount,
            pivot: CINEMATIC_PIVOT,
            tint_rgb: Vec3::from_array(self.tint_rgb),
            enabled: 1.0,
        }
    }

    /// The Bevy grade for this cinematic grade.
    ///
    /// Bevy applies `post_saturation` after its tone curve, in linear RGB.
    /// Brightness, contrast and tint use the separate sRGB-encoded post pass.
    /// This split preserves the approved experimental appearance; it does not
    /// reproduce a verified Skyrim pipeline. Bevy's luminance weights also
    /// differ slightly from the weights used by the custom tint pass.
    ///
    /// Sectional gamma, lift, contrast and gain remain at identity. Global
    /// exposure, temperature, tint and hue remain at zero. The reported day
    /// slot's `HNAM` fields (Eye Adapt Speed 45.0, Bloom Blur Radius 7.0,
    /// Bloom Threshold 0.8, Bloom Scale 4.0, Receive Bloom Threshold 0.8,
    /// White 1.0, Sunlight Scale 2.8, Sky Scale 0.05, Eye Adapt Strength 5.0)
    /// remain unapplied. No calibrated mapping to Bevy exposure, bloom or
    /// adaptation has been established.
    pub fn color_grading(&self) -> ColorGrading {
        ColorGrading::with_identical_sections(
            ColorGradingGlobal {
                post_saturation: self.saturation,
                ..default()
            },
            ColorGradingSection::default(),
        )
    }
}

/// Post-tonemap cinematic settings for one camera: the part of the image-space
/// grade Bevy's `ColorGrading` cannot hold in the right domain. Carried as a
/// fullscreen-material component on every scene camera. A disabled setting
/// (the water reflection) returns its input untouched in the shader, so HDR
/// values reach the main view unclamped and are graded once there.
#[derive(Component, Clone, Copy, Debug, PartialEq, ShaderType, ExtractComponent)]
#[extract_component_filter(With<Camera>)]
pub struct SkyrimCinematicPost {
    pub brightness: f32,
    pub contrast: f32,
    pub tint_amount: f32,
    /// Display-range pivot for brightness/contrast; see [`CINEMATIC_PIVOT`].
    pub pivot: f32,
    pub tint_rgb: Vec3,
    /// 1.0 applies the grade; 0.0 passes the input through untouched.
    pub enabled: f32,
}

impl Default for SkyrimCinematicPost {
    /// Disabled: pass the input through untouched.
    fn default() -> Self {
        Self {
            brightness: 1.0,
            contrast: 1.0,
            tint_amount: 0.0,
            pivot: CINEMATIC_PIVOT,
            tint_rgb: Vec3::ONE,
            enabled: 0.0,
        }
    }
}

impl FullscreenMaterial for SkyrimCinematicPost {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/cinematic_post.wgsl".into()
    }

    /// Run after Bevy tonemapping, inside `PostProcess` and before upscaling.
    /// The shader expects linear RGB storage and grades its sRGB encoding.
    /// Cameras using this experiment must retain `CompositingSpace::Linear`.
    /// Ordering relative to AA and CAS has not been validated.
    fn schedule_configs(system: ScheduleConfigs<BoxedSystem>) -> ScheduleConfigs<BoxedSystem> {
        system
            .in_set(Core3dSystems::PostProcess)
            .after(tonemapping_system)
    }
}

/// Registers the post-tonemap cinematic pass and its embedded shader.
pub struct SkyrimCinematicPostPlugin;

impl Plugin for SkyrimCinematicPostPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/cinematic_post.wgsl");
        app.init_resource::<SceneGradeMode>()
            .add_console_command(scene_grade_command())
            .add_systems(PostUpdate, apply_scene_grade_mode)
            .add_plugins(FullscreenMaterialPlugin::<SkyrimCinematicPost>::default());
    }
}

/// Explicit camera settings for scene composition. A reflection uses the same exposure but no
/// display transform and no grade: the receiving water surface joins it to the main view
/// before tone mapping, so the main view grades it once.
#[derive(Bundle)]
pub struct SceneColorPipeline {
    pub grade_target: SceneGradeTarget,
    pub hdr: Hdr,
    /// The custom post pass expects linear RGB input.
    pub compositing_space: CompositingSpace,
    pub exposure: Exposure,
    pub tonemapping: Tonemapping,
    pub grading: ColorGrading,
    /// Post-tonemap cinematic pass settings; disabled passes through.
    pub cinematic_post: SkyrimCinematicPost,
}

impl Default for SceneColorPipeline {
    fn default() -> Self {
        Self {
            grade_target: SceneGradeTarget::Scene,
            hdr: Hdr,
            compositing_space: CompositingSpace::Linear,
            exposure: Exposure {
                ev100: DEFAULT_SCENE_EV100,
            },
            tonemapping: Tonemapping::TonyMcMapface,
            grading: SkyrimCinematicGrade::SKYRIM_CLEAR_DAY.color_grading(),
            cinematic_post: SkyrimCinematicGrade::SKYRIM_CLEAR_DAY.post(),
        }
    }
}

impl SceneColorPipeline {
    /// Compose reflected radiance without applying the main view's display transform.
    pub fn reflection() -> Self {
        Self {
            grade_target: SceneGradeTarget::Reflection,
            tonemapping: Tonemapping::None,
            grading: ColorGrading::default(),
            cinematic_post: SkyrimCinematicPost::default(),
            ..default()
        }
    }
}

#[cfg(test)]
fn assert_grading_matches(grading: &ColorGrading, wanted: &SkyrimCinematicGrade) {
    for actual in grading.all_sections() {
        assert_eq!(*actual, ColorGradingSection::default());
    }
    assert_eq!(grading.global.exposure, 0.0);
    assert_eq!(grading.global.temperature, 0.0);
    assert_eq!(grading.global.tint, 0.0);
    assert_eq!(grading.global.hue, 0.0);
    assert_eq!(grading.global.post_saturation, wanted.saturation);
    assert_eq!(grading.global.midtones_range, 0.2..0.7);
}

#[cfg(test)]
pub(crate) fn assert_scene_camera_output(world: &mut World, expected_count: usize) {
    let mut cameras = world.query_filtered::<(
        Option<&Hdr>,
        Option<&Exposure>,
        Option<&Tonemapping>,
        Option<&ColorGrading>,
        Option<&SkyrimCinematicPost>,
    ), With<Camera3d>>();
    let mut count = 0;
    for (hdr, exposure, tonemapping, grading, post) in cameras.iter(world) {
        assert!(hdr.is_some(), "scene camera must compose in HDR");
        assert_eq!(exposure.unwrap().ev100, DEFAULT_SCENE_EV100);
        assert_eq!(*tonemapping.unwrap(), Tonemapping::TonyMcMapface);
        assert_grading_matches(
            grading.expect("scene camera must carry the Skyrim grade"),
            &SkyrimCinematicGrade::SKYRIM_CLEAR_DAY,
        );
        assert_eq!(
            post.expect("scene camera must carry the cinematic post pass"),
            &SkyrimCinematicGrade::SKYRIM_CLEAR_DAY.post()
        );
        count += 1;
    }
    assert_eq!(count, expected_count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_modes_preserve_scene_state_and_leave_reflection_ungraded() {
        let mut app = App::new();
        app.init_resource::<SceneGradeMode>()
            .init_resource::<crate::console::ConsoleState>()
            .add_console_command(scene_grade_command())
            .add_systems(PostUpdate, apply_scene_grade_mode);
        let transform = Transform::from_xyz(7.0, 11.0, 13.0);
        let fog = bevy::pbr::DistanceFog {
            color: Color::srgb(0.2, 0.3, 0.4),
            ..default()
        };
        let main = app
            .world_mut()
            .spawn((SceneColorPipeline::default(), transform, fog))
            .id();
        let reflection = app.world_mut().spawn(SceneColorPipeline::reflection()).id();
        for (command, saturation, enabled) in [
            ("SceneGrade off", 1.0, 0.0),
            ("scenegrade SATURATION", 1.5, 0.0),
            ("SceneGrade full", 1.5, 1.0),
        ] {
            crate::console::execute_line(app.world_mut(), command);
            app.update();
            let world = app.world();
            assert_eq!(
                world
                    .get::<ColorGrading>(main)
                    .unwrap()
                    .global
                    .post_saturation,
                saturation
            );
            let post = world.get::<SkyrimCinematicPost>(main).unwrap();
            assert_eq!(post.enabled, enabled);
            assert_eq!(post.brightness, 1.135);
            assert_eq!(
                world.get::<Exposure>(main).unwrap().ev100,
                DEFAULT_SCENE_EV100
            );
            assert_eq!(
                *world.get::<Tonemapping>(main).unwrap(),
                Tonemapping::TonyMcMapface
            );
            assert_eq!(*world.get::<Transform>(main).unwrap(), transform);
            assert_eq!(
                world.get::<bevy::pbr::DistanceFog>(main).unwrap().color,
                Color::srgb(0.2, 0.3, 0.4)
            );
            assert!(world.get::<Hdr>(main).is_some());
            assert_eq!(
                world
                    .get::<ColorGrading>(reflection)
                    .unwrap()
                    .global
                    .post_saturation,
                1.0
            );
            assert_eq!(
                world
                    .get::<SkyrimCinematicPost>(reflection)
                    .unwrap()
                    .enabled,
                0.0
            );
        }
        crate::console::execute_line(app.world_mut(), "SceneGrade invalid");
        assert_eq!(
            *app.world().resource::<SceneGradeMode>(),
            SceneGradeMode::Full
        );
        assert!(
            app.world()
                .resource::<crate::console::ConsoleState>()
                .scrollback
                .last()
                .unwrap()
                .contains("usage:")
        );
        crate::console::execute_line(app.world_mut(), "SceneGrade off");
        app.update();
        let later = app.world_mut().spawn(SceneColorPipeline::default()).id();
        app.update();
        assert_eq!(
            app.world()
                .get::<SkyrimCinematicPost>(later)
                .unwrap()
                .enabled,
            0.0
        );
        assert_eq!(
            app.world()
                .get::<ColorGrading>(later)
                .unwrap()
                .global
                .post_saturation,
            1.0
        );
    }

    #[test]
    fn cinematic_shader_validates_and_matches_uniform_layout() {
        // Supply only Bevy's imported vertex interface; validate the actual
        // embedded fragment source rather than a second copy of its formula.
        let source = include_str!("shaders/cinematic_post.wgsl").replace(
            "#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput",
            "struct FullscreenVertexOutput {\n\
                @builtin(position) position: vec4<f32>,\n\
                @location(0) uv: vec2<f32>,\n\
            }",
        );
        let module = wgpu::naga::front::wgsl::parse_str(&source).unwrap();
        wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap();
        let uniform = module
            .types
            .iter()
            .find_map(|(_, ty)| {
                (ty.name.as_deref() == Some("SkyrimCinematicPost")).then_some(&ty.inner)
            })
            .unwrap();
        let wgpu::naga::TypeInner::Struct { members, span } = uniform else {
            panic!("cinematic uniform must be a struct");
        };
        assert_eq!(u64::from(*span), SkyrimCinematicPost::min_size().get());
        assert_eq!(
            members
                .iter()
                .map(|member| member.offset)
                .collect::<Vec<_>>(),
            [0, 4, 8, 12, 16, 28]
        );
    }

    #[test]
    fn scene_views_compose_in_hdr_with_explicit_output_settings() {
        let mut world = World::new();
        let entity = world.spawn(SceneColorPipeline::default()).id();
        assert!(world.get::<Hdr>(entity).is_some());
        assert_eq!(
            *world.get::<CompositingSpace>(entity).unwrap(),
            CompositingSpace::Linear
        );
        assert_eq!(world.get::<Exposure>(entity).unwrap().ev100, 9.7);
        assert_eq!(
            *world.get::<Tonemapping>(entity).unwrap(),
            Tonemapping::TonyMcMapface
        );
        assert_grading_matches(
            world.get::<ColorGrading>(entity).unwrap(),
            &SkyrimCinematicGrade::SKYRIM_CLEAR_DAY,
        );
        assert_eq!(
            world.get::<SkyrimCinematicPost>(entity).unwrap(),
            &SkyrimCinematicGrade::SKYRIM_CLEAR_DAY.post()
        );
    }

    #[test]
    fn skyrim_clear_day_grade_preserves_reported_record_constants() {
        // ISSkyrimClearDAY (IMGS 0x00012F88, Update.esm winner), read from the
        // local game data: CNAM Saturation 1.5, Brightness 1.135, Contrast 1.4;
        // TNAM tint amount 0.42 toward RGB 0.894, 0.839, 0.773.
        let grade = SkyrimCinematicGrade::SKYRIM_CLEAR_DAY;
        assert_eq!(
            (grade.saturation, grade.brightness, grade.contrast),
            (1.5, 1.135, 1.4)
        );
        assert_eq!(grade.tint_amount, 0.42);
        assert_eq!(grade.tint_rgb, [0.894_118, 0.839_216, 0.772_549]);
        let grading = grade.color_grading();
        for section in grading.all_sections() {
            assert_eq!(*section, ColorGradingSection::default());
        }
        // Bevy saturation lands after its tone curve in linear RGB.
        assert_eq!(grading.global.post_saturation, 1.5);
        assert_eq!(grading.global.exposure, 0.0);
        // Brightness, contrast and tint land in the post-tonemap pass, in
        // the experimental sRGB domain, around the artistic pivot.
        assert_eq!(
            grade.post(),
            SkyrimCinematicPost {
                brightness: 1.135,
                contrast: 1.4,
                tint_amount: 0.42,
                pivot: CINEMATIC_PIVOT,
                tint_rgb: Vec3::new(0.894_118, 0.839_216, 0.772_549),
                enabled: 1.0,
            }
        );
    }

    #[test]
    fn a_cinematic_grade_maps_saturation_after_the_tone_curve_only() {
        let grade = SkyrimCinematicGrade {
            saturation: 0.9,
            brightness: 1.1,
            contrast: 1.3,
            tint_amount: 0.25,
            tint_rgb: [1.0, 0.5, 0.0],
        };
        let grading = grade.color_grading();
        assert_eq!(grading.global.post_saturation, 0.9);
        for section in grading.all_sections() {
            assert_eq!(*section, ColorGradingSection::default());
        }
        let post = grade.post();
        assert_eq!(post.brightness, 1.1);
        assert_eq!(post.contrast, 1.3);
        assert_eq!(post.tint_amount, 0.25);
        assert_eq!(post.tint_rgb, Vec3::new(1.0, 0.5, 0.0));
        assert_eq!(post.pivot, CINEMATIC_PIVOT);
    }

    #[test]
    fn reflection_keeps_scene_exposure_without_a_display_transform() {
        let scene = SceneColorPipeline::default();
        let reflection = SceneColorPipeline::reflection();
        assert_eq!(reflection.exposure.ev100, scene.exposure.ev100);
        assert_eq!(reflection.tonemapping, Tonemapping::None);
        // The reflection is graded once by the main view, so its own grade is
        // Bevy's identity grade rather than the Skyrim scene grade.
        for section in reflection.grading.all_sections() {
            assert_eq!(*section, ColorGradingSection::default());
        }
        assert_eq!(reflection.grading.global.post_saturation, 1.0);
        assert_eq!(reflection.grading.global.exposure, 0.0);
        // No post pass on the reflection: its HDR values must reach the main
        // view unclamped, to be graded once there.
        assert_eq!(reflection.cinematic_post.enabled, 0.0);
    }
}
