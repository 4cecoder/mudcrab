//! Optional canonical INSERT subsite diagnostics, separate from the 18 outer sites.
//! Child wall timings overlap the outer SQL site and must not be added to its total.

use std::{
    ffi::OsStr,
    io::Write,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(crate) enum BindSite {
    Bind,
    ExecuteReset,
    Clear,
}

const NAMES: [&str; 3] = ["bind_six", "execute_reset", "clear_bindings"];
const MAX_LINE_BYTES: usize = 1536;
const BATCH: u64 = 1024;
const INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Observation {
    seconds: Duration,
    calls: u64,
    completed_calls: u64,
    failed_calls: u64,
    payload_bytes: u64,
}

pub(crate) struct CanonicalBindProfile {
    enabled: bool,
    finished: bool,
    started: Option<Instant>,
    last_ended: Option<Instant>,
    last_progress: Option<Instant>,
    rows_attempted: u64,
    rows_inserted_and_cleared: u64,
    attempted_payload_bytes: u64,
    reported_sql_changes: u64,
    logging_failures: u64,
    suppressed_lines: u64,
    observations: [Observation; 3],
}

impl CanonicalBindProfile {
    pub(crate) fn new() -> Self {
        Self::with_enabled(
            std::env::var_os("MUDCRAB_PROFILE_CANONICAL_BIND").as_deref() == Some(OsStr::new("1")),
        )
    }

    pub(crate) fn with_enabled(enabled: bool) -> Self {
        let started = enabled.then(Instant::now);
        Self {
            enabled,
            finished: false,
            started,
            last_ended: started,
            last_progress: started,
            rows_attempted: 0,
            rows_inserted_and_cleared: 0,
            attempted_payload_bytes: 0,
            reported_sql_changes: 0,
            logging_failures: 0,
            suppressed_lines: 0,
            observations: std::array::from_fn(|_| Observation::default()),
        }
    }

    pub(crate) fn begin(&self) -> Option<Instant> {
        self.begin_with(Instant::now)
    }

    fn begin_with(&self, clock: impl FnOnce() -> Instant) -> Option<Instant> {
        if self.enabled { Some(clock()) } else { None }
    }

    pub(crate) fn attempt_row(&mut self, bytes: usize) {
        if !self.enabled {
            return;
        }
        self.rows_attempted = self.rows_attempted.saturating_add(1);
        self.attempted_payload_bytes = self.attempted_payload_bytes.saturating_add(bytes as u64);
    }

    /// `completed` describes the public API call, not an additional SQLite claim.
    /// Clear has no Result in rusqlite: its completed call is separately labelled.
    pub(crate) fn record(
        &mut self,
        site: BindSite,
        start: Option<Instant>,
        completed: bool,
        bytes: usize,
    ) {
        self.record_with(site, start, completed, bytes, Instant::now);
    }

    fn record_with(
        &mut self,
        site: BindSite,
        start: Option<Instant>,
        completed: bool,
        bytes: usize,
        clock: impl FnOnce() -> Instant,
    ) {
        if !self.enabled {
            return;
        }
        let Some(start) = start else {
            return;
        };
        let ended = clock();
        let item = &mut self.observations[site as usize];
        item.seconds = item
            .seconds
            .saturating_add(ended.saturating_duration_since(start));
        item.calls = item.calls.saturating_add(1);
        item.completed_calls = item.completed_calls.saturating_add(u64::from(completed));
        item.failed_calls = item.failed_calls.saturating_add(u64::from(!completed));
        item.payload_bytes = item.payload_bytes.saturating_add(bytes as u64);
        self.last_ended = Some(ended);
    }

    pub(crate) fn sql_changes(&mut self, changed: usize) {
        if self.enabled {
            self.reported_sql_changes = self.reported_sql_changes.saturating_add(changed as u64);
        }
    }

    /// Check a reused end timestamp only every batch; read no additional clock.
    /// Called after the outer RecordsSql site closes, so progress I/O is not a subsite.
    pub(crate) fn completed_row(&mut self) {
        if !self.enabled {
            return;
        }
        self.completed_row_to(&mut std::io::stderr());
    }

    fn completed_row_to(&mut self, sink: &mut impl Write) {
        if !self.enabled {
            return;
        }
        self.rows_inserted_and_cleared = self.rows_inserted_and_cleared.saturating_add(1);
        if !self.rows_inserted_and_cleared.is_multiple_of(BATCH) {
            return;
        }
        let (Some(ended), Some(previous)) = (self.last_ended, self.last_progress) else {
            return;
        };
        if ended.saturating_duration_since(previous) < INTERVAL {
            return;
        }
        self.last_progress = Some(ended);
        let event = self.event("running");
        self.emit_to("records-canonical-bind-progress ", &event, sink);
    }

    fn event(&self, outcome: &str) -> String {
        let observations = NAMES
            .iter()
            .zip(self.observations.iter())
            .map(|(name, item)| {
                serde_json::json!({"site": name, "seconds": item.seconds.as_secs_f64(),
                "calls": item.calls, "completed_calls": item.completed_calls,
                "failed_calls": item.failed_calls, "attempted_payload_bytes": item.payload_bytes})
            })
            .collect::<Vec<_>>();
        serde_json::json!({"schema": "canonical-bind-subsites-v1", "clock": "wall", "outcome": outcome,
            "not_additive_with_outer_18_sites": true, "rows_attempted": self.rows_attempted,
            "rows_inserted_and_cleared": self.rows_inserted_and_cleared,
            "attempted_payload_bytes": self.attempted_payload_bytes,
            "reported_sql_changes": self.reported_sql_changes,
            "elapsed_to_last_canonical_call_seconds": self.started.zip(self.last_ended)
                .map(|(a,b)| b.saturating_duration_since(a).as_secs_f64()),
            "logging_failures_before_write": self.logging_failures, "suppressed_lines": self.suppressed_lines,
            "observations": observations}).to_string()
    }

    fn emit_to(&mut self, prefix: &str, event: &str, sink: &mut impl Write) {
        if !self.enabled {
            return;
        }
        if prefix.len() + event.len() + 1 > MAX_LINE_BYTES {
            self.suppressed_lines = self.suppressed_lines.saturating_add(1);
        } else if writeln!(sink, "{prefix}{event}").is_err() {
            self.logging_failures = self.logging_failures.saturating_add(1);
        }
    }

    pub(crate) fn finish(&mut self, committed: bool) {
        if !self.enabled || self.finished {
            return;
        }
        self.finish_to(
            if committed {
                "committed"
            } else {
                "commit_error"
            },
            &mut std::io::stderr(),
        );
    }

    fn finish_to(&mut self, outcome: &str, sink: &mut impl Write) {
        if !self.enabled || self.finished {
            return;
        }
        self.finished = true;
        let event = self.event(outcome);
        self.emit_to("records-canonical-bind-detail ", &event, sink);
    }
}

impl Drop for CanonicalBindProfile {
    fn drop(&mut self) {
        if self.enabled && !self.finished {
            self.finish_to("unfinished_or_failed", &mut std::io::stderr());
        }
    }
}

#[cfg(test)]
include!("canonical_bind_fixture_v1.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct Broken {
        left: usize,
        written: Vec<u8>,
    }
    impl Write for Broken {
        fn write(&mut self, value: &[u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let n = self.left.min(value.len());
            self.written.extend_from_slice(&value[..n]);
            self.left -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn disabled_reads_no_clock_and_touches_no_sink() {
        let mut profile = CanonicalBindProfile::with_enabled(false);
        assert!(profile.begin_with(|| panic!("disabled clock")).is_none());
        profile.record_with(BindSite::Bind, Some(Instant::now()), true, 29, || {
            panic!("disabled end clock")
        });
        profile.attempt_row(29);
        profile.sql_changes(1);
        profile.completed_row();
        let mut sink = Broken {
            left: 0,
            written: Vec::new(),
        };
        profile.finish_to("committed", &mut sink);
        assert_eq!(profile.rows_attempted, 0);
        assert_eq!(profile.logging_failures, 0);
        assert!(sink.written.is_empty());
    }

    #[test]
    fn asymmetric_subsites_and_failures_are_not_double_counted() {
        let mut profile = CanonicalBindProfile::with_enabled(true);
        let start = profile.started.unwrap();
        profile.attempt_row(29);
        profile.record_with(BindSite::Bind, Some(start), true, 29, || {
            start + Duration::from_millis(3)
        });
        profile.record_with(BindSite::ExecuteReset, Some(start), false, 29, || {
            start + Duration::from_millis(11)
        });
        let mut sink = Vec::new();
        profile.finish_to("unfinished_or_failed", &mut sink);
        let text = String::from_utf8(sink).unwrap();
        let fact: serde_json::Value =
            serde_json::from_str(text.strip_prefix("records-canonical-bind-detail ").unwrap())
                .unwrap();
        assert_eq!(fact["rows_attempted"], 1);
        assert_eq!(fact["rows_inserted_and_cleared"], 0);
        assert_eq!(fact["observations"][0]["seconds"], 0.003);
        assert_eq!(fact["observations"][1]["seconds"], 0.011);
        assert_eq!(fact["observations"][1]["failed_calls"], 1);
        assert_eq!(fact["observations"][2]["calls"], 0);
        assert_eq!(fact["not_additive_with_outer_18_sites"], true);
    }

    #[test]
    fn failing_and_partial_writes_do_not_change_site_counts() {
        for left in [0, 7] {
            let mut profile = CanonicalBindProfile::with_enabled(true);
            profile.rows_attempted = 17;
            profile.attempted_payload_bytes = 419;
            let mut sink = Broken {
                left,
                written: Vec::new(),
            };
            profile.finish_to("commit_error", &mut sink);
            assert_eq!(profile.logging_failures, 1);
            assert_eq!(profile.rows_attempted, 17);
            assert_eq!(profile.attempted_payload_bytes, 419);
            assert_eq!(sink.written.len(), left);
            profile.finish_to("committed", &mut sink);
            assert_eq!(profile.logging_failures, 1);
        }
    }

    #[test]
    fn line_cap_and_saturated_counters_are_bounded() {
        let mut profile = CanonicalBindProfile::with_enabled(true);
        profile.rows_attempted = u64::MAX;
        profile.attempt_row(19);
        assert_eq!(profile.rows_attempted, u64::MAX);
        let mut sink = Vec::new();
        profile.emit_to("x", &"y".repeat(MAX_LINE_BYTES), &mut sink);
        assert!(sink.is_empty());
        assert_eq!(profile.suppressed_lines, 1);
        let event = profile.event("committed");
        assert!("records-canonical-bind-detail ".len() + event.len() < MAX_LINE_BYTES);
        profile.finish_to("committed", &mut sink);
    }

    #[test]
    fn progress_requires_batch_and_interval_and_reuses_last_end() {
        let mut profile = CanonicalBindProfile::with_enabled(true);
        let start = profile.started.unwrap();
        let mut sink = Vec::new();
        profile.rows_inserted_and_cleared = 1022;
        profile.last_ended = Some(start + Duration::from_secs(31));
        profile.completed_row_to(&mut sink);
        assert!(sink.is_empty());
        profile.last_ended = Some(start + Duration::from_secs(29));
        profile.completed_row_to(&mut sink);
        assert!(sink.is_empty());
        assert_eq!(profile.last_progress, Some(start));
        profile.rows_inserted_and_cleared = 2047;
        profile.last_ended = Some(start + Duration::from_secs(31));
        profile.completed_row_to(&mut sink);
        assert!(!sink.is_empty());
        assert!(sink.len() <= MAX_LINE_BYTES);
        assert_eq!(profile.last_progress, profile.last_ended);
        let bytes = sink.len();
        profile.rows_inserted_and_cleared = 3071;
        profile.last_ended = Some(start + Duration::from_secs(32));
        profile.completed_row_to(&mut sink);
        assert_eq!(sink.len(), bytes);
        profile.finish_to("committed", &mut Vec::new());
    }
}
