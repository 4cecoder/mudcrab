//! Optional tooling provenance: header-only chains and lazy source-version decoding.
use super::{DecodedRecord, PluginDiagnostics, Value, decoder, scanner};
use crate::esm::{binary::PluginMetadata, load_order::LoadOrder};
use color_eyre::{Result, eyre::ensure};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// Stored filename, original source path and native SHA-256.
pub(crate) type PluginSource = (String, PathBuf, Vec<u8>);

/// One physical native record header; rejected versions never participate in winner selection.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub form_id: Option<u32>,
    pub plugin_index: u32,
    pub source_form_id: u32,
    pub record_type: [u8; 4],
    pub flags: u32,
    pub source_record_offset: u64,
    /// Declared on-disk payload bytes, excluding the 24-byte main-record header.
    pub source_record_size: u32,
    pub source_group_end: u64,
    pub frame_complete: bool,
    pub accepted: bool,
    pub reason: Option<String>,
    pub source_cell_form_id: Option<u32>,
    pub source_worldspace_form_id: Option<u32>,
    pub source_topic_form_id: Option<u32>,
    pub topic_group_offset: Option<u64>,
    /// Temporary decoded identity only; no losing payload or typed fields are retained.
    pub editor_id: Option<String>,
}

/// Map header identities using the existing slot/master facts, including undecodable payloads.
pub(crate) fn header_entry(
    header: scanner::ObservedHeader,
    order: &LoadOrder,
    index: usize,
) -> HistoryEntry {
    let form_id = super::source_identity::header_runtime_id(
        &header.record_type,
        header.source_form_id,
        index,
        order,
    );
    HistoryEntry {
        form_id,
        plugin_index: index as u32,
        source_form_id: header.source_form_id,
        record_type: header.record_type,
        flags: header.flags,
        source_record_offset: header.source_record_offset,
        source_record_size: header.source_record_size,
        source_group_end: header.source_group_end,
        frame_complete: header.frame_complete,
        accepted: false,
        // Decoder/candidate rejection supplies its reason only when it occurs.
        reason: header.failure,
        source_cell_form_id: header.cell_form_id,
        source_worldspace_form_id: header.worldspace_form_id,
        source_topic_form_id: header.source_topic_form_id,
        topic_group_offset: header.topic_group_offset,
        editor_id: None,
    }
}

/// Associate rejected GMST identities only through an already decoded EditorID.
pub(crate) fn reconcile_gmst(entries: &mut [HistoryEntry], ids: &HashMap<String, u32>) {
    for entry in entries
        .iter_mut()
        .filter(|entry| !entry.accepted && entry.record_type == *b"GMST")
    {
        if let Some(canonical) = entry
            .editor_id
            .as_ref()
            .and_then(|name| ids.get(name))
            .copied()
        {
            entry.form_id = Some(canonical);
        }
    }
}

