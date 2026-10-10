//! Asset settings and explicit producer compatibility controls.

use super::*;
use crate::config::{PipelineConfig, RecordReader, TextureEncoder};

/// Producer-24 compatibility is explicitly bound to the producer-26 contract.
#[test]
fn producer_24_mesh_compatibility_does_not_follow_a_future_producer() {
    assert!(can_reuse_meshes_for_producer(24, 26));
    assert!(can_reuse_meshes_for_producer(26, 26));
    assert!(!can_reuse_meshes_for_producer(25, 26));
    assert!(!can_reuse_meshes_for_producer(27, 26));
    assert!(!can_reuse_meshes_for_producer(24, 27));
    assert!(!can_reuse_meshes_for_producer(26, 27));
    assert!(can_reuse_meshes_for_producer(27, 27));
}

/// Every asset kind ignores the full reader identity while metadata retains it.
#[test]
fn asset_settings_are_independent_of_reader_metadata() {
    let legacy = PipelineConfig::new("synthetic-data", "synthetic-output");
    let mut inhouse = legacy.clone();
    inhouse.record_reader = RecordReader::Inhouse;
    assert_ne!(
        configuration_hash(&legacy).unwrap(),
        configuration_hash(&inhouse).unwrap()
    );
    for kind in ["nif", "dds", "pex"] {
        assert_eq!(
            asset_configuration_hash_for_schema(&legacy, 26, kind).unwrap(),
            asset_configuration_hash_for_schema(&inhouse, 26, kind).unwrap(),
        );
    }
}

/// Changes to texture encoding settings do not discard unrelated mesh/script proof.
#[test]
fn texture_settings_invalidate_only_the_texture_kind() {
    let original = PipelineConfig::new("synthetic-data", "synthetic-output");
    let mut variants = Vec::new();
    let mut quality = original.clone();
    quality.texture_fallback_quality -= 1;
    variants.push(quality);
    let mut uastc = original.clone();
    uastc.texture_uastc_level += 1;
    variants.push(uastc);
    let mut zstd = original.clone();
    zstd.texture_zstd_level = 0;
    variants.push(zstd);
    let mut gpu = original.clone();
    gpu.texture_encoder = TextureEncoder::Gpu {
        quality: 2,
        batch_mb: 4,
    };
    variants.push(gpu);
    for changed in variants {
        assert_ne!(
            asset_configuration_hash_for_schema(&original, 26, "dds").unwrap(),
            asset_configuration_hash_for_schema(&changed, 26, "dds").unwrap(),
        );
        for kind in ["nif", "pex"] {
            assert_eq!(
                asset_configuration_hash_for_schema(&original, 26, kind).unwrap(),
                asset_configuration_hash_for_schema(&changed, 26, kind).unwrap(),
            );
        }
    }
}

/// ABI changes discard script proof while leaving mesh/texture settings intact.
#[test]
fn script_abi_invalidates_only_the_script_kind() {
    let original = PipelineConfig::new("synthetic-data", "synthetic-output");
    let mut changed = original.clone();
    changed.script_abi_version += 1;
    assert_ne!(
        asset_configuration_hash_for_schema(&original, 26, "pex").unwrap(),
        asset_configuration_hash_for_schema(&changed, 26, "pex").unwrap(),
    );
    for kind in ["nif", "dds"] {
        assert_eq!(
            asset_configuration_hash_for_schema(&original, 26, kind).unwrap(),
            asset_configuration_hash_for_schema(&changed, 26, kind).unwrap(),
        );
    }
}

/// Script settings and GPU scheduling stay independent of unrelated output producers.
#[test]
fn script_settings_and_gpu_batch_size_keep_their_existing_contracts() {
    let mut first = PipelineConfig::new("synthetic-data", "synthetic-output");
    assert_eq!(
        asset_configuration_hash_for_schema(&first, 16, "pex").unwrap(),
        asset_configuration_hash_for_schema(&first, 26, "pex").unwrap(),
    );
    first.texture_encoder = TextureEncoder::Gpu {
        quality: 2,
        batch_mb: 4,
    };
    let mut second = first.clone();
    second.texture_encoder = TextureEncoder::Gpu {
        quality: 2,
        batch_mb: 8,
    };
    assert_eq!(
        asset_configuration_hash_for_schema(&first, 26, "dds").unwrap(),
        asset_configuration_hash_for_schema(&second, 26, "dds").unwrap(),
    );
}
