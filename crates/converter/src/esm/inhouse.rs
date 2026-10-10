//! Adapt schema-decoded fields to the established runtime projections.
//! Source payloads remain separate from resolved canonical projection fields.

use crate::{
    config::RecordReader,
    esm::{
        cell_cache::validate_inhouse_land,
        exporter::{
            create_tables, export_inhouse_record_archive_to_db, export_inhouse_to_db,
            validate_inhouse_record,
        },
        load_order::LoadOrder,
        records::RawRecord,
    },
    records::{self, Value},
};
use color_eyre::Result;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

/// Decoder/adapter behavior version for output changes outside the authored schema.
pub const ADAPTER_VERSION: u32 = 7;

/// Explicit diagnostic export scope; existing publication APIs always select Full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordBundleMode {
    Full,
    RecordArchiveOnly,
}

/// SQL schemas remain present but these runtime views are deliberately unpopulated in archives.
const DEFERRED_RUNTIME_TABLES: &[&str] = &[
    "worldspaces",
    "cells",
    "references",
    "exterior_spatial",
    "land",
    "statics",
    "lights",
    "npcs",
    "movement_types",
    "race_movement_links",
    "movement_game_settings",
    "waters",
    "texture_sets",
    "landscape_textures",
    "grass_types",
    "landscape_texture_grasses",
    "scripts",
];

/// Identity binds decoder behavior and schema bytes so stale output cannot prove reuse.
pub fn reader_identity(reader: RecordReader) -> serde_json::Value {
    match reader {
        RecordReader::Legacy => serde_json::json!({"mode": "legacy"}),
        RecordReader::Inhouse => serde_json::json!({
            "mode": "inhouse", "adapter_version": ADAPTER_VERSION,
            "schema_sha256": crate::cache::hash_bytes(records::SCHEMA_BYTES),
            "language": "english",
            "text_encoding": "windows-1252", "vmad_text_encoding": "utf-8",
        }),
    }
}

/// Export a fresh diagnostic record bundle without converting meshes, textures or scripts.
/// Inputs stay read-only; retained STRINGS extraction is evidence workspace data.
pub fn export_record_bundle(data: &Path, plugins_file: &Path, output: &Path) -> Result<usize> {
    export_record_bundle_typed(data, plugins_file, output, &[])
}

/// Include ordered typed family fields and their localized lookup evidence in JSONL.
pub fn export_record_bundle_typed(
    data: &Path,
    plugins_file: &Path,
    output: &Path,
    signatures: &[[u8; 4]],
) -> Result<usize> {
    export_record_bundle_typed_with_database_work_dir(data, plugins_file, output, signatures, None)
}

/// Optionally build SQLite on a separate local disk while retaining all working evidence.
/// Typed JSONL, extracted strings and final caches remain in the requested output directory.
pub fn export_record_bundle_typed_with_database_work_dir(
    data: &Path,
    plugins_file: &Path,
    output: &Path,
    signatures: &[[u8; 4]],
    database_work_dir: Option<&Path>,
) -> Result<usize> {
    export_record_bundle_typed_with_options(
        data,
        plugins_file,
        output,
        signatures,
        database_work_dir,
        RecordBundleMode::Full,
    )
}

/// Fresh diagnostic-only archive option; source decoding, canonical archives and caches stay complete.
pub fn export_record_bundle_typed_with_options(
    data: &Path,
    plugins_file: &Path,
    output: &Path,
    signatures: &[[u8; 4]],
    database_work_dir: Option<&Path>,
    mode: RecordBundleMode,
) -> Result<usize> {
    let bundle_stage = records::profile::ProfileStage::start("complete_record_bundle");
    let config = crate::config::PipelineConfig::new(data, output);
    config.validate()?;
    let output_location = fresh_record_bundle_location(output)?;
    let work_location = database_work_dir
        .map(fresh_record_bundle_location)
        .transpose()?;
    let separate_work = work_location
        .as_deref()
        .filter(|work| *work != output_location.as_path());
    if let Some(work) = separate_work {
        crate::config::PipelineConfig::new(data, work).validate()?;
        color_eyre::eyre::ensure!(
            !work.starts_with(&output_location) && !output_location.starts_with(work),
            "database work directory must be separate from the record bundle output"
        );
    }
    let plugins = crate::esm::read_plugins_txt(plugins_file, data)?;
    fs::create_dir(output)?;
    if mode == RecordBundleMode::RecordArchiveOnly {
        write_record_archive_scope(output, false, signatures)?;
    }
    if let Some(work) = separate_work {
        // Exclusive creation: failures retain the new output; existing paths are never reused.
        fs::create_dir(work)?;
    }
    let database_root = separate_work.unwrap_or(output);
    let database_path = database_root.join("skyrim_world.db");
    let strings_root = output.join(".strings-input");
    fs::create_dir(&strings_root)?;
    let paths = localized_string_paths(&plugins);
    let mut archives = fs::read_dir(data)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("bsa") || extension.eq_ignore_ascii_case("ba2")
            })
        })
        .filter(|path| crate::pipeline::archive_load_order_priority(path, &plugins).is_some())
        .collect::<Vec<_>>();
    crate::pipeline::sort_archives_by_load_order(&mut archives, &plugins);
    let strings_stage = records::profile::ProfileStage::start("english_archive_inputs");
    for archive in archives {
        if let Err(error) =
            crate::archive::ArchiveExtractor::extract_paths(&archive, &strings_root, &paths)
        {
            eprintln!(
                "warning: {}: unable to extract localized names: {error}",
                archive.display()
            );
        }
    }
    strings_stage.finish(plugins.len());
    let records = convert_plugins_with_dump(
        &plugins,
        &database_path,
        &strings_root,
        (!signatures.is_empty()).then_some((output, signatures)),
        mode,
    )?;
    let terrain_stage = records::profile::ProfileStage::start("terrain_cache_pair");
    let count = write_terrain_caches(&records, &database_path, output)?;
    terrain_stage.finish(records.len());
    if let Some(work) = separate_work {
        let copy_stage = records::profile::ProfileStage::start("database_work_dir_publish_copy");
        // Conversion has returned and its SQLite connection is closed; caches read the same DB.
        copy_completed_record_database(work, output)?;
        copy_stage.finish(2);
    }
    write_reader_identity(output, RecordReader::Inhouse)?;
    fs::write(output.join("records-schema.json"), records::SCHEMA_BYTES)?;
    let record_count = records.len();
    let runtime_release_stage = records::profile::ProfileStage::start("runtime_master_release");
    drop(records);
    runtime_release_stage.finish(record_count);
    if mode == RecordBundleMode::RecordArchiveOnly {
        let mut identity = reader_identity(RecordReader::Inhouse);
        identity["bundle_mode"] = serde_json::json!("record_archive_only");
        identity["runtime_ready"] = serde_json::json!(false);
        fs::write(
            output.join("record-reader.json"),
            serde_json::to_vec_pretty(&identity)?,
        )?;
        write_record_archive_scope(output, true, signatures)?;
    }
    bundle_stage.finish(record_count);
    Ok(count)
}

