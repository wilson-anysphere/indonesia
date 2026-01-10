use crate::pressure::MemoryPressure;
use serde::{Deserialize, Serialize};

/// Background indexing policy based on current memory conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundIndexingMode {
    /// Full background indexing.
    Full,
    /// Reduced-rate indexing (smaller batches, fewer threads, less eager).
    Reduced,
    /// No background indexing; only on-demand work.
    Paused,
}

/// Feature throttles used when the system is under memory pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegradedSettings {
    /// Skip diagnostics known to be expensive (e.g. global checks).
    pub skip_expensive_diagnostics: bool,
    /// Cap completion candidates returned to the client.
    pub completion_candidate_cap: usize,
    /// Reduce/disable background indexing.
    pub background_indexing: BackgroundIndexingMode,
}

impl DegradedSettings {
    pub fn for_pressure(pressure: MemoryPressure) -> Self {
        match pressure {
            MemoryPressure::Low | MemoryPressure::Medium => Self {
                skip_expensive_diagnostics: false,
                completion_candidate_cap: 200,
                background_indexing: BackgroundIndexingMode::Full,
            },
            MemoryPressure::High => Self {
                skip_expensive_diagnostics: true,
                completion_candidate_cap: 50,
                background_indexing: BackgroundIndexingMode::Reduced,
            },
            MemoryPressure::Critical => Self {
                skip_expensive_diagnostics: true,
                completion_candidate_cap: 20,
                background_indexing: BackgroundIndexingMode::Paused,
            },
        }
    }
}
