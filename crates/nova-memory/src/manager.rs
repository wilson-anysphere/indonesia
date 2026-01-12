use crate::budget::MemoryBudget;
use crate::degraded::DegradedSettings;
use crate::eviction::{EvictionRequest, MemoryEvictor};
use crate::pressure::{MemoryPressure, MemoryPressureThresholds};
use crate::process;
use crate::report::{ComponentUsage, MemoryReport};
use crate::types::{MemoryBreakdown, MemoryCategory};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

type MemoryEventListener = Arc<dyn Fn(MemoryEvent) + Send + Sync>;
type EvictorEntry = (u64, MemoryCategory, Arc<AtomicU64>, Arc<dyn MemoryEvictor>);

struct RegistrationEntry {
    name: String,
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
    pub fn subscribe(&self, listener: MemoryEventListener) {
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
                name: name.clone(),
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

    /// Current memory pressure level (no eviction).
    ///
    /// When supported, process RSS is incorporated as an upper bound over the
    /// self-reported component totals.
    pub fn pressure(&self) -> MemoryPressure {
        let usage_total = self.usage_breakdown().total();
        let rss_bytes = process::current_rss_bytes();
        let effective_total = effective_usage_total(usage_total, rss_bytes);
        self.pressure_for_total(effective_total)
    }

    /// Current degraded settings derived from the last computed pressure.
    pub fn degraded_settings(&self) -> DegradedSettings {
        self.inner.state.lock().unwrap().degraded
    }

    /// Snapshot of current memory state (no eviction).
    pub fn report(&self) -> MemoryReport {
        let usage = self.usage_breakdown();
        let rss_bytes = process::current_rss_bytes();
        let effective_total = effective_usage_total(usage.total(), rss_bytes);
        let pressure = self.pressure_for_total(effective_total);
        let degraded = DegradedSettings::for_pressure(pressure);
        MemoryReport {
            budget: self.inner.budget,
            usage,
            rss_bytes,
            pressure,
            degraded,
        }
    }

    /// Snapshot of current memory state (no eviction) plus per-component usage.
    pub fn report_detailed(&self) -> (MemoryReport, Vec<ComponentUsage>) {
        let entries: Vec<(String, MemoryCategory, Arc<AtomicU64>)> = {
            let registrations = self.inner.registrations.lock().unwrap();
            registrations
                .values()
                .map(|entry| {
                    (
                        entry.name.clone(),
                        entry.category,
                        entry.usage_bytes.clone(),
                    )
                })
                .collect()
        };

        let mut usage = MemoryBreakdown::default();
        let mut components = Vec::with_capacity(entries.len());
        for (name, category, counter) in entries {
            let bytes = counter.load(Ordering::Relaxed);
            let prev = usage.get(category);
            usage.set(category, prev.saturating_add(bytes));
            components.push(ComponentUsage {
                name,
                category,
                bytes,
            });
        }

        components.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| category_sort_key(a.category).cmp(&category_sort_key(b.category)))
        });

        let rss_bytes = process::current_rss_bytes();
        let effective_total = effective_usage_total(usage.total(), rss_bytes);
        let pressure = self.pressure_for_total(effective_total);
        let degraded = DegradedSettings::for_pressure(pressure);

        (
            MemoryReport {
                budget: self.inner.budget,
                usage,
                rss_bytes,
                pressure,
                degraded,
            },
            components,
        )
    }

    /// Recompute pressure and attempt eviction if needed.
    ///
    /// This function is deterministic and synchronous; callers can drive it from
    /// a timer, after large allocations, or before starting expensive work.
    pub fn enforce(&self) -> MemoryReport {
        let before_usage = self.usage_breakdown();
        let before_rss = process::current_rss_bytes();
        let before_effective_total = effective_usage_total(before_usage.total(), before_rss);
        let before_pressure = self.pressure_for_total(before_effective_total);

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
        let evictors: Vec<Arc<dyn MemoryEvictor>> = {
            let registrations = self.inner.registrations.lock().unwrap();
            registrations
                .values()
                .filter_map(|entry| entry.evictor.clone())
                .collect()
        };

        for evictor in evictors {
            let _ = evictor.flush_to_disk();
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

        // Snapshot current registrations so we don't hold the lock while calling
        // out into evictors. We keep the counters by `Arc` so we can read the
        // latest usage each round without re-locking.
        let entries = self.collect_evictor_entries();

        // Try multiple passes to give evictors a chance to converge without
        // risking long stalls.
        for _round in 0..3 {
            let usage = self.usage_breakdown();
            let compensated_target = self.compensated_target(usage, target, &entries);

            if self.within_targets(usage, compensated_target) {
                break;
            }

            self.evict_once(pressure, usage, compensated_target, &entries);
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

    fn collect_evictor_entries(&self) -> Vec<EvictorEntry> {
        {
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
        }
    }

    fn evict_once(
        &self,
        pressure: MemoryPressure,
        usage: MemoryBreakdown,
        target: MemoryBreakdown,
        entries: &[EvictorEntry],
    ) {
        for category in MemoryBreakdown::categories() {
            let category_usage = usage.get(category);
            let category_target = target.get(category);

            if category_usage <= category_target {
                continue;
            }

            // Sum evictable usage for this category.
            let mut evictable_usage = 0u64;
            let mut candidates = Vec::new();
            for (_id, entry_category, usage_counter, evictor) in entries {
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

    fn compensated_target(
        &self,
        usage: MemoryBreakdown,
        target: MemoryBreakdown,
        entries: &[EvictorEntry],
    ) -> MemoryBreakdown {
        // --- Cross-category compensation ---
        //
        // Eviction targets start out as per-category budgets (`target`). That
        // works well when each category's usage is mostly evictable.
        //
        // In practice, some large consumers are *non-evictable* (for example,
        // file texts stored as Salsa inputs). These often get tracked in a
        // category that has few/no evictors (e.g. `Other`).
        //
        // If such a category exceeds its target, `within_targets()` keeps
        // requesting eviction passes, but the per-category loop below will
        // skip it (`evictable_usage == 0`). Without cross-category
        // compensation, evictors in other categories may never run if they're
        // already under their own category targets, even though overall memory
        // pressure is high and *evicting caches is the only remaining lever*.
        //
        // To address this, we compute a global "evictable budget":
        //
        //   global_evictable_budget = target_total - total_non_evictable
        //
        // and, if necessary, scale down per-category evictable allowances so
        // the remaining evictable memory fits within it. This allows
        // non-evictable input memory (like file texts) to drive eviction of
        // evictable caches in other categories.
        {
            let mut evictable_by_category = MemoryBreakdown::default();
            for (_id, category, usage_counter, _evictor) in entries {
                let bytes = usage_counter.load(Ordering::Relaxed);
                let prev = evictable_by_category.get(*category);
                evictable_by_category.set(*category, prev.saturating_add(bytes));
            }

            let mut non_evictable_by_category = MemoryBreakdown::default();
            for category in MemoryBreakdown::categories() {
                let category_usage = usage.get(category);
                let evictable_usage = evictable_by_category.get(category);
                non_evictable_by_category
                    .set(category, category_usage.saturating_sub(evictable_usage));
            }

            let total_non_evictable = non_evictable_by_category.total();
            let global_evictable_budget = target.total().saturating_sub(total_non_evictable);

            // First, compute the per-category "desired" eviction outcome:
            // shrink evictable usage down to its max budget, but do not request
            // growth if currently below budget.
            let mut desired_keep = MemoryBreakdown::default();
            for category in MemoryBreakdown::categories() {
                let evictable_usage = evictable_by_category.get(category);
                let max_keep = target
                    .get(category)
                    .saturating_sub(non_evictable_by_category.get(category));
                desired_keep.set(category, evictable_usage.min(max_keep));
            }

            let total_desired_keep = desired_keep.total();
            let mut adjusted_keep = desired_keep;

            if total_desired_keep > global_evictable_budget && total_desired_keep > 0 {
                for category in MemoryBreakdown::categories() {
                    let desired = desired_keep.get(category);
                    let scaled = ((desired as u128) * (global_evictable_budget as u128)
                        / (total_desired_keep as u128)) as u64;
                    adjusted_keep.set(category, scaled);
                }
            }

            let mut compensated = MemoryBreakdown::default();
            for category in MemoryBreakdown::categories() {
                let non_evictable = non_evictable_by_category.get(category);
                let keep = adjusted_keep.get(category);
                compensated.set(category, non_evictable.saturating_add(keep));
            }

            compensated
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

fn effective_usage_total(tracked_total: u64, rss_bytes: Option<u64>) -> u64 {
    rss_bytes
        .map(|rss| tracked_total.max(rss))
        .unwrap_or(tracked_total)
}

fn category_sort_key(category: MemoryCategory) -> u8 {
    match category {
        MemoryCategory::QueryCache => 0,
        MemoryCategory::SyntaxTrees => 1,
        MemoryCategory::Indexes => 2,
        MemoryCategory::TypeInfo => 3,
        MemoryCategory::Other => 4,
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
