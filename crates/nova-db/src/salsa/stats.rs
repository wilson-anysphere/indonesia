use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Database functionality needed by query implementations to record timing stats.
pub trait HasQueryStats {
    fn record_query_stat(&self, query_name: &'static str, duration: Duration);
}

/// Lightweight query timing/execution stats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub by_query: BTreeMap<String, QueryStat>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryStat {
    pub executions: u64,
    pub total_time: Duration,
    pub max_time: Duration,
}

#[derive(Clone, Default)]
pub(super) struct QueryStatsCollector {
    inner: Arc<Mutex<BTreeMap<String, QueryStat>>>,
}

impl QueryStatsCollector {
    pub(super) fn record(&self, key: String, duration: Duration) {
        let mut guard = self.inner.lock().expect("query stats mutex poisoned");
        let entry = guard.entry(key).or_default();
        entry.executions = entry.executions.saturating_add(1);
        entry.total_time = entry.total_time.saturating_add(duration);
        entry.max_time = entry.max_time.max(duration);
    }

    pub(super) fn snapshot(&self) -> QueryStats {
        let guard = self.inner.lock().expect("query stats mutex poisoned");
        QueryStats {
            by_query: guard.clone(),
        }
    }

    pub(super) fn clear(&self) {
        self.inner
            .lock()
            .expect("query stats mutex poisoned")
            .clear();
    }
}
