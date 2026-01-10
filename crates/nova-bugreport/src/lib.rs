use nova_config::{LogBuffer, NovaConfig};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct BugReportBundle {
    path: PathBuf,
}

impl BugReportBundle {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone)]
pub struct BugReportOptions {
    pub max_log_lines: usize,
    pub reproduction: Option<String>,
}

impl Default for BugReportOptions {
    fn default() -> Self {
        Self {
            max_log_lines: 500,
            reproduction: None,
        }
    }
}

#[derive(Debug)]
pub enum BugReportError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for BugReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BugReportError::Io(err) => write!(f, "io error: {err}"),
            BugReportError::Json(err) => write!(f, "json error: {err}"),
        }
    }
}

impl std::error::Error for BugReportError {}

impl From<std::io::Error> for BugReportError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for BugReportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CrashRecord {
    pub timestamp_unix_ms: u128,
    pub message: String,
    pub location: Option<String>,
    pub backtrace: Option<String>,
}

#[derive(Debug)]
pub struct CrashStore {
    capacity: usize,
    inner: Mutex<VecDeque<CrashRecord>>,
}

impl CrashStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(512))),
        }
    }

    pub fn record(&self, record: CrashRecord) {
        let mut inner = self.inner.lock().expect("CrashStore mutex poisoned");
        if inner.len() == self.capacity {
            inner.pop_front();
        }
        inner.push_back(record);
    }

    pub fn snapshot(&self) -> Vec<CrashRecord> {
        let inner = self.inner.lock().expect("CrashStore mutex poisoned");
        inner.iter().cloned().collect()
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("CrashStore mutex poisoned");
        inner.clear();
    }
}

static GLOBAL_CRASH_STORE: OnceLock<Arc<CrashStore>> = OnceLock::new();

pub fn global_crash_store() -> Arc<CrashStore> {
    GLOBAL_CRASH_STORE
        .get_or_init(|| Arc::new(CrashStore::new(128)))
        .clone()
}

#[derive(Debug, Default)]
pub struct PerfStats {
    request_count: AtomicU64,
    timeout_count: AtomicU64,
    panic_count: AtomicU64,
    safe_mode_entries: AtomicU64,
}