/// Persist all headers, including singletons, in a deterministic primary-key tree.
/// The caller supplies its provenance transaction; database failures remain infrastructure errors.
pub(crate) fn write_tables(
    conn: &Connection,
    entries: &[HistoryEntry],
    order: &LoadOrder,
    paths: &[PathBuf],
) -> Result<()> {
    // Complete conversion replaces this owned tooling snapshot atomically with provenance.
    // Recreate only this table so a retained older optional-history schema is not reused.
    conn.execute_batch("DROP TABLE IF EXISTS record_overrides;
        CREATE TABLE record_overrides(
        form_id INTEGER, chain_position INTEGER NOT NULL,
        plugin_id INTEGER NOT NULL, source_form_id INTEGER NOT NULL,
        deleted INTEGER NOT NULL CHECK(deleted IN (0,1)), record_type TEXT NOT NULL,
        flags INTEGER NOT NULL, source_record_offset INTEGER NOT NULL, source_record_size INTEGER NOT NULL,
        status TEXT NOT NULL CHECK(status IN ('accepted','skipped','header')), reason TEXT,
        source_cell_form_id INTEGER, source_worldspace_form_id INTEGER,
        source_topic_form_id INTEGER, topic_group_offset INTEGER,
        plugin_masters TEXT, source_path TEXT, schema_sha256 TEXT,
        original_owner TEXT, full_local INTEGER, source_identity_state TEXT NOT NULL,
        source_group_end INTEGER NOT NULL, frame_complete INTEGER NOT NULL CHECK(frame_complete IN (0,1)),
        record_signature BLOB NOT NULL CHECK(length(record_signature)=4),
        PRIMARY KEY(plugin_id,source_record_offset)) WITHOUT ROWID;
        CREATE INDEX record_overrides_chain ON record_overrides(form_id,chain_position);
        CREATE INDEX record_overrides_source ON record_overrides(original_owner,full_local,plugin_id,source_record_offset);")?;
    let source_paths = paths
        .iter()
        .map(|p| p.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut sorted: Vec<_> = (0..entries.len()).collect();
    sorted.sort_by_key(|&index| {
        let entry = &entries[index];
        (
            entry.form_id,
            entry.plugin_index,
            entry.source_record_offset,
        )
    });
    let mut chain_positions = vec![0u32; entries.len()];
    let mut previous = None;
    let mut position = 0u32;
    for &index in &sorted {
        let entry = &entries[index];
        let key = entry.form_id;
        if key != previous {
            position = 0;
            previous = key;
        }
        chain_positions[index] = position;
        position += 1;
    }
    // Preserve runtime-chain numbering while writing in the owned table's physical primary key.
    sorted.sort_unstable_by_key(|&index| {
        let entry = &entries[index];
        (entry.plugin_index, entry.source_record_offset)
    });
    let mut insert = conn.prepare("INSERT INTO record_overrides VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)")?;
    for index in sorted {
        let entry = &entries[index];
        let key = entry.form_id;
        // Unmappable headers retain NULL plus native identity and physical offset.
        let is_header = entry.record_type == *b"TES4";
        let identity = super::source_identity::resolve(
            &entry.record_type,
            entry.source_form_id,
            entry.plugin_index as usize,
            order,
        );
        insert.execute(params![
            key,
            chain_positions[index],
            entry.plugin_index,
            entry.source_form_id,
            entry.flags & 0x20 != 0,
            String::from_utf8_lossy(&entry.record_type),
            entry.flags,
            entry.source_record_offset,
            entry.source_record_size,
            if is_header {
                "header"
            } else if entry.accepted {
                "accepted"
            } else {
                "skipped"
            },
            entry.reason,
            entry.source_cell_form_id,
            entry.source_worldspace_form_id,
            entry.source_topic_form_id,
            entry.topic_group_offset,
            is_header
                .then(|| serde_json::to_string(
                    &order.metadata[entry.plugin_index as usize].masters
                ))
                .transpose()?,
            is_header.then(|| source_paths[entry.plugin_index as usize].to_string_lossy()),
            is_header.then(|| crate::cache::hash_bytes(super::SCHEMA_BYTES)),
            identity.original_owner,
            identity.full_local,
            identity.state.as_str(),
            entry.source_group_end,
            entry.frame_complete,
            entry.record_type.as_slice()
        ])?;
    }
    Ok(())
}

/// Header/source facts and the independently decoded result for one version.
#[derive(Debug, Serialize)]
pub struct HistoryVersion {
    pub chain_position: u32,
    pub plugin: String,
    pub plugin_id: u32,
    pub source_form_id: u32,
    pub deleted: bool,
    pub source_record_offset: u64,
    pub source_record_size: u32,
    pub status: String,
    pub reason: Option<String>,
    pub winner: bool,
    pub decode_status: String,
    pub decode_reason: Option<String>,
    pub record: Option<DecodedRecord>,
    pub diagnostics: PluginDiagnostics,
}

/// Requested chain plus a field-occurrence comparison, suitable for scripts and text views.
#[derive(Debug, Serialize)]
pub struct HistoryReport {
    pub form_id: u32,
    pub versions: Vec<HistoryVersion>,
    pub fields: Vec<HistoryField>,
}

/// Compare a stable authored role and occurrence across the requested source versions.
#[derive(Debug, Serialize)]
pub struct HistoryField {
    pub field: String,
    pub different: bool,
    pub values: Vec<Option<Value>>,
}

/// Reconstitute original slot/master assignment without reading any source plugin headers.
pub(crate) fn stored_order(conn: &Connection) -> Result<(LoadOrder, Vec<PluginSource>)> {
    let mut statement = conn.prepare("SELECT p.name,h.flags,h.plugin_masters,h.source_path,p.checksum FROM plugins p JOIN record_overrides h ON h.plugin_id=p.id AND h.record_type='TES4' AND h.source_record_offset=0 ORDER BY p.priority,p.id")?;
    let rows = statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    let mut names = Vec::new();
    let mut metadata = Vec::new();
    let mut sources = Vec::new();
    for row in rows {
        let (name, flags, masters, source, checksum) = row?;
        names.push(name.clone());
        metadata.push(PluginMetadata {
            flags,
            masters: serde_json::from_str(&masters)?,
        });
        sources.push((name, source.into(), checksum));
    }
    Ok((LoadOrder::from_metadata(names, metadata)?, sources))
}

/// Hash a source handle, then read only the requested native record with bounded allocation.
fn decode_version(
    path: &Path,
    expected: &[u8],
    entry: &HistoryEntry,
    order: &LoadOrder,
    kinds: &HashMap<u32, [u8; 4]>,
    diagnostics: &mut PluginDiagnostics,
) -> Result<DecodedRecord> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    ensure!(
        hasher.finalize().as_slice() == expected,
        "plugin changed: checksum mismatch; version not decoded"
    );
    file.seek(SeekFrom::Start(entry.source_record_offset))?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header)?;
    let word = |offset| {
        u32::from_le_bytes(
            header[offset..offset + 4]
                .try_into()
                .expect("fixed native header"),
        )
    };
    ensure!(
        header[..4] == entry.record_type
            && word(4) == entry.source_record_size
            && word(8) == entry.flags
            && word(12) == entry.source_form_id,
        "stored record header does not match source"
    );
    let length = file.metadata()?.len();
    ensure!(
        entry.frame_complete
            && entry.source_group_end <= length
            && entry.source_record_offset.saturating_add(24) <= entry.source_group_end
            && u64::from(entry.source_record_size)
                <= entry
                    .source_group_end
                    .saturating_sub(entry.source_record_offset.saturating_add(24)),
        "truncated native record payload"
    );
    ensure!(
        entry.source_record_size <= 64 * 1024 * 1024,
        "record payload exceeds diagnostic allocation bound"
    );
    let mut payload = vec![0; entry.source_record_size as usize];
    file.read_exact(&mut payload)?;
    if entry.flags & 0x40000 != 0 {
        payload = scanner::inflate(&payload).map_err(color_eyre::eyre::Error::msg)?;
    }
    let scanned = scanner::ScannedRecord {
        record_type: entry.record_type,
        flags: entry.flags,
        source_form_id: entry.source_form_id,
        version_control: word(16),
        form_version: u16::from_le_bytes(header[20..22].try_into().expect("fixed native header")),
        header_unknown: u16::from_le_bytes(header[22..24].try_into().expect("fixed native header")),
        cell_form_id: entry.source_cell_form_id,
        worldspace_form_id: entry.source_worldspace_form_id,
        source_topic_form_id: entry.source_topic_form_id,
        topic_group_offset: entry.topic_group_offset,
        source_record_offset: entry.source_record_offset,
        payload,
    };
    let context = decoder::Context {
        order,
        plugin_index: entry.plugin_index as usize,
        localized: order.metadata[entry.plugin_index as usize].flags & 0x80 != 0,
        winning_types: Some(kinds),
    };
    let mut record = decoder::decode(scanned, super::schema(), &context, diagnostics)
        .map_err(color_eyre::eyre::Error::msg)?;
    record.form_id = entry.form_id.unwrap_or(record.form_id);
    ensure!(
        record.payload_complete && record.rejected_fields.is_empty(),
        "undecodable fields or incomplete subrecord framing: {:?}",
        record.rejected_fields
    );
    Ok(record)
}

