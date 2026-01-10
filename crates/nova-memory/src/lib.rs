//! Memory budgeting, accounting and eviction orchestration for Nova.
//!
//! This crate is intentionally lightweight and "best-effort":
//! - Accounting is approximate and driven by the owning components.
//! - Eviction is cooperative via [`MemoryEvictor`] implementors.
//! - All values that may outlive cache entries should be stored behind `Arc`
//!   (mirroring Salsa snapshot semantics). Eviction drops cache references,
//!   but does not invalidate values held by other parts of the system.

mod budget;
mod degraded;
mod eviction;
mod manager;
mod pressure;
mod report;
mod types;

pub use budget::MemoryBreakdownOverrides;
pub use budget::{MemoryBudget, MemoryBudgetOverrides, GB, MB};
pub use degraded::{BackgroundIndexingMode, DegradedSettings};
pub use eviction::{EvictionRequest, EvictionResult, MemoryEvictor};
pub use manager::{MemoryEvent, MemoryManager, MemoryRegistration, MemoryTracker};
pub use pressure::{MemoryPressure, MemoryPressureThresholds};
pub use report::MemoryReport;
pub use types::{MemoryBreakdown, MemoryCategory};