/// Declare incomplete/completed diagnostic scope without claiming runtime readiness.
fn write_record_archive_scope(output: &Path, complete: bool, signatures: &[[u8; 4]]) -> Result<()> {
    // This manifest is owned by this invocation in its exclusively created fresh output.
    let typed_signatures = signatures
        .iter()
        .map(|tag| String::from_utf8_lossy(tag).into_owned())
        .collect::<Vec<_>>();
    fs::write(
        output.join("record-bundle-scope.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "inhouse-diagnostic-record-bundle-scope-v1",
            "mode": "record_archive_only",
            "status": if complete { "COMPLETE_DIAGNOSTIC_ARCHIVE_RUNTIME_VIEWS_DEFERRED" } else { "INCOMPLETE" },
            "runtime_ready": false,
            "runtime_views_materialized": false,
            "preserved_sql_tables": ["schema_info", "plugins", "records", "formid_map",
                "inhouse_source_records", "inhouse_localized_fields", "inhouse_terrain_source_order",
                "inhouse_terrain_layers", "record_overrides"],
        "deferred_runtime_tables": DEFERRED_RUNTIME_TABLES,
        "unchanged_auxiliary_tables": ["conversion_cache", "lod", "lod_chunks", "lod_build",
            "lod_chunks_spatial", "lod_chunks_spatial_node", "lod_chunks_spatial_parent", "lod_chunks_spatial_rowid"],
            "preserved_cache_files": ["cell_cache.rkyv", "cell_cache_preserved.rkyv"],
            "typed_signatures": typed_signatures,
            "typed_selection_changed": false,
            "asset_conversion": false,
            "no_lod": true,
            "whole_runtime_SQL_equivalence_claim": false,
        }))?,
    )?;
    Ok(())
}

/// Resolve a fresh leaf through its existing parent, rejecting files, directories and symlinks.
/// Like the existing create_dir output contract, the parent must already exist.
fn fresh_record_bundle_location(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => color_eyre::eyre::bail!("record bundle output/work directory must be new"),
    }
    let name = path.file_name().ok_or_else(|| {
        color_eyre::eyre::eyre!("output/work directory must have a new leaf name")
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(fs::canonicalize(parent)?.join(name))
}

/// Exclusive, bounded copy: failures retain both work artifacts and any partial destination.
/// No replacement, delete, rename or journal-policy change is performed.
fn copy_completed_record_database(work: &Path, output: &Path) -> Result<()> {
    for name in ["skyrim_world.db", "inhouse-reader-diagnostics.json"] {
        let source = work.join(name);
        color_eyre::eyre::ensure!(
            fs::symlink_metadata(&source)?.file_type().is_file(),
            "completed database artifact must be a regular file"
        );
    }
    for name in ["skyrim_world.db", "inhouse-reader-diagnostics.json"] {
        let mut source = fs::File::open(work.join(name))?;
        let mut destination = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join(name))?;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let size = std::io::Read::read(&mut source, &mut buffer)?;
            if size == 0 {
                break;
            }
            destination.write_all(&buffer[..size])?;
        }
        destination.flush()?;
        destination.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "database_work_dir_tests.rs"]
mod database_work_dir_tests;

#[cfg(test)]
#[path = "inhouse_review_tests.rs"]
mod inhouse_review_tests;

/// Stamp generated pack metadata after database/cache regeneration.
pub(crate) fn write_reader_identity(staging: &Path, reader: RecordReader) -> Result<()> {
    let path = staging.join("record-reader.json");
    if path.is_file() {
        fs::remove_file(&path)?;
    }
    fs::write(path, serde_json::to_vec_pretty(&reader_identity(reader))?)?;
    Ok(())
}

/// English localized-name tables needed by the selected plugins.
pub(crate) fn localized_string_paths(plugins: &[PathBuf]) -> BTreeSet<PathBuf> {
    plugins
        .iter()
        .filter_map(|path| path.file_stem())
        .flat_map(|stem| {
            ["strings", "dlstrings", "ilstrings"].map(|bank| {
                PathBuf::from(format!(
                    "strings/{}_english.{bank}",
                    stem.to_string_lossy().to_ascii_lowercase()
                ))
            })
        })
        .collect()
}

