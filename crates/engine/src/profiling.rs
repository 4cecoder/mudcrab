use crate::{
    config::EngineConfig, fingerprint::RunFingerprint, render::RendererMetrics,
    streaming::StreamingMetrics,
};
use bevy::{
    diagnostic::{DiagnosticPath, DiagnosticsStore},
    prelude::*,
    render::{
        diagnostic::{
            ErasedRenderAssetDiagnosticPlugin, MeshAllocatorDiagnosticPlugin,
            RenderAssetDiagnosticPlugin,
        },
        mesh::RenderMesh,
        texture::GpuImage,
    },
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SAMPLES_PER_METRIC: usize = 200_000;
const MAX_TIMELINE_EVENTS: usize = 100_000;

/// Why the report has no per-pass GPU times: Bevy's render diagnostics can time GPU work only on a
/// backend with timestamp queries (Vulkan and DX12), and record nothing on the others.
const TIMESTAMP_QUERIES_UNAVAILABLE: &str =
    "the graphics backend does not support timestamp queries, so Bevy records no per-pass GPU time";

/// Why the report has no per-pass shader-invocation counters: DX12 does not offer pipeline
/// statistics.
const PIPELINE_STATISTICS_UNAVAILABLE: &str =
    "the graphics backend does not support pipeline statistics queries";

/// Why a mesh-allocator counter is `null`: the diagnostic is installed only for measured runs, and
/// only a rendered frame samples it.
const MESH_ALLOCATOR_UNAVAILABLE: &str =
    "Bevy's mesh-allocator diagnostic was not sampled in this run";

/// Why a render-asset count is missing: the diagnostic is installed only for measured runs, and
/// only a rendered frame samples it.
const RENDER_ASSET_UNAVAILABLE: &str =
    "Bevy's render-asset diagnostic for this type was not sampled in this run";

pub struct ProfilingPlugin;

impl Plugin for ProfilingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ProfilingState>()
            .add_systems(Update, sample_scene_inventory);
    }
}

#[derive(Resource)]
pub struct ProfilingState {
    started: Instant,
    frame: u64,
    cpu_spans_ms: BTreeMap<String, Vec<f64>>,
    render_metrics: BTreeMap<String, Vec<f64>>,
    /// The last value sampled for each GPU-memory diagnostic (the mesh allocator's counters and the
    /// live render-asset counts). These are gauges, not distributions: only the value at the end of
    /// the run is reported.
    memory_metrics: BTreeMap<String, f64>,
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, f64>,
    timeline: Vec<TimelineEvent>,
    memory: Vec<MemorySample>,
}

impl Default for ProfilingState {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            frame: 0,
            cpu_spans_ms: BTreeMap::new(),
            render_metrics: BTreeMap::new(),
            memory_metrics: BTreeMap::new(),
            counters: BTreeMap::new(),
            gauges: BTreeMap::new(),
            timeline: Vec::new(),
            memory: Vec::new(),
        }
    }
}

impl ProfilingState {
    pub fn record_elapsed(&mut self, name: impl Into<String>, started: Instant) {
        self.record_ms(name, started.elapsed().as_secs_f64() * 1000.0);
    }

    /// The samples recorded for a span so far, in milliseconds.
    #[cfg(test)]
    pub fn span_samples(&self, name: &str) -> &[f64] {
        self.cpu_spans_ms.get(name).map_or(&[], Vec::as_slice)
    }

    /// Whether a timeline event with exactly this stage was recorded.
    #[cfg(test)]
    pub fn has_event_stage(&self, stage: &str) -> bool {
        self.timeline.iter().any(|event| event.stage == stage)
    }

    pub fn record_micros(&mut self, name: impl Into<String>, micros: u64) {
        self.record_ms(name, micros as f64 / 1000.0);
    }

    pub fn record_ms(&mut self, name: impl Into<String>, value: f64) {
        push_bounded(self.cpu_spans_ms.entry(name.into()).or_default(), value);
    }

    pub fn increment(&mut self, name: impl Into<String>, amount: u64) {
        let counter = self.counters.entry(name.into()).or_default();
        *counter = counter.saturating_add(amount);
    }

    pub fn set_gauge(&mut self, name: impl Into<String>, value: f64) {
        self.gauges.insert(name.into(), value);
    }

    pub fn event(
        &mut self,
        subject: impl Into<String>,
        stage: impl Into<String>,
        value_ms: Option<f64>,
    ) {
        if self.timeline.len() >= MAX_TIMELINE_EVENTS {
            return;
        }
        self.timeline.push(TimelineEvent {
            elapsed_ms: self.started.elapsed().as_secs_f64() * 1000.0,
            frame: self.frame,
            subject: subject.into(),
            stage: stage.into(),
            value_ms,
        });
    }

