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
        *self.bytes.lock().unwrap() = bytes;
        self.tracker.get().unwrap().set_bytes(bytes);
    }

    fn add_bytes(&self, delta: u64) {
        let mut bytes = self.bytes.lock().unwrap();
        *bytes = bytes.saturating_add(delta);
        self.tracker.get().unwrap().set_bytes(*bytes);
    }

    fn bytes(&self) -> u64 {
        *self.bytes.lock().unwrap()
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
        let mut bytes = self.bytes.lock().unwrap();
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
            events.lock().unwrap().push(event);
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

    let events = events.lock().unwrap();
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
#[cfg(target_os = "linux")]
fn pressure_uses_process_rss_when_higher_than_tracked_usage() {
    let budget = MemoryBudget::from_total(1);
    let manager = MemoryManager::new(budget);

    let report = manager.report();
    assert!(report.rss_bytes.is_some());
    assert_eq!(report.pressure, nova_memory::MemoryPressure::Critical);
}
