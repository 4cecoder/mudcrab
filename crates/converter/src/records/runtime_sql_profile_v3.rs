//! Optional serial runtime SQL observations and bounded completed-record progress.
//!
//! Seconds are wall time; optional diagnostic I/O never changes the SQL result.

use std::{
    io::Write,
    time::{Duration, Instant},
};

/// Disjoint timed sites within one runtime export, excluding its outer stage.
#[derive(Clone, Copy)]
pub(crate) enum Site {
    Setup,
    TerrainWinners,
    Cells,
    Ordering,
    RecordSerialization,
    RecordsSql,
    FormIdMap,
    RuntimeProjection,
    SubrecordView,
    VmadParsing,
    ScriptSerialization,
    ScriptSql,
    ReferenceDerivedFields,
    ReferenceSerialization,
    ReferenceSql,
    ReferenceRtree,
    RemainingProjections,
    Commit,
}

const NAMES: [&str; 18] = [
    "setup",
    "terrain_winners",
    "cells",
    "ordering",
    "record_serialization",
    "records_sql",
    "formid_map",
    "runtime_projection",
    "subrecord_view",
    "vmad_parsing",
    "script_serialization",
    "script_sql",
    "reference_derived_fields",
    "reference_serialization",
    "reference_sql",
    "reference_rtree",
    "remaining_projections",
    "commit",
];
const PROGRESS_BATCH: u64 = 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);
// Inclusive of prefix and newline: at most 180 KiB per hour, plus one final line.
const MAX_PROGRESS_LINE_BYTES: usize = 1536;
const PROGRESS_PREFIX: &str = "records-sql-progress ";

/// Write an optional observation through a fallible writer; never unwrap its result.
fn write_observation_to(sink: &mut impl Write, prefix: &str, event: &str) -> bool {
    writeln!(sink, "{prefix}{event}").is_ok()
}

/// One bounded aggregate; overflow saturation does not change export behavior.
#[derive(Default)]
struct Observation {
    elapsed: Duration,
    records: u64,
    bytes: u64,
}

/// One serial owner, explicitly passed to helpers, with no shared accumulator locks.
pub(crate) struct RuntimeSqlProfile {
    enabled: bool,
    started: Option<Instant>,
    last_ended: Option<Instant>,
    last_progress: Option<Instant>,
    processed_records: u64,
    suppressed_progress: u64,
    logging_failures: u64,
    observations: [Observation; 18],
}

impl RuntimeSqlProfile {
    /// Read the existing once-enabled switch and read one initial clock if enabled.
    pub(crate) fn new() -> Self {
        let enabled = super::profile::enabled();
        let started = enabled.then(Instant::now);
        Self {
            enabled,
            started,
            last_ended: started,
            last_progress: started,
            processed_records: 0,
            suppressed_progress: 0,
            logging_failures: 0,
            observations: std::array::from_fn(|_| Observation::default()),
        }
    }

