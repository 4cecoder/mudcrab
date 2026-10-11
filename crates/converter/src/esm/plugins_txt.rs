//! Explicit Skyrim SE selection. Optional game lists never consult ambient profiles.
use super::{binary::parse_plugin_metadata, load_order};
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use shared::plugin_selection::decode_windows_1252;
pub use shared::plugin_selection::{OFFICIAL_MASTERS, read_skyrim_ccc};

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

/// Count repeated diagnostics and emit one line for each distinct problem.
#[derive(Default)]
struct Warnings(BTreeMap<String, usize>);

impl Warnings {
    /// Accumulate repeated occurrences without producing unbounded log output.
    fn add(&mut self, message: String) {
        *self
            .0
            .entry(message.replace(['\r', '\n'], " "))
            .or_default() += 1;
    }

    /// Forward warnings through the pipeline notice channel or public reader.
    fn finish(self, notices: &mut Vec<String>) {
        for (message, count) in self.0 {
            notices.push(if count == 1 {
                message
            } else {
                format!("{message} ({count} occurrences)")
            });
        }
    }
}

/// Read only explicit General sTestFile1 through sTestFile10 settings.
fn ini_plugins(path: &Path, warnings: &mut Warnings) -> Vec<String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            warnings.add(format!(
                "could not read optional Skyrim.ini {}: {error}",
                path.display()
            ));
            return Vec::new();
        }
    };
    let text = decode_windows_1252(&bytes);
    let mut general = false;
    let mut settings = BTreeMap::new();
    for line in lines(&text) {
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            general = section.trim().eq_ignore_ascii_case("General");
            continue;
        }
        if !general {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let Some(index) = key
            .strip_prefix("stestfile")
            .and_then(|index| index.parse::<u8>().ok())
            .filter(|index| (1..=10).contains(index))
        else {
            continue;
        };
        let value = value.trim();
        settings.insert(index, value.to_owned());
    }
    settings
        .into_values()
        .filter(|name| plugin_name(name))
        .collect()
}

