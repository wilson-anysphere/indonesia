use crate::budget::MemoryBudget;
use crate::degraded::DegradedSettings;
use crate::eviction::{EvictionRequest, MemoryEvictor};
use crate::pressure::{MemoryPressure, MemoryPressureThresholds};
use crate::report::MemoryReport;
use crate::types::{MemoryBreakdown, MemoryCategory};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

type MemoryEventListener = Arc<dyn Fn(MemoryEvent) + Send + Sync>;
type EvictorEntry = (u64, MemoryCategory, Arc<AtomicU64>, Arc<dyn MemoryEvictor>);

struct RegistrationEntry {
    category: MemoryCategory,
    usage_bytes: Arc<AtomicU64>,
    evictor: Option<Arc<dyn MemoryEvictor>>,
}

struct State {
    pressure: MemoryPressure,
    degraded: DegradedSettings,
}

struct Inner {
    budget: MemoryBudget,
    thresholds: MemoryPressureThresholds,
    next_id: AtomicU64,
    registrations: Mutex<HashMap<u64, RegistrationEntry>>,
    state: Mutex<State>,
    listeners: Mutex<Vec<MemoryEventListener>>,
}

/// An update emitted when pressure crosses a threshold (after enforcement).
#[derive(Debug, Clone)]
pub struct MemoryEvent {
    pub previous_pressure: MemoryPressure,
    pub pressure: MemoryPressure,
    pub report: MemoryReport,
}

/// Central coordinator for memory budgeting and eviction.
#[derive(Clone)]
pub struct MemoryManager {
    inner: Arc<Inner>,
}

impl MemoryManager {
    pub fn new(budget: MemoryBudget) -> Self {
        Self::with_thresholds(budget, MemoryPressureThresholds::default())
    }

