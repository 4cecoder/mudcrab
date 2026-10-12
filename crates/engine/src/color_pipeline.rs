//! Shared scene-linear composition for world views and diagnostic fixtures.
//!
//! HDR moves the display transform after sky, fog and transparent composition. The exposure
//! and tone curve below are pinned Bevy baselines; the color grade is the one part of
//! Skyrim's image space (`IMGS`) carried here, mapped onto Bevy's `ColorGrading`.

use bevy::{
    asset::embedded_asset,
    camera::{Exposure, Hdr},
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

/// Pinned Bevy exposure baseline; not a calibrated Skyrim image-space value.
pub const DEFAULT_SCENE_EV100: f32 = 9.7;

/// Fixed display-range pivot for the post-tonemap brightness/contrast, as a
/// stand-in for the adapted scene luminance vanilla pivots on. Phase 2 replaces
/// this with the metered average from Bevy's auto-exposure, which is also where
/// the `HNAM` eye-adaptation fields (Speed 45.0, Strength 5.0 for the day slot)
/// belong.
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
    /// `TNAM` tint amount: how far each pixel moves toward its luminance tinted by
    /// [`SkyrimCinematicGrade::tint_rgb`].
    pub tint_amount: f32,
    /// `TNAM` tint colour, RGB in 0..1.
    pub tint_rgb: [f32; 3],
}

impl SkyrimCinematicGrade {
    /// The grade for `SkyrimClear`'s day slot, read from the winning records in the
    /// local game data (`Skyrim.esm` as overridden by `Update.esm`): the weather's
    /// `IMSP` day slot names `ISSkyrimClearDAY` (`IMGS` 0x00012F88), whose `CNAM`
    /// holds Saturation 1.5, Brightness 1.135 and Contrast 1.4, and whose `TNAM`
    /// holds tint amount 0.42 toward RGB 0.894, 0.839, 0.773.
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
    /// Domain decides the mapping. Vanilla's cinematic pass
    /// (`ISHDRTonemapBlendCinematic` in the shipped image-space shaders) runs
    /// after its HDR tone curve, on display-range values: saturation mixes
    /// toward luminance there, and brightness/contrast pivot around the adapted
    /// scene luminance. Bevy's sectional grading runs before its tone curve, in
    /// linear HDR, with contrast pivoted on a fixed 0.5; carrying the record
    /// values there (saturation 1.5, contrast 1.4, gain 1.135) blows the image
    /// out. Only one Bevy field sits in vanilla's position:
    ///
    /// - Saturation maps to the global `post_saturation`, applied after the
    ///   tone curve with the same mix-toward-luminance formula vanilla uses.
    /// - Brightness, Contrast and the `TNAM` tint are applied by
    ///   [`SkyrimCinematicPost`], a fullscreen pass scheduled after the tone
    ///   curve, in vanilla's order (tint, then brightness/contrast). Bevy's
    ///   sectional fields cannot carry them: wrong domain, wrong pivot.
    /// - Gamma and lift stay at their identities (1.0 and 0.0): no `IMGS` field
    ///   corresponds to them.
    /// - The global exposure offset stays at 0.0 and temperature, tint and hue stay
    ///   at 0.0. The day slot's `HNAM` HDR fields (Eye Adapt Speed 45.0, Bloom Blur
    ///   Radius 7.0, Bloom Threshold 0.8, Bloom Scale 4.0, Receive Bloom Threshold
    ///   0.8, White 1.0, Sunlight Scale 2.8, Sky Scale 0.05, Eye Adapt Strength 5.0)
    ///   have no sourced mapping onto Bevy exposure or grading, so they are recorded
    ///   here and left unapplied; exposure and bloom remain separate systems.
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

    /// Run after the tonemapping system, still inside `PostProcess` (before
    /// upscaling), so the pass reads display-range values exactly where
    /// vanilla's cinematic pass reads them. The stock fullscreen-material
    /// order runs before tonemapping, which is the pre-tonemap domain that
    /// blew out with these values.
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
        app.add_plugins(FullscreenMaterialPlugin::<SkyrimCinematicPost>::default());
    }
}

/// Explicit camera settings for scene composition. A reflection uses the same exposure but no
/// display transform and no grade: the receiving water surface joins it to the main view
/// before tone mapping, so the main view grades it once.
#[derive(Bundle)]
pub struct SceneColorPipeline {
    pub hdr: Hdr,
    pub exposure: Exposure,
    pub tonemapping: Tonemapping,
    pub grading: ColorGrading,
    /// Post-tonemap cinematic pass settings; disabled passes through.
    pub cinematic_post: SkyrimCinematicPost,
}

impl Default for SceneColorPipeline {
    fn default() -> Self {
        Self {
            hdr: Hdr,
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
            tonemapping: Tonemapping::None,
            grading: ColorGrading::default(),
            cinematic_post: SkyrimCinematicPost::default(),
            ..default()
        }
    }
}

#[cfg(test)]
fn assert_grading_matches(grading: &ColorGrading, wanted: &SkyrimCinematicGrade) {
    let expected = wanted.color_grading();
    for (actual, section) in grading.all_sections().zip(expected.all_sections()) {
        assert_eq!(actual, section, "sections stay at the converted grade");
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
    fn scene_views_compose_in_hdr_with_explicit_output_settings() {
        let mut world = World::new();
        let entity = world.spawn(SceneColorPipeline::default()).id();
        assert!(world.get::<Hdr>(entity).is_some());
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
    fn skyrim_clear_day_grade_matches_the_winning_imagespace_record() {
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
        // Saturation lands after the tone curve, where vanilla applies it.
        assert_eq!(grading.global.post_saturation, 1.5);
        assert_eq!(grading.global.exposure, 0.0);
        // Brightness, contrast and tint land in the post-tonemap pass, in
        // vanilla's domain, around the fixed pivot.
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
