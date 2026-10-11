//! Ordered subrecord matching and generic values with typed FormID resolution.
use super::{
    DecodedField, DecodedRecord, PluginDiagnostics, ReadResult, Value,
    scanner::ScannedRecord,
    schema_format::{FieldSchema, Schema},
};
use crate::esm::load_order::LoadOrder;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    sync::OnceLock,
};

/// Per-file interpretation supplied by the shared load-order authority.
pub(crate) struct Context<'a> {
    pub order: &'a LoadOrder,
    pub plugin_index: usize,
    pub localized: bool,
    pub winning_types: Option<&'a HashMap<u32, [u8; 4]>>,
}

/// One flattened subrecord occurrence, including its enclosing repeat groups.
struct Entry {
    field: FieldSchema,
    repeat_ranges: Vec<RepeatRange>,
}

/// An authored repeated group; explicit end markers prevent interior rewinds.
struct RepeatRange {
    start: usize,
    end: usize,
    terminated: bool,
}

/// Prepared schemas avoid re-expanding reusable definitions for every record.
pub(crate) struct PreparedRecord {
    entries: Vec<Entry>,
    unordered: bool,
}

/// One prepared map shared by initial decoding, deferred replay and target validation.
pub(crate) type PreparedSchemas = HashMap<[u8; 4], PreparedRecord>;

/// Flatten only subrecord groups; byte structs and arrays stay typed and nested.
fn flatten(fields: &[FieldSchema], schema: &Schema, output: &mut Vec<Entry>) {
    for field in fields {
        let field = schema.resolve(field).expect("validated definition");
        if field.kind == "group" {
            let start = output.len();
            flatten(&field.fields, schema, output);
            let end = output.len();
            if field.repeat {
                for entry in &mut output[start..end] {
                    entry.repeat_ranges.push(RepeatRange {
                        start,
                        end,
                        terminated: field.repeat_terminated,
                    });
                }
            }
        } else if field.signature.is_some() {
            output.push(Entry {
                field,
                repeat_ranges: Vec::new(),
            });
        }
    }
}

/// Prepare matched entries from a checked schema without changing the embedded singleton.
pub(crate) fn prepare_schema(schema: &Schema) -> PreparedSchemas {
    schema
        .records
        .iter()
        .map(|record| {
            let mut entries = Vec::new();
            flatten(&record.fields, schema, &mut entries);
            (
                record
                    .signature
                    .as_bytes()
                    .try_into()
                    .expect("checked signature"),
                PreparedRecord {
                    entries,
                    unordered: record.allow_unordered,
                },
            )
        })
        .collect()
}

/// Prepare the build-validated embedded schema exactly once per process.
pub(crate) fn prepared() -> &'static PreparedSchemas {
    static PREPARED: OnceLock<PreparedSchemas> = OnceLock::new();
    PREPARED.get_or_init(|| prepare_schema(super::schema()))
}

pub(crate) fn string_table(record_type: &[u8; 4], field_name: &str) -> &'static str {
    prepared()
        .get(record_type)
        .and_then(|record| {
            record
                .entries
                .iter()
                .find(|entry| entry.field.name == field_name)
        })
        .and_then(|entry| entry.field.string_table.as_deref())
        .unwrap_or("strings")
}

/// Match a file-order occurrence without moving the cursor on an unknown tag.
fn match_field(
    record: &PreparedRecord,
    signature: &[u8; 4],
    cursor: usize,
    last: Option<usize>,
) -> Option<usize> {
    let matches = |index: usize| {
        let entry = &record.entries[index];
        entry
            .field
            .signature
            .as_deref()
            .is_some_and(|tag| tag.as_bytes() == signature)
            && entry.repeat_ranges.iter().all(|range| {
                !range.terminated
                    || index == range.start
                    || (cursor > range.start && cursor < range.end)
            })
    };
    if let Some(last) = last {
        for range in &record.entries[last].repeat_ranges {
            let (start, end) = (range.start, range.end);
            if cursor <= end {
                if range.terminated && cursor == end {
                    if matches(start) {
                        return Some(start);
                    }
                    continue;
                }
                if let Some(index) = (cursor..end).find(|&index| matches(index)) {
                    return Some(index);
                }
                if let Some(index) = (start..end).find(|&index| matches(index)) {
                    return Some(index);
                }
            }
        }
    }
    if let Some(index) = (cursor..record.entries.len()).find(|&index| matches(index)) {
        return Some(index);
    }
    if record.unordered {
        return (0..record.entries.len()).find(|&index| matches(index));
    }
    None
}

/// LAND's ATXT belongs inside its repeated layer group, rather than starting it.
fn layer_repeat_range(record: &PreparedRecord, index: usize) -> Option<(usize, usize)> {
    record.entries[index]
        .repeat_ranges
        .iter()
        .find(|range| index >= range.start && index < range.end)
        .map(|range| (range.start, range.end))
}

/// Framed prefix and whether its remaining payload was safe to scan.
struct FramedSubrecords<'a> {
    fields: Vec<([u8; 4], &'a [u8])>,
    complete: bool,
}

/// Frame subrecords with XXXX lengths; an unrecoverable boundary drops only this remainder.
fn subrecords<'a>(
    payload: &'a [u8],
    diagnostics: &mut PluginDiagnostics,
    id: u32,
) -> FramedSubrecords<'a> {
    let mut position = 0;
    let mut output = Vec::new();
    let mut extended = None;
    let mut complete = true;
    while position < payload.len() {
        if payload.len() - position < 6 {
            complete = false;
            diagnostics.note("field", || {
                format!("record {id:08X}: incomplete subrecord header at {position}")
            });
            break;
        }
        let signature: [u8; 4] = payload[position..position + 4]
            .try_into()
            .expect("checked subrecord");
        let short = u16::from_le_bytes(
            payload[position + 4..position + 6]
                .try_into()
                .expect("checked size"),
        ) as usize;
        position += 6;
        if signature == *b"XXXX" {
            if short != 4 || payload.len() - position < 4 || extended.is_some() {
                complete = false;
                diagnostics.note("field", || format!("record {id:08X}: malformed XXXX"));
                break;
            }
            extended = Some(u32::from_le_bytes(
                payload[position..position + 4]
                    .try_into()
                    .expect("checked XXXX"),
            ) as usize);
            position += 4;
            continue;
        }
        let size = extended.take().unwrap_or(short);
        if size > payload.len() - position {
            complete = false;
            diagnostics.note("field", || {
                format!(
                    "record {id:08X}: {} payload exceeds record boundary",
                    String::from_utf8_lossy(&signature)
                )
            });
            break;
        }
        output.push((signature, &payload[position..position + size]));
        position += size;
    }
    if extended.is_some() {
        complete = false;
        diagnostics.note("field", || format!("record {id:08X}: orphan XXXX"));
    }
    FramedSubrecords {
        fields: output,
        complete,
    }
}

/// Resolve a file-relative link; invalid optional values become null with one counted diagnostic.
fn remap(
    value: u32,
    context: &Context<'_>,
    optional: bool,
    diagnostics: &mut PluginDiagnostics,
) -> Result<u32, String> {
    if value == 0 {
        return Ok(0);
    }
    let masters = &context.order.metadata[context.plugin_index].masters;
    let index = (value >> 24) as usize;
    if optional && index > masters.len() {
        diagnostics.note("link", || {
            format!("optional FormID {value:08X} exceeds master list")
        });
        return Ok(0);
    }
    let owner = if index < masters.len() {
        masters[index].to_ascii_lowercase()
    } else {
        context.order.names[context.plugin_index].clone()
    };
    if let Some(&slot) = context.order.light.get(&owner) {
        if value & 0x00ff_ffff > 0xfff {
            if optional {
                diagnostics.note("link", || {
                    format!("optional light FormID {value:08X} is wider than 12 bits")
                });
                return Ok(0);
            }
            return Err(format!(
                "record light FormID {value:08X} is wider than 12 bits"
            ));
        }
        return Ok(0xfe00_0000 | slot << 12 | value & 0xfff);
    }
    if let Some(slot) = context.order.normal.get(&owner) {
        return Ok(slot << 24 | value & 0x00ff_ffff);
    }
    if optional {
        diagnostics.note("link", || {
            format!("optional FormID {value:08X} has absent owner {owner}")
        });
        Ok(0)
    } else {
        Err(format!(
            "record FormID {value:08X} has absent owner {owner}"
        ))
    }
}

