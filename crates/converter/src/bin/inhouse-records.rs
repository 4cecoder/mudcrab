//! Produce a diagnostic in-house database/cache bundle without asset conversion.
use color_eyre::{Result, eyre::ensure};
use std::{path::PathBuf, time::Instant};

/// Export selected plugins to a fresh evidence directory without publishing a runtime pack.
fn main() -> Result<()> {
    color_eyre::install()?;
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "history") {
        // Per-version data failures are represented in the report, and a bad pack
        // receives a diagnostic without making this inspection command fail as a whole.
        match history(&args[1..]) {
            Ok(()) => {}
            Err(error) => println!("history unavailable: {error}"),
        }
        return Ok(());
    }
    args.retain(|arg| arg != "--no-lod"); // This diagnostic exporter never compiles LOD.
    ensure!(
        args.len() == 3 || args.len() == 5,
        "usage: inhouse-records <Skyrim Data> <plugins.txt> <new output directory> --no-lod [--typed-signatures WEAP,NPC_,SPEL]; or history <pack> <hex FormID> [--data DIR] [--json]"
    );
    let data = PathBuf::from(&args[0]);
    let plugins = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    let started = Instant::now();
    let mut signatures = Vec::new();
    if args.len() == 5 {
        ensure!(args[3] == "--typed-signatures", "unknown diagnostic option");
        for tag in args[4].to_string_lossy().split(',') {
            ensure!(
                tag.len() == 4 && tag.is_ascii(),
                "typed signatures must be four ASCII bytes"
            );
            signatures.push(tag.as_bytes().try_into().expect("checked signature"));
        }
    }
    let cells =
        converter::esm::inhouse::export_record_bundle_typed(&data, &plugins, &output, &signatures)?;
    println!(
        "inhouse diagnostic records: cells={cells} seconds={:.3} output={}",
        started.elapsed().as_secs_f64(),
        output.display()
    );
    Ok(())
}

/// Inspect source versions without altering a pack or writing to source plugins.
fn history(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(
        args.len() >= 2,
        "usage: inhouse-records history <pack> <hex FormID> [--data DIR] [--json]"
    );
    let pack = PathBuf::from(&args[0]);
    let id = args[1].to_string_lossy();
    let id = u32::from_str_radix(id.trim_start_matches("0x").trim_start_matches("0X"), 16)?;
    let mut data = None;
    let mut json = false;
    let mut cursor = 2;
    while cursor < args.len() {
        if args[cursor] == "--json" {
            json = true;
            cursor += 1;
        } else if args[cursor] == "--data" && cursor + 1 < args.len() {
            data = Some(PathBuf::from(&args[cursor + 1]));
            cursor += 2;
        } else {
            color_eyre::eyre::bail!("unknown or incomplete history option");
        }
    }
    let report = converter::records::history::read_history(&pack, data.as_deref(), id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", converter::records::history::display(&report));
    }
    Ok(())
}
