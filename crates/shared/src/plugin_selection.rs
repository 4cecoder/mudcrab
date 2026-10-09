//! Skyrim SE implicit plugin inputs shared with converter and mod-manager readers.
use color_eyre::{Result, eyre::WrapErr};
use std::path::Path;

/// Installed official masters are always active in this conventional order.
pub const OFFICIAL_MASTERS: [&str; 5] = [
    "Skyrim.esm",
    "Update.esm",
    "Dawnguard.esm",
    "HearthFires.esm",
    "Dragonborn.esm",
];

/// Decode Windows-1252 without discarding accented plugin or master names.
pub fn decode_windows_1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}',
        '\u{8f}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}',
        '\u{2014}', '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}',
        '\u{178}',
    ];
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9f => HIGH[(byte - 0x80) as usize],
            _ => char::from(byte),
        })
        .collect()
}

/// Recognize plugin filenames directly inside Data, excluding path components.
fn plugin_name(name: &str) -> bool {
    !name.contains(['/', '\\', ':'])
        && matches!(
            Path::new(name)
                .extension()
                .and_then(|ext| ext.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("esm" | "esp" | "esl")
        )
}

/// Ignore comments and blank lines without interpreting checkbox state.
fn lines(contents: &str) -> impl Iterator<Item = &str> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with(['#', ';']))
}

/// Read Creation filenames in file order for explicit lists and mod-manager reuse.
/// Callers should warn and continue without Creations if this optional file fails.
pub fn read_skyrim_ccc(path: &Path) -> Result<Vec<String>> {
    let bytes = std::fs::read(path).wrap_err_with(|| format!("reading {}", path.display()))?;
    Ok(lines(&decode_windows_1252(&bytes))
        .filter(|name| plugin_name(name))
        .map(str::to_owned)
        .collect())
}