/// Decode only matched fields; retain source bytes separately for raw-record preservation.
pub(crate) fn decode(
    scanned: ScannedRecord,
    schema: &Schema,
    context: &Context<'_>,
    diagnostics: &mut PluginDiagnostics,
) -> Result<DecodedRecord, String> {
    decode_with_prepared(scanned, schema, prepared(), context, diagnostics)
}

/// Use the same checked prepared entries for initial and catalogue-dependent decoding.
pub(crate) fn decode_with_prepared(
    scanned: ScannedRecord,
    schema: &Schema,
    prepared: &PreparedSchemas,
    context: &Context<'_>,
    diagnostics: &mut PluginDiagnostics,
) -> Result<DecodedRecord, String> {
    decode_with_identity(scanned, schema, prepared, context, diagnostics, &mut |_| {})
}

/// Observe only a successfully decoded GMST EDID, even if a later field rejects the record.
pub(crate) fn decode_with_identity(
    scanned: ScannedRecord,
    schema: &Schema,
    prepared: &PreparedSchemas,
    context: &Context<'_>,
    diagnostics: &mut PluginDiagnostics,
    identity: &mut impl FnMut(&str),
) -> Result<DecodedRecord, String> {
    // Original ownership stays strict; required headers retain supplier-self runtime compatibility.
    let source_identity = super::source_identity::resolve(
        &scanned.record_type,
        scanned.source_form_id,
        context.plugin_index,
        context.order,
    );
    let id = remap(scanned.source_form_id, context, false, diagnostics)?;
    if id == 0 && !matches!(&scanned.record_type, b"GMST" | b"TES4") {
        return Err("non-GMST record has null FormID".into());
    }
    if source_identity.state == super::source_identity::SourceIdentityState::MasterIndexOutOfRange {
        // Count header projection, not final admission; later body/collision checks remain independent.
        diagnostics.note("compatibility", || {
            format!(
                "{} {:08X}: original owner unresolved; supplier-self runtime projection {id:08X}",
                String::from_utf8_lossy(&scanned.record_type),
                scanned.source_form_id
            )
        });
    }
    let cell = scanned
        .cell_form_id
        .map(|id| remap(id, context, true, diagnostics))
        .transpose()?
        .filter(|&id| id != 0);
    let world = scanned
        .worldspace_form_id
        .map(|id| remap(id, context, true, diagnostics))
        .transpose()?
        .filter(|&id| id != 0);
    let topic = scanned
        .source_topic_form_id
        .map(|id| remap(id, context, true, diagnostics))
        .transpose()?
        .filter(|&id| id != 0);
    let record_schema = prepared.get(&scanned.record_type);
    let mut cursor = 0;
    let mut last = None;
    let mut fields = Vec::new();
    let mut rejected_fields = Vec::new();
    let mut editor_id = String::new();
    // CIS leaves belong to the preceding CTDA, not the preceding surviving one.
    let mut rejected_condition_strings = false;
    // Response members must not move beneath an earlier surviving TRDT.
    let mut rejected_info_response: Option<(usize, usize)> = None;
    // Weights belong to their physical layer anchor, including an anchor that was rejected.
    let mut rejected_land_layer: Option<(usize, usize)> = None;
    let framed = subrecords(&scanned.payload, diagnostics, scanned.source_form_id);
    let link_kind = |source_id| {
        let id = remap(source_id, context, true, &mut PluginDiagnostics::default()).ok()?;
        context.winning_types?.get(&id).copied()
    };
    for (position, &(signature, bytes)) in framed.fields.iter().enumerate() {
        let land_anchor =
            scanned.record_type == *b"LAND" && matches!(&signature, b"BTXT" | b"ATXT");
        if land_anchor {
            rejected_land_layer = None;
        } else if signature == *b"VTXT" && rejected_land_layer.is_some() {
            if !rejected_fields.contains(&signature) {
                rejected_fields.push(signature);
            }
            diagnostics.note("field", || {
                format!("LAND {id:08X} VTXT: weights belong to a rejected physical layer anchor")
            });
            continue;
        }
        let condition_string = matches!(&signature, b"CIS1" | b"CIS2");
        if condition_string && rejected_condition_strings {
            if !rejected_fields.contains(&signature) {
                rejected_fields.push(signature);
            }
            diagnostics.note("field", || {
                format!(
                    "{} {id:08X} {}: string belongs to rejected CTDA",
                    String::from_utf8_lossy(&scanned.record_type),
                    String::from_utf8_lossy(&signature)
                )
            });
            continue;
        }
        if !condition_string {
            // A new source field ends this rejected condition's contiguous string block.
            rejected_condition_strings = false;
        }
        let selection = super::deciders::Context {
            record_type: scanned.record_type,
            form_version: scanned.form_version,
            editor_id: &editor_id,
            preceding_subrecords: &framed.fields[..position],
            parent_payload: None,
            link_record_type: context
                .winning_types
                .map(|_| &link_kind as &dyn Fn(u32) -> Option<[u8; 4]>),
        };
        let Some(record_schema) = record_schema else {
            if let Some(common) = schema.common_fields.iter().find(|field| {
                field
                    .signature
                    .as_deref()
                    .is_some_and(|tag| tag.as_bytes() == signature)
            }) {
                let common = schema.resolve(common)?;
                match value(&common, bytes, schema, context, diagnostics, &selection, 0) {
                    Ok((value, canonical_bytes)) => fields.push(DecodedField {
                        signature,
                        name: common.name,
                        value,
                        canonical_bytes,
                    }),
                    Err(error) => {
                        if !rejected_fields.contains(&signature) {
                            rejected_fields.push(signature);
                        }
                        diagnostics.note("field", || {
                            format!(
                                "{} {id:08X} {}: {error}",
                                String::from_utf8_lossy(&scanned.record_type),
                                String::from_utf8_lossy(&signature)
                            )
                        });
                    }
                }
                continue;
            }
            fields.push(DecodedField {
                signature,
                name: String::from_utf8_lossy(&signature).into_owned(),
                value: Value::Bytes(bytes.to_vec()),
                canonical_bytes: bytes.to_vec(),
            });
            continue;
        };
        let Some(index) = match_field(record_schema, &signature, cursor, last) else {
            if land_anchor {
                rejected_land_layer = record_schema
                    .entries
                    .iter()
                    .position(|entry| {
                        entry
                            .field
                            .signature
                            .as_deref()
                            .is_some_and(|tag| tag.as_bytes() == signature)
                    })
                    .and_then(|index| layer_repeat_range(record_schema, index));
            }
            if signature == *b"CTDA" {
                rejected_condition_strings = true;
            }
            if record_schema.entries.iter().any(|entry| {
                entry
                    .field
                    .signature
                    .as_deref()
                    .is_some_and(|tag| tag.as_bytes() == signature)
            }) && !rejected_fields.contains(&signature)
            {
                rejected_fields.push(signature);
            }
            diagnostics.note("unexpected", || {
                format!(
                    "{} {id:08X}: unexpected/out-of-order {}",
                    String::from_utf8_lossy(&scanned.record_type),
                    String::from_utf8_lossy(&signature)
                )
            });
            continue;
        };
        let field_schema = &record_schema.entries[index].field;
        cursor = index + usize::from(!field_schema.repeat);
        last = Some(index);
        if let Some((start, end)) = rejected_land_layer
            && (index < start || index >= end)
        {
            // Only a real matched group boundary or a new physical anchor ends ownership.
            rejected_land_layer = None;
        }
        if let Some((start, end)) = rejected_info_response {
            if index > start && index < end {
                if !rejected_fields.contains(&signature) {
                    rejected_fields.push(signature);
                }
                diagnostics.note("field", || {
                    format!(
                        "{} {id:08X} {}: member belongs to rejected TRDT",
                        String::from_utf8_lossy(&scanned.record_type),
                        String::from_utf8_lossy(&signature)
                    )
                });
                continue;
            }
            // A new response anchor or matched field outside this group ends it.
            rejected_info_response = None;
        }
        match value(
            field_schema,
            bytes,
            schema,
            context,
            diagnostics,
            &selection,
            0,
        ) {
            Ok((value, canonical_bytes)) => {
                if signature == *b"EDID"
                    && let Value::String(name) = &value
                {
                    editor_id.clone_from(name);
                    if scanned.record_type == *b"GMST" {
                        identity(name);
                    }
                }
                fields.push(DecodedField {
                    signature,
                    name: field_schema.name.clone(),
                    value,
                    canonical_bytes,
                });
            }
            Err(error) => {
                if land_anchor {
                    rejected_land_layer = layer_repeat_range(record_schema, index);
                }
                if signature == *b"CTDA" {
                    rejected_condition_strings = true;
                }
                if scanned.record_type == *b"INFO" && signature == *b"TRDT" {
                    rejected_info_response = record_schema.entries[index]
                        .repeat_ranges
                        .iter()
                        .find(|range| range.start == index)
                        .map(|range| (range.start, range.end));
                }
                if field_schema.reject_record_on_error && scanned.flags & 0x20 == 0 {
                    return Err(format!(
                        "{} {id:08X} mandatory {}: {error}",
                        String::from_utf8_lossy(&scanned.record_type),
                        field_schema.name
                    ));
                }
                if !rejected_fields.contains(&signature) {
                    rejected_fields.push(signature);
                }
                diagnostics.note("field", || {
                    format!(
                        "{} {id:08X} {}: {error}",
                        String::from_utf8_lossy(&scanned.record_type),
                        String::from_utf8_lossy(&signature)
                    )
                });
            }
        }
    }
    Ok(DecodedRecord {
        form_id: id,
        source_form_id: scanned.source_form_id,
        record_type: scanned.record_type,
        flags: scanned.flags,
        version_control: scanned.version_control,
        form_version: scanned.form_version,
        header_unknown: scanned.header_unknown,
        load_order: context.plugin_index as u32,
        cell_form_id: cell,
        worldspace_form_id: world,
        topic_form_id: topic,
        source_topic_form_id: scanned.source_topic_form_id,
        topic_group_offset: scanned.topic_group_offset,
        source_record_offset: scanned.source_record_offset,
        fields,
        rejected_fields,
        payload_complete: framed.complete,
        raw_payload: scanned.payload,
        supported: record_schema.is_some(),
    })
}

