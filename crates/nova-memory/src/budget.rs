use crate::types::MemoryBreakdown;
use serde::{Deserialize, Serialize};

/// One mebibyte in bytes.
pub const MB: u64 = 1024 * 1024;

/// One gibibyte in bytes.
pub const GB: u64 = 1024 * MB;

/// A memory budget for Nova.
///
/// The budget is split into coarse categories; individual components register
/// their usage under one of them. Enforcement is best-effort and cooperative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    /// Total memory budget in bytes.
    pub total: u64,
    /// Per-category budgets in bytes.
    pub categories: MemoryBreakdown,
}

/// Optional overrides for [`MemoryBudget`], intended to be populated by a
/// configuration layer (`nova-config`) once it exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudgetOverrides {
    /// Override total budget. When set, category defaults are derived from this.
    pub total: Option<u64>,
    /// Override individual category budgets.
    pub categories: MemoryBreakdownOverrides,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBreakdownOverrides {
    pub query_cache: Option<u64>,
    pub syntax_trees: Option<u64>,
    pub indexes: Option<u64>,
    pub type_info: Option<u64>,
    pub other: Option<u64>,
}

impl MemoryBudget {
    /// Derive a default budget from the system's total RAM (best-effort).
    ///
    /// Strategy from `docs/10-performance-engineering.md`:
    /// - budget = min(total_ram / 4, 4GiB) clamped to at least 512MiB.
    pub fn default_for_system() -> Self {
        let total_ram = system_total_memory_bytes().unwrap_or(8 * GB);
        let budget = (total_ram / 4).clamp(512 * MB, 4 * GB);
        Self::from_total(budget)
    }

    /// Build a budget from an explicit total, using the default category split.
    pub fn from_total(total: u64) -> Self {
        // Percentages from docs/10-performance-engineering.md
        let query_cache = total * 40 / 100;
        let syntax_trees = total * 25 / 100;
        let indexes = total * 20 / 100;
        let type_info = total * 10 / 100;
        let assigned = query_cache + syntax_trees + indexes + type_info;
        let other = total.saturating_sub(assigned);

        Self {
            total,
            categories: MemoryBreakdown {
                query_cache,
                syntax_trees,
                indexes,
                type_info,
                other,
            },
        }
    }

    /// Apply overrides to this budget.
    ///
    /// If the override causes per-category budgets to exceed `total`, budgets are
    /// scaled down proportionally to preserve the `sum(categories) == total`
    /// invariant.
    pub fn apply_overrides(mut self, overrides: MemoryBudgetOverrides) -> Self {
        if let Some(total) = overrides.total {
            self = Self::from_total(total);
        }

        if let Some(bytes) = overrides.categories.query_cache {
            self.categories.query_cache = bytes;
        }
        if let Some(bytes) = overrides.categories.syntax_trees {
            self.categories.syntax_trees = bytes;
        }
        if let Some(bytes) = overrides.categories.indexes {
            self.categories.indexes = bytes;
        }
        if let Some(bytes) = overrides.categories.type_info {
            self.categories.type_info = bytes;
        }
        if let Some(bytes) = overrides.categories.other {
            self.categories.other = bytes;
        }

        // Re-normalize.
        let sum = self.categories.total();
        match sum.cmp(&self.total) {
            std::cmp::Ordering::Less => {
                // Give remaining headroom to "other" to keep accounting simple.
                self.categories.other = self.categories.other.saturating_add(self.total - sum);
            }
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => {
                // Scale categories down proportionally; last category picks up remainder.
                // This is a best-effort "never exceed total" policy.
                let total = self.total.max(1);
                let query_cache = self.categories.query_cache.saturating_mul(total) / sum;
                let syntax_trees = self.categories.syntax_trees.saturating_mul(total) / sum;
                let indexes = self.categories.indexes.saturating_mul(total) / sum;
                let type_info = self.categories.type_info.saturating_mul(total) / sum;
                let assigned = query_cache + syntax_trees + indexes + type_info;
                let other = total.saturating_sub(assigned);
                self.categories = MemoryBreakdown {
                    query_cache,
                    syntax_trees,
                    indexes,
                    type_info,
                    other,
                };
            }
        }

        self
    }
}

fn system_total_memory_bytes() -> Option<u64> {
    use sysinfo::System;

    let mut sys = System::new();
    sys.refresh_memory();
    // sysinfo reports KiB.
    Some(sys.total_memory().saturating_mul(1024))
}