    pub fn sample_frame(
        &mut self,
        diagnostics: &DiagnosticsStore,
        process_memory_gib: Option<f64>,
    ) {
        self.frame = self.frame.saturating_add(1);
        for diagnostic in diagnostics.iter() {
            let path = diagnostic.path().as_str();
            if let Some(value) = diagnostic.value() {
                if path.starts_with("render/") {
                    push_bounded(
                        self.render_metrics.entry(path.to_owned()).or_default(),
                        value,
                    );
                }
                // The GPU-memory diagnostics do not sit under `render/`, so they are matched by
                // their own names, deliberately: the mesh allocator's counters and the live
                // render-asset counts, the only paths the report's memory section reads.
                else if is_gpu_memory_path(path) {
                    self.memory_metrics.insert(path.to_owned(), value);
                }
            }
        }
        if self.frame.is_multiple_of(60)
            && let Some(process_memory_gib) = process_memory_gib
        {
            self.memory.push(MemorySample {
                elapsed_seconds: self.started.elapsed().as_secs_f64(),
                process_gib: process_memory_gib,
            });
        }
    }

    /// The per-pass GPU times, summarised over every sample taken this run (see [`GpuPassReport`]).
    pub fn gpu_passes(
        &self,
        timestamp_queries_supported: bool,
        pipeline_statistics_supported: bool,
    ) -> GpuPassReport {
        // Without timestamp queries no such sample can be real, so a stray one (a diagnostic left
        // over from another device, say) is dropped rather than reported as a pass time.
        let passes: BTreeMap<String, MetricSummary> = if timestamp_queries_supported {
            self.render_metrics
                .iter()
                .filter(|(path, _)| path.ends_with("/elapsed_gpu"))
                .map(|(path, samples)| (path.clone(), summarize(samples)))
                .collect()
        } else {
            BTreeMap::new()
        };
        let mut unavailable = BTreeMap::new();
        if !timestamp_queries_supported {
            unavailable.insert(
                "elapsed_gpu".to_owned(),
                TIMESTAMP_QUERIES_UNAVAILABLE.to_owned(),
            );
        } else if passes.is_empty() {
            unavailable.insert(
                "elapsed_gpu".to_owned(),
                "no per-pass GPU time was recorded for this run".to_owned(),
            );
        }
        if !pipeline_statistics_supported {
            unavailable.insert(
                "pipeline_statistics".to_owned(),
                PIPELINE_STATISTICS_UNAVAILABLE.to_owned(),
            );
        }
        GpuPassReport {
            timestamp_queries_supported,
            pipeline_statistics_supported,
            passes,
            unavailable,
        }
    }

    /// The last sampled value of the GPU-memory diagnostics (see [`GpuMemoryReport`]).
    pub fn gpu_memory(&self) -> GpuMemoryReport {
        let mut unavailable = BTreeMap::new();
        let mut mesh_allocator = |path: &DiagnosticPath, field: &str| match self.memory_sample(path)
        {
            Some(value) => Some(value.max(0.0) as u64),
            None => {
                unavailable.insert(field.to_owned(), MESH_ALLOCATOR_UNAVAILABLE.to_owned());
                None
            }
        };
        let mesh_allocator_slab_bytes = mesh_allocator(
            MeshAllocatorDiagnosticPlugin::slabs_size_diagnostic_path(),
            "mesh_allocator_slab_bytes",
        );
        let mesh_allocator_slabs = mesh_allocator(
            MeshAllocatorDiagnosticPlugin::slabs_diagnostic_path(),
            "mesh_allocator_slabs",
        );
        let mesh_allocator_allocations = mesh_allocator(
            MeshAllocatorDiagnosticPlugin::allocations_diagnostic_path(),
            "mesh_allocator_allocations",
        );
        let mut render_asset_counts = BTreeMap::new();
        for (label, path) in gpu_asset_diagnostics() {
            match self.memory_sample(&path) {
                Some(value) => {
                    render_asset_counts.insert(label.to_owned(), value.max(0.0) as u64);
                }
                None => {
                    unavailable.insert(
                        format!("gpu_asset_counts/{label}"),
                        RENDER_ASSET_UNAVAILABLE.to_owned(),
                    );
                }
            }
        }
        GpuMemoryReport {
            mesh_allocator_slab_bytes,
            mesh_allocator_slabs,
            mesh_allocator_allocations,
            render_asset_counts,
            unavailable,
        }
    }

    /// The last value sampled for a diagnostic path, if it was sampled at all.
    fn memory_sample(&self, path: &DiagnosticPath) -> Option<f64> {
        self.memory_metrics.get(path.as_str()).copied()
    }