/// Bounded diagnostics retain one concrete example per plugin.
#[derive(Default)]
struct Warnings(BTreeMap<String, (u64, String)>);

impl Warnings {
    /// Count a local decode or lookup issue without accumulating every failing record.
    fn skipped(&mut self, plugin: &str, message: String) {
        let entry = self.0.entry(plugin.to_owned()).or_insert((0, message));
        entry.0 += 1;
    }

    /// Emit deterministic plugin summaries after usable neighbors are exported.
    fn report(&self) {
        for (plugin, (count, first)) in &self.0 {
            eprintln!(
                "warning: {plugin}: inhouse adapter reported {count} local decode/lookup issues (first: {first})"
            );
        }
    }
}

/// A localized occurrence keeps its original key separately from safe runtime text.
struct LocalizedField {
    form_id: u32,
    field_index: usize,
    signature: [u8; 4],
    field_name: String,
    string_table: &'static str,
    string_id: u32,
    load_order: u32,
    resolved_text: Option<String>,
    status: &'static str,
}

/// Recover winning LAND physical order from this reader's published provenance.
pub(crate) fn terrain_source_offsets(db_path: &Path) -> Result<HashMap<u32, u64>> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement =
        conn.prepare("SELECT form_id,source_record_offset FROM inhouse_terrain_source_order")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

/// Read normalized native layer geometry and globally remapped source keys for accepted LAND.
pub(crate) fn terrain_preserved_layers(
    db_path: &Path,
) -> Result<HashMap<u32, Vec<shared::TerrainLayer>>> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare("SELECT form_id,layer_data FROM inhouse_terrain_layers")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut output = HashMap::new();
    for row in rows {
        let (id, bytes) = row?;
        output.insert(
            id,
            rkyv::from_bytes::<Vec<shared::TerrainLayer>, rkyv::rancor::Error>(&bytes)?,
        );
    }
    Ok(output)
}

/// Keep every layer and blend weight; unresolved texture keys become explicit runtime placeholders.
pub(crate) fn terrain_runtime_layers(
    db_path: &Path,
    preserved: &HashMap<u32, Vec<shared::TerrainLayer>>,
) -> Result<HashMap<u32, Vec<shared::TerrainLayer>>> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare("SELECT form_id FROM records WHERE record_type='LTEX'")?;
    let textures = statement
        .query_map([], |row| row.get::<_, u32>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let mut output = preserved.clone();
    for layers in output.values_mut() {
        for layer in layers {
            if layer.texture_form_id != 0 && !textures.contains(&layer.texture_form_id) {
                layer.texture_form_id = 0;
            }
        }
    }
    Ok(output)
}

/// Share source-order and layer preservation across conversion, diagnostics and metadata rebuild.
pub(crate) fn write_terrain_caches(
    records: &HashMap<u32, RawRecord>,
    db_path: &Path,
    output: &Path,
) -> Result<usize> {
    let offsets = terrain_source_offsets(db_path)?;
    let preserved = terrain_preserved_layers(db_path)?;
    let runtime = terrain_runtime_layers(db_path, &preserved)?;
    let count = crate::esm::cell_cache::write_cell_cache_with_preserved_layers(
        records,
        &offsets,
        &runtime,
        &output.join("cell_cache.rkyv"),
    )?;
    crate::esm::cell_cache::write_cell_cache_with_preserved_layers(
        records,
        &offsets,
        &preserved,
        &output.join("cell_cache_preserved.rkyv"),
    )?;
    Ok(count)
}

/// Decode and export with one shared load-order mapping; plugin data failures stay local.
pub(crate) fn convert_plugins(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    strings_root: &Path,
) -> Result<HashMap<u32, RawRecord>> {
    convert_plugins_with_dump(
        plugin_paths,
        db_path,
        strings_root,
        None,
        RecordBundleMode::Full,
    )
}

/// Diagnostic export shares the exact decoder, winner and text lookup paths with publication.
fn convert_plugins_with_dump(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    strings_root: &Path,
    dump: Option<(&Path, &[[u8; 4]])>,
    mode: RecordBundleMode,
) -> Result<HashMap<u32, RawRecord>> {
    convert_plugins_with_dump_impl(
        plugin_paths,
        db_path,
        strings_root,
        dump,
        mode,
        #[cfg(test)]
        None,
    )
}