/// Keep usable selections and recursively omit dependents of missing/disabled masters.
fn usable_plugins(paths: Vec<PathBuf>, warnings: &mut Warnings) -> Vec<PathBuf> {
    let indices: HashMap<_, _> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            (
                load_order::normalize_plugin_name(
                    &path.file_name().unwrap_or_default().to_string_lossy(),
                ),
                i,
            )
        })
        .collect();
    let mut metadata = Vec::with_capacity(paths.len());
    let mut missing = vec![None; paths.len()];
    for (index, path) in paths.iter().enumerate() {
        match parse_plugin_metadata(path) {
            Ok(header) => metadata.push(Some(header)),
            Err(error) => {
                warnings.add(format!(
                    "skipping {}: unusable plugin header: {error:?}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                missing[index] = Some(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                );
                metadata.push(None);
            }
        }
    }
    loop {
        let mut changed = false;
        for (index, header) in metadata.iter().enumerate() {
            if missing[index].is_some() {
                continue;
            }
            let Some(header) = header else {
                continue;
            };
            for master in &header.masters {
                let cause = match indices.get(&load_order::normalize_plugin_name(master)) {
                    Some(&dependency) => missing[dependency].clone(),
                    None => Some(master.clone()),
                };
                if let Some(cause) = cause {
                    missing[index] = Some(cause);
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }
    paths
        .into_iter()
        .enumerate()
        .filter_map(|(index, path)| {
            if let Some(master) = &missing[index] {
                if metadata[index].is_some() {
                    warnings.add(format!(
                        "skipping {}: required master {master} is missing, disabled or unusable",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                None
            } else {
                Some(path)
            }
        })
        .collect()
}

/// Select plugins using explicit CCC/INI inputs, collecting bounded diagnostics.
pub(crate) fn read_with_notices(
    path: &Path,
    data_dir: &Path,
    ccc: Option<&Path>,
    ini: Option<&Path>,
    notices: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let bytes =
        std::fs::read(path).wrap_err_with(|| format!("reading plugins.txt {}", path.display()))?;
    let text = decode_windows_1252(&bytes);
    let mut warnings = Warnings::default();
    let mut installed = HashMap::new();
    for entry in std::fs::read_dir(data_dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.add(format!("could not read Data directory entry: {error}"));
                continue;
            }
        };
        if entry.path().is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if plugin_name(&name) {
                installed.insert(load_order::normalize_plugin_name(&name), entry.path());
            }
        }
    }
    let default_ccc = data_dir.parent().unwrap_or(data_dir).join("Skyrim.ccc");
    let ccc_path = ccc.unwrap_or(&default_ccc);
    let creations = match read_skyrim_ccc(ccc_path) {
        Ok(names) => names,
        Err(error) => {
            warnings.add(format!(
                "could not read optional Skyrim.ccc {}: {error:?}",
                ccc_path.display()
            ));
            Vec::new()
        }
    };
    let test_files = ini
        .map(|path| ini_plugins(path, &mut warnings))
        .unwrap_or_default();
    let test_names: HashSet<_> = test_files
        .iter()
        .map(|name| load_order::normalize_plugin_name(name))
        .collect();
    let mut listed = Vec::new();
    let mut last = HashMap::new();
    for line in lines(&text) {
        let checked = line.starts_with('*');
        let name = line.strip_prefix('*').unwrap_or(line).trim();
        if !plugin_name(name) {
            continue;
        }
        let key = load_order::normalize_plugin_name(name);
        if last.insert(key.clone(), listed.len()).is_some() {
            warnings.add(format!(
                "duplicate plugins.txt entry {key}; keeping the last position and checkbox"
            ));
        }
        listed.push((name.to_owned(), key, checked));
    }
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |name: &str, warnings: &mut Warnings| {
        let key = load_order::normalize_plugin_name(name);
        if let Some(path) = installed.get(&key) {
            if seen.insert(key) {
                selected.push(path.clone());
            }
        } else {
            warnings.add(format!("plugin {key} is not installed; skipping"));
        }
    };
    for name in OFFICIAL_MASTERS {
        add(name, &mut warnings);
    }
    for name in &creations {
        add(name, &mut warnings);
    }
    for (index, (name, key, checked)) in listed.iter().enumerate() {
        if last[key] != index {
            continue;
        }
        if *checked || test_names.contains(key) {
            add(name, &mut warnings);
        } else if !installed.contains_key(key) {
            warnings.add(format!("plugin {key} is not installed; skipping"));
        }
    }
    let mut unlisted: Vec<_> = test_files
        .iter()
        .filter(|name| !last.contains_key(&load_order::normalize_plugin_name(name)))
        .collect();
    unlisted.sort_by_key(|name| {
        let key = load_order::normalize_plugin_name(name);
        let modified = installed
            .get(&key)
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(UNIX_EPOCH);
        (modified, key)
    });
    for name in unlisted {
        add(name, &mut warnings);
    }
    let selected = usable_plugins(selected, &mut warnings);
    warnings.finish(notices);
    ensure!(
        !selected.is_empty(),
        "no usable active plugins remain after plugins.txt selection"
    );
    load_order::order_explicit_plugins(selected)
}

/// Read an explicit player list with optional CCC and INI paths; omitted INI stays unused.
pub fn read_plugins_txt_with_options(
    path: &Path,
    data_dir: &Path,
    ccc: Option<&Path>,
    ini: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut notices = Vec::new();
    let result = read_with_notices(path, data_dir, ccc, ini, &mut notices);
    for notice in notices {
        eprintln!("warning: {notice}");
    }
    result
}

/// Read a player list with the game-folder CCC default and no ambient Skyrim.ini.
pub fn read_plugins_txt(path: &Path, data_dir: &Path) -> Result<Vec<PathBuf>> {
    read_plugins_txt_with_options(path, data_dir, None, None)
}