/// Obtain a primitive's binary width; variable-sized kinds supply explicit lengths.
fn width(field: &FieldSchema) -> Option<usize> {
    match field.kind.as_str() {
        "u8" | "i8" => Some(1),
        "u16" | "i16" => Some(2),
        "u32" | "i32" | "f32" | "form_id" => Some(4),
        "u64" | "i64" => Some(8),
        _ => field.size,
    }
}

/// Choose a schema alternative using a small named Rust policy, never external tool code.
fn decide<'a>(
    field: &'a FieldSchema,
    bytes: &[u8],
    selection: &super::deciders::Context<'_>,
) -> Result<Option<&'a FieldSchema>, String> {
    let decider = field.decider.as_deref().ok_or("union has no decider")?;
    let target = match decider {
        "gmst_value" => match selection.editor_id.as_bytes().first() {
            Some(b'f' | b'F') => Some("f32"),
            Some(b'i' | b'I') => Some("i32"),
            Some(b'b' | b'B') => Some("u32"),
            Some(b's' | b'S') => Some("lstring"),
            _ => return Err("GMST EditorID does not select a value kind".into()),
        },
        "localized_string" => Some("lstring"),
        "legacy_linked_reference" | "cell_grid" | "movement_speeds" | "water_visual" | "size" => {
            None
        }
        _ => {
            return match super::deciders::select(decider, field, bytes, selection)? {
                super::deciders::Selection::Alternative(index) => field
                    .fields
                    .get(index)
                    .map(Some)
                    .ok_or_else(|| format!("{decider} selected absent alternative {index}")),
                super::deciders::Selection::Deferred => Ok(None),
            };
        }
    };
    field
        .fields
        .iter()
        .find(|alternative| {
            target.map_or_else(
                || {
                    alternative.size == Some(bytes.len())
                        || alternative.sizes.contains(&bytes.len())
                },
                |kind| alternative.kind == kind,
            )
        })
        .map(Some)
        .ok_or_else(|| format!("no {decider} alternative accepts {} bytes", bytes.len()))
}

/// Interpret one value within checked bytes, rewriting only typed link positions.
fn value(
    field: &FieldSchema,
    bytes: &[u8],
    schema: &Schema,
    context: &Context<'_>,
    diagnostics: &mut PluginDiagnostics,
    selection: &super::deciders::Context<'_>,
    depth: usize,
) -> Result<(Value, Vec<u8>), String> {
    if depth >= 32 {
        return Err("value nesting exceeds 32".into());
    }
    if field.definition.is_some() {
        return value(
            &schema.resolve(field)?,
            bytes,
            schema,
            context,
            diagnostics,
            selection,
            depth + 1,
        );
    }
    if field.size.is_some_and(|size| size != bytes.len()) && field.kind != "array"
        || !field.sizes.is_empty() && !field.sizes.contains(&bytes.len())
    {
        return Err(format!(
            "length {} does not match schema size {:?}/{:?}",
            bytes.len(),
            field.size,
            field.sizes
        ));
    }
    if let Some(layout) = super::deciders::layout(field, bytes)? {
        return value(
            &layout,
            bytes,
            schema,
            context,
            diagnostics,
            selection,
            depth + 1,
        );
    }
    if field.kind == "union" {
        let Some(alternative) = decide(field, bytes, selection)? else {
            return Ok((Value::Deferred(bytes.to_vec()), bytes.to_vec()));
        };
        return value(
            alternative,
            bytes,
            schema,
            context,
            diagnostics,
            selection,
            depth + 1,
        );
    }
    if field.decider.as_deref() == Some("alternate_textures") {
        return alternate_textures(bytes, context, diagnostics);
    }
    if field.decider.as_deref() == Some("vmad") {
        return super::vmad::decode(bytes, &selection.record_type, |id| {
            remap(id, context, true, diagnostics)
        });
    }
    if let Some(width) = width(field)
        && matches!(
            field.kind.as_str(),
            "u8" | "i8" | "u16" | "i16" | "u32" | "i32" | "u64" | "i64" | "f32" | "form_id"
        )
        && bytes.len() != width
    {
        return Err(format!("expected {width} bytes, got {}", bytes.len()));
    }
    let unsigned = || {
        let mut padded = [0u8; 8];
        padded[..bytes.len()].copy_from_slice(bytes);
        u64::from_le_bytes(padded)
    };
    let decoded = match field.kind.as_str() {
        "u8" | "u16" | "u32" | "u64" => Value::Unsigned(unsigned()),
        "i8" => Value::Signed(i64::from(bytes[0] as i8)),
        "i16" => Value::Signed(i64::from(i16::from_le_bytes(
            bytes.try_into().expect("checked i16"),
        ))),
        "i32" => Value::Signed(i64::from(i32::from_le_bytes(
            bytes.try_into().expect("checked i32"),
        ))),
        "i64" => Value::Signed(i64::from_le_bytes(bytes.try_into().expect("checked i64"))),
        "f32" => {
            let number = f32::from_le_bytes(bytes.try_into().expect("checked f32"));
            if !number.is_finite() {
                return Err("non-finite float".into());
            }
            Value::Float(number)
        }
        "form_id" => {
            let resolved = remap(
                u32::from_le_bytes(bytes.try_into().expect("checked FormID")),
                context,
                true,
                diagnostics,
            )?;
            return Ok((Value::FormId(resolved), resolved.to_le_bytes().to_vec()));
        }
        "zstring" => {
            let end = bytes
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(bytes.len());
            Value::String(decode_string(&bytes[..end]))
        }
        "lstring" => {
            if context.localized {
                if bytes.len() != 4 {
                    return Err("localized string ID must be four bytes".into());
                }
                Value::LocalizedString(u32::from_le_bytes(
                    bytes.try_into().expect("checked localized ID"),
                ))
            } else {
                let end = bytes
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(bytes.len());
                Value::String(decode_string(&bytes[..end]))
            }
        }
        "bytes" => Value::Bytes(bytes.to_vec()),
        "struct" => {
            let mut values = Vec::new();
            let mut canonical = bytes.to_vec();
            for member in &field.members {
                let member = schema.resolve(member)?;
                if member.optional && member.offset >= bytes.len() {
                    continue;
                }
                let size = width(&member)
                    .or_else(|| bytes.len().checked_sub(member.offset))
                    .ok_or("member offset out of bounds")?;
                let end = member
                    .offset
                    .checked_add(size)
                    .ok_or("member extent overflow")?;
                let slice = bytes
                    .get(member.offset..end)
                    .ok_or_else(|| format!("{} member out of bounds", member.name))?;
                let (decoded, encoded) = value(
                    &member,
                    slice,
                    schema,
                    context,
                    diagnostics,
                    &super::deciders::Context {
                        parent_payload: Some(bytes),
                        ..*selection
                    },
                    depth + 1,
                )?;
                canonical[member.offset..end].copy_from_slice(&encoded);
                values.push((member.name, decoded));
            }
            return Ok((Value::Struct(values), canonical));
        }
        "array" => {
            return array(
                field,
                bytes,
                schema,
                context,
                diagnostics,
                selection,
                depth + 1,
            );
        }
        kind => return Err(format!("field kind {kind} is not a value")),
    };
    Ok((decoded, bytes.to_vec()))
}