/// Keep synthetic schema selection private to tests of the real publication path.
fn convert_plugins_with_dump_impl(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    strings_root: &Path,
    dump: Option<(&Path, &[[u8; 4]])>,
    mode: RecordBundleMode,
    #[cfg(test)] test_schema: Option<&records::schema_format::Schema>,
) -> Result<HashMap<u32, RawRecord>> {
    let order_stage = records::profile::ProfileStage::start("read_load_order");
    let order = LoadOrder::read(plugin_paths)?;
    order_stage.finish(plugin_paths.len());
    let mut warnings = Warnings::default();
    let mut preserved_layers = HashMap::new();
    let preserve = |accepted: &records::ReadResult| {
        for record in accepted
            .records
            .values()
            .filter(|record| record.record_type == *b"LAND")
        {
            match crate::esm::cell_cache::preserved_texture_layers(&record.to_raw_record()) {
                Ok(layers) => {
                    preserved_layers.insert(record.form_id, layers);
                }
                Err(error) => warnings.skipped(
                    &order.names[record.load_order as usize],
                    format!("{:08X} terrain layer provenance: {error}", record.form_id),
                ),
            }
        }
    };
    #[cfg(test)]
    let mut decoded = if let Some(schema) = test_schema {
        records::read_plugins_with_validation_and_observer_with_schema(
            plugin_paths,
            &order,
            schema,
            validate_candidate,
            preserve,
        )?
    } else {
        records::read_plugins_with_validation_and_observer(
            plugin_paths,
            &order,
            validate_candidate,
            preserve,
        )?
    };
    #[cfg(not(test))]
    let mut decoded = records::read_plugins_with_validation_and_observer(
        plugin_paths,
        &order,
        validate_candidate,
        preserve,
    )?;
    let mut localized_fields = Vec::new();
    let mut unresolved_fields = HashMap::new();
    let mut tables: HashMap<(usize, String), HashMap<u32, String>> = HashMap::new();
    let mut typed_output = dump
        .map(|(output, _)| {
            fs::File::create(output.join("typed-records.jsonl"))
                .map(|file| BufWriter::with_capacity(1024 * 1024, file))
        })
        .transpose()?;
    let projection_stage = records::profile::ProfileStage::start("typed_export_runtime_projection");
    let mut master = HashMap::with_capacity(decoded.records.len());
    for record in decoded.records.values_mut() {
        let priority = record.load_order as usize;
        let plugin = &order.names[priority];
        let mut runtime_text_updates = decoded_text_updates(record);
        let mut unresolved = BTreeSet::new();
        let mut localized = Vec::new();
        let mut localization = BTreeMap::new();
        for (index, field) in record.fields.iter().enumerate() {
            if let Value::LocalizedString(id) = &field.value {
                let bank = records::string_table(&record.record_type, &field.name);
                let table = tables
                    .entry((priority, bank.to_owned()))
                    .or_insert_with(|| {
                        read_plugin_strings(&plugin_paths[priority], strings_root, bank)
                            .unwrap_or_else(|error| {
                                warnings.skipped(plugin, format!("localized table: {error}"));
                                HashMap::new()
                            })
                    });
                // Zero is a null key even if a malformed table gives it text.
                let resolved = if *id == 0 { None } else { table.get(id) };
                let status = if *id == 0 {
                    "null_id"
                } else if resolved.is_some() {
                    "resolved"
                } else {
                    "missing"
                };
                localization.insert(
                    index,
                    serde_json::json!({"bank": bank, "id": id, "text": resolved, "status": status}),
                );
                localized.push(LocalizedField {
                    form_id: record.form_id,
                    field_index: index,
                    signature: field.signature,
                    field_name: field.name.clone(),
                    string_table: bank,
                    string_id: *id,
                    load_order: record.load_order,
                    resolved_text: resolved.cloned(),
                    status,
                });
                // Keep the occurrence and key; unresolved text is never invented.
                if let Some(text) = resolved {
                    runtime_text_updates.push((index, runtime_string(text)));
                } else {
                    runtime_text_updates.push((index, runtime_string("")));
                    unresolved.insert(index);
                    if *id != 0 {
                        warnings.skipped(
                            plugin,
                            format!(
                                "{:08X} {} missing localized ID {id}",
                                record.form_id, field.name
                            ),
                        );
                    }
                }
            }
        }
        if let Some(writer) = &mut typed_output
            && dump.is_some_and(|(_, signatures)| signatures.contains(&record.record_type))
        {
            records::typed_export::write_record(writer, record, &localization)?;
            writer.write_all(b"\n")?;
        }
        // Typed fields and the source digest have been written before ownership moves.
        let mut raw = take_runtime_record(record);
        for (index, text) in runtime_text_updates {
            raw.subrecords[index].1 = text;
        }
        let invalid = validate_inhouse_record(&raw)
            .map_err(|error| error.to_string())
            .and_then(|_| {
                if raw.record_type == *b"LAND" {
                    validate_inhouse_land(&raw).map_err(|error| error.to_string())
                } else {
                    Ok(())
                }
            });
        if let Err(error) = invalid {
            warnings.skipped(
                plugin,
                format!(
                    "{:08X} {}: {error}",
                    raw.form_id,
                    String::from_utf8_lossy(&raw.record_type)
                ),
            );
            continue;
        }
        if !unresolved.is_empty() {
            unresolved_fields.insert(raw.form_id, unresolved);
        }
        localized_fields.extend(localized);
        master.insert(raw.form_id, raw);
    }
    if let Some(writer) = &mut typed_output {
        writer.flush()?;
    }
    projection_stage.finish(master.len());
    let setup_stage = records::profile::ProfileStage::start("database_setup_plugin_hashes");
    let conn = Connection::open(db_path)?;
    // Keep a modest page-cache target for this conversion's private connection.
    // Negative cache_size is in KiB; journal, sync and spill policies stay intact.
    conn.pragma_update(Some(rusqlite::DatabaseName::Main), "cache_size", -65_536)?;
    conn.set_prepared_statement_cache_capacity(64);
    create_tables(&conn)?;
    for (priority, path) in plugin_paths.iter().enumerate() {
        let checksum = Sha256::digest(fs::read(path)?);
        conn.prepare_cached(
            "INSERT OR REPLACE INTO plugins(id,name,priority,checksum) VALUES (?1,?2,?3,?4)",
        )?
        .execute(params![
            priority as i64,
            path.file_name().unwrap_or_default().to_string_lossy(),
            priority as i64,
            checksum.as_slice()
        ])?;
    }
    let terrain_offsets = decoded
        .records
        .values()
        .filter(|record| record.record_type == *b"LAND" && master.contains_key(&record.form_id))
        .map(|record| (record.form_id, record.source_record_offset))
        .collect::<HashMap<_, _>>();
    setup_stage.finish(plugin_paths.len());
    let runtime_stage = records::profile::ProfileStage::start(if mode == RecordBundleMode::Full {
        "runtime_database_export"
    } else {
        "canonical_record_database_export"
    });
    if mode == RecordBundleMode::Full {
        export_inhouse_to_db(&conn, &master, &order, &unresolved_fields, &terrain_offsets)?;
    } else {
        export_inhouse_record_archive_to_db(&conn, &master, &order)?;
    }
    runtime_stage.finish(master.len());
    // Keep the established rkyv records.data contract for runtime tools.
    // Auxiliary provenance holds verbatim source framing and file-relative IDs.
    let provenance_stage = records::profile::ProfileStage::start("source_database_export");
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS inhouse_source_records(form_id INTEGER PRIMARY KEY, source_form_id INTEGER NOT NULL, load_order INTEGER NOT NULL, record_type TEXT NOT NULL, flags INTEGER NOT NULL, payload BLOB NOT NULL); DELETE FROM inhouse_source_records;")?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS inhouse_localized_fields(form_id INTEGER NOT NULL, field_index INTEGER NOT NULL, signature TEXT NOT NULL, field_name TEXT NOT NULL, string_table TEXT NOT NULL, string_id INTEGER NOT NULL, load_order INTEGER NOT NULL, resolved_text TEXT, status TEXT NOT NULL CHECK(status IN ('resolved','null_id','missing')), PRIMARY KEY(form_id,field_index)); DELETE FROM inhouse_localized_fields; CREATE TABLE IF NOT EXISTS inhouse_terrain_source_order(form_id INTEGER PRIMARY KEY, source_record_offset INTEGER NOT NULL); DELETE FROM inhouse_terrain_source_order;")?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS inhouse_terrain_layers(form_id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL, load_order INTEGER NOT NULL, layer_data BLOB NOT NULL, unresolved_texture_ids TEXT NOT NULL); DELETE FROM inhouse_terrain_layers;")?;
    for (id, layers) in preserved_layers {
        let Some(record) = master.get(&id) else {
            continue;
        };
        let unresolved = layers
            .iter()
            .map(|layer| layer.texture_form_id)
            .filter(|id| {
                *id != 0
                    && !master
                        .get(id)
                        .is_some_and(|record| record.record_type == *b"LTEX")
            })
            .collect::<BTreeSet<_>>();
        tx.prepare_cached(
            "INSERT INTO inhouse_terrain_layers(form_id,cell_id,load_order,layer_data,unresolved_texture_ids) VALUES (?1,?2,?3,?4,?5)")?.execute(
            params![id, record.cell_form_id.unwrap_or(id), record.load_order, rkyv::to_bytes::<rkyv::rancor::Error>(&layers)?.as_slice(), serde_json::to_string(&unresolved)?],
        )?;
    }
    for field in localized_fields {
        tx.prepare_cached(
            "INSERT INTO inhouse_localized_fields(form_id,field_index,signature,field_name,string_table,string_id,load_order,resolved_text,status) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)")?.execute(
            params![field.form_id, field.field_index as u64, String::from_utf8_lossy(&field.signature), field.field_name, field.string_table, field.string_id, field.load_order, field.resolved_text, field.status],
        )?;
    }
    for (id, offset) in terrain_offsets {
        tx.prepare_cached(
            "INSERT INTO inhouse_terrain_source_order(form_id,source_record_offset) VALUES (?1,?2)",
        )?
        .execute(params![id, offset])?;
    }
    let mut source_ids = decoded.records.keys().copied().collect::<Vec<_>>();
    source_ids.sort_unstable();
    for id in source_ids {
        let record = &decoded.records[&id];
        tx.prepare_cached(
            "INSERT INTO inhouse_source_records(form_id,source_form_id,load_order,record_type,flags,payload) VALUES (?1,?2,?3,?4,?5,?6)")?.execute(
            params![id, record.source_form_id, record.load_order, String::from_utf8_lossy(&record.record_type), record.flags, record.raw_payload],
        )?;
    }
    records::history::write_tables(&tx, &decoded.history, &order, plugin_paths)?;
    tx.commit()?;
    provenance_stage.finish(decoded.records.len());
    if let Some(staging) = db_path.parent() {
        let path = staging.join("inhouse-reader-diagnostics.json");
        if path.is_file() {
            fs::remove_file(&path)?;
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "decoder": decoded.diagnostics, "adapter": warnings.0,
            }))?,
        )?;
    }
    let release_stage = records::profile::ProfileStage::start("decoded_release");
    drop(decoded);
    release_stage.finish(master.len());
    warnings.report();
    Ok(master)
}

