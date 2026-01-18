use nova_memory::{
    ComponentUsage, EvictionRequest, EvictionResult, MemoryBudget, MemoryCategory, MemoryEvent,
    MemoryEvictor, MemoryManager, MemoryPressureThresholds,
};
use std::sync::{Arc, Mutex, OnceLock};

struct TestEvictor {
    name: String,
    category: MemoryCategory,
    bytes: Mutex<u64>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl TestEvictor {
    fn new(manager: &MemoryManager, name: &str, category: MemoryCategory) -> Arc<Self> {
        let evictor = Arc::new(Self {
            name: name.to_string(),
            category,
            bytes: Mutex::new(0),
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration = manager.register_evictor(name.to_string(), category, evictor.clone());
        evictor
            .tracker
            .set(registration.tracker())
            .unwrap_or_else(|_| panic!("tracker only set once"));
        evictor
            .registration
            .set(registration)
            .unwrap_or_else(|_| panic!("registration only set once"));

        evictor
    }

    fn set_bytes(&self, bytes: u64) {
        *self.bytes.lock().expect("bytes mutex poisoned") = bytes;
        self.tracker.get().unwrap().set_bytes(bytes);
    }

    fn add_bytes(&self, delta: u64) {
        let mut bytes = self.bytes.lock().expect("bytes mutex poisoned");
        *bytes = bytes.saturating_add(delta);
        self.tracker.get().unwrap().set_bytes(*bytes);
    }

    fn bytes(&self) -> u64 {
        *self.bytes.lock().expect("bytes mutex poisoned")
    }
}

impl MemoryEvictor for TestEvictor {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        self.category
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let mut bytes = self.bytes.lock().expect("bytes mutex poisoned");
        let before = *bytes;
        let after = before.min(request.target_bytes);
        *bytes = after;
        self.tracker.get().unwrap().set_bytes(after);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

struct OrderingEvictor {
    name: String,
    category: MemoryCategory,
    bytes: Mutex<u64>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

struct RecordingEvictor {
    name: &'static str,
    category: MemoryCategory,
    bytes: Mutex<u64>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

struct PriorityRecordingEvictor {
    name: &'static str,
    category: MemoryCategory,
    priority: u8,
    bytes: Mutex<u64>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

impl RecordingEvictor {
    fn new(
        manager: &MemoryManager,
        name: &'static str,
        category: MemoryCategory,
        calls: Arc<Mutex<Vec<&'static str>>>,
    ) -> Arc<Self> {
        let evictor = Arc::new(Self {
            name,
            category,
            bytes: Mutex::new(0),
            calls,
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration = manager.register_evictor(name.to_string(), category, evictor.clone());
        evictor
            .tracker
            .set(registration.tracker())
            .unwrap_or_else(|_| panic!("tracker only set once"));
        evictor
            .registration
            .set(registration)
            .unwrap_or_else(|_| panic!("registration only set once"));

        evictor
    }

    fn set_bytes(&self, bytes: u64) {
        *self.bytes.lock().expect("bytes mutex poisoned") = bytes;
        self.tracker.get().unwrap().set_bytes(bytes);
    }

    fn bytes(&self) -> u64 {
        *self.bytes.lock().expect("bytes mutex poisoned")
    }
}

impl PriorityRecordingEvictor {
    fn new(
        manager: &MemoryManager,
        name: &'static str,
        category: MemoryCategory,
        priority: u8,
        calls: Arc<Mutex<Vec<&'static str>>>,
    ) -> Arc<Self> {
        let evictor = Arc::new(Self {
            name,
            category,
            priority,
            bytes: Mutex::new(0),
            calls,
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration = manager.register_evictor(name.to_string(), category, evictor.clone());
        evictor
            .tracker
            .set(registration.tracker())
            .unwrap_or_else(|_| panic!("tracker only set once"));
        evictor
            .registration
            .set(registration)
            .unwrap_or_else(|_| panic!("registration only set once"));

        evictor
    }

    fn set_bytes(&self, bytes: u64) {
        *self.bytes.lock().expect("bytes mutex poisoned") = bytes;
        self.tracker.get().unwrap().set_bytes(bytes);
    }

    fn bytes(&self) -> u64 {
        *self.bytes.lock().expect("bytes mutex poisoned")
    }
}

impl MemoryEvictor for RecordingEvictor {
    fn name(&self) -> &str {
        self.name
    }

    fn category(&self) -> MemoryCategory {
        self.category
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push(self.name);

        let mut bytes = self.bytes.lock().expect("bytes mutex poisoned");
        let before = *bytes;
        let after = before.min(request.target_bytes);
        *bytes = after;
        self.tracker.get().unwrap().set_bytes(after);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

impl MemoryEvictor for PriorityRecordingEvictor {
    fn name(&self) -> &str {
        self.name
    }

    fn category(&self) -> MemoryCategory {
        self.category
    }

    fn eviction_priority(&self) -> u8 {
        self.priority
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push(self.name);

        let mut bytes = self.bytes.lock().expect("bytes mutex poisoned");
        let before = *bytes;
        let after = before.min(request.target_bytes);
        *bytes = after;
        self.tracker.get().unwrap().set_bytes(after);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

impl OrderingEvictor {
    fn new(
        manager: &MemoryManager,
        name: &str,
        category: MemoryCategory,
        calls: Arc<Mutex<Vec<&'static str>>>,
    ) -> Arc<Self> {
        let evictor = Arc::new(Self {
            name: name.to_string(),
            category,
            bytes: Mutex::new(0),
            calls,
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration = manager.register_evictor(name.to_string(), category, evictor.clone());
        evictor
            .tracker
            .set(registration.tracker())
            .unwrap_or_else(|_| panic!("tracker only set once"));
        evictor
            .registration
            .set(registration)
            .unwrap_or_else(|_| panic!("registration only set once"));

        evictor
    }

    fn set_bytes(&self, bytes: u64) {
        *self.bytes.lock().expect("bytes mutex poisoned") = bytes;
        self.tracker.get().unwrap().set_bytes(bytes);
    }
}

impl MemoryEvictor for OrderingEvictor {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> MemoryCategory {
        self.category
    }

    fn flush_to_disk(&self) -> std::io::Result<()> {
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push("flush_to_disk");
        Ok(())
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push("evict");

        let mut bytes = self.bytes.lock().expect("bytes mutex poisoned");
        let before = *bytes;
        let after = before.min(request.target_bytes);
        *bytes = after;
        self.tracker.get().unwrap().set_bytes(after);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

#[test]
fn evicts_over_category_budget_even_under_low_pressure() {
    let budget = MemoryBudget::from_total(1_000_000_000_000);
    let manager = MemoryManager::new(budget);

    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    cache.set_bytes(budget.categories.query_cache + 123);

    assert_eq!(manager.pressure(), nova_memory::MemoryPressure::Low);

    let report = manager.enforce();
    assert_eq!(cache.bytes(), budget.categories.query_cache);
    assert_eq!(report.pressure, nova_memory::MemoryPressure::Low);
    assert!(!report.degraded.skip_expensive_diagnostics);
}

#[test]
fn pressure_event_and_degraded_mode_when_non_evictable_memory_dominates() {
    let budget = MemoryBudget::from_total(1_000_000_000_000);
    let manager = MemoryManager::new(budget);

    let events: Arc<Mutex<Vec<MemoryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    manager.subscribe({
        let events = events.clone();
        Arc::new(move |event: MemoryEvent| {
            events.lock().expect("events mutex poisoned").push(event);
        })
    });

    let other = manager.register_tracker("other", MemoryCategory::Other);
    other.tracker().set_bytes(budget.total * 90 / 100);

    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    cache.set_bytes(budget.total * 20 / 100);

    let report = manager.enforce();

    // Critical pressure forces aggressive eviction of evictable components.
    assert_eq!(cache.bytes(), 0);

    // Non-evictable memory keeps us in a degraded mode.
    assert_eq!(report.pressure, nova_memory::MemoryPressure::High);
    assert!(report.degraded.skip_expensive_diagnostics);

    let events = events.lock().expect("events mutex poisoned");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].previous_pressure,
        nova_memory::MemoryPressure::Low
    );
    assert_eq!(events[0].pressure, report.pressure);
    assert_eq!(events[0].report, report);
}

#[test]
fn non_evictable_other_can_force_query_cache_eviction_under_high_pressure() {
    // Regression test:
    // A large non-evictable consumer (like file texts stored as Salsa inputs)
    // may be tracked under `Other` and have no evictors. Under high total
    // pressure, we still want evictable caches (QueryCache) to shrink, even if
    // they're below their own per-category target.
    //
    // Use a small synthetic budget and bump the "Critical" threshold very high
    // so process RSS cannot force `Critical` pressure on Linux. The regression
    // is about *tracked* non-evictable memory (file texts), not RSS noise.
    let budget = MemoryBudget::from_total(1_000);
    let thresholds = MemoryPressureThresholds {
        critical: 1e12,
        ..MemoryPressureThresholds::default()
    };
    let manager = MemoryManager::with_thresholds(budget, thresholds);

    // Simulate file text memory (non-evictable) dominating `Other`.
    let file_texts = manager.register_tracker("file_texts", MemoryCategory::Other);
    file_texts.tracker().set_bytes(budget.total * 90 / 100);

    // An evictable cache in QueryCache that is still under its category target
    // at High pressure (targets are scaled to 50%).
    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    let cache_bytes = budget.total * 4 / 100; // total tracked usage = 94% => High (not Critical)
    cache.set_bytes(cache_bytes);

    let high_target = ((budget.categories.query_cache as f64) * 0.50).round() as u64;
    assert!(
        cache_bytes < high_target,
        "cache should start under its per-category target to reproduce the regression"
    );

    assert_eq!(manager.report().pressure, nova_memory::MemoryPressure::High);

    manager.enforce();

    assert_eq!(
        cache.bytes(),
        0,
        "non-evictable memory in `Other` should drive eviction of query caches"
    );
}

#[test]
fn non_evictable_other_can_scale_down_query_cache_even_if_under_category_target() {
    // Similar to `non_evictable_other_can_force_query_cache_eviction_under_high_pressure`, but
    // exercises the non-zero-budget path:
    //
    // Even if an evictable cache is under its *category* target, it may still need to shrink when
    // a large non-evictable category (with no evictors) consumes most of the *global* budget.
    let budget = MemoryBudget::from_total(1_000);
    let thresholds = MemoryPressureThresholds {
        // Make `High` easy to reach deterministically (RSS-independent), but keep `Critical`
        // unreachable so the test is stable on Linux where RSS can dominate.
        medium: 0.0,
        high: 0.5,
        critical: 1e12,
    };
    let manager = MemoryManager::with_thresholds(budget, thresholds);

    let file_texts = manager.register_tracker("file_texts", MemoryCategory::Other);
    file_texts.tracker().set_bytes(450);

    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    cache.set_bytes(150);

    // Under `High`, category targets are scaled to 50%.
    let high_target = ((budget.categories.query_cache as f64) * 0.50).round() as u64;
    assert!(
        cache.bytes() < high_target,
        "cache should start under its per-category target to reproduce the regression"
    );

    assert_eq!(manager.report().pressure, nova_memory::MemoryPressure::High);

    manager.enforce();

    // Global High-pressure target total is 50% of the budget (500). With 450 bytes of non-evictable
    // inputs, the global evictable budget is 50 bytes, so the cache must shrink accordingly.
    assert_eq!(cache.bytes(), 50);
}

#[test]
fn medium_pressure_scales_targets() {
    let budget = MemoryBudget::from_total(1_000_000_000_000);
    let manager = MemoryManager::new(budget);

    let query = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    let trees = TestEvictor::new(&manager, "syntax_trees", MemoryCategory::SyntaxTrees);
    query.set_bytes(budget.total * 60 / 100);
    trees.set_bytes(budget.total * 20 / 100);

    // 80% usage is medium pressure for default thresholds.
    let report = manager.enforce();

    let expected_query = ((budget.categories.query_cache as f64) * 0.70).round() as u64;
    let expected_trees = ((budget.categories.syntax_trees as f64) * 0.70).round() as u64;
    assert_eq!(query.bytes(), expected_query);
    assert_eq!(trees.bytes(), expected_trees);
    assert_eq!(report.pressure, nova_memory::MemoryPressure::Low);
}

#[test]
fn synthetic_growth_is_bounded_by_budget() {
    let budget = MemoryBudget::from_total(1_000_000_000_000);
    let manager = MemoryManager::new(budget);

    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);

    for _ in 0..50 {
        cache.add_bytes(budget.categories.query_cache / 2);
        manager.enforce();
        assert!(cache.bytes() <= budget.categories.query_cache);
    }
}

#[test]
fn report_detailed_is_sorted_by_bytes_desc() {
    let budget = MemoryBudget::from_total(1_000_000_000_000);
    let manager = MemoryManager::new(budget);

    let a = manager.register_tracker("a", MemoryCategory::Other);
    let b = manager.register_tracker("b", MemoryCategory::QueryCache);
    let c = manager.register_tracker("c", MemoryCategory::Indexes);

    a.tracker().set_bytes(10);
    b.tracker().set_bytes(30);
    c.tracker().set_bytes(20);

    let (_report, components) = manager.report_detailed();
    assert_eq!(
        components,
        vec![
            ComponentUsage {
                name: "b".to_string(),
                category: MemoryCategory::QueryCache,
                bytes: 30,
            },
            ComponentUsage {
                name: "c".to_string(),
                category: MemoryCategory::Indexes,
                bytes: 20,
            },
            ComponentUsage {
                name: "a".to_string(),
                category: MemoryCategory::Other,
                bytes: 10,
            },
        ]
    );
}

#[test]
fn stops_evicting_within_category_once_under_target() {
    // Regression test: when multiple evictors exist in the same category, once we have
    // reduced total category usage under its target we should stop calling additional
    // evictors. This avoids over-evicting (e.g. rebuilding Salsa memo tables) when a cheaper
    // evictor already freed enough memory.
    let budget = MemoryBudget::from_total(1_000);
    // Keep pressure deterministically Low even if process RSS dwarfs the synthetic budget.
    let thresholds = MemoryPressureThresholds {
        medium: 1e12,
        high: 1e12,
        critical: 1e12,
    };
    let manager = MemoryManager::with_thresholds(budget, thresholds);

    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let a = RecordingEvictor::new(&manager, "a", MemoryCategory::QueryCache, calls.clone());
    let b = RecordingEvictor::new(&manager, "b", MemoryCategory::QueryCache, calls.clone());

    // Slightly exceed the query_cache category budget; evicting `a` alone should be enough.
    a.set_bytes(budget.categories.query_cache + 1);
    b.set_bytes(1);

    manager.enforce();

    assert!(
        a.bytes() <= budget.categories.query_cache,
        "expected primary evictor to shrink under budget"
    );
    assert_eq!(
        b.bytes(),
        1,
        "expected secondary evictor to not be called once category is within target"
    );
    let calls = calls.lock().expect("calls mutex poisoned");
    assert_eq!(
        calls.as_slice(),
        ["a"],
        "expected only the first evictor to be invoked"
    );
}

#[test]
fn evicts_lower_priority_first_even_if_smaller() {
    // Regression test:
    // Some evictors are very expensive (or high UX impact) and should be treated
    // as a last resort, even when they currently report the largest tracked
    // usage (e.g. rebuilding Salsa memo tables).
    //
    // Ensure we consult `MemoryEvictor::eviction_priority()` before falling back
    // to "largest first" ordering.
    let budget = MemoryBudget::from_total(1_495); // query_cache budget = 598
                                                  // Keep pressure deterministically Low even if process RSS dwarfs the synthetic budget.
    let thresholds = MemoryPressureThresholds {
        medium: 1e12,
        high: 1e12,
        critical: 1e12,
    };
    let manager = MemoryManager::with_thresholds(budget, thresholds);

    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    // `expensive` is slightly larger; without priority ordering it would be evicted first.
    let cheap = PriorityRecordingEvictor::new(
        &manager,
        "cheap",
        MemoryCategory::QueryCache,
        0,
        calls.clone(),
    );
    let expensive = PriorityRecordingEvictor::new(
        &manager,
        "expensive",
        MemoryCategory::QueryCache,
        10,
        calls.clone(),
    );

    // Exceed the category target by 1 byte: evicting `cheap` alone is enough to
    // restore the category under budget.
    expensive.set_bytes(300);
    cheap.set_bytes(299);

    manager.enforce();

    let calls = calls.lock().expect("calls mutex poisoned");
    assert_eq!(
        calls.as_slice(),
        ["cheap"],
        "expected low-priority evictor to run first and satisfy the target without invoking the expensive evictor"
    );
    assert_eq!(expensive.bytes(), 300);
    assert_eq!(cheap.bytes(), 298);
}

#[test]
fn enforce_flushes_to_disk_before_evicting_under_high_and_critical_pressure() {
    fn run(thresholds: MemoryPressureThresholds, budget_total: u64, bytes: u64) {
        let budget = MemoryBudget::from_total(budget_total);
        let manager = MemoryManager::with_thresholds(budget, thresholds);

        let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let evictor = OrderingEvictor::new(
            &manager,
            "ordering_evictor",
            MemoryCategory::QueryCache,
            calls.clone(),
        );
        evictor.set_bytes(bytes);

        manager.enforce();

        let calls = calls.lock().expect("calls mutex poisoned");
        let flush_pos = calls
            .iter()
            .position(|entry| *entry == "flush_to_disk")
            .expect("expected flush_to_disk to be called");
        let evict_pos = calls
            .iter()
            .position(|entry| *entry == "evict")
            .expect("expected evict to be called");
        assert!(
            flush_pos < evict_pos,
            "expected flush_to_disk before evict, got calls={calls:?}"
        );
    }

    // Force `High` without accidentally hitting `Critical` due to process RSS by making the
    // critical threshold unreachable.
    run(
        MemoryPressureThresholds {
            medium: 0.0,
            high: 0.0,
            critical: 1e18,
        },
        100,
        1000,
    );

    // Default thresholds with a tiny budget reliably produce `Critical` pressure.
    run(MemoryPressureThresholds::default(), 1, 1);
}

#[test]
#[cfg(target_os = "linux")]
fn pressure_uses_process_rss_when_higher_than_tracked_usage() {
    let budget = MemoryBudget::from_total(1);
    let manager = MemoryManager::new(budget);

    let report = manager.report();
    assert!(report.rss_bytes.is_some());
    assert_eq!(report.pressure, nova_memory::MemoryPressure::Critical);
}