/// Ordinary Skyrim plugin text uses Windows-1252; VMAD owns its separate UTF-8 decoder.
pub(crate) fn decode_string(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '€', '\u{0081}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{008d}', 'Ž',
        '\u{008f}', '\u{0090}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
        '\u{009d}', 'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&byte| {
            if (0x80..0xa0).contains(&byte) {
                HIGH[usize::from(byte - 0x80)]
            } else {
                char::from(byte)
            }
        })
        .collect()
}

/// Decode fixed, remaining or prefix-counted arrays with checked arithmetic.
fn array(
    field: &FieldSchema,
    bytes: &[u8],
    schema: &Schema,
    context: &Context<'_>,
    diagnostics: &mut PluginDiagnostics,
    selection: &super::deciders::Context<'_>,
    depth: usize,
) -> Result<(Value, Vec<u8>), String> {
    if field.decider.as_deref() == Some("alternate_textures") {
        return alternate_textures(bytes, context, diagnostics);
    }
    let count_rule = field.count.as_ref();
    let kind = count_rule
        .and_then(|rule| rule.get("kind"))
        .and_then(|value| value.as_str())
        .unwrap_or("remaining");
    let stride = count_rule
        .and_then(|rule| rule.get("stride"))
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .or(field.size)
        .or_else(|| field.members.first().and_then(width))
        .ok_or("array has no stride")?;
    if stride == 0 {
        return Err("zero array stride".into());
    }
    let (offset, count) = match kind {
        "remaining" => (0, bytes.len() / stride),
        "fixed" => (
            0,
            count_rule
                .and_then(|rule| rule.get("value"))
                .and_then(|value| value.as_u64())
                .ok_or("fixed array lacks count")? as usize,
        ),
        "prefix_u32" => {
            let prefix = bytes.get(..4).ok_or("array lacks count prefix")?;
            (
                4,
                u32::from_le_bytes(prefix.try_into().expect("checked count")) as usize,
            )
        }
        _ => return Err(format!("unsupported array count rule {kind}")),
    };
    if count > 1_000_000 {
        return Err("array exceeds one million elements".into());
    }
    if count
        .checked_mul(stride)
        .and_then(|size| size.checked_add(offset))
        != Some(bytes.len())
    {
        return Err("array extent/count mismatch".into());
    }
    let element = if field.members.len() == 1 && field.members[0].offset == 0 {
        schema.resolve(&field.members[0])?
    } else {
        FieldSchema {
            kind: "struct".into(),
            size: Some(stride),
            members: field.members.clone(),
            ..Default::default()
        }
    };
    if field.decider.is_none()
        && let Some(compact) = super::compact_array::try_decode(
            &element,
            &bytes[offset..],
            count,
            stride,
            schema,
            depth,
        )?
    {
        return Ok((Value::CompactArray(compact), bytes.to_vec()));
    }
    let mut canonical = bytes.to_vec();
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let start = offset + index * stride;
        let (decoded, encoded) = value(
            &element,
            &bytes[start..start + stride],
            schema,
            context,
            diagnostics,
            selection,
            depth,
        )?;
        canonical[start..start + stride].copy_from_slice(&encoded);
        values.push(decoded);
    }
    Ok((Value::Array(values), canonical))
}

/// Alternate textures have counted length-prefixed names, then a TXST link and index.
fn alternate_textures(
    bytes: &[u8],
    context: &Context<'_>,
    diagnostics: &mut PluginDiagnostics,
) -> Result<(Value, Vec<u8>), String> {
    let read_u32 = |position: usize| -> Result<u32, String> {
        bytes
            .get(
                position
                    ..position
                        .checked_add(4)
                        .ok_or("alternate texture offset overflow")?,
            )
            .map(|slice| {
                u32::from_le_bytes(slice.try_into().expect("checked alternate texture integer"))
            })
            .ok_or_else(|| "truncated alternate textures".into())
    };
    let count = read_u32(0)? as usize;
    if count > bytes.len() / 12 {
        return Err("alternate texture count exceeds payload".into());
    }
    let mut position = 4;
    let mut canonical = bytes.to_vec();
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let size = read_u32(position)? as usize;
        position += 4;
        let end = position
            .checked_add(size)
            .ok_or("alternate texture name overflow")?;
        let name = decode_string(
            bytes
                .get(position..end)
                .ok_or("truncated alternate texture name")?,
        );
        position = end;
        let link = remap(read_u32(position)?, context, true, diagnostics)?;
        canonical[position..position + 4].copy_from_slice(&link.to_le_bytes());
        let index = read_u32(position + 4)?;
        position += 8;
        values.push(Value::Struct(vec![
            ("name".into(), Value::String(name)),
            ("texture_set".into(), Value::FormId(link)),
            ("index".into(), Value::Unsigned(u64::from(index))),
        ]));
    }
    if position != bytes.len() {
        return Err("trailing alternate texture bytes".into());
    }
    Ok((Value::Array(values), canonical))
}