/// Validate required runtime semantics before merge so a bad override leaves its predecessor.
fn validate_candidate(record: &records::DecodedRecord) -> std::result::Result<(), String> {
    if !matches!(&record.record_type, b"LAND" | b"MOVT" | b"GMST" | b"RACE") {
        return Ok(());
    }
    if record.record_type == *b"LAND" {
        if !record.payload_complete {
            return Err("incomplete terrain payload".into());
        }
        if let Some(signature) = record
            .rejected_fields
            .iter()
            .copied()
            .find(|signature| matches!(signature, b"VHGT" | b"BTXT" | b"ATXT" | b"VTXT"))
        {
            return Err(format!(
                "rejected authored terrain field {}",
                String::from_utf8_lossy(&signature)
            ));
        }
    }
    let mut raw = record.to_raw_record();
    for (index, text) in decoded_text_updates(record) {
        raw.subrecords[index].1 = text;
    }
    validate_inhouse_record(&raw).map_err(|error| error.to_string())?;
    if record.record_type == *b"LAND" {
        validate_inhouse_land(&raw).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Move canonical buffers only after selected typed serialization has completed.
/// Source payload, identity, provenance and rejection metadata survive for the existing SQL pass.
fn take_runtime_record(record: &mut records::DecodedRecord) -> RawRecord {
    let fields = std::mem::take(&mut record.fields);
    RawRecord {
        form_id: record.form_id,
        record_type: record.record_type,
        flags: record.flags,
        subrecords: fields
            .into_iter()
            .map(|field| (field.signature.to_vec(), field.canonical_bytes))
            .collect(),
        cell_form_id: record.cell_form_id,
        worldspace_form_id: record.worldspace_form_id,
        load_order: record.load_order,
    }
}

/// Use the same decoded inline text in candidate validation and final publication.
/// Field indices follow both raw projections; localized keys are resolved separately.
fn decoded_text_updates(record: &records::DecodedRecord) -> Vec<(usize, Vec<u8>)> {
    record
        .fields
        .iter()
        .enumerate()
        .filter_map(|(index, field)| {
            let Value::String(text) = &field.value else {
                return None;
            };
            runtime_text(text, &field.canonical_bytes).map(|bytes| (index, bytes))
        })
        .collect()
}

/// Encode decoded text for the shared runtime extractor without changing original source bytes.
/// Typed Windows-1252 decoding owns ordinary text; runtime bytes are UTF-8 with one NUL.
/// A fixed-width ASCII code stored without a terminator keeps its exact source width.
fn runtime_text(text: &str, source: &[u8]) -> Option<Vec<u8>> {
    (!(text.is_ascii() && source == text.as_bytes())).then(|| runtime_string(text))
}

/// Encode a resolved or normalized runtime string as UTF-8 followed by one NUL.
fn runtime_string(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// Find a plugin's loose STRINGS override first, then the effective staged archive VFS.
fn read_plugin_strings(
    plugin: &Path,
    strings_root: &Path,
    bank: &str,
) -> Result<HashMap<u32, String>> {
    let file_name = format!(
        "{}_english.{bank}",
        plugin.file_stem().unwrap_or_default().to_string_lossy()
    );
    for root in [plugin.parent().unwrap_or(Path::new(".")), strings_root] {
        if let Some(folder) = child_case_insensitive(root, "strings")
            && let Some(path) = child_case_insensitive(&folder, &file_name)
        {
            return parse_text_table(&fs::read(path)?, bank != "strings");
        }
    }
    Ok(HashMap::new())
}

/// Resolve a single path component on both case-sensitive and Windows filesystems.
fn child_case_insensitive(parent: &Path, name: &str) -> Option<PathBuf> {
    let direct = parent.join(name);
    if direct.exists() {
        return Some(direct);
    }
    fs::read_dir(parent)
        .ok()?
        .filter_map(|entry| entry.ok())
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(name)
        })
        .map(|entry| entry.path())
}

/// Read STRINGS offsets within their declared data region; one bad entry is skipped.
/// DLSTRINGS/ILSTRINGS directory offsets point to a checked length prefix followed by text.
fn parse_text_table(bytes: &[u8], length_prefixed: bool) -> Result<HashMap<u32, String>> {
    let word = |offset: usize| {
        bytes
            .get(offset..offset + 4)
            .and_then(|data| data.try_into().ok())
            .map(u32::from_le_bytes)
    };
    let count =
        word(0).ok_or_else(|| color_eyre::eyre::eyre!("truncated STRINGS header"))? as usize;
    let size = word(4).ok_or_else(|| color_eyre::eyre::eyre!("truncated STRINGS header"))? as usize;
    let data_start = count
        .checked_mul(8)
        .and_then(|n| n.checked_add(8))
        .ok_or_else(|| color_eyre::eyre::eyre!("STRINGS directory overflow"))?;
    let data_end = data_start
        .checked_add(size)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated STRINGS directory/data"))?;
    let data = &bytes[data_start..data_end];
    let mut strings = HashMap::with_capacity(count);
    for index in 0..count {
        let id = word(8 + index * 8).expect("bounded directory");
        let offset = word(12 + index * 8).expect("bounded directory") as usize;
        let tail = data.get(offset..);
        let tail = if length_prefixed {
            tail.and_then(|tail| {
                let size = u32::from_le_bytes(tail.get(..4)?.try_into().ok()?) as usize;
                tail.get(4..4usize.checked_add(size)?)
            })
        } else {
            tail
        };
        if let Some(tail) = tail
            && let Some(end) = tail.iter().position(|byte| *byte == 0)
        {
            strings.insert(id, records::decode_text(&tail[..end]));
        }
    }
    Ok(strings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A four-byte event code without a terminator keeps its width; padded and accented
    /// text become canonical UTF-8 with one NUL.
    #[test]
    fn runtime_text_keeps_unterminated_codes_and_canonicalizes_other_text() {
        assert_eq!(runtime_text("ADIA", b"ADIA"), None);
        assert_eq!(runtime_text("Bool", b"Bool\0\0"), Some(b"Bool\0".to_vec()));
        assert_eq!(
            runtime_text("Caf\u{e9}", b"Caf\xe9\0"),
            Some("Caf\u{e9}\0".as_bytes().to_vec())
        );
    }

    /// A broken length-prefixed entry cannot consume its valid table neighbor.
    #[test]
    fn dlstrings_and_ilstrings_bound_each_entry() {
        let mut table = Vec::new();
        table.extend(2u32.to_le_bytes());
        table.extend(12u32.to_le_bytes());
        table.extend(41u32.to_le_bytes());
        table.extend(0u32.to_le_bytes());
        table.extend(42u32.to_le_bytes());
        table.extend(4u32.to_le_bytes());
        table.extend(u32::MAX.to_le_bytes());
        table.extend(4u32.to_le_bytes());
        table.extend(b"yes\0");
        let decoded = parse_text_table(&table, true).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[&42], "yes");
        assert!(!decoded.contains_key(&41));
        table[4..8].copy_from_slice(&13u32.to_le_bytes());
        assert!(parse_text_table(&table, true).is_err());
    }
}

#[cfg(test)]
mod runtime_move_tests {
    use super::*;
    use crate::records::{DecodedField, DecodedRecord};

    fn record() -> DecodedRecord {
        DecodedRecord {
            form_id: 0x0100_1234,
            source_form_id: 0x0200_5678,
            record_type: *b"INFO",
            flags: 0x4000,
            version_control: 0x1234_5678,
            form_version: 44,
            header_unknown: 19,
            load_order: 7,
            cell_form_id: Some(91),
            worldspace_form_id: Some(92),
            topic_form_id: Some(93),
            source_topic_form_id: Some(0x0200_0001),
            topic_group_offset: Some(129),
            source_record_offset: 257,
            fields: vec![
                DecodedField {
                    signature: *b"DATA",
                    name: "first".into(),
                    value: Value::Bytes(vec![9, 9]),
                    canonical_bytes: vec![1, 2, 3],
                },
                DecodedField {
                    signature: *b"DATA",
                    name: "second".into(),
                    value: Value::Struct(vec![(
                        "nested".into(),
                        Value::Array(vec![Value::Float(-0.0), Value::Unsigned(u64::MAX)]),
                    )]),
                    canonical_bytes: vec![4, 5],
                },
                DecodedField {
                    signature: *b"FULL",
                    name: "localized".into(),
                    value: Value::LocalizedString(100),
                    canonical_bytes: 100u32.to_le_bytes().to_vec(),
                },
            ],
            rejected_fields: vec![*b"VMAD"],
            payload_complete: false,
            raw_payload: b"source".to_vec(),
            supported: false,
        }
    }

    #[test]
    fn consuming_projection_matches_borrowed_oracle_but_moves_exact_ordered_buffers() {
        let mut decoded = record();
        let oracle = decoded.to_raw_record();
        let pointers = decoded
            .fields
            .iter()
            .map(|f| f.canonical_bytes.as_ptr())
            .collect::<Vec<_>>();
        let before = decoded.clone();
        let raw = take_runtime_record(&mut decoded);
        assert_eq!(raw.subrecords, oracle.subrecords);
        assert_eq!(
            raw.subrecords,
            vec![
                (b"DATA".to_vec(), vec![1, 2, 3]),
                (b"DATA".to_vec(), vec![4, 5]),
                (b"FULL".to_vec(), vec![100, 0, 0, 0])
            ]
        );
        assert_eq!(
            (
                raw.form_id,
                raw.record_type,
                raw.flags,
                raw.cell_form_id,
                raw.worldspace_form_id,
                raw.load_order
            ),
            (0x0100_1234, *b"INFO", 0x4000, Some(91), Some(92), 7)
        );
        for (index, ptr) in pointers.into_iter().enumerate() {
            assert_eq!(raw.subrecords[index].1.as_ptr(), ptr);
        }
        assert!(decoded.fields.is_empty());
        let mut expected_survivors = before;
        expected_survivors.fields.clear();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(expected_survivors).unwrap()
        );
        assert_eq!(decoded.raw_payload, b"source");
    }

    #[test]
    fn typed_localized_key_and_digest_are_written_before_runtime_replacement() {
        let mut decoded = record();
        let localization = BTreeMap::from([(
            2usize,
            serde_json::json!({"bank":"strings","id":100,"text":"different UTF8 runtime","status":"resolved"}),
        )]);
        let mut wire = Vec::new();
        records::typed_export::write_record(&mut wire, &decoded, &localization).unwrap();
        let typed: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(
            typed["fields"][2]["value"],
            serde_json::json!({"LocalizedString":100})
        );
        assert_eq!(
            typed["fields"][2]["canonical_bytes"],
            serde_json::json!([100, 0, 0, 0])
        );
        assert_eq!(
            typed["source_payload_sha256"],
            "41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d"
        );
        let mut runtime = take_runtime_record(&mut decoded);
        runtime.subrecords[2].1 = runtime_string("different UTF8 runtime");
        assert_ne!(runtime.subrecords[2].1, vec![100, 0, 0, 0]);
        assert_eq!(decoded.raw_payload, b"source");
        assert_eq!(
            typed["fields"][2]["canonical_bytes"],
            serde_json::json!([100, 0, 0, 0])
        );
    }

    #[test]
    fn empty_fields_still_preserve_source_and_runtime_header_metadata() {
        let mut decoded = record();
        decoded.fields.clear();
        let raw = take_runtime_record(&mut decoded);
        assert!(raw.subrecords.is_empty());
        assert_eq!(raw.form_id, 0x0100_1234);
        assert_eq!(raw.load_order, 7);
        assert_eq!(decoded.source_form_id, 0x0200_5678);
        assert_eq!(decoded.raw_payload, b"source");
        assert_eq!(decoded.rejected_fields, vec![*b"VMAD"]);
        assert!(!decoded.payload_complete);
    }
}