/// Load a requested chain read-only; one missing, changed or undecodable source remains local.
pub fn read_history(pack: &Path, data: Option<&Path>, form_id: u32) -> Result<HistoryReport> {
    let database = if pack.is_file() {
        pack.to_path_buf()
    } else {
        pack.join("skyrim_world.db")
    };
    let conn = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (order, sources) = stored_order(&conn)?;
    let schema_hash: String = conn.query_row("SELECT schema_sha256 FROM record_overrides WHERE record_type='TES4' ORDER BY plugin_id LIMIT 1", [], |r| r.get(0))?;
    let schema_matches = schema_hash == crate::cache::hash_bytes(super::SCHEMA_BYTES);
    let mut kind_statement =
        conn.prepare("SELECT form_id,record_type FROM inhouse_source_records")?;
    let kinds = kind_statement
        .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|row| row.ok())
        .filter_map(|(id, tag)| tag.as_bytes().try_into().ok().map(|tag| (id, tag)))
        .collect();
    let mut statement = conn.prepare("SELECT chain_position,plugin_id,source_form_id,record_type,flags,source_record_offset,source_record_size,status,reason,source_cell_form_id,source_worldspace_form_id,source_topic_form_id,topic_group_offset,source_group_end,frame_complete,record_signature FROM record_overrides WHERE form_id=?1 ORDER BY chain_position")?;
    let rows = statement.query_map([form_id], |r| {
        Ok((
            r.get::<_, u32>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, u32>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, u32>(4)?,
            r.get::<_, u64>(5)?,
            r.get::<_, u32>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, Option<u32>>(9)?,
            r.get::<_, Option<u32>>(10)?,
            r.get::<_, Option<u32>>(11)?,
            r.get::<_, Option<u64>>(12)?,
            r.get::<_, u64>(13)?,
            r.get::<_, bool>(14)?,
            r.get::<_, Vec<u8>>(15)?,
        ))
    })?;
    let mut versions = Vec::new();
    for row in rows {
        let (
            position,
            index,
            source,
            _tag,
            flags,
            offset,
            size,
            status,
            reason,
            cell,
            world,
            topic,
            group,
            containing_group_end,
            frame_complete,
            signature,
        ) = row?;
        let mut version = HistoryVersion {
            chain_position: position,
            plugin: sources
                .get(index as usize)
                .map(|s| s.0.clone())
                .unwrap_or_else(|| format!("plugin {index}")),
            plugin_id: index,
            source_form_id: source,
            deleted: flags & 0x20 != 0,
            source_record_offset: offset,
            source_record_size: size,
            status: status.clone(),
            reason: reason.clone(),
            winner: false,
            decode_status: "undecodable".into(),
            decode_reason: None,
            record: None,
            diagnostics: PluginDiagnostics::default(),
        };
        let decoded = (|| -> Result<DecodedRecord> {
            ensure!(
                schema_matches,
                "diagnostic schema differs from the producing schema"
            );
            let (name, path, checksum) = sources
                .get(index as usize)
                .ok_or_else(|| color_eyre::eyre::eyre!("absent stored source metadata"))?;
            let path = data
                .map(|root| root.join(name))
                .unwrap_or_else(|| path.clone());
            let entry = HistoryEntry {
                form_id: Some(form_id),
                plugin_index: index,
                source_form_id: source,
                record_type: signature
                    .as_slice()
                    .try_into()
                    .map_err(|_| color_eyre::eyre::eyre!("invalid stored signature"))?,
                flags,
                source_record_offset: offset,
                source_record_size: size,
                source_group_end: containing_group_end,
                frame_complete,
                accepted: status == "accepted",
                reason,
                source_cell_form_id: cell,
                source_worldspace_form_id: world,
                source_topic_form_id: topic,
                topic_group_offset: group,
                editor_id: None,
            };
            decode_version(
                &path,
                checksum,
                &entry,
                &order,
                &kinds,
                &mut version.diagnostics,
            )
        })();
        match decoded {
            Ok(record) => {
                version.decode_status = "decoded".into();
                version.record = Some(record);
            }
            Err(error) => version.decode_reason = Some(format!("{error}")),
        }
        versions.push(version);
    }
    if let Some(last) = versions.iter_mut().rev().find(|v| v.status == "accepted") {
        last.winner = !last.deleted;
    }
    let mut roles = BTreeMap::<String, Vec<Option<Value>>>::new();
    for (column, version) in versions.iter().enumerate() {
        let mut occurrences = HashMap::<String, u32>::new();
        if let Some(record) = &version.record {
            for field in &record.fields {
                let count = occurrences.entry(field.name.clone()).or_default();
                let name = format!(
                    "{}:{}[{}]",
                    String::from_utf8_lossy(&field.signature),
                    field.name,
                    *count
                );
                *count += 1;
                roles
                    .entry(name)
                    .or_insert_with(|| vec![None; versions.len()])[column] =
                    Some(field.value.clone());
            }
        }
    }
    let fields = roles
        .into_iter()
        .map(|(field, values)| HistoryField {
            different: values.windows(2).any(|v| v[0] != v[1]),
            field,
            values,
        })
        .collect();
    Ok(HistoryReport {
        form_id,
        versions,
        fields,
    })
}