/// Add only diagnostics newly discovered by a contextual replay, keeping existing examples.
fn merge_new_diagnostics(
    target: &mut PluginDiagnostics,
    baseline: &PluginDiagnostics,
    resolved: PluginDiagnostics,
) {
    let additions = [
        (
            "record",
            resolved
                .skipped_records
                .saturating_sub(baseline.skipped_records),
        ),
        (
            "field",
            resolved
                .skipped_fields
                .saturating_sub(baseline.skipped_fields),
        ),
        (
            "unexpected",
            resolved
                .unexpected_subrecords
                .saturating_sub(baseline.unexpected_subrecords),
        ),
        (
            "link",
            resolved
                .invalid_links
                .saturating_sub(baseline.invalid_links),
        ),
        (
            "compatibility",
            resolved
                .compatibility_headers
                .saturating_sub(baseline.compatibility_headers),
        ),
    ];
    for (category, count) in additions {
        if count == 0 {
            continue;
        }
        match category {
            "record" => target.skipped_records += count,
            "field" => target.skipped_fields += count,
            "unexpected" => target.unexpected_subrecords += count,
            "compatibility" => target.compatibility_headers += count,
            _ => target.invalid_links += count,
        }
        if !target.first_by_category.contains_key(category)
            && let Some(example) = resolved.first_by_category.get(category)
        {
            if target.first_example.is_none() {
                target.first_example = Some(example.clone());
            }
            target
                .first_by_category
                .insert(category.to_owned(), example.clone());
        }
    }
}

#[cfg(test)]
#[path = "master_index_compatibility_diagnostic_tests.rs"]
mod master_index_compatibility_diagnostic_tests;

/// Catalogue replay must never expose unresolved members in a canonical field.
pub(crate) fn value_contains_deferred(value: &Value) -> bool {
    match value {
        Value::Deferred(_) => true,
        Value::Struct(members) => members
            .iter()
            .any(|(_, value)| value_contains_deferred(value)),
        Value::Array(values) => values.iter().any(value_contains_deferred),
        _ => false,
    }
}

/// Retain recovery state only for winners with an unresolved canonical member.
pub(crate) fn record_contains_deferred(record: &DecodedRecord) -> bool {
    record
        .fields
        .iter()
        .any(|field| value_contains_deferred(&field.value))
}

/// Only tentative winners retain their original fields while resolved fields are published.
struct DeferredSource {
    fields: Vec<DecodedField>,
    rejected_fields: Vec<[u8; 4]>,
}

impl DeferredSource {
    /// Move first-pass fields without duplicating retained winner payloads.
    fn take(record: &mut DecodedRecord) -> Self {
        Self {
            fields: std::mem::take(&mut record.fields),
            rejected_fields: std::mem::take(&mut record.rejected_fields),
        }
    }
}

/// Change the exact physical occurrence while retaining its native source metadata.
fn set_deferred_history(
    result: &mut ReadResult,
    record: &DecodedRecord,
    accepted: bool,
    reason: String,
) {
    if let Some(entry) = result.history.iter_mut().find(|entry| {
        entry.plugin_index == record.load_order
            && entry.source_record_offset == record.source_record_offset
    }) {
        entry.accepted = accepted;
        entry.reason = Some(reason);
    }
}

/// Diagnose local candidate failure and revoke that occurrence's earlier admission.
fn reject_deferred_candidate(
    result: &mut ReadResult,
    record: &DecodedRecord,
    order: &LoadOrder,
    reason: String,
) {
    set_deferred_history(result, record, false, reason.clone());
    result
        .diagnostics
        .entry(order.names[record.load_order as usize].clone())
        .or_default()
        .note("record", || {
            format!(
                "{} {:08X}: {reason}",
                String::from_utf8_lossy(&record.record_type),
                record.form_id
            )
        });
}

/// Finalize tentative winners against a stable catalogue, preserving only selective fallback chains.
pub(crate) fn resolve_deferred(
    result: &mut ReadResult,
    schema: &Schema,
    prepared: &PreparedSchemas,
    order: &LoadOrder,
    predecessors: &mut HashMap<u32, Vec<DecodedRecord>>,
    validate: &mut impl FnMut(&DecodedRecord) -> Result<(), String>,
) {
    let mut pending = BTreeMap::new();
    for (&id, record) in &mut result.records {
        if record_contains_deferred(record) {
            pending.insert(id, DeferredSource::take(record));
        }
    }
    let affected: Vec<_> = pending.keys().copied().collect();
    let candidate_count = pending.len()
        + affected
            .iter()
            .filter_map(|id| predecessors.get(id))
            .map(Vec::len)
            .sum::<usize>();
    let mut replay_diagnostics = HashMap::new();
    let mut changed_chains: HashMap<u32, Vec<super::RecordSource>> = HashMap::new();
    // Every failure consumes a candidate or finishes its safe prefix. One extra
    // round permits all remaining candidates to replay after the last fallback.
    for _ in 0..=candidate_count {
        if pending.is_empty() {
            break;
        }
        let kinds: HashMap<_, _> = result
            .records
            .iter()
            .map(|(&id, record)| (id, record.record_type))
            .collect();
        let ids: Vec<_> = pending.keys().copied().collect();
        let mut recovered = false;
        for id in ids {
            let Some(mut record) = result.records.remove(&id) else {
                pending.remove(&id);
                continue;
            };
            let priority = record.load_order as usize;
            let context = Context {
                order,
                plugin_index: priority,
                localized: order.metadata[priority].flags & 0x80 != 0,
                winning_types: Some(&kinds),
            };
            let scanned = || ScannedRecord {
                record_type: record.record_type,
                flags: record.flags,
                source_form_id: record.source_form_id,
                version_control: record.version_control,
                form_version: record.form_version,
                header_unknown: record.header_unknown,
                cell_form_id: None,
                worldspace_form_id: None,
                source_topic_form_id: record.source_topic_form_id,
                topic_group_offset: record.topic_group_offset,
                source_record_offset: record.source_record_offset,
                payload: record.raw_payload.clone(),
            };
            let baseline_context = Context {
                winning_types: None,
                ..context
            };
            let mut baseline = PluginDiagnostics::default();
            let mut replay = PluginDiagnostics::default();
            let failure = match decode_with_prepared(
                scanned(),
                schema,
                prepared,
                &baseline_context,
                &mut baseline,
            ) {
                Err(error) => Some(format!("deferred baseline {id:08X}: {error}")),
                Ok(_) => {
                    match decode_with_prepared(scanned(), schema, prepared, &context, &mut replay) {
                        Err(error) => Some(format!("deferred winner {id:08X}: {error}")),
                        Ok(resolved) if record_contains_deferred(&resolved) => Some(format!(
                            "deferred winner {id:08X}: retained unresolved values"
                        )),
                        Ok(resolved) => {
                            record.fields = resolved.fields;
                            record.rejected_fields = resolved.rejected_fields;
                            validate(&record).err().map(|error| {
                                format!("deferred resolved candidate {id:08X} unusable: {error}")
                            })
                        }
                    }
                }
            };
            let Some(reason) = failure else {
                replay_diagnostics.insert(id, (baseline, replay));
                result.records.insert(id, record);
                continue;
            };
            merge_new_diagnostics(
                result
                    .diagnostics
                    .entry(order.names[priority].clone())
                    .or_default(),
                &baseline,
                replay,
            );
            replay_diagnostics.remove(&id);
            changed_chains.entry(id).or_default();
            let source = pending.remove(&id).expect("selected deferred source");
            if let Some(mut previous) = predecessors.get_mut(&id).and_then(|chain| chain.pop()) {
                reject_deferred_candidate(result, &record, order, reason);
                loop {
                    let failure = if !previous.payload_complete {
                        Some("deferred recovery predecessor has incomplete payload".to_owned())
                    } else if record_contains_deferred(&previous) {
                        None
                    } else {
                        validate(&previous)
                            .err()
                            .map(|error| format!("deferred recovery predecessor unusable: {error}"))
                    };
                    if let Some(reason) = failure {
                        reject_deferred_candidate(result, &previous, order, reason);
                        if let Some(earlier) =
                            predecessors.get_mut(&id).and_then(|chain| chain.pop())
                        {
                            previous = earlier;
                            continue;
                        }
                    } else {
                        if record_contains_deferred(&previous) {
                            pending.insert(id, DeferredSource::take(&mut previous));
                        }
                        result.records.insert(id, previous);
                    }
                    break;
                }
            } else {
                record.fields = source.fields;
                record.rejected_fields = source.rejected_fields;
                let mut omitted = Vec::new();
                record.fields.retain(|field| {
                    if value_contains_deferred(&field.value) {
                        omitted.push(field.signature);
                        false
                    } else {
                        true
                    }
                });
                for signature in &omitted {
                    if !record.rejected_fields.contains(signature) {
                        record.rejected_fields.push(*signature);
                    }
                    result
                        .diagnostics
                        .entry(order.names[priority].clone())
                        .or_default()
                        .note("field", || {
                            format!(
                                "{} {id:08X} {}: omitted after {reason}",
                                String::from_utf8_lossy(&record.record_type),
                                String::from_utf8_lossy(signature)
                            )
                        });
                }
                let fields = omitted
                    .iter()
                    .map(|signature| String::from_utf8_lossy(signature))
                    .collect::<Vec<_>>()
                    .join(", ");
                let reason = format!("deferred recovery omitted {fields}: {reason}");
                if let Err(error) = validate(&record) {
                    reject_deferred_candidate(
                        result,
                        &record,
                        order,
                        format!("{reason}; recovered prefix unusable: {error}"),
                    );
                } else {
                    set_deferred_history(result, &record, true, reason);
                    result.records.insert(id, record);
                }
            }
            recovered = true;
            // A changed kind can affect a previously successful dependent. Restart
            // before rejecting any other candidate against the superseded catalogue.
            break;
        }
        if !recovered {
            pending.clear();
            break;
        }
    }
    debug_assert!(
        pending.is_empty(),
        "finite deferred candidate population exhausted"
    );
    for (id, (baseline, replay)) in replay_diagnostics {
        if let Some(record) = result.records.get(&id) {
            merge_new_diagnostics(
                result
                    .diagnostics
                    .entry(order.names[record.load_order as usize].clone())
                    .or_default(),
                &baseline,
                replay,
            );
        }
    }
    // Metadata follows final admission; full physical history keeps every rejected version.
    for entry in &result.history {
        if entry.accepted
            && let Some(id) = entry.form_id
            && let Some(chain) = changed_chains.get_mut(&id)
        {
            chain.push(super::RecordSource {
                plugin_index: entry.plugin_index,
                source_form_id: entry.source_form_id,
                deleted: entry.flags & 0x20 != 0,
            });
        }
    }
    for (id, chain) in changed_chains {
        if chain.len() > 1 || chain.iter().any(|source| source.deleted) {
            result.overrides.insert(id, chain);
        } else {
            result.overrides.remove(&id);
        }
    }
    debug_assert!(
        result
            .records
            .values()
            .all(|record| !record_contains_deferred(record))
    );
}