#[cfg(test)]
mod history_reuse_private_controls {
    use super::convert_plugins;
    use crate::records::history_source::{SourceQuery, visit_source_history};
    use rusqlite::{Connection, OpenFlags};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    /// Independent own native main-header bytes, including unequal padding sentinels.
    fn native_frame(tag: &[u8; 4], raw_id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend((payload.len() as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(raw_id.to_le_bytes());
        out.extend(0x18764231u32.to_le_bytes());
        out.extend(44u16.to_le_bytes());
        out.extend(0x2113u16.to_le_bytes());
        out.extend(payload);
        out
    }

    /// Independent own subrecord bytes; no production framing helper supplies expected bytes.
    fn native_subrecord(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend((payload.len() as u16).to_le_bytes());
        out.extend(payload);
        out
    }

    /// Write one independent full plugin with no MAST and native self index zero.
    fn independent_plugin(
        data: &Path,
        name: &str,
        local: u32,
        edid: &[u8],
    ) -> (PathBuf, Vec<u8>, Vec<u8>) {
        let payload = native_subrecord(b"EDID", edid);
        let record = native_frame(b"STAT", local, &payload);
        let mut original = native_frame(b"TES4", 0, &native_subrecord(b"HEDR", &[0; 12]));
        original.extend(&record);
        let path = data.join(name);
        fs::write(&path, original).unwrap();
        (path, payload, record[..24].to_vec())
    }

    /// Repeat real in-house setup/export against one DB, then prove new snapshot source identity.
    /// The old live history FK fails at the second conversion's plugin replacements.
    #[test]
    fn reused_inhouse_database_reorders_plugins_and_refreshes_source_history() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("Data");
        let output = root.path().join("output");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&output).unwrap();
        let (a, a_payload, a_header) = independent_plugin(&data, "A.esm", 0x1800, b"different_a\0");
        let (b, b_payload, b_header) = independent_plugin(&data, "B.esm", 0x1900, b"unequal_b\0");
        let a_pin = crate::cache::hash_bytes(&fs::read(&a).unwrap());
        let b_pin = crate::cache::hash_bytes(&fs::read(&b).unwrap());
        let database = output.join("skyrim_world.db");

        let first = convert_plugins(&[a.clone(), b.clone()], &database, &data).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[&0x1800].load_order, 0);
        assert_eq!(first[&0x0100_1900].load_order, 1);
        drop(first);
        let second = convert_plugins(&[b.clone(), a.clone()], &database, &data).unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(second[&0x1900].load_order, 0);
        assert_eq!(second[&0x0100_1800].load_order, 1);
        drop(second);