/// Print load-order columns and mark every changed or absent authored field occurrence.
pub fn display(report: &HistoryReport) -> String {
    let mut output = format!("Record {:08X}\n", report.form_id);
    for version in &report.versions {
        output.push_str(&format!(
            "{}: {} source={:08X} offset={} size={} {}{}{} {}{}\n",
            version.chain_position,
            version.plugin,
            version.source_form_id,
            version.source_record_offset,
            version.source_record_size,
            version.status,
            if version.deleted { " deleted" } else { "" },
            if version.winner { " winner" } else { "" },
            version.decode_status,
            version
                .decode_reason
                .as_ref()
                .map(|s| format!(": {s}"))
                .unwrap_or_default()
        ));
    }
    output.push_str("Field | ");
    output.push_str(
        &report
            .versions
            .iter()
            .map(|v| v.plugin.as_str())
            .collect::<Vec<_>>()
            .join(" | "),
    );
    output.push('\n');
    for field in &report.fields {
        output.push_str(&format!(
            "{}{} | {}\n",
            if field.different { "* " } else { "  " },
            field.field,
            field
                .values
                .iter()
                .map(|v| v
                    .as_ref()
                    .map(|v| serde_json::to_string(v)
                        .unwrap_or_else(|_| "unrepresentable value".into()))
                    .unwrap_or_else(|| "<absent>".into()))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    output
}
