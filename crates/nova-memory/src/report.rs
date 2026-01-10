use crate::budget::MemoryBudget;
use crate::degraded::DegradedSettings;
use crate::pressure::MemoryPressure;
use crate::types::MemoryBreakdown;
use serde::{Deserialize, Serialize};

/// Snapshot of memory state intended for telemetry/LSP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryReport {
    pub budget: MemoryBudget,
    pub usage: MemoryBreakdown,
    pub pressure: MemoryPressure,
    pub degraded: DegradedSettings,
}

impl MemoryReport {
    pub fn usage_total_bytes(&self) -> u64 {
        self.usage.total()
    }
}
