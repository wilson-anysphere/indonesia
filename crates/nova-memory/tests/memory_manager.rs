use nova_memory::{
    EvictionRequest, EvictionResult, MemoryBudget, MemoryCategory, MemoryEvent, MemoryEvictor,
    MemoryManager,
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
    let budget = MemoryBudget::from_total(1_000);
    let manager = MemoryManager::new(budget);

    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    cache.set_bytes(500);

    assert_eq!(manager.pressure(), nova_memory::MemoryPressure::Low);

    let report = manager.enforce();
    assert_eq!(cache.bytes(), budget.categories.query_cache);
    assert_eq!(report.pressure, nova_memory::MemoryPressure::Low);
    assert!(!report.degraded.skip_expensive_diagnostics);
}

#[test]
fn pressure_event_and_degraded_mode_when_non_evictable_memory_dominates() {
    let budget = MemoryBudget::from_total(1_000);
    let manager = MemoryManager::new(budget);

    let events: Arc<Mutex<Vec<MemoryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    manager.subscribe({
        let events = events.clone();
        Arc::new(move |event: MemoryEvent| {
            events.lock().unwrap().push(event);
        })
    });

    let other = manager.register_tracker("other", MemoryCategory::Other);
    other.tracker().set_bytes(900);

    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    cache.set_bytes(200);

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
fn medium_pressure_scales_targets() {
    let budget = MemoryBudget::from_total(1_000);
    let manager = MemoryManager::new(budget);

    let query = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);
    let trees = TestEvictor::new(&manager, "syntax_trees", MemoryCategory::SyntaxTrees);
    query.set_bytes(600);
    trees.set_bytes(200);

    // 800/1000 is medium pressure for default thresholds.
    let report = manager.enforce();

    assert_eq!(query.bytes(), 280);
    assert_eq!(trees.bytes(), 175);
    assert_eq!(report.pressure, nova_memory::MemoryPressure::Low);
}

#[test]
fn synthetic_growth_is_bounded_by_budget() {
    let budget = MemoryBudget::from_total(1_000);
    let manager = MemoryManager::new(budget);

    let cache = TestEvictor::new(&manager, "query_cache", MemoryCategory::QueryCache);

    for _ in 0..50 {
        cache.add_bytes(50);
        manager.enforce();
        assert!(cache.bytes() <= budget.categories.query_cache);
    }
}