impl PerfStats {
    pub fn record_request(&self) {
        self.request_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_timeout(&self) {
        self.timeout_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_panic(&self) {
        self.panic_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_safe_mode_entry(&self) {
        self.safe_mode_entries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> PerfReport {
        PerfReport {
            request_count: self.request_count.load(Ordering::Relaxed),
            timeout_count: self.timeout_count.load(Ordering::Relaxed),
            panic_count: self.panic_count.load(Ordering::Relaxed),
            safe_mode_entries: self.safe_mode_entries.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PerfReport {
    pub request_count: u64,
    pub timeout_count: u64,
    pub panic_count: u64,
    pub safe_mode_entries: u64,
}

pub struct PanicHookConfig {
    pub include_backtrace: bool,
}

impl Default for PanicHookConfig {
    fn default() -> Self {
        Self {
            include_backtrace: false,
        }
    }
}

type PanicNotifier = Arc<dyn Fn(&str) + Send + Sync + 'static>;

struct PanicHookState {
    include_backtrace: AtomicBool,
    crash_store: Arc<CrashStore>,
    notifiers: Mutex<Vec<PanicNotifier>>,
}

static PANIC_HOOK_STATE: OnceLock<Arc<PanicHookState>> = OnceLock::new();

/// Installs a global panic hook that records crashes, logs details via
/// `tracing`, and notifies any registered clients.
///
/// Safe to call multiple times: subsequent calls register additional notifiers
/// and widen the hook configuration (e.g., enabling backtraces if requested).
pub fn install_panic_hook(config: PanicHookConfig, notifier: PanicNotifier) {
    let state = PANIC_HOOK_STATE
        .get_or_init(|| {
            let crash_store = global_crash_store();

            // Preserve the previous hook (e.g., the default printing hook) while
            // adding structured crash recording.
            let previous = std::panic::take_hook();

            let state = Arc::new(PanicHookState {
                include_backtrace: AtomicBool::new(config.include_backtrace),
                crash_store: crash_store.clone(),
                notifiers: Mutex::new(Vec::new()),
            });

            let state_for_hook = state.clone();
            std::panic::set_hook(Box::new(move |info| {
                previous(info);
                let include_backtrace = state_for_hook.include_backtrace.load(Ordering::Relaxed);

                let message = panic_message(info);
                let location = info.location().map(|loc| format!("{loc}"));
                let backtrace = include_backtrace.then(|| format!("{:?}", std::backtrace::Backtrace::force_capture()));

                tracing::error!(
                    target = "nova.panic",
                    panic.message = %message,
                    panic.location = %location.as_deref().unwrap_or("<unknown>"),
                    "panic captured"
                );

                let record = CrashRecord {
                    timestamp_unix_ms: unix_ms_now(),
                    message: message.clone(),
                    location,
                    backtrace,
                };
                state_for_hook.crash_store.record(record);

                let notification = "Nova hit an internal error. The server will attempt to continue in safe-mode. Run `nova/bugReport` to generate a diagnostic bundle.";
                let notifiers = state_for_hook
                    .notifiers
                    .lock()
                    .expect("PanicHookState notifiers mutex poisoned")
                    .clone();
                for notify in notifiers {
                    notify(notification);
                }
            }));

            state
        })
        .clone();

    if config.include_backtrace {
        state.include_backtrace.store(true, Ordering::Relaxed);
    }

    state
        .notifiers
        .lock()
        .expect("PanicHookState notifiers mutex poisoned")
        .push(notifier);
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "panic payload (non-string)".to_owned()
    }
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_millis()
}

pub fn create_bug_report_bundle(
    config: &NovaConfig,
    log_buffer: &LogBuffer,
    crash_store: &CrashStore,
    perf: &PerfStats,
    options: BugReportOptions,
) -> Result<BugReportBundle, BugReportError> {
    let dir = tempfile::Builder::new()
        .prefix("nova-bugreport-")
        .tempdir()?;
    let path = dir.keep();

    write_json(
        path.join("meta.json"),
        &serde_json::json!({
            "crate_version": env!("CARGO_PKG_VERSION"),
        }),
    )?;

    let sanitized = sanitize_config(config)?;
    write_json(path.join("config.json"), &sanitized)?;

    let logs = log_buffer.last_lines(options.max_log_lines);
    std::fs::write(path.join("logs.txt"), logs.join("\n"))?;

    write_json(path.join("performance.json"), &perf.snapshot())?;
    write_json(path.join("crashes.json"), &crash_store.snapshot())?;

    if let Some(repro) = options.reproduction {
        std::fs::write(path.join("repro.txt"), repro)?;
    }

    Ok(BugReportBundle { path })
}

fn write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<(), BugReportError> {
    let contents = serde_json::to_string_pretty(value)?;
    std::fs::write(path, contents)?;
    Ok(())
}

fn sanitize_config(config: &NovaConfig) -> Result<serde_json::Value, BugReportError> {
    let json = serde_json::to_value(config)?;
    Ok(sanitize_value(json))
}

fn sanitize_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(mut map) => {
            for (key, val) in map.iter_mut() {
                if is_secret_key(key) {
                    *val = serde_json::Value::String("<redacted>".to_owned());
                } else {
                    *val = sanitize_value(std::mem::take(val));
                }
            }
            serde_json::Value::Object(map)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(sanitize_value).collect())
        }
        other => other,
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("authorization")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bug_report_bundle_sanitizes_secrets() {
        let mut config = NovaConfig::default();
        config.ai.enabled = true;
        config.ai.api_key = Some("SUPER-SECRET".to_owned());

        let buffer = LogBuffer::new(10);
        buffer.push_line("hello world".to_owned());

        let crash_store = CrashStore::new(10);
        crash_store.record(CrashRecord {
            timestamp_unix_ms: 0,
            message: "boom".to_owned(),
            location: None,
            backtrace: None,
        });

        let perf = PerfStats::default();
        perf.record_request();

        let bundle = create_bug_report_bundle(
            &config,
            &buffer,
            &crash_store,
            &perf,
            BugReportOptions::default(),
        )
        .expect("bundle creation failed");

        let contents =
            std::fs::read_to_string(bundle.path().join("config.json")).expect("config read failed");
        assert!(!contents.contains("SUPER-SECRET"));
        assert!(contents.contains("<redacted>"));
    }
}
