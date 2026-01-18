use hdrhistogram::Histogram;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::Duration;

// We record latencies in microseconds.
const LATENCY_SIGFIG: u8 = 3;
// 10 minutes should cover even the slowest Nova operations without making per-method histograms
// too large. Values above this are clamped.
const MAX_LATENCY_US: u64 = 10 * 60 * 1_000_000;

/// Thread-safe runtime metrics registry (counters + per-method latency histograms).
///
/// The registry is designed for low overhead: recording a metric is a single mutex acquisition and
/// no allocations on the hot path after a method is first seen.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    methods: HashMap<String, MethodMetrics>,
}

#[derive(Debug)]
struct MethodMetrics {
    request_count: u64,
    error_count: u64,
    timeout_count: u64,
    panic_count: u64,
    latency_us: Histogram<u64>,
}

impl MethodMetrics {
    fn new() -> Self {
        static HISTOGRAM_BOUNDS_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        let latency_us = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, LATENCY_SIGFIG)
            .unwrap_or_else(|err| {
                if HISTOGRAM_BOUNDS_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.metrics",
                        error = %err,
                        "failed to construct bounded latency histogram; falling back to unbounded histogram"
                    );
                }
                // Safety net: hdrhistogram only errors for invalid bounds/precision. Fall back to the
                // default constructor which should always succeed.
                Histogram::<u64>::new(LATENCY_SIGFIG).expect("histogram")
            });
        Self {
            request_count: 0,
            error_count: 0,
            timeout_count: 0,
            panic_count: 0,
            latency_us,
        }
    }
}

impl MetricsRegistry {
    /// Returns the global metrics registry.
    pub fn global() -> &'static MetricsRegistry {
        static GLOBAL: OnceLock<MetricsRegistry> = OnceLock::new();
        GLOBAL.get_or_init(MetricsRegistry::default)
    }

    /// Record a completed request/notification for `method`, including total handling latency.
    pub fn record_request(&self, method: &str, duration: Duration) {
        static HISTOGRAM_RECORD_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        let micros = duration.as_micros().min(u128::from(MAX_LATENCY_US)) as u64;
        let micros = micros.max(1);

        let mut inner = self.inner.lock();
        let metrics = inner
            .methods
            .entry(method.to_owned())
            .or_insert_with(MethodMetrics::new);
        metrics.request_count = metrics.request_count.saturating_add(1);
        if let Err(err) = metrics.latency_us.record(micros) {
            if HISTOGRAM_RECORD_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.metrics",
                    method,
                    micros,
                    error = %err,
                    "failed to record latency sample"
                );
            }
        }
    }

    /// Record a request/notification that completed with an error response.
    pub fn record_error(&self, method: &str) {
        let mut inner = self.inner.lock();
        let metrics = inner
            .methods
            .entry(method.to_owned())
            .or_insert_with(MethodMetrics::new);
        metrics.error_count = metrics.error_count.saturating_add(1);
    }

    /// Record a request/notification that exceeded its watchdog / timeout budget.
    pub fn record_timeout(&self, method: &str) {
        let mut inner = self.inner.lock();
        let metrics = inner
            .methods
            .entry(method.to_owned())
            .or_insert_with(MethodMetrics::new);
        metrics.timeout_count = metrics.timeout_count.saturating_add(1);
    }

    /// Record a request/notification that panicked.
    pub fn record_panic(&self, method: &str) {
        let mut inner = self.inner.lock();
        let metrics = inner
            .methods
            .entry(method.to_owned())
            .or_insert_with(MethodMetrics::new);
        metrics.panic_count = metrics.panic_count.saturating_add(1);
    }

    /// Reset all recorded metrics.
    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        inner.methods.clear();
    }

    /// Create a snapshot of all recorded metrics suitable for debug export.
    pub fn snapshot(&self) -> MetricsSnapshot {
        static TOTAL_HISTOGRAM_BOUNDS_ERROR_LOGGED: OnceLock<()> = OnceLock::new();
        static TOTAL_HISTOGRAM_ADD_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        let inner = self.inner.lock();

        let mut methods = BTreeMap::new();
        let mut total_requests = 0u64;
        let mut total_errors = 0u64;
        let mut total_timeouts = 0u64;
        let mut total_panics = 0u64;

        let mut total_hist = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, LATENCY_SIGFIG)
            .unwrap_or_else(|err| {
                if TOTAL_HISTOGRAM_BOUNDS_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.metrics",
                        error = %err,
                        "failed to construct bounded total latency histogram; falling back to unbounded histogram"
                    );
                }
                Histogram::<u64>::new(LATENCY_SIGFIG).expect("histogram")
            });

        for (method, metrics) in inner.methods.iter() {
            total_requests = total_requests.saturating_add(metrics.request_count);
            total_errors = total_errors.saturating_add(metrics.error_count);
            total_timeouts = total_timeouts.saturating_add(metrics.timeout_count);
            total_panics = total_panics.saturating_add(metrics.panic_count);

            if let Err(err) = total_hist.add(&metrics.latency_us) {
                if TOTAL_HISTOGRAM_ADD_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.metrics",
                        method,
                        error = %err,
                        "failed to merge per-method latency histogram into totals"
                    );
                }
            }

            methods.insert(
                method.clone(),
                MethodMetricsSnapshot {
                    request_count: metrics.request_count,
                    error_count: metrics.error_count,
                    timeout_count: metrics.timeout_count,
                    panic_count: metrics.panic_count,
                    latency_us: latency_summary(&metrics.latency_us),
                },
            );
        }

        MetricsSnapshot {
            totals: MethodMetricsSnapshot {
                request_count: total_requests,
                error_count: total_errors,
                timeout_count: total_timeouts,
                panic_count: total_panics,
                latency_us: latency_summary(&total_hist),
            },
            methods,
        }
    }
}

