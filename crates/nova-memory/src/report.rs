use crate::budget::MemoryBudget;
use crate::degraded::DegradedSettings;
use crate::pressure::MemoryPressure;
use crate::types::{MemoryBreakdown, MemoryCategory};
use serde::{Deserialize, Serialize};

/// Snapshot of memory state intended for telemetry/LSP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryReport {
    pub budget: MemoryBudget,
    pub usage: MemoryBreakdown,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    pub pressure: MemoryPressure,
    pub degraded: DegradedSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentUsage {
    pub name: String,
    pub category: MemoryCategory,
    pub bytes: u64,
}

impl MemoryReport {
    pub fn usage_total_bytes(&self) -> u64 {
        self.usage.total()
    }
}
