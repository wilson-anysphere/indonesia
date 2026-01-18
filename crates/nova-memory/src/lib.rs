//! Memory budgeting, accounting and eviction orchestration for Nova.
//!
//! This crate is intentionally lightweight and "best-effort":
//! - Accounting is approximate and driven by the owning components.
//! - Eviction is cooperative via [`MemoryEvictor`] implementors.
//! - All values that may outlive cache entries should be stored behind `Arc`
//!   (mirroring Salsa snapshot semantics). Eviction drops cache references,
//!   but does not invalidate values held by other parts of the system.
//!
//! ## Budgets under cgroups and RLIMIT_AS
//!
//! [`MemoryBudget::default_for_system`] budgets against the smallest applicable
//! memory ceiling:
//! - Linux cgroup memory limit (cgroup v2 `memory.max`, cgroup v1 `memory.limit_in_bytes`)
//! - process `RLIMIT_AS` (address space limit) when set
//! - host total RAM
//!
//! This makes Nova respect container/agent limits and avoids budgeting above the
//! hard ceiling enforced in some operational environments.

mod budget;
#[doc(hidden)]
pub mod cgroup;
mod degraded;
mod eviction;
mod manager;
mod pressure;
mod process;
mod report;
mod types;

pub use budget::MemoryBreakdownOverrides;
#[doc(hidden)]
pub use budget::{effective_system_total_memory_bytes, interpret_rlimit_as_bytes};
pub use budget::{
    parse_byte_size, MemoryBudget, MemoryBudgetOverrides, ParseByteSizeError,
    ENV_MEMORY_BUDGET_INDEXES, ENV_MEMORY_BUDGET_OTHER, ENV_MEMORY_BUDGET_QUERY_CACHE,
    ENV_MEMORY_BUDGET_SYNTAX_TREES, ENV_MEMORY_BUDGET_TOTAL, ENV_MEMORY_BUDGET_TYPE_INFO, GB, MB,
};
pub use degraded::{BackgroundIndexingMode, DegradedSettings};
pub use eviction::{EvictionRequest, EvictionResult, MemoryEvictor};
pub use manager::{MemoryEvent, MemoryManager, MemoryRegistration, MemoryTracker};
pub use pressure::{MemoryPressure, MemoryPressureThresholds};
pub use process::current_rss_bytes;
pub use report::{ComponentUsage, MemoryReport};
pub use types::{MemoryBreakdown, MemoryCategory};