fn latency_summary(hist: &Histogram<u64>) -> LatencySummary {
    if hist.is_empty() {
        return LatencySummary {
            p50_us: 0,
            p95_us: 0,
            max_us: 0,
        };
    }

    LatencySummary {
        p50_us: hist.value_at_quantile(0.50),
        p95_us: hist.value_at_quantile(0.95),
        max_us: hist.max(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSnapshot {
    pub totals: MethodMetricsSnapshot,
    pub methods: BTreeMap<String, MethodMetricsSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodMetricsSnapshot {
    pub request_count: u64,
    pub error_count: u64,
    pub timeout_count: u64,
    pub panic_count: u64,
    pub latency_us: LatencySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencySummary {
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn histogram_percentiles_are_reasonable() {
        let registry = MetricsRegistry::default();
        for i in 1u64..=100 {
            registry.record_request("m", Duration::from_micros(i));
        }

        let snap = registry.snapshot();
        let m = snap.methods.get("m").expect("method present");
        assert_eq!(m.request_count, 100);
        assert_eq!(m.latency_us.p50_us, 50);
        assert_eq!(m.latency_us.p95_us, 95);
        assert_eq!(m.latency_us.max_us, 100);
        assert_eq!(snap.totals.request_count, 100);
    }

    #[test]
    fn registry_is_thread_safe() {
        let registry = Arc::new(MetricsRegistry::default());
        let threads = 8;
        let iters = 10_000;

        let mut handles = Vec::new();
        for _ in 0..threads {
            let registry = registry.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..iters {
                    registry.record_request("m", Duration::from_micros(10));
                }
            }));
        }

        for handle in handles {
            handle.join().expect("thread join");
        }

        let snap = registry.snapshot();
        assert_eq!(snap.totals.request_count, (threads * iters) as u64);
        let m = snap.methods.get("m").expect("method present");
        assert_eq!(m.request_count, (threads * iters) as u64);
    }
}
