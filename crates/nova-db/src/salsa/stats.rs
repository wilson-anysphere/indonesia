use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[track_caller]
fn lock_mutex<'a, T>(mutex: &'a Mutex<T>, context: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = std::panic::Location::caller();
            tracing::error!(
                target = "nova.db",
                context,
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "mutex poisoned; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}

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

/// Serializable, stable representation of [`QueryStats`] intended for diagnostics and metrics
/// export.
///
/// Durations are converted into integer milliseconds to avoid embedding Rust's [`Duration`] type
/// (which is not a stable, language-agnostic JSON format).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryStatsReport {
    pub by_query: BTreeMap<String, QueryStatReport>,
    pub cancel_checks: u64,
}

/// Serializable report for a single query's timing statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryStatReport {
    pub executions: u64,
    pub validated_memoized: u64,
    pub blocked_on_other_runtime: u64,
    pub disk_hits: u64,
    pub disk_misses: u64,
    pub total_time_ms: u64,
    pub max_time_ms: u64,
    pub avg_time_ms: u64,
}

fn duration_as_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl QueryStats {
    pub fn to_report(&self) -> QueryStatsReport {
        QueryStatsReport::from(self)
    }
}

impl QueryStatsReport {
    /// Return the `n` slowest queries sorted by `max_time_ms` (descending).
    ///
    /// Ties are broken by query name to keep the result deterministic.
    pub fn top_n_slowest_by_max_time(&self, n: usize) -> Vec<(String, QueryStatReport)> {
        let mut entries: Vec<_> = self.by_query.iter().map(|(k, v)| (k.clone(), *v)).collect();
        entries.sort_by(|(a_name, a), (b_name, b)| {
            b.max_time_ms
                .cmp(&a.max_time_ms)
                .then_with(|| a_name.cmp(b_name))
        });
        entries.truncate(n);
        entries
    }
}

impl From<&QueryStat> for QueryStatReport {
    fn from(stat: &QueryStat) -> Self {
        let total_time_ms = duration_as_millis_u64(stat.total_time);
        let executions = stat.executions;
        Self {
            executions,
            validated_memoized: stat.validated_memoized,
            blocked_on_other_runtime: stat.blocked_on_other_runtime,
            disk_hits: stat.disk_hits,
            disk_misses: stat.disk_misses,
            total_time_ms,
            max_time_ms: duration_as_millis_u64(stat.max_time),
            avg_time_ms: if executions == 0 {
                0
            } else {
                total_time_ms / executions
            },
        }
    }
}

impl From<&QueryStats> for QueryStatsReport {
    fn from(stats: &QueryStats) -> Self {
        Self {
            by_query: stats
                .by_query
                .iter()
                .map(|(name, stat)| (name.clone(), QueryStatReport::from(stat)))
                .collect(),
            cancel_checks: stats.cancel_checks,
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct QueryStatsCollector {
    inner: Arc<Mutex<BTreeMap<String, QueryStat>>>,
    cancel_checks: Arc<AtomicU64>,
}

impl QueryStatsCollector {
    pub(super) fn record_time(&self, query_name: &str, duration: Duration) {
        let mut guard = lock_mutex(&self.inner, "query_stats.record_time");
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
        let guard = lock_mutex(&self.inner, "query_stats.snapshot");
        QueryStats {
            by_query: guard.clone(),
            cancel_checks: self.cancel_checks.load(Ordering::Relaxed),
        }
    }

    pub(super) fn clear(&self) {
        lock_mutex(&self.inner, "query_stats.clear").clear();
        self.cancel_checks.store(0, Ordering::Relaxed);
    }

    fn record_event(&self, query_name: &str, f: impl FnOnce(&mut QueryStat)) {
        let mut guard = lock_mutex(&self.inner, "query_stats.record_event");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_stats_report_converts_durations_and_computes_average() {
        let mut by_query = BTreeMap::new();
        by_query.insert(
            "fast".to_string(),
            QueryStat {
                executions: 4,
                validated_memoized: 3,
                blocked_on_other_runtime: 2,
                disk_hits: 1,
                disk_misses: 0,
                total_time: Duration::from_millis(10),
                max_time: Duration::from_millis(4),
            },
        );

        by_query.insert(
            "never".to_string(),
            QueryStat {
                executions: 0,
                validated_memoized: 0,
                blocked_on_other_runtime: 0,
                disk_hits: 0,
                disk_misses: 0,
                total_time: Duration::from_millis(5),
                max_time: Duration::from_millis(5),
            },
        );

        let stats = QueryStats {
            by_query,
            cancel_checks: 7,
        };

        let report = stats.to_report();
        assert_eq!(report.cancel_checks, 7);

        assert_eq!(
            *report.by_query.get("fast").unwrap(),
            QueryStatReport {
                executions: 4,
                validated_memoized: 3,
                blocked_on_other_runtime: 2,
                disk_hits: 1,
                disk_misses: 0,
                total_time_ms: 10,
                max_time_ms: 4,
                avg_time_ms: 2,
            }
        );

        assert_eq!(
            *report.by_query.get("never").unwrap(),
            QueryStatReport {
                executions: 0,
                validated_memoized: 0,
                blocked_on_other_runtime: 0,
                disk_hits: 0,
                disk_misses: 0,
                total_time_ms: 5,
                max_time_ms: 5,
                avg_time_ms: 0,
            }
        );
    }

    #[test]
    fn query_stats_report_json_roundtrip() {
        let mut by_query = BTreeMap::new();
        by_query.insert(
            "parse".to_string(),
            QueryStat {
                executions: 2,
                validated_memoized: 1,
                blocked_on_other_runtime: 0,
                disk_hits: 3,
                disk_misses: 4,
                total_time: Duration::from_millis(7),
                max_time: Duration::from_millis(4),
            },
        );

        let report = QueryStats {
            by_query,
            cancel_checks: 0,
        }
        .to_report();

        let json = serde_json::to_string(&report).expect("serialize QueryStatsReport");
        let decoded: QueryStatsReport =
            serde_json::from_str(&json).expect("deserialize QueryStatsReport");
        assert_eq!(decoded, report);
    }
}