/// Resolve named definitions with occurrence overrides; plain fields are already complete.
fn validation_schema<'a>(
    schema: &Schema,
    field: &'a FieldSchema,
) -> Result<Cow<'a, FieldSchema>, String> {
    if field.definition.is_none() {
        Ok(Cow::Borrowed(field))
    } else {
        schema.resolve(field).map(Cow::Owned)
    }
}

/// Only nonzero typed links can require generic target validation and selector snapshots.
fn contains_target_link(value: &Value) -> bool {
    match value {
        Value::FormId(id) => *id != 0,
        Value::Struct(members) => members.iter().any(|(_, value)| contains_target_link(value)),
        Value::Array(values) => values.iter().any(contains_target_link),
        _ => false,
    }
}

/// Keep direct validator probes on the embedded prepared schema.
#[cfg(test)]
pub(crate) fn validate_targets(result: &mut ReadResult, schema: &Schema, order: &LoadOrder) {
    validate_targets_with_prepared(result, schema, prepared(), order);
}

/// Validate declared target kinds after all winners exist; dangling optional links are nulled.
pub(crate) fn validate_targets_with_prepared(
    result: &mut ReadResult,
    schema: &Schema,
    prepared: &PreparedSchemas,
    order: &LoadOrder,
) {
    let kinds: HashMap<u32, [u8; 4]> = result
        .records
        .iter()
        .map(|(&id, record)| (id, record.record_type))
        .collect();
    for record in result.records.values_mut() {
        let record_schema = prepared.get(&record.record_type);
        let plugin = order.names.get(record.load_order as usize).cloned();
        let mut diagnostics = PluginDiagnostics::default();
        if let Some(topic) = record.topic_form_id {
            if kinds.get(&topic) != Some(b"DIAL") {
                diagnostics.note("link", || {
                    format!(
                        "INFO {:08X} GRUP7 owner {topic:08X} is absent or is not DIAL",
                        record.form_id
                    )
                });
                record.topic_form_id = None;
            }
        } else if record.source_topic_form_id == Some(0) {
            diagnostics.note("link", || {
                format!("INFO {:08X} GRUP7 has a null owner", record.form_id)
            });
        }
        // Keep original selector bytes when any typed link could need them.
        // VMAD and INFO ownership validation remain independent of this snapshot.
        let needs_context = record
            .fields
            .iter()
            .any(|field| contains_target_link(&field.value));
        let preceding: Vec<_> = if needs_context {
            record
                .fields
                .iter()
                .map(|field| (field.signature, field.canonical_bytes.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let preceding: Vec<_> = preceding
            .iter()
            .map(|(tag, bytes)| (*tag, bytes.as_slice()))
            .collect();
        let editor_id = if needs_context {
            record
                .fields
                .iter()
                .find_map(|field| {
                    if field.signature == *b"EDID" {
                        if let Value::String(name) = &field.value {
                            Some(name.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        let link_kind = |id| kinds.get(&id).copied();
        for (position, field) in record.fields.iter_mut().enumerate() {
            let field_schema = record_schema
                .and_then(|prepared| {
                    prepared
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.field.name == field.name
                                && entry
                                    .field
                                    .signature
                                    .as_deref()
                                    .is_some_and(|tag| tag.as_bytes() == field.signature)
                        })
                        .map(|entry| &entry.field)
                })
                .or_else(|| {
                    schema.common_fields.iter().find(|common| {
                        common
                            .signature
                            .as_deref()
                            .is_some_and(|signature| signature.as_bytes() == field.signature)
                    })
                });
            let Some(field_schema) = field_schema else {
                continue;
            };
            let resolved = validation_schema(schema, field_schema).expect("validated common field");
            if resolved.decider.as_deref() == Some("vmad") {
                // Canonical VMAD links are already global; check all primary and alias objects.
                let validation =
                    super::vmad::decode(&field.canonical_bytes, &record.record_type, |id| {
                        if id == 0 || id < 0x800 || kinds.contains_key(&id) {
                            return Ok(id);
                        }
                        diagnostics.note("link", || {
                            format!(
                                "record {:08X} VMAD object {id:08X} is absent",
                                record.form_id
                            )
                        });
                        Ok(0)
                    });
                if let Ok((value, canonical)) = validation {
                    field.value = value;
                    field.canonical_bytes = canonical;
                }
                continue;
            }
            validate_value(
                &mut field.value,
                &mut field.canonical_bytes,
                field_schema,
                schema,
                &kinds,
                &mut diagnostics,
                &super::deciders::Context {
                    record_type: record.record_type,
                    form_version: record.form_version,
                    editor_id: &editor_id,
                    preceding_subrecords: if needs_context {
                        &preceding[..position]
                    } else {
                        &[]
                    },
                    parent_payload: None,
                    link_record_type: Some(&link_kind),
                },
            );
        }
        if diagnostics.invalid_links != 0
            && let Some(plugin) = plugin
        {
            let target = result.diagnostics.entry(plugin).or_default();
            target.invalid_links += diagnostics.invalid_links;
            if target.first_example.is_none() {
                target.first_example = diagnostics.first_example;
            }
            for (category, example) in diagnostics.first_by_category {
                target.first_by_category.entry(category).or_insert(example);
            }
        }
    }
}

/// Walk the same schema offsets used by decoding when nulling a mistyped or dangling link.
fn validate_value(
    decoded: &mut Value,
    bytes: &mut [u8],
    field: &FieldSchema,
    schema: &Schema,
    kinds: &HashMap<u32, [u8; 4]>,
    diagnostics: &mut PluginDiagnostics,
    selection: &super::deciders::Context<'_>,
) {
    // Nonlink leaves never mutate bytes or emit target diagnostics. Keep this
    // inside the generic walker, after the independent outer VMAD dispatch.
    match decoded {
        Value::FormId(0)
        | Value::Unsigned(_)
        | Value::Signed(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::LocalizedString(_)
        | Value::Bytes(_)
        | Value::Deferred(_)
        | Value::CompactArray(_) => return,
        _ => {}
    }
    let Ok(field) = validation_schema(schema, field) else {
        return;
    };
    if let Ok(Some(layout)) = super::deciders::layout(&field, bytes) {
        validate_value(
            decoded,
            bytes,
            &layout,
            schema,
            kinds,
            diagnostics,
            selection,
        );
        return;
    }
    if field.kind == "union" {
        if let Ok(Some(alternative)) = decide(&field, bytes, selection) {
            validate_value(
                decoded,
                bytes,
                alternative,
                schema,
                kinds,
                diagnostics,
                selection,
            );
        }
        return;
    }
    match decoded {
        Value::FormId(id) if *id != 0 => {
            // Hardcoded IDs (including the player) need not have a plugin record.
            let allowed = kinds.get(id).map_or(*id < 0x800, |kind| {
                field
                    .targets
                    .iter()
                    .any(|target| target == "*" || target == "ANY_" || target.as_bytes() == kind)
            });
            if !allowed {
                diagnostics.note("link", || {
                    format!(
                        "{} link {:08X} is absent or has a disallowed target type",
                        field.name, *id
                    )
                });
                *id = 0;
                if bytes.len() == 4 {
                    bytes.copy_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        Value::Struct(values) => {
            let parent = bytes.to_vec();
            for (name, value) in values {
                if let Some(member) = field.members.iter().find(|member| member.name == *name) {
                    let size = width(member).unwrap_or(bytes.len().saturating_sub(member.offset));
                    if let Some(slice) =
                        bytes.get_mut(member.offset..member.offset.saturating_add(size))
                    {
                        validate_value(
                            value,
                            slice,
                            member,
                            schema,
                            kinds,
                            diagnostics,
                            &super::deciders::Context {
                                parent_payload: Some(&parent),
                                ..*selection
                            },
                        );
                    }
                }
            }
        }
        Value::Array(values) if field.decider.as_deref() == Some("alternate_textures") => {
            let mut position: usize = 4;
            let target_schema = FieldSchema {
                kind: "form_id".into(),
                name: "alternate_texture".into(),
                targets: vec!["TXST".into()],
                ..Default::default()
            };
            for item in values {
                let Some(length_bytes) = bytes.get(position..position.saturating_add(4)) else {
                    break;
                };
                let length =
                    u32::from_le_bytes(length_bytes.try_into().expect("checked name length"))
                        as usize;
                position = position.saturating_add(4).saturating_add(length);
                if let Value::Struct(members) = item
                    && let Some((_, link)) =
                        members.iter_mut().find(|(name, _)| name == "texture_set")
                    && let Some(link_bytes) = bytes.get_mut(position..position.saturating_add(4))
                {
                    validate_value(
                        link,
                        link_bytes,
                        &target_schema,
                        schema,
                        kinds,
                        diagnostics,
                        selection,
                    );
                }
                position = position.saturating_add(8);
            }
        }
        Value::Array(values) => {
            let stride = field
                .count
                .as_ref()
                .and_then(|rule| rule.get("stride"))
                .and_then(|value| value.as_u64())
                .map(|value| value as usize)
                .or(field.size)
                .or_else(|| field.members.first().and_then(width));
            if let Some(stride) = stride.filter(|&stride| stride != 0) {
                let offset = usize::from(
                    field
                        .count
                        .as_ref()
                        .and_then(|rule| rule.get("kind"))
                        .and_then(|value| value.as_str())
                        == Some("prefix_u32"),
                ) * 4;
                let element = if field.members.len() == 1 {
                    field.members[0].clone()
                } else {
                    FieldSchema {
                        kind: "struct".into(),
                        members: field.members.clone(),
                        ..Default::default()
                    }
                };
                for (index, value) in values.iter_mut().enumerate() {
                    let start = offset + index * stride;
                    if let Some(slice) = bytes.get_mut(start..start + stride) {
                        validate_value(
                            value,
                            slice,
                            &element,
                            schema,
                            kinds,
                            diagnostics,
                            selection,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "target_validation_tests.rs"]
mod target_validation_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Count prefixes and mixed structs retain every canonical byte; an optional policy
    /// marker forces the unchanged materialized path for an asymmetric decoder oracle.
    #[test]
    fn compact_arrays_match_materialized_counts_padding_and_field_local_errors() {
        let schema = Schema::parse(br#"{"version":1,"records":[]}"#).unwrap();
        let order = LoadOrder::read(&[]).unwrap();
        let context = Context {
            order: &order,
            plugin_index: 0,
            localized: false,
            winning_types: None,
        };
        let selection = super::super::deciders::Context {
            record_type: *b"TEST",
            form_version: 44,
            editor_id: "",
            preceding_subrecords: &[],
            parent_payload: None,
            link_record_type: None,
        };
        let mut diagnostics = PluginDiagnostics::default();
        let mut field = FieldSchema {
            kind: "array".into(),
            count: Some(serde_json::json!({"kind":"prefix_u32","stride":8})),
            members: vec![
                FieldSchema {
                    name: "weight".into(),
                    kind: "f32".into(),
                    offset: 4,
                    ..Default::default()
                },
                FieldSchema {
                    name: "index".into(),
                    kind: "u16".into(),
                    ..Default::default()
                },
                FieldSchema {
                    name: "padding".into(),
                    kind: "bytes".into(),
                    offset: 2,
                    size: Some(2),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut bytes = 2_u32.to_le_bytes().to_vec();
        bytes.extend([1, 128, 99, 255]);
        bytes.extend((-0.0_f32).to_le_bytes());
        bytes.extend([7, 0, 66, 77]);
        bytes.extend(f32::from_bits(1).to_le_bytes());
        let (compact, canonical) = array(
            &field,
            &bytes,
            &schema,
            &context,
            &mut diagnostics,
            &selection,
            1,
        )
        .unwrap();
        assert!(matches!(&compact, Value::CompactArray(_)));
        assert_eq!(canonical, bytes);
        field.decider = Some("size".into());
        let (old, old_canonical) = array(
            &field,
            &bytes,
            &schema,
            &context,
            &mut diagnostics,
            &selection,
            1,
        )
        .unwrap();
        assert!(matches!(&old, Value::Array(_)));
        assert_eq!(compact, old);
        assert_eq!(canonical, old_canonical);
        assert_eq!(
            serde_json::to_vec(&compact).unwrap(),
            serde_json::to_vec(&old).unwrap()
        );
        field.decider = None;
        // An invalid second element must reject the field, never publish a prefix array.
        let mut bad = bytes.clone();
        bad[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(
            array(
                &field,
                &bad,
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                1
            )
            .unwrap_err(),
            "non-finite float"
        );
        assert!(
            array(
                &field,
                &bytes,
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                1
            )
            .is_ok()
        );
        // Framing/extent failures still use the preexisting path and diagnostic boundary.
        assert_eq!(
            array(
                &field,
                &bytes[..19],
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                1
            )
            .unwrap_err(),
            "array extent/count mismatch"
        );
        assert_eq!(diagnostics.invalid_links, 0);
        assert!(!contains_target_link(&compact));
        let mut after = canonical.clone();
        let mut compact = compact;
        validate_value(
            &mut compact,
            &mut after,
            &field,
            &schema,
            &HashMap::new(),
            &mut diagnostics,
            &selection,
        );
        assert_eq!(after, canonical);
    }

    /// Preserve fixed, remaining-byte and empty-array counts across both storage paths.
    #[test]
    fn compact_fixed_remaining_and_empty_arrays_preserve_count_authority() {
        let schema = Schema::parse(br#"{"version":1,"records":[]}"#).unwrap();
        let order = LoadOrder::read(&[]).unwrap();
        let context = Context {
            order: &order,
            plugin_index: 0,
            localized: false,
            winning_types: None,
        };
        let selection = super::super::deciders::Context {
            record_type: *b"TEST",
            form_version: 44,
            editor_id: "",
            preceding_subrecords: &[],
            parent_payload: None,
            link_record_type: None,
        };
        let mut diagnostics = PluginDiagnostics::default();
        let mut field = FieldSchema {
            kind: "array".into(),
            size: Some(1),
            members: vec![FieldSchema {
                kind: "i8".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        for rule in [
            serde_json::json!({"kind":"remaining"}),
            serde_json::json!({"kind":"fixed","value":3}),
        ] {
            field.count = Some(rule);
            let (actual, bytes) = array(
                &field,
                &[128, 0, 127],
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                1,
            )
            .unwrap();
            assert_eq!(
                actual,
                Value::Array(vec![
                    Value::Signed(-128),
                    Value::Signed(0),
                    Value::Signed(127)
                ])
            );
            assert_eq!(bytes, [128, 0, 127]);
        }
        field.count = Some(serde_json::json!({"kind":"fixed","value":0}));
        let (empty, _) = array(
            &field,
            &[],
            &schema,
            &context,
            &mut diagnostics,
            &selection,
            1,
        )
        .unwrap();
        assert!(matches!(empty, Value::Array(values) if values.is_empty()));
        field.count = Some(serde_json::json!({"kind":"fixed","value":1_000_001}));
        assert_eq!(
            array(
                &field,
                &[],
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                1
            )
            .unwrap_err(),
            "array exceeds one million elements"
        );
        field.count = None;
        field.size = Some(0);
        assert_eq!(
            array(
                &field,
                &[],
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                1
            )
            .unwrap_err(),
            "zero array stride"
        );
    }

    /// Unknown and out-of-order fields cannot rewind a nonrepeating schema cursor.
    #[test]
    fn matching_respects_order_and_repeat_groups() {
        let fields = vec![
            FieldSchema {
                signature: Some("EDID".into()),
                kind: "zstring".into(),
                ..Default::default()
            },
            FieldSchema {
                kind: "group".into(),
                repeat: true,
                fields: vec![
                    FieldSchema {
                        signature: Some("BTXT".into()),
                        kind: "bytes".into(),
                        ..Default::default()
                    },
                    FieldSchema {
                        signature: Some("ATXT".into()),
                        kind: "bytes".into(),
                        repeat: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            FieldSchema {
                signature: Some("DATA".into()),
                kind: "bytes".into(),
                ..Default::default()
            },
        ];
        let schema = Schema {
            version: 1,
            sources: Default::default(),
            definitions: Default::default(),
            common_fields: Default::default(),
            records: vec![],
        };
        let mut entries = Vec::new();
        flatten(&fields, &schema, &mut entries);
        let record = PreparedRecord {
            entries,
            unordered: false,
        };
        assert_eq!(match_field(&record, b"BTXT", 2, Some(1)), Some(1));
        assert_eq!(match_field(&record, b"EDID", 2, Some(1)), None);
        assert_eq!(match_field(&record, b"ZZZZ", 1, Some(0)), None);
        assert_eq!(match_field(&record, b"DATA", 1, Some(0)), Some(3));
    }

    /// XXXX frames large fields, while a corrupt neighbor preserves the established prefix.
    #[test]
    fn extended_sizes_and_truncation_are_bounded() {
        let mut payload = b"XXXX\x04\x00".to_vec();
        payload.extend(70_000u32.to_le_bytes());
        payload.extend(b"TEST\x00\x00");
        payload.resize(payload.len() + 70_000, 19);
        payload.extend(b"BAD_\x20\x00x");
        let mut diagnostics = PluginDiagnostics::default();
        let fields = subrecords(&payload, &mut diagnostics, 123);
        assert_eq!(fields.fields.len(), 1);
        assert_eq!(fields.fields[0].1.len(), 70_000);
        assert!(!fields.complete);
        assert_eq!(diagnostics.skipped_fields, 1);
    }

    /// Matching a repeated male child must not jump to a later female role with the same tag.
    #[test]
    fn active_repeat_precedes_later_duplicate_signature() {
        let tag = |signature: &str| FieldSchema {
            signature: Some(signature.into()),
            kind: "bytes".into(),
            ..Default::default()
        };
        let fields = vec![
            FieldSchema {
                kind: "group".into(),
                repeat: true,
                fields: vec![tag("INDX"), tag("MODL")],
                ..Default::default()
            },
            tag("FNAM"),
            FieldSchema {
                kind: "group".into(),
                repeat: true,
                fields: vec![tag("INDX"), tag("MODL")],
                ..Default::default()
            },
        ];
        let schema = Schema::parse(br#"{"version":1,"records":[]}"#).unwrap();
        let mut entries = Vec::new();
        flatten(&fields, &schema, &mut entries);
        let prepared = PreparedRecord {
            entries,
            unordered: false,
        };
        assert_eq!(match_field(&prepared, b"INDX", 2, Some(1)), Some(0));
        assert_eq!(match_field(&prepared, b"FNAM", 2, Some(1)), Some(2));
        assert_eq!(match_field(&prepared, b"INDX", 3, Some(2)), Some(3));
    }

    /// Accepted optional tails omit whole members and reject a partial trailing float.
    #[test]
    fn optional_members_are_whole_checked_tails() {
        let schema = Schema::parse(br#"{"version":1,"records":[]}"#).unwrap();
        let field = FieldSchema {
            kind: "struct".into(),
            sizes: vec![4, 6, 8],
            members: vec![
                FieldSchema {
                    name: "flags".into(),
                    kind: "u32".into(),
                    ..Default::default()
                },
                FieldSchema {
                    name: "tail".into(),
                    kind: "f32".into(),
                    offset: 4,
                    optional: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let order = LoadOrder::read(&[]).unwrap();
        let context = Context {
            order: &order,
            plugin_index: 0,
            localized: false,
            winning_types: None,
        };
        let selection = super::super::deciders::Context {
            record_type: *b"TEST",
            form_version: 44,
            editor_id: "",
            preceding_subrecords: &[],
            parent_payload: None,
            link_record_type: None,
        };
        let mut diagnostics = PluginDiagnostics::default();
        let (decoded, _) = value(
            &field,
            &[3, 0, 0, 0],
            &schema,
            &context,
            &mut diagnostics,
            &selection,
            0,
        )
        .unwrap();
        assert_eq!(
            decoded,
            Value::Struct(vec![("flags".into(), Value::Unsigned(3))])
        );
        assert!(
            value(
                &field,
                &[3, 0, 0, 0, 0, 0],
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                0
            )
            .is_err()
        );
        let mut extended = 3u32.to_le_bytes().to_vec();
        extended.extend(1.25f32.to_le_bytes());
        assert!(
            value(
                &field,
                &extended,
                &schema,
                &context,
                &mut diagnostics,
                &selection,
                0
            )
            .is_ok()
        );
    }
}