    /// Keep direct public reference insertion outside export detail and progress.
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            started: None,
            last_ended: None,
            last_progress: None,
            processed_records: 0,
            suppressed_progress: 0,
            logging_failures: 0,
            observations: std::array::from_fn(|_| Observation::default()),
        }
    }

    /// Read no clock when profiling is disabled.
    pub(crate) fn begin(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    /// Reuse this site's single end timestamp for accounting and progress checks.
    pub(crate) fn record(
        &mut self,
        site: Site,
        started: Option<Instant>,
        records: usize,
        bytes: usize,
    ) {
        if !self.enabled {
            return;
        }
        let Some(started) = started else { return };
        let ended = Instant::now();
        let observation = &mut self.observations[site as usize];
        observation.elapsed = observation
            .elapsed
            .saturating_add(ended.duration_since(started));
        observation.records = observation.records.saturating_add(records as u64);
        observation.bytes = observation.bytes.saturating_add(bytes as u64);
        self.last_ended = Some(ended);
    }

    /// Count one completed loop record, including nonwinning LAND, without a clock.
    pub(crate) fn completed_record(&mut self, form_id: u32, signature: &[u8; 4]) {
        if !self.enabled {
            return;
        }
        self.processed_records = self.processed_records.saturating_add(1);
        if !self.processed_records.is_multiple_of(PROGRESS_BATCH) {
            return;
        }
        let (Some(ended), Some(previous)) = (self.last_ended, self.last_progress) else {
            return;
        };
        if ended.duration_since(previous) < PROGRESS_INTERVAL {
            return;
        }
        // Update the throttle even if the cap or writer suppresses this observation.
        self.last_progress = Some(ended);
        let sites = self
            .observations
            .iter()
            .map(|observation| {
                serde_json::json!([
                    observation.elapsed.as_secs_f64(),
                    observation.records,
                    observation.bytes,
                ])
            })
            .collect::<Vec<_>>();
        let event = serde_json::json!({
            "schema": "runtime-sql-progress-v3",
            "processed_records": self.processed_records,
            "last_completed_form_id": form_id,
            "last_completed_signature": String::from_utf8_lossy(signature),
            "elapsed_seconds": self.started.map(|started| ended.duration_since(started).as_secs_f64()),
            "site_columns": ["seconds", "records", "bytes"],
            "sites": sites,
        }).to_string();
        self.progress_to(&event, &mut std::io::stderr());
    }

    /// Preserve bounded observations and all cumulative counters on writer failure.
    fn progress_to(&mut self, event: &str, sink: &mut impl Write) {
        if !self.enabled {
            return;
        }
        if PROGRESS_PREFIX.len() + event.len() + 1 > MAX_PROGRESS_LINE_BYTES {
            self.suppressed_progress = self.suppressed_progress.saturating_add(1);
        } else if !write_observation_to(sink, PROGRESS_PREFIX, event) {
            self.logging_failures = self.logging_failures.saturating_add(1);
        }
    }

    /// Emit after successful commit; callers may ignore diagnostic failure counts.
    pub(crate) fn finish(self, records: usize) -> u64 {
        if !self.enabled {
            return 0;
        }
        self.finish_to(records, &mut std::io::stderr())
    }

    /// Make ignored final-write errors explicit without returning an export error.
    fn finish_to(mut self, records: usize, sink: &mut impl Write) -> u64 {
        if !self.enabled {
            return 0;
        }
        let total_seconds = self
            .started
            .zip(self.last_ended)
            .map(|(started, ended)| ended.duration_since(started).as_secs_f64());
        let measured_site_seconds = self
            .observations
            .iter()
            .map(|observation| observation.elapsed.as_secs_f64())
            .sum::<f64>();
        let observations = NAMES
            .iter()
            .zip(self.observations)
            .map(|(name, observation)| {
                serde_json::json!({
                    "site": name,
                    "seconds": observation.elapsed.as_secs_f64(),
                    "records": observation.records,
                    "bytes": observation.bytes,
                })
            })
            .collect::<Vec<_>>();
        let event = serde_json::json!({
            "schema": "runtime-sql-detail-v3",
            "stage": "record_database_export",
            "records": records,
            "processed_records": self.processed_records,
            "total_seconds": total_seconds,
            "measured_site_seconds": measured_site_seconds,
            "suppressed_progress": self.suppressed_progress,
            "logging_failures_before_final": self.logging_failures,
            "clock": "wall",
            "observations": observations,
        })
        .to_string();
        if !write_observation_to(sink, "records-sql-detail ", &event) {
            self.logging_failures = self.logging_failures.saturating_add(1);
        }
        self.logging_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// Fail at an unequal byte boundary, allowing partial writes to be exercised.
    struct FailingWriter {
        remaining: usize,
        written: Vec<u8>,
    }

    impl Write for FailingWriter {
        /// Retain the permitted prefix and return BrokenPipe after that boundary.
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            let count = self.remaining.min(bytes.len());
            self.written.extend_from_slice(&bytes[..count]);
            self.remaining -= count;
            Ok(count)
        }

        /// Flush has no separate behavior in this synthetic sink.
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A precommit-style failed progress write preserves the unequal site ledger.
    #[test]
    fn progress_writer_failure_preserves_counters() {
        for remaining in [0, 7] {
            let mut profile = RuntimeSqlProfile::disabled();
            profile.enabled = true;
            profile.processed_records = 2048;
            profile.observations[Site::RecordsSql as usize].records = 17;
            profile.observations[Site::RecordsSql as usize].bytes = 419;
            let mut sink = FailingWriter {
                remaining,
                written: Vec::new(),
            };
            profile.progress_to("{\"fixture\":1}", &mut sink);
            assert_eq!(profile.logging_failures, 1);
            assert_eq!(profile.suppressed_progress, 0);
            assert_eq!(profile.processed_records, 2048);
            assert_eq!(profile.observations[Site::RecordsSql as usize].records, 17);
            assert_eq!(profile.observations[Site::RecordsSql as usize].bytes, 419);
            assert_eq!(sink.written.len(), remaining);
            assert_eq!(profile.finish_to(2048, &mut sink), 2);
        }
    }

    /// A successful final line reports prior failures; an oversize event never writes.
    #[test]
    fn final_reports_prior_failure_and_cap_is_retained() {
        let mut profile = RuntimeSqlProfile::disabled();
        profile.enabled = true;
        profile.progress_to(
            "{}",
            &mut FailingWriter {
                remaining: 0,
                written: Vec::new(),
            },
        );
        let mut output = Vec::new();
        profile.progress_to(&"x".repeat(MAX_PROGRESS_LINE_BYTES), &mut output);
        assert!(output.is_empty());
        assert_eq!(profile.suppressed_progress, 1);
        assert_eq!(profile.finish_to(0, &mut output), 1);
        let line = String::from_utf8(output).expect("synthetic observation is UTF-8");
        let event: serde_json::Value = serde_json::from_str(
            line.strip_prefix("records-sql-detail ")
                .expect("distinct final prefix"),
        )
        .expect("one complete JSON observation");
        assert_eq!(event["logging_failures_before_final"], 1);
        assert_eq!(event["suppressed_progress"], 1);
        assert_eq!(
            event["observations"]
                .as_array()
                .expect("fixed site array")
                .len(),
            18
        );
    }

    /// Disabled optional observations neither invoke a failing sink nor count errors.
    #[test]
    fn disabled_observations_ignore_writer() {
        let mut profile = RuntimeSqlProfile::disabled();
        let mut sink = FailingWriter {
            remaining: 0,
            written: Vec::new(),
        };
        profile.progress_to("{}", &mut sink);
        assert_eq!(profile.logging_failures, 0);
        assert_eq!(profile.finish_to(3, &mut sink), 0);
        assert!(sink.written.is_empty());
    }
}