        let conn =
            Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let source_count: u32 = conn
            .query_row("SELECT count(*) FROM inhouse_source_records", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(source_count, 2); // This owned provenance table is fully refreshed.
        for (runtime, local, supplier, owner, payload) in [
            (0x1900u32, 0x1900u32, 0u32, "b.esm", b_payload.as_slice()),
            (0x0100_1800, 0x1800, 1, "a.esm", a_payload.as_slice()),
        ] {
            let (raw,priority,tag,bytes):(u32,u32,String,Vec<u8>)=conn.query_row(
                "SELECT source_form_id,load_order,record_type,payload FROM inhouse_source_records WHERE form_id=?1",
                [runtime],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
            assert_eq!((raw, priority, tag), (local, supplier, "STAT".into()));
            assert_eq!(bytes, payload);
            let (stored_runtime,stored_owner,stored_local):(Option<u32>,String,u32)=conn.query_row(
                "SELECT form_id,original_owner,full_local FROM record_overrides WHERE plugin_id=?1 AND source_form_id=?2 AND status='accepted'",
                rusqlite::params![supplier,local],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
            assert_eq!(
                (stored_runtime, stored_owner, stored_local),
                (Some(runtime), owner.into(), local)
            );
            let (runtime_tag, runtime_supplier): (String, u32) = conn
                .query_row(
                    "SELECT record_type,load_order FROM records WHERE form_id=?1",
                    [runtime],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((runtime_tag, runtime_supplier), ("STAT".into(), supplier));
        }
        let headers: u32 = conn
            .query_row("SELECT count(*) FROM record_overrides", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(headers, 4);
        drop(conn);
        // Authenticate this completed synthetic producer's current DB for the raw API call.
        let produced_database_pin = crate::cache::hash_bytes(&fs::read(&database).unwrap());
        for (owner, local, runtime, supplier, payload, header) in [
            (
                "A.esm",
                0x1800u32,
                0x0100_1800u32,
                1u32,
                a_payload.as_slice(),
                a_header.as_slice(),
            ),
            (
                "B.esm",
                0x1900,
                0x1900,
                0,
                b_payload.as_slice(),
                b_header.as_slice(),
            ),
        ] {
            let mut seen = 0;
            let summary = visit_source_history(
                &output,
                None,
                SourceQuery::Source {
                    original_owner: owner.into(),
                    full_local: local,
                },
                |version| {
                    assert_eq!(version.plugin_id, supplier);
                    assert_eq!(version.source_form_id, local);
                    assert_eq!(version.runtime_form_id, Some(runtime));
                    assert_eq!(
                        version.identity.original_owner.as_deref(),
                        Some(owner.to_ascii_lowercase().as_str())
                    );
                    assert_eq!(version.identity.full_local, Some(local));
                    assert!(
                        version.latest_source_candidate && version.latest_reader_admitted_candidate
                    );
                    assert!(version.accepted_source_winner && version.runtime_record_present);
                    assert!(!version.runtime_fallback_before_latest_source);
                    let raw = version.raw.unwrap();
                    assert_eq!(raw.header.as_slice(), header);
                    assert_eq!(raw.stored_payload.as_deref(), Some(payload));
                    seen += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(seen, 1);
            assert!(summary.fully_raw_available);
        }
        assert_eq!(
            crate::cache::hash_bytes(&fs::read(&database).unwrap()),
            produced_database_pin
        );
        assert_eq!(crate::cache::hash_bytes(&fs::read(&a).unwrap()), a_pin);
        assert_eq!(crate::cache::hash_bytes(&fs::read(&b).unwrap()), b_pin);
        // Unrelated old runtime rows are outside this gate; no global cleanup claim is made.
    }
}