    pub fn write_bundle(
        &self,
        config: &EngineConfig,
        frame_metrics: &serde_json::Value,
        streaming: Option<&StreamingMetrics>,
        renderer: &RendererMetrics,
        system: Option<SystemMetadata>,
        fingerprint: Option<&RunFingerprint>,
    ) -> std::io::Result<()> {
        let Some(root) = &config.profile_output_dir else {
            return Ok(());
        };
        fs::create_dir_all(root)?;
        let cpu: BTreeMap<_, _> = self
            .cpu_spans_ms
            .iter()
            .map(|(name, samples)| (name.clone(), summarize(samples)))
            .collect();
        let render: BTreeMap<_, _> = self
            .render_metrics
            .iter()
            .map(|(name, samples)| (name.clone(), summarize(samples)))
            .collect();
        let gpu_supported = render.keys().any(|path| path.ends_with("/elapsed_gpu"));
        let pipeline_statistics_supported = render.keys().any(|path| {
            path.ends_with("/vertex_shader_invocations")
                || path.ends_with("/compute_shader_invocations")
        });
        write_json(
            &root.join("metadata.json"),
            &Metadata {
                format_version: 2,
                generated_unix_ms: unix_ms(),
                scenario: config.profile_scenario.clone(),
                run_id: config.profile_run_id.clone(),
                hardware_label: config.profile_hardware.clone(),
                worldspace_id: config.worldspace_id,
                start_grid: config.start_grid,
                synthetic_instances: config.synthetic_instances,
                system,
                fingerprint: fingerprint.cloned(),
            },
        )?;
        write_json(&root.join("frame-metrics.json"), frame_metrics)?;
        write_json(
            &root.join("cpu-spans.json"),
            &CpuProfile {
                spans: cpu.clone(),
                counters: self.counters.clone(),
                gauges: self.gauges.clone(),
            },
        )?;
        write_json(
            &root.join("gpu-passes.json"),
            &GpuProfile {
                timestamp_queries_supported: gpu_supported,
                pipeline_statistics_supported,
                metrics: render.clone(),
                unavailable: unavailable_render_metrics(&render),
            },
        )?;
        write_json(
            &root.join("streaming.json"),
            &StreamingProfile {
                aggregate: streaming.cloned(),
                timeline: self.timeline.clone(),
            },
        )?;
        write_json(&root.join("renderer.json"), renderer)?;
        write_json(
            &root.join("memory.json"),
            &MemoryProfile {
                samples: self.memory.clone(),
                slope_gib_per_minute: memory_slope(&self.memory),
            },
        )?;
        fs::write(
            root.join("summary.md"),
            summary_markdown(config, frame_metrics, &cpu, &render, streaming, renderer),
        )?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn sample_scene_inventory(
    mut profiler: ResMut<ProfilingState>,
    mut frames: Local<u64>,
    meshes: Query<(), With<Mesh3d>>,
    terrain: Query<&ViewVisibility, With<crate::world::components::TerrainPatch>>,
    water: Query<(), With<crate::world::components::WaterSurface>>,
    bounds: Query<(), With<crate::world::components::InstanceBounds>>,
    mesh_assets: Res<Assets<Mesh>>,
    images: Res<Assets<Image>>,
) {
    *frames = (*frames).saturating_add(1);
    if !(*frames).is_multiple_of(60) {
        return;
    }
    let started = Instant::now();
    profiler.set_gauge("scene/mesh_entities", meshes.iter().count() as f64);
    profiler.set_gauge("scene/terrain_patches", terrain.iter().count() as f64);
    profiler.set_gauge(
        "scene/visible_terrain_patches",
        terrain.iter().filter(|visible| visible.get()).count() as f64,
    );
    profiler.set_gauge("scene/water_surfaces", water.iter().count() as f64);
    profiler.set_gauge("scene/bounded_instances", bounds.iter().count() as f64);
    profiler.set_gauge("assets/meshes", mesh_assets.len() as f64);
    profiler.set_gauge("assets/images", images.len() as f64);
    profiler.record_elapsed("profiling/scene_inventory", started);
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemMetadata {
    pub os: String,
    pub kernel: String,
    pub cpu: String,
    pub core_count: String,
    pub memory: String,
}

/// The profile bundle's metadata: what this run was, and where the bundle came from.
///
/// Everything the run fingerprint already records (commit, dirty state, build profile, window
/// resolution, engine and Bevy versions, stream radius) lives in `fingerprint` alone, so the two
/// cannot disagree.
#[derive(Debug, Clone, Serialize)]
struct Metadata {
    format_version: u32,
    generated_unix_ms: u128,
    scenario: String,
    run_id: String,
    hardware_label: String,
    worldspace_id: u32,
    start_grid: (i32, i32),
    synthetic_instances: usize,
    system: Option<SystemMetadata>,
    fingerprint: Option<RunFingerprint>,
}

#[derive(Debug, Clone, Serialize)]
struct TimelineEvent {
    elapsed_ms: f64,
    frame: u64,
    subject: String,
    stage: String,
    value_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct MemorySample {
    elapsed_seconds: f64,
    process_gib: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricSummary {
    pub count: usize,
    pub total: f64,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub worst: f64,
}

/// The report's per-pass GPU times: one summary per Bevy render-diagnostic path, plus what the
/// device could not measure.
///
/// The paths are Bevy's own: `render/<pass>/elapsed_gpu`, where `<pass>` is the label the render
/// pass was created with (`main_opaque_pass_3d`, `tonemapping`, `shadow_directional_light_0_cascade_1`,
/// …). Several cameras that run the same pass write the same path, so a summary covers every camera
/// that ran it. When the device cannot time GPU work the passes are left empty and `unavailable`
/// says why, rather than reporting zeros.
#[derive(Debug, Clone, Serialize)]
pub struct GpuPassReport {
    pub timestamp_queries_supported: bool,
    pub pipeline_statistics_supported: bool,
    pub passes: BTreeMap<String, MetricSummary>,
    pub unavailable: BTreeMap<String, String>,
}

/// The run's memory, split by where it is: Bevy's CPU-side asset data, and the GPU-side counters
/// the render diagnostics expose.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryReport {
    pub cpu: CpuAssetBytes,
    pub gpu: GpuMemoryReport,
}

/// The CPU-side bytes of the mesh and image assets, read once at the end of the run.
///
/// A mesh Bevy has extracted to the render world without keeping a main-world copy — the glTF
/// loader's default, and so every streamed model — has no attribute or index data left to measure;
/// those meshes are counted in `meshes_without_cpu_data` and contribute no bytes to `mesh_bytes`.
#[derive(Debug, Clone, Serialize)]
pub struct CpuAssetBytes {
    pub meshes: usize,
    pub mesh_bytes: u64,
    pub meshes_without_cpu_data: usize,
    pub images: usize,
    pub image_bytes: u64,
}

/// The GPU-side memory counters, from Bevy's mesh-allocator and render-asset diagnostics.
///
/// Every field is a counter, not a VRAM total: wgpu reports how many resources are live, and the
/// engine can read no vendor memory counter. `mesh_allocator_slab_bytes` is the byte size of the
/// allocator's own slabs, the one real byte figure available. A counter that was not sampled is
/// `null` with the reason in `unavailable`, never zero.
#[derive(Debug, Clone, Serialize)]
pub struct GpuMemoryReport {
    pub mesh_allocator_slab_bytes: Option<u64>,
    pub mesh_allocator_slabs: Option<u64>,
    pub mesh_allocator_allocations: Option<u64>,
    /// Live render-asset counts, by asset type: a count of assets, never a byte size.
    pub render_asset_counts: BTreeMap<String, u64>,
    pub unavailable: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct CpuProfile {
    spans: BTreeMap<String, MetricSummary>,
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, f64>,
}

#[derive(Serialize)]
struct GpuProfile {
    timestamp_queries_supported: bool,
    pipeline_statistics_supported: bool,
    metrics: BTreeMap<String, MetricSummary>,
    unavailable: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct StreamingProfile {
    aggregate: Option<StreamingMetrics>,
    timeline: Vec<TimelineEvent>,
}

#[derive(Serialize)]
struct MemoryProfile {
    samples: Vec<MemorySample>,
    slope_gib_per_minute: Option<f64>,
}

pub fn summarize(samples: &[f64]) -> MetricSummary {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let total = sorted.iter().sum::<f64>();
    MetricSummary {
        count: sorted.len(),
        total,
        mean: total / sorted.len().max(1) as f64,
        p50: percentile(&sorted, 0.50),
        p95: percentile(&sorted, 0.95),
        p99: percentile(&sorted, 0.99),
        worst: sorted.last().copied().unwrap_or_default(),
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn memory_slope(samples: &[MemorySample]) -> Option<f64> {
    let first = samples.first()?;
    let last = samples.last()?;
    let minutes = (last.elapsed_seconds - first.elapsed_seconds) / 60.0;
    (minutes > 0.0).then_some((last.process_gib - first.process_gib) / minutes)
}

/// Whether a diagnostic path is one of the GPU-memory counters.
///
/// These sit outside the `render/` tree — the mesh allocator's counters are top-level paths and the
/// render-asset counts are under `render_asset/` — so [`ProfilingState::sample_frame`] matches them
/// by name instead of by prefix, keeping only the paths the report's memory section reads.
fn is_gpu_memory_path(path: &str) -> bool {
    path.starts_with("mesh_allocator_")
        || path.starts_with("render_asset/")
        || path.starts_with("erased_render_asset/")
}

/// The live GPU-asset counts the report records, each with the label it is reported under.
///
/// Bevy's plugins build each path out of the Rust type name of the *prepared* asset (Bevy 0.19
/// prepares a source asset into a GPU-side one), so each path is taken from the plugin itself
/// rather than spelled out here; the labels are the report's own, stable across versions.
///
/// Materials are one shared count, not one per material type: every `MeshMaterial3d<M>` prepares
/// into the same erased `PreparedMaterial` map, so Bevy reports the same number for each material
/// type it is asked about (see `ErasedRenderAssetDiagnosticPlugin`).
fn gpu_asset_diagnostics() -> [(&'static str, DiagnosticPath); 3] {
    [
        (
            "mesh",
            RenderAssetDiagnosticPlugin::<RenderMesh>::render_asset_diagnostic_path(),
        ),
        (
            "image",
            RenderAssetDiagnosticPlugin::<GpuImage>::render_asset_diagnostic_path(),
        ),
        (
            "material",
            ErasedRenderAssetDiagnosticPlugin::<MeshMaterial3d<StandardMaterial>>::
                render_asset_diagnostic_path(),
        ),
    ]
}

/// The CPU-side bytes of the run's mesh and image assets (see [`CpuAssetBytes`]).
///
/// A mesh's bytes are its vertex attributes (each attribute's element size times its vertex count)
/// plus its indices; an image's are the bytes of its pixel data. Both are read once, at the end of
/// the run, rather than per frame: the figures move only when assets load or unload.
pub fn cpu_asset_bytes(meshes: &Assets<Mesh>, images: &Assets<Image>) -> CpuAssetBytes {
    let mut mesh_bytes = 0;
    let mut meshes_without_cpu_data = 0;
    for (_, mesh) in meshes.iter() {
        let Ok(attributes) = mesh.try_attributes() else {
            meshes_without_cpu_data += 1;
            continue;
        };
        mesh_bytes += attributes
            .map(|(attribute, values)| values.len() as u64 * attribute.format.size())
            .sum::<u64>();
        if let Ok(indices) = mesh.try_indices() {
            mesh_bytes += indices.len() as u64 * index_size(indices);
        }
    }
    let image_bytes = images
        .iter()
        .map(|(_, image)| image.data.as_ref().map_or(0, Vec::len) as u64)
        .sum();
    CpuAssetBytes {
        meshes: meshes.len(),
        mesh_bytes,
        meshes_without_cpu_data,
        images: images.len(),
        image_bytes,
    }
}

/// The size in bytes of one index of a mesh's index buffer.
fn index_size(indices: &bevy::mesh::Indices) -> u64 {
    match indices {
        bevy::mesh::Indices::U16(_) => 2,
        bevy::mesh::Indices::U32(_) => 4,
    }
}

fn unavailable_render_metrics(
    render: &BTreeMap<String, MetricSummary>,
) -> BTreeMap<String, String> {
    [
        ("draw_calls", "Bevy's built-in render diagnostics do not expose draw-call count for every render phase"),
        ("indirect_draw_count", "the native GPU preprocessing path does not currently publish this counter"),
        ("occlusion_rejected_instances", "OcclusionCulling does not export the rejected instance count to the main world"),
    ]
    .into_iter()
    .filter(|(name, _)| !render.keys().any(|path| path.ends_with(name)))
    .map(|(name, reason)| (name.to_owned(), reason.to_owned()))
    .collect()
}

fn summary_markdown(
    config: &EngineConfig,
    frame: &serde_json::Value,
    cpu: &BTreeMap<String, MetricSummary>,
    render: &BTreeMap<String, MetricSummary>,
    streaming: Option<&StreamingMetrics>,
    renderer: &RendererMetrics,
) -> String {
    // `assets_added/*` spans hold counts, not milliseconds (see `schedule_timing`), so they must not
    // be listed among the slowest spans. They stay in `cpu-spans.json`.
    let mut top_cpu: Vec<_> = cpu
        .iter()
        .filter(|(name, _)| !name.starts_with("assets_added/"))
        .collect();
    top_cpu.sort_by(|left, right| right.1.total.total_cmp(&left.1.total));
    let mut top_gpu: Vec<_> = render
        .iter()
        .filter(|(name, _)| name.ends_with("/elapsed_gpu"))
        .collect();
    top_gpu.sort_by(|left, right| right.1.mean.total_cmp(&left.1.mean));
    let mut output = format!(
        "# Profiling summary — {}\n\n- Average FPS: {:.2}\n- Frame P95: {:.2} ms\n- Passed: {}\n- GPU preprocessing: {}\n- GPU culling: {}\n- Indirect drawing: {}\n- HZB views: {}\n",
        config.profile_scenario,
        frame["average_fps"].as_f64().unwrap_or_default(),
        frame["frame_ms_p95"].as_f64().unwrap_or_default(),
        frame["passed"].as_bool().unwrap_or(false),
        renderer.gpu_preprocessing_active,
        renderer.gpu_culling_active,
        renderer.indirect_drawing_active,
        renderer.hzb_views,
    );
    for (key, title) in [
        ("frames_after_ready", "Frames after world ready"),
        ("jump_load_window", "Jump loading window"),
    ] {
        let window = &frame[key];
        if window.is_object() {
            output.push_str(&format!(
                "- {title}: {} frames, P99 {:.2} ms, worst {:.2} ms, over 33 ms: {}, over 50 ms: {}\n",
                window["frames"].as_u64().unwrap_or_default(),
                window["p99_ms"].as_f64().unwrap_or_default(),
                window["worst_ms"].as_f64().unwrap_or_default(),
                window["over_33ms"].as_u64().unwrap_or_default(),
                window["over_50ms"].as_u64().unwrap_or_default(),
            ));
        }
    }
    output.push_str(
        "\n## Top CPU spans\n\n| Span | Mean ms | P95 ms | Total ms |\n|---|---:|---:|---:|\n",
    );
    for (name, value) in top_cpu.into_iter().take(10) {
        output.push_str(&format!(
            "| {name} | {:.3} | {:.3} | {:.3} |\n",
            value.mean, value.p95, value.total
        ));
    }
    output.push_str("\n## Top GPU passes\n\n| Pass | Mean ms | P95 ms |\n|---|---:|---:|\n");
    for (name, value) in top_gpu.into_iter().take(10) {
        output.push_str(&format!(
            "| {name} | {:.3} | {:.3} |\n",
            value.mean, value.p95
        ));
    }
    if let Some(streaming) = streaming {
        output.push_str(&format!(
            "\n## Streaming and assets\n\n- Requests: {}\n- Active requests: {} (peak {})\n- Resident roots: {}\n- Failed cells: {}\n- Stale responses: {}\n- Unloaded cells: {}\n- Max despawns per frame: {}\n- Despawned entities: {}\n- Max instances spawned per frame: {}\n- Max instances armed per frame: {}\n- Arming queue depth: {} (peak {})\n- Max instances completed per scan: {}\n- Retiring cells: {} (peak {})\n- Revived cells: {}\n- Retire backlog overflows: {}\n- Origin rebases: {}\n- Lifecycle invariant failures: {}\n- Duplicate/orphaned/missing/out-of-range roots: {}/{}/{}/{}\n- Streaming fixture validated: {}\n- Assets ready: {}\n- Empty model references: {}\n- Model assets pending: {}\n- Surface assets pending: {}\n- Meshes validated: {}\n- Materials validated: {}\n- Images validated: {}\n- Asset failures: {}\n- Material validation failures: {}\n- Diagnostic fallbacks: {}\n- Canonical fixture validated: {}\n- Terrain patches validated: {}\n- Terrain seams validated: {}\n- Terrain seam points welded: {}\n- Terrain edges left as authored: {}\n- Terrain failures: {}\n- Water surfaces validated: {}\n- Water failures: {}\n- Terrain/water fixture validated: {}\n- Max query: {:.3} ms\n- Max cell commit: {:.3} ms\n- Max frame commit: {:.3} ms (budget {:.3} ms, violations {})\n",
            streaming.requests_submitted,
            streaming.active_requests,
            streaming.peak_active_requests,
            streaming.resident_roots,
            streaming.failed_cells,
            streaming.stale_responses,
            streaming.unloaded_cells,
            streaming.max_despawns_per_frame,
            streaming.despawned_entities,
            streaming.max_instances_spawned_per_frame,
            streaming.max_instances_armed_per_frame,
            streaming.arming_queue_depth,
            streaming.peak_arming_queue_depth,
            streaming.max_instances_completed_per_scan,
            streaming.retiring_cells,
            streaming.peak_retiring_cells,
            streaming.revived_cells,
            streaming.retire_backlog_overflows,
            streaming.origin_rebases,
            streaming.streaming_invariant_failures,
            streaming.duplicate_cell_roots,
            streaming.orphaned_cell_roots,
            streaming.missing_cell_roots,
            streaming.out_of_range_cell_roots,
            streaming.streaming_fixture_validated,
            streaming.assets_ready,
            streaming.empty_model_references,
            streaming.pending_asset_instances,
            streaming.pending_surface_instances,
            streaming.meshes_validated,
            streaming.materials_validated,
            streaming.images_validated,
            streaming.asset_load_failures,
            streaming.material_validation_failures,
            streaming.diagnostic_fallbacks,
            streaming.canonical_fixture_validated,
            streaming.terrain_patches_validated,
            streaming.terrain_seams_validated,
            streaming.terrain_seam_points_welded,
            streaming.terrain_edges_left_as_authored,
            streaming.terrain_validation_failures,
            streaming.water_surfaces_validated,
            streaming.water_validation_failures,
            streaming.terrain_water_fixture_validated,
            streaming.max_query_micros as f64 / 1000.0,
            streaming.max_commit_micros as f64 / 1000.0,
            streaming.max_frame_commit_micros as f64 / 1000.0,
            streaming.commit_budget_micros as f64 / 1000.0,
            streaming.commit_budget_violations,
        ));
    }
    output
}

fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?,
    )
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn push_bounded(values: &mut Vec<f64>, value: f64) {
    if values.len() < MAX_SAMPLES_PER_METRIC && value.is_finite() {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::diagnostic::{Diagnostic, DiagnosticMeasurement};

    /// Feeds a diagnostic store with one measurement of a path, the way a diagnostic plugin's own
    /// system does, so the tests drive the real sampling rather than a shortcut.
    fn feed(diagnostics: &mut DiagnosticsStore, path: &str, value: f64) {
        let path = DiagnosticPath::new(path.to_owned());
        if diagnostics.get(&path).is_none() {
            diagnostics.add(Diagnostic::new(path.clone()));
        }
        diagnostics
            .get_mut(&path)
            .expect("the diagnostic was just added")
            .add_measurement(DiagnosticMeasurement {
                time: Instant::now(),
                value,
            });
    }

    /// Per-pass GPU time is summarised over every measured frame, and the pipeline-statistics
    /// counters that share the `render/` tree stay out of the per-pass table.
    #[test]
    fn summarises_per_pass_gpu_time_from_sampled_diagnostics() {
        let mut diagnostics = DiagnosticsStore::default();
        let mut profiler = ProfilingState::default();
        for value in [1.0, 2.0, 3.0, 4.0] {
            feed(
                &mut diagnostics,
                "render/main_opaque_pass_3d/elapsed_gpu",
                value,
            );
            feed(
                &mut diagnostics,
                "render/tonemapping/elapsed_gpu",
                value * 2.0,
            );
            feed(
                &mut diagnostics,
                "render/main_opaque_pass_3d/vertex_shader_invocations",
                100.0,
            );
            profiler.sample_frame(&diagnostics, None);
        }
        let report = profiler.gpu_passes(true, true);
        assert!(report.timestamp_queries_supported);
        assert!(report.pipeline_statistics_supported);
        assert!(report.unavailable.is_empty());
        let pass = &report.passes["render/main_opaque_pass_3d/elapsed_gpu"];
        assert_eq!(pass.count, 4);
        assert_eq!(pass.mean, 2.5);
        assert_eq!(pass.total, 10.0);
        assert_eq!(pass.p95, 4.0);
        assert_eq!(pass.worst, 4.0);
        assert_eq!(report.passes["render/tonemapping/elapsed_gpu"].mean, 5.0);
        assert!(
            !report
                .passes
                .contains_key("render/main_opaque_pass_3d/vertex_shader_invocations"),
            "a pipeline-statistics counter is not a pass time"
        );
    }

    /// A backend without timestamp queries reports the reason and no pass times — never zeros, and
    /// never a stray sample from somewhere else.
    #[test]
    fn gpu_passes_say_why_they_are_empty_instead_of_reporting_zeros() {
        let mut diagnostics = DiagnosticsStore::default();
        let mut profiler = ProfilingState::default();
        feed(
            &mut diagnostics,
            "render/main_opaque_pass_3d/elapsed_gpu",
            3.0,
        );
        profiler.sample_frame(&diagnostics, None);

        let unsupported = profiler.gpu_passes(false, false);
        assert!(unsupported.passes.is_empty());
        assert!(!unsupported.timestamp_queries_supported);
        assert!(!unsupported.pipeline_statistics_supported);
        assert!(unsupported.unavailable.contains_key("elapsed_gpu"));
        assert!(unsupported.unavailable.contains_key("pipeline_statistics"));

        // A device that can time GPU work, but a run in which no pass was recorded.
        let empty = ProfilingState::default().gpu_passes(true, true);
        assert!(empty.passes.is_empty());
        assert!(
            empty.unavailable["elapsed_gpu"].contains("no per-pass GPU time"),
            "an empty run must say so: {:?}",
            empty.unavailable
        );
    }

    /// The memory section reports the last sample of each counter, counts assets as counts, and
    /// names a reason for every counter the run did not sample.
    #[test]
    fn reports_the_gpu_memory_counters_that_were_sampled() {
        let mut diagnostics = DiagnosticsStore::default();
        let mut profiler = ProfilingState::default();
        feed(&mut diagnostics, "mesh_allocator_slabs", 2.0);
        feed(&mut diagnostics, "mesh_allocator_slabs_size", 8_388_608.0);
        feed(&mut diagnostics, "mesh_allocator_allocations", 42.0);
        feed(
            &mut diagnostics,
            RenderAssetDiagnosticPlugin::<RenderMesh>::render_asset_diagnostic_path().as_str(),
            7.0,
        );
        profiler.sample_frame(&diagnostics, None);
        feed(&mut diagnostics, "mesh_allocator_allocations", 45.0);
        profiler.sample_frame(&diagnostics, None);

        let gpu = profiler.gpu_memory();
        assert_eq!(gpu.mesh_allocator_slab_bytes, Some(8_388_608));
        assert_eq!(gpu.mesh_allocator_slabs, Some(2));
        assert_eq!(
            gpu.mesh_allocator_allocations,
            Some(45),
            "the reported value is the run's last sample"
        );
        assert_eq!(gpu.render_asset_counts["mesh"], 7);
        assert!(
            !gpu.unavailable.contains_key("gpu_asset_counts/mesh"),
            "a sampled counter is not unavailable"
        );
        assert!(gpu.unavailable.contains_key("gpu_asset_counts/image"));
        assert!(gpu.unavailable.contains_key("gpu_asset_counts/material"));

        let unsampled = ProfilingState::default().gpu_memory();
        assert_eq!(unsampled.mesh_allocator_slab_bytes, None);
        assert!(unsampled.render_asset_counts.is_empty());
        assert!(
            unsampled
                .unavailable
                .contains_key("mesh_allocator_slab_bytes")
        );
        assert!(unsampled.unavailable.contains_key("gpu_asset_counts/mesh"));
    }

    /// The CPU-side bytes count each mesh's attributes and indices and each image's pixels; a mesh
    /// Bevy extracted without a main-world copy is counted but contributes no bytes.
    #[test]
    fn sums_the_cpu_bytes_of_meshes_and_images() {
        use bevy::{
            asset::RenderAssetUsages,
            render::render_resource::{
                Extent3d, PrimitiveTopology, TextureDimension, TextureFormat,
            },
        };

        let mut meshes = Assets::<Mesh>::default();
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        );
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_POSITION,
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        );
        mesh.insert_indices(bevy::mesh::Indices::U32(vec![0, 1, 2]));
        meshes.add(mesh);
        let mut extracted = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::RENDER_WORLD,
        );
        extracted.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0, 0.0, 0.0]]);
        extracted
            .take_gpu_data()
            .expect("the render-world-only mesh's data must be extractable");
        meshes.add(extracted);

        let mut images = Assets::<Image>::default();
        images.add(Image::new_fill(
            Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[0, 0, 0, 0],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        ));

        let bytes = cpu_asset_bytes(&meshes, &images);
        assert_eq!(bytes.meshes, 2);
        assert_eq!(
            bytes.mesh_bytes,
            3 * 12 + 3 * 4,
            "three positions and three u32 indices, and no bytes from the extracted mesh"
        );
        assert_eq!(bytes.meshes_without_cpu_data, 1);
        assert_eq!(bytes.images, 1);
        assert_eq!(bytes.image_bytes, 16);
    }

    #[test]
    fn summarizes_percentiles_and_worst_value() {
        let values: Vec<_> = (1..=100).map(f64::from).collect();
        let summary = summarize(&values);
        assert_eq!(summary.count, 100);
        assert_eq!(summary.p95, 96.0);
        assert_eq!(summary.worst, 100.0);
    }

    #[test]
    fn the_top_cpu_table_omits_asset_count_spans() {
        let mut cpu = BTreeMap::new();
        cpu.insert("assets_added/mesh".to_owned(), summarize(&[9.0, 9.0]));
        cpu.insert("streaming/plan".to_owned(), summarize(&[2.0, 3.0]));
        let render = BTreeMap::new();
        let summary = summary_markdown(
            &EngineConfig::default(),
            &serde_json::json!({}),
            &cpu,
            &render,
            None,
            &RendererMetrics::default(),
        );
        assert!(
            summary.contains("| streaming/plan |"),
            "a real span must stay in the table"
        );
        assert!(
            !summary.contains("assets_added/mesh"),
            "a count span must not be shown as milliseconds"
        );
    }

    #[test]
    fn computes_memory_growth_slope() {
        let samples = vec![
            MemorySample {
                elapsed_seconds: 0.0,
                process_gib: 1.0,
            },
            MemorySample {
                elapsed_seconds: 120.0,
                process_gib: 1.2,
            },
        ];
        assert!((memory_slope(&samples).unwrap() - 0.1).abs() < 0.0001);
    }

    #[test]
    fn writes_complete_profile_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            profile_output_dir: Some(directory.path().to_owned()),
            profile_scenario: "synthetic".into(),
            ..Default::default()
        };
        let mut profiler = ProfilingState::default();
        profiler.record_ms("streaming/test", 2.0);
        profiler.increment("test/count", 1);
        profiler.event("cell", "requested", None);
        let frame = serde_json::json!({
            "average_fps": 90.0,
            "frame_ms_p95": 14.0,
            "passed": true
        });
        let fingerprint = RunFingerprint::collect(&config, None, None, None, false);
        profiler
            .write_bundle(
                &config,
                &frame,
                Some(&StreamingMetrics::default()),
                &RendererMetrics::default(),
                None,
                Some(&fingerprint),
            )
            .unwrap();
        for name in [
            "metadata.json",
            "frame-metrics.json",
            "cpu-spans.json",
            "gpu-passes.json",
            "streaming.json",
            "renderer.json",
            "memory.json",
            "summary.md",
        ] {
            assert!(directory.path().join(name).is_file(), "missing {name}");
        }
        let summary = fs::read_to_string(directory.path().join("summary.md")).unwrap();
        assert!(
            summary.contains("Terrain seam points welded: 0"),
            "the summary must report the welded terrain seam points"
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["format_version"], serde_json::json!(2));
        assert!(
            metadata["fingerprint"]["commit"].is_string(),
            "the bundle metadata must carry the run fingerprint"
        );
        assert!(
            metadata.get("commit").is_none(),
            "the fields the fingerprint duplicates must not be written twice"
        );
    }
}
