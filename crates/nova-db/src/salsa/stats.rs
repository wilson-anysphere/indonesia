use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Database functionality needed by query implementations to record timing stats.
pub trait HasQueryStats {
    fn record_query_stat(&self, query_name: &'static str, duration: Duration);

    /// Optional hook for cache-aware queries to report persisted-cache hits.
    #[inline]
    fn record_disk_cache_hit(&self, _query_name: &'static str) {}

    /// Optional hook for cache-aware queries to report persisted-cache misses.
    #[inline]
    fn record_disk_cache_miss(&self, _query_name: &'static str) {}
}

/// Lightweight query timing/execution stats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub by_query: BTreeMap<String, QueryStat>,
    pub cancel_checks: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryStat {
    pub executions: u64,
    pub validated_memoized: u64,
    pub blocked_on_other_runtime: u64,
    pub disk_hits: u64,
    pub disk_misses: u64,
    pub total_time: Duration,
    pub max_time: Duration,
}

#[derive(Clone, Default)]
pub(super) struct QueryStatsCollector {
    inner: Arc<Mutex<BTreeMap<String, QueryStat>>>,
    cancel_checks: Arc<AtomicU64>,
}

impl QueryStatsCollector {
    pub(super) fn record_time(&self, query_name: &str, duration: Duration) {
        let mut guard = self.inner.lock().expect("query stats mutex poisoned");
        let entry = if let Some(entry) = guard.get_mut(query_name) {
            entry
        } else {
            guard.insert(query_name.to_string(), QueryStat::default());
            guard
                .get_mut(query_name)
                .expect("query stat entry just inserted")
        };
        entry.total_time = entry.total_time.saturating_add(duration);
        entry.max_time = entry.max_time.max(duration);
    }

    pub(super) fn record_execution(&self, query_name: &str) {
        self.record_event(query_name, |entry| {
            entry.executions = entry.executions.saturating_add(1);
        });
    }

    pub(super) fn record_validated_memoized(&self, query_name: &str) {
        self.record_event(query_name, |entry| {
            entry.validated_memoized = entry.validated_memoized.saturating_add(1);
        });
    }

    pub(super) fn record_blocked_on_other_runtime(&self, query_name: &str) {
        self.record_event(query_name, |entry| {
            entry.blocked_on_other_runtime = entry.blocked_on_other_runtime.saturating_add(1);
        });
    }

    pub(super) fn record_disk_hit(&self, query_name: &str) {
        self.record_event(query_name, |entry| {
            entry.disk_hits = entry.disk_hits.saturating_add(1);
        });
    }

    pub(super) fn record_disk_miss(&self, query_name: &str) {
        self.record_event(query_name, |entry| {
            entry.disk_misses = entry.disk_misses.saturating_add(1);
        });
    }

    pub(super) fn record_cancel_check(&self) {
        self.cancel_checks.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> QueryStats {
        let guard = self.inner.lock().expect("query stats mutex poisoned");
        QueryStats {
            by_query: guard.clone(),
            cancel_checks: self.cancel_checks.load(Ordering::Relaxed),
        }
    }

    pub(super) fn clear(&self) {
        self.inner
            .lock()
            .expect("query stats mutex poisoned")
            .clear();
        self.cancel_checks.store(0, Ordering::Relaxed);
    }

    fn record_event(&self, query_name: &str, f: impl FnOnce(&mut QueryStat)) {
        let mut guard = self.inner.lock().expect("query stats mutex poisoned");
        let entry = if let Some(entry) = guard.get_mut(query_name) {
            entry
        } else {
            guard.insert(query_name.to_string(), QueryStat::default());
            guard
                .get_mut(query_name)
                .expect("query stat entry just inserted")
        };
        f(entry);
    }
}
