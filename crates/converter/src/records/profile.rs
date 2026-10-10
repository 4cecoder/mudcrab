//! Optional stage timings for complete record exports, independent of native content.

use std::{sync::OnceLock, time::Instant};

/// Read the same process-wide profile flag for stage and detailed observations.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("MUDCRAB_RECORDS_PROFILE").is_some_and(|value| value == "1")
    })
}

/// A stage timer enabled once by `MUDCRAB_RECORDS_PROFILE=1` at process startup.
pub(crate) struct ProfileStage {
    name: &'static str,
    started: Option<Instant>,
}

impl ProfileStage {
    /// Start a stage without reading the clock when profiling is disabled.
    pub(crate) fn start(name: &'static str) -> Self {
        Self {
            name,
            started: enabled().then(Instant::now),
        }
    }

    /// Emit one completed-stage observation; errors never imply a completed stage.
    pub(crate) fn finish(self, records: usize) {
        if let Some(started) = self.started {
            eprintln!(
                "records-profile {}",
                serde_json::json!({
                    "stage": self.name,
                    "seconds": started.elapsed().as_secs_f64(),
                    "records": records,
                })
            );
        }
    }
}
