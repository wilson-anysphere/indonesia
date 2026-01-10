use serde::{Deserialize, Serialize};

/// Coarse-grained memory pressure levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Low,
    Medium,
    High,
    Critical,
}

/// Thresholds for computing [`MemoryPressure`] from budget usage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryPressureThresholds {
    /// Enter `Medium` when `usage / budget >= medium`.
    pub medium: f64,
    /// Enter `High` when `usage / budget >= high`.
    pub high: f64,
    /// Enter `Critical` when `usage / budget >= critical`.
    pub critical: f64,
}

impl Default for MemoryPressureThresholds {
    fn default() -> Self {
        Self {
            medium: 0.70,
            high: 0.85,
            critical: 0.95,
        }
    }
}

impl MemoryPressureThresholds {
    pub fn level_for_ratio(self, ratio: f64) -> MemoryPressure {
        if ratio >= self.critical {
            MemoryPressure::Critical
        } else if ratio >= self.high {
            MemoryPressure::High
        } else if ratio >= self.medium {
            MemoryPressure::Medium
        } else {
            MemoryPressure::Low
        }
    }
}
