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
pub use budget::{MemoryBudget, MemoryBudgetOverrides, GB, MB};
#[doc(hidden)]
pub use budget::{effective_system_total_memory_bytes, interpret_rlimit_as_bytes};
pub use degraded::{BackgroundIndexingMode, DegradedSettings};
pub use eviction::{EvictionRequest, EvictionResult, MemoryEvictor};
pub use manager::{MemoryEvent, MemoryManager, MemoryRegistration, MemoryTracker};
pub use pressure::{MemoryPressure, MemoryPressureThresholds};
pub use report::{ComponentUsage, MemoryReport};
pub use types::{MemoryBreakdown, MemoryCategory};