    pub fn with_thresholds(budget: MemoryBudget, thresholds: MemoryPressureThresholds) -> Self {
        let pressure = MemoryPressure::Low;
        let degraded = DegradedSettings::for_pressure(pressure);
        Self {
            inner: Arc::new(Inner {
                budget,
                thresholds,
                next_id: AtomicU64::new(1),
                registrations: Mutex::new(HashMap::new()),
                state: Mutex::new(State { pressure, degraded }),
                listeners: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn budget(&self) -> MemoryBudget {
        self.inner.budget
    }

    /// Subscribe to memory pressure events.
    pub fn subscribe(&self, listener: Arc<dyn Fn(MemoryEvent) + Send + Sync>) {
        self.inner.listeners.lock().unwrap().push(listener);
    }

    /// Register a component for memory accounting only.
    pub fn register_tracker(
        &self,
        name: impl Into<String>,
        category: MemoryCategory,
    ) -> MemoryRegistration {
        self.register_inner(name.into(), category, None)
    }

    /// Register a component for memory accounting and eviction participation.
    pub fn register_evictor(
        &self,
        name: impl Into<String>,
        category: MemoryCategory,
        evictor: Arc<dyn MemoryEvictor>,
    ) -> MemoryRegistration {
        self.register_inner(name.into(), category, Some(evictor))
    }

    fn register_inner(
        &self,
        name: String,
        category: MemoryCategory,
        evictor: Option<Arc<dyn MemoryEvictor>>,
    ) -> MemoryRegistration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let usage_bytes = Arc::new(AtomicU64::new(0));

        self.inner.registrations.lock().unwrap().insert(
            id,
            RegistrationEntry {
                category,
                usage_bytes: usage_bytes.clone(),
                evictor,
            },
        );

        MemoryRegistration {
            id,
            name,
            category,
            usage_bytes,
            manager: Arc::downgrade(&self.inner),
        }
    }

    /// Current pressure level based on tracked usage (no eviction).
    pub fn pressure(&self) -> MemoryPressure {
        let usage_total = self.usage_breakdown().total();
        self.pressure_for_total(usage_total)
    }

    /// Current degraded settings derived from the last computed pressure.
    pub fn degraded_settings(&self) -> DegradedSettings {
        self.inner.state.lock().unwrap().degraded
    }

    /// Snapshot of current memory state (no eviction).
    pub fn report(&self) -> MemoryReport {
        let usage = self.usage_breakdown();
        let pressure = self.pressure_for_total(usage.total());
        let degraded = DegradedSettings::for_pressure(pressure);
        MemoryReport {
            budget: self.inner.budget,
            usage,
            pressure,
            degraded,
        }
    }

    /// Recompute pressure and attempt eviction if needed.
    ///
    /// This function is deterministic and synchronous; callers can drive it from
    /// a timer, after large allocations, or before starting expensive work.
    pub fn enforce(&self) -> MemoryReport {
        let before_usage = self.usage_breakdown();
        let before_pressure = self.pressure_for_total(before_usage.total());

        // Under high pressure, ask evictors to persist cold artifacts first.
        if matches!(
            before_pressure,
            MemoryPressure::High | MemoryPressure::Critical
        ) {
            self.flush_to_disk_best_effort();
        }

        let target_ratio = eviction_target_ratio(before_pressure);
        self.evict_to_ratio(before_pressure, target_ratio);

        let after_report = self.report();

        self.maybe_emit_event(before_pressure, after_report.clone());
        after_report
    }

    fn flush_to_disk_best_effort(&self) {
        let registrations = self.inner.registrations.lock().unwrap();
        for entry in registrations.values() {
            if let Some(evictor) = &entry.evictor {
                // Best-effort: ignore errors. Persistence is a performance knob,
                // not correctness.
                let _ = evictor.flush_to_disk();
            }
        }
    }

    fn evict_to_ratio(&self, pressure: MemoryPressure, ratio: f64) {
        // Targets are derived from the per-category budgets.
        let mut target = self.inner.budget.categories;
        for category in MemoryBreakdown::categories() {
            let bytes = target.get(category);
            let target_bytes = ((bytes as f64) * ratio).round() as u64;
            target.set(category, target_bytes);
        }

        // Try multiple passes to give evictors a chance to converge without
        // risking long stalls.
        for _round in 0..3 {
            let usage = self.usage_breakdown();
            if self.within_targets(usage, target) {
                break;
            }

            self.evict_once(pressure, usage, target);
        }
    }

    fn within_targets(&self, usage: MemoryBreakdown, target: MemoryBreakdown) -> bool {
        for category in MemoryBreakdown::categories() {
            if usage.get(category) > target.get(category) {
                return false;
            }
        }
        true
    }

    fn evict_once(
        &self,
        pressure: MemoryPressure,
        usage: MemoryBreakdown,
        target: MemoryBreakdown,
    ) {
        // Snapshot current registrations so we don't hold the lock while calling
        // out into evictors.
        let entries: Vec<EvictorEntry> = {
            let registrations = self.inner.registrations.lock().unwrap();
            registrations
                .iter()
                .filter_map(|(&id, entry)| {
                    entry.evictor.as_ref().map(|evictor| {
                        (
                            id,
                            entry.category,
                            entry.usage_bytes.clone(),
                            evictor.clone(),
                        )
                    })
                })
                .collect()
        };

        for category in MemoryBreakdown::categories() {
            let category_usage = usage.get(category);
            let category_target = target.get(category);

            if category_usage <= category_target {
                continue;
            }

            // Sum evictable usage for this category.
            let mut evictable_usage = 0u64;
            let mut candidates = Vec::new();
            for (_id, entry_category, usage_counter, evictor) in &entries {
                if *entry_category != category {
                    continue;
                }
                let component_usage = usage_counter.load(Ordering::Relaxed);
                evictable_usage = evictable_usage.saturating_add(component_usage);
                candidates.push((component_usage, evictor.clone()));
            }

            if evictable_usage == 0 {
                continue;
            }

            // Non-evictable usage is approximated as the delta between total
            // usage and evictable usage (for this simplified skeleton).
            // In a full implementation, all memory participants would register.
            let non_evictable = category_usage.saturating_sub(evictable_usage);
            let effective_target = category_target.max(non_evictable);
            let evictable_target = effective_target.saturating_sub(non_evictable);

            for (component_usage, evictor) in candidates {
                let component_target = if category_usage == 0 {
                    0
                } else {
                    // Proportional share of the evictable target.
                    let numer = (component_usage as u128) * (evictable_target as u128);
                    let denom = evictable_usage.max(1) as u128;
                    (numer / denom) as u64
                };

                let request = EvictionRequest {
                    pressure,
                    target_bytes: component_target,
                };
                let _ = evictor.evict(request);
            }
        }
    }

    fn maybe_emit_event(&self, before_pressure: MemoryPressure, report: MemoryReport) {
        let mut state = self.inner.state.lock().unwrap();
        let after_pressure = report.pressure;
        let after_degraded = report.degraded;

        let pressure_changed = state.pressure != after_pressure;
        let degraded_changed = state.degraded != after_degraded;
        if !pressure_changed && !degraded_changed {
            return;
        }

        let previous_pressure = state.pressure;
        state.pressure = after_pressure;
        state.degraded = after_degraded;
        drop(state);

        let listeners = self.inner.listeners.lock().unwrap().clone();
        if listeners.is_empty() {
            return;
        }

        let event = MemoryEvent {
            previous_pressure,
            pressure: after_pressure,
            report,
        };

        for listener in listeners {
            listener(event.clone());
        }

        // Also consider the "before" pressure for correctness; if the system
        // reduced pressure without crossing thresholds, we still updated state.
        let _ = before_pressure;
    }

    fn pressure_for_total(&self, usage_total: u64) -> MemoryPressure {
        let budget_total = self.inner.budget.total.max(1);
        let ratio = (usage_total as f64) / (budget_total as f64);
        self.inner.thresholds.level_for_ratio(ratio)
    }

    fn usage_breakdown(&self) -> MemoryBreakdown {
        let registrations = self.inner.registrations.lock().unwrap();
        let mut breakdown = MemoryBreakdown::default();
        for entry in registrations.values() {
            let bytes = entry.usage_bytes.load(Ordering::Relaxed);
            let prev = breakdown.get(entry.category);
            breakdown.set(entry.category, prev.saturating_add(bytes));
        }
        breakdown
    }
}

fn eviction_target_ratio(pressure: MemoryPressure) -> f64 {
    match pressure {
        MemoryPressure::Low => 1.0,
        MemoryPressure::Medium => 0.70,
        MemoryPressure::High => 0.50,
        MemoryPressure::Critical => 0.0,
    }
}

/// Handle kept by the registering component; dropping it unregisters the
/// component and removes its contribution from memory accounting.
pub struct MemoryRegistration {
    id: u64,
    name: String,
    category: MemoryCategory,
    usage_bytes: Arc<AtomicU64>,
    manager: Weak<Inner>,
}

impl MemoryRegistration {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn category(&self) -> MemoryCategory {
        self.category
    }

    pub fn tracker(&self) -> MemoryTracker {
        MemoryTracker {
            usage_bytes: self.usage_bytes.clone(),
        }
    }
}

impl std::fmt::Debug for MemoryRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryRegistration")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("category", &self.category)
            .field("usage_bytes", &self.usage_bytes.load(Ordering::Relaxed))
            .finish()
    }
}

impl Drop for MemoryRegistration {
    fn drop(&mut self) {
        self.usage_bytes.store(0, Ordering::Relaxed);
        if let Some(manager) = self.manager.upgrade() {
            manager.registrations.lock().unwrap().remove(&self.id);
        }
    }
}

/// Lightweight per-component memory accounting handle.
#[derive(Clone)]
pub struct MemoryTracker {
    usage_bytes: Arc<AtomicU64>,
}

impl MemoryTracker {
    pub fn set_bytes(&self, bytes: u64) {
        self.usage_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, delta: i64) {
        let mut current = self.usage_bytes.load(Ordering::Relaxed);
        loop {
            let next = if delta >= 0 {
                current.saturating_add(delta as u64)
            } else {
                current.saturating_sub(delta.unsigned_abs())
            };
            match self.usage_bytes.compare_exchange(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn bytes(&self) -> u64 {
        self.usage_bytes.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for MemoryTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTracker")
            .field("bytes", &self.bytes())
            .finish()
    }
}
