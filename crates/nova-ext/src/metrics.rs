use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::Duration;

/// Sink for recording extension-provider runtime metrics.
///
/// This is intentionally decoupled from [`nova_metrics::MetricsRegistry`] so unit tests can assert on
/// metrics deterministically without interacting with the global registry.
pub trait ExtensionMetricsSink: Send + Sync {
    fn record_request(&self, key: &str, duration: Duration);
    fn record_error(&self, key: &str);
    fn record_timeout(&self, key: &str);
    fn record_panic(&self, key: &str);
}

/// Production metrics sink that forwards to [`nova_metrics::MetricsRegistry::global`].
#[derive(Debug, Default)]
pub struct NovaMetricsSink;

impl ExtensionMetricsSink for NovaMetricsSink {
    fn record_request(&self, key: &str, duration: Duration) {
        nova_metrics::MetricsRegistry::global().record_request(key, duration);
    }

    fn record_error(&self, key: &str) {
        nova_metrics::MetricsRegistry::global().record_error(key);
    }

    fn record_timeout(&self, key: &str) {
        nova_metrics::MetricsRegistry::global().record_timeout(key);
    }

    fn record_panic(&self, key: &str) {
        nova_metrics::MetricsRegistry::global().record_panic(key);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestMetricsSnapshot {
    pub request_count: u64,
    pub error_count: u64,
    pub timeout_count: u64,
    pub panic_count: u64,
    pub durations: Vec<Duration>,
}

#[derive(Debug, Default)]
pub struct TestMetricsSink {
    inner: Mutex<HashMap<String, TestMetricsSnapshot>>,
}

impl TestMetricsSink {
    /// Returns a point-in-time snapshot of all recorded metrics.
    pub fn snapshot(&self) -> HashMap<String, TestMetricsSnapshot> {
        self.inner.lock().clone()
    }

    /// Returns a snapshot for `key`, or `None` if `key` was never recorded.
    pub fn snapshot_for(&self, key: &str) -> Option<TestMetricsSnapshot> {
        self.inner.lock().get(key).cloned()
    }

    /// Returns a snapshot for `key`, or an empty one if `key` was never recorded.
    pub fn snapshot_for_or_default(&self, key: &str) -> TestMetricsSnapshot {
        self.inner
            .lock()
            .get(key)
            .cloned()
            .unwrap_or_else(TestMetricsSnapshot::default)
    }
}

impl ExtensionMetricsSink for TestMetricsSink {
    fn record_request(&self, key: &str, duration: Duration) {
        let mut inner = self.inner.lock();
        let entry = inner.entry(key.to_owned()).or_default();
        entry.request_count = entry.request_count.saturating_add(1);
        entry.durations.push(duration);
    }

    fn record_error(&self, key: &str) {
        let mut inner = self.inner.lock();
        let entry = inner.entry(key.to_owned()).or_default();
        entry.error_count = entry.error_count.saturating_add(1);
    }

    fn record_timeout(&self, key: &str) {
        let mut inner = self.inner.lock();
        let entry = inner.entry(key.to_owned()).or_default();
        entry.timeout_count = entry.timeout_count.saturating_add(1);
    }

    fn record_panic(&self, key: &str) {
        let mut inner = self.inner.lock();
        let entry = inner.entry(key.to_owned()).or_default();
        entry.panic_count = entry.panic_count.saturating_add(1);
    }
}
