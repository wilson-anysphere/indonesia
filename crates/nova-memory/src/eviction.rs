use crate::pressure::MemoryPressure;
use crate::types::MemoryCategory;

/// Eviction request passed from the memory manager to an eviction participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictionRequest {
    /// Current system pressure level.
    pub pressure: MemoryPressure,
    /// Target memory usage in bytes for this component (best-effort).
    pub target_bytes: u64,
}

/// Result of an eviction attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictionResult {
    pub before_bytes: u64,
    pub after_bytes: u64,
}

impl EvictionResult {
    pub fn freed_bytes(self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }
}

/// A component that can evict memory under pressure.
///
/// Implementations are expected to:
/// - Store cached values behind `Arc` (or otherwise ensure values remain valid
///   if referenced elsewhere) to avoid invalid frees with Salsa snapshots.
/// - Update their registered [`crate::MemoryTracker`] after eviction.
pub trait MemoryEvictor: Send + Sync {
    fn name(&self) -> &str;
    fn category(&self) -> MemoryCategory;

    /// Ordering hint for eviction within a single [`MemoryCategory`].
    ///
    /// Lower values are evicted first.
    ///
    /// This is intended for cases where "largest first" is not the right
    /// policy, such as preferring cheap-to-rebuild caches over
    /// expensive/destructive eviction.
    fn eviction_priority(&self) -> u8 {
        0
    }

    /// Attempt to reduce memory usage down to `target_bytes`.
    fn evict(&self, request: EvictionRequest) -> EvictionResult;

    /// Persist cold artifacts to disk when possible.
    ///
    /// This is called by the memory manager under `High`/`Critical` pressure
    /// prior to more destructive eviction.
    fn flush_to_disk(&self) -> std::io::Result<()> {
        Ok(())
    }
}
