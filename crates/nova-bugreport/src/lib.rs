mod redact;

use nova_config::{LogBuffer, NovaConfig};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct BugReportBundle {
    directory: PathBuf,
    archive: Option<PathBuf>,
}

impl BugReportBundle {
    pub fn path(&self) -> &Path {
        &self.directory
    }

    pub fn archive_path(&self) -> Option<&Path> {
        self.archive.as_deref()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.len() == self.capacity {
            inner.pop_front();
        }
        inner.push_back(record);
    }

    pub fn snapshot(&self) -> Vec<CrashRecord> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.iter().cloned().collect()
    }

    pub fn clear(&self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.clear();
    }

    /// Best-effort: load crash records from a JSONL file and append them to the
    /// in-memory ring buffer.
    pub fn load_from_file(&self, path: impl AsRef<Path>) {
        let records = read_persisted_crashes(path.as_ref(), self.capacity);
        for record in records {
            self.record(record);
        }
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
            safe_mode_active: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PerfReport {
    pub request_count: u64,
    pub timeout_count: u64,
    pub panic_count: u64,
    pub safe_mode_entries: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_mode_active: Option<bool>,
}

pub struct PanicHookConfig {
    pub include_backtrace: bool,
    /// Path to append panic crash records (JSONL).
    ///
    /// When unset, panics are still recorded in-memory but are not persisted to
    /// disk.
    pub persisted_crash_log_path: Option<PathBuf>,
}

impl Default for PanicHookConfig {
    fn default() -> Self {
        Self {
            include_backtrace: false,
            persisted_crash_log_path: Some(default_crash_log_path()),
        }
    }
}

type PanicNotifier = Arc<dyn Fn(&str) + Send + Sync + 'static>;

struct PanicHookState {
    include_backtrace: AtomicBool,
    crash_store: Arc<CrashStore>,
    persisted_crash_log_path: Mutex<Option<PathBuf>>,
    notifiers: Mutex<Vec<PanicNotifier>>,
}

static PANIC_HOOK_STATE: OnceLock<Arc<PanicHookState>> = OnceLock::new();
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Installs a global panic hook that records crashes, logs details via
/// `tracing`, and notifies any registered clients.
///
/// Safe to call multiple times: subsequent calls register additional notifiers
/// and widen the hook configuration (e.g., enabling backtraces if requested).
pub fn install_panic_hook(config: PanicHookConfig, notifier: PanicNotifier) {
    let _ = PROCESS_START.get_or_init(Instant::now);
    let state = PANIC_HOOK_STATE
        .get_or_init(|| {
            let crash_store = global_crash_store();

            // Preserve the previous hook (e.g., the default printing hook) while
            // adding structured crash recording.
            let previous = std::panic::take_hook();

            let state = Arc::new(PanicHookState {
                include_backtrace: AtomicBool::new(config.include_backtrace),
                crash_store: crash_store.clone(),
                persisted_crash_log_path: Mutex::new(config.persisted_crash_log_path.clone()),
                notifiers: Mutex::new(Vec::new()),
            });

            let state_for_hook = state.clone();
            std::panic::set_hook(Box::new(move |info| {
                previous(info);
                let include_backtrace = state_for_hook.include_backtrace.load(Ordering::Relaxed);

                let timestamp_unix_ms = unix_ms_now();
                let message = redact::redact_string(&panic_message(info));
                let location = info.location().map(|loc| format!("{loc}"));
                let backtrace = include_backtrace
                    .then(|| format!("{:?}", std::backtrace::Backtrace::force_capture()))
                    .map(|bt| redact::redact_string(&bt));

                tracing::error!(
                    target = "nova.panic",
                    panic.message = %message,
                    panic.location = %location.as_deref().unwrap_or("<unknown>"),
                    "panic captured"
                );

                let record = CrashRecord {
                    timestamp_unix_ms,
                    message,
                    location,
                    backtrace,
                };
                state_for_hook.crash_store.record(record.clone());

                let persisted_path = state_for_hook
                    .persisted_crash_log_path
                    .lock()
                    .ok()
                    .and_then(|p| p.clone());
                if let Some(path) = persisted_path.as_ref() {
                    let _ = append_crash_record(path, &record);
                }

                let notification = "Nova hit an internal error. The server will attempt to continue in safe-mode. Run `nova/bugReport` to generate a diagnostic bundle.";
                let notifiers = state_for_hook.notifiers.lock().map(|n| n.clone()).unwrap_or_default();
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

    if let Ok(mut path) = state.persisted_crash_log_path.lock() {
        *path = config.persisted_crash_log_path;
    }

    state
        .notifiers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// Default path for the persistent crash log (JSONL).
///
/// This is best-effort and platform-aware; if Nova can't determine a suitable
/// per-user state directory, it falls back to the system temp dir.
pub fn default_crash_log_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .unwrap_or_else(std::env::temp_dir);
        base.join("nova").join("crashes.jsonl")
    }

    #[cfg(target_os = "macos")]
    {
        let base = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Logs"))
            .unwrap_or_else(std::env::temp_dir);
        base.join("nova").join("crashes.jsonl")
    }

    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("Nova").join("crashes.jsonl")
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        std::env::temp_dir().join("nova").join("crashes.jsonl")
    }
}

fn append_crash_record(path: &Path, record: &CrashRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

fn read_persisted_crashes(path: &Path, max_records: usize) -> Vec<CrashRecord> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };

    let mut ring = VecDeque::with_capacity(max_records.min(512));
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }

        let Ok(mut record) = serde_json::from_str::<CrashRecord>(&line) else {
            continue;
        };
        sanitize_crash_record(&mut record);

        if ring.len() == max_records {
            ring.pop_front();
        }
        ring.push_back(record);
    }

    ring.into_iter().collect()
}

fn sanitize_crash_record(record: &mut CrashRecord) {
    record.message = redact::redact_string(&record.message);
    if let Some(bt) = record.backtrace.as_mut() {
        *bt = redact::redact_string(bt);
    }
}

const BUGREPORT_BUNDLE_VERSION: u32 = 2;
const PERSISTED_CRASH_LIMIT: usize = 128;

#[derive(Debug, Serialize)]
struct MetaReport {
    /// Bundle schema version (not the Nova version).
    bundle_version: u32,
    /// Version of the Nova workspace (shared across core crates).
    nova_version: &'static str,
    /// Version of the `nova-bugreport` crate.
    nova_bugreport_version: &'static str,
    timestamp_utc: String,
    timestamp_unix_ms: u128,
    target_triple: &'static str,
    os: &'static str,
    arch: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_sha: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct SystemReport {
    cpu_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rss_bytes: Option<u64>,
    uptime_ms: u64,
}

#[derive(Debug, Serialize)]
struct CrashReport {
    in_memory: Vec<CrashRecord>,
    persisted: Vec<CrashRecord>,
}

type ExtraAttachmentsCallback<'a> = dyn Fn(&Path) -> Result<(), BugReportError> + Send + Sync + 'a;

pub struct BugReportBuilder<'a> {
    config: &'a NovaConfig,
    log_buffer: &'a LogBuffer,
    crash_store: &'a CrashStore,
    perf: &'a PerfStats,
    options: BugReportOptions,
    persisted_crash_log_path: Option<PathBuf>,
    create_archive: bool,
    safe_mode_active: Option<bool>,
    extra_attachments: Option<Box<ExtraAttachmentsCallback<'a>>>,
}

impl<'a> BugReportBuilder<'a> {
    pub fn new(
        config: &'a NovaConfig,
        log_buffer: &'a LogBuffer,
        crash_store: &'a CrashStore,
        perf: &'a PerfStats,
    ) -> Self {
        Self {
            config,
            log_buffer,
            crash_store,
            perf,
            options: BugReportOptions::default(),
            persisted_crash_log_path: Some(default_crash_log_path()),
            create_archive: true,
            safe_mode_active: None,
            extra_attachments: None,
        }
    }

    pub fn options(mut self, options: BugReportOptions) -> Self {
        self.options = options;
        self
    }

    pub fn persisted_crash_log_path(mut self, path: Option<PathBuf>) -> Self {
        self.persisted_crash_log_path = path;
        self
    }

    pub fn create_archive(mut self, create: bool) -> Self {
        self.create_archive = create;
        self
    }

    pub fn safe_mode_active(mut self, active: Option<bool>) -> Self {
        self.safe_mode_active = active;
        self
    }

    pub fn extra_attachments<F>(mut self, callback: F) -> Self
    where
        F: Fn(&Path) -> Result<(), BugReportError> + Send + Sync + 'a,
    {
        self.extra_attachments = Some(Box::new(callback));
        self
    }

    pub fn build(self) -> Result<BugReportBundle, BugReportError> {
        let start = PROCESS_START.get_or_init(Instant::now);
        let uptime_ms = start.elapsed().as_millis() as u64;

        let dir = tempfile::Builder::new()
            .prefix("nova-bugreport-")
            .tempdir()?;
        let path = dir.keep();

        let meta = MetaReport {
            bundle_version: BUGREPORT_BUNDLE_VERSION,
            nova_version: nova_core::NOVA_VERSION,
            nova_bugreport_version: env!("CARGO_PKG_VERSION"),
            timestamp_utc: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "<unknown>".to_owned()),
            timestamp_unix_ms: unix_ms_now(),
            target_triple: env!("NOVA_BUGREPORT_TARGET_TRIPLE"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            git_sha: option_env!("GIT_SHA"),
        };
        write_json(path.join("meta.json"), &meta)?;

        let system = SystemReport {
            cpu_count: std::thread::available_parallelism().ok().map(|n| n.get()),
            total_memory_bytes: total_memory_bytes(),
            rss_bytes: current_rss_bytes(),
            uptime_ms,
        };
        write_json(path.join("system.json"), &system)?;

        write_json(path.join("env.json"), &collect_env_snapshot())?;

        let sanitized = sanitize_config(self.config)?;
        write_json(path.join("config.json"), &sanitized)?;

        let logs = self.log_buffer.last_lines(self.options.max_log_lines);
        let redacted_logs: Vec<String> = logs
            .into_iter()
            .map(|line| redact::redact_string(&line))
            .collect();
        std::fs::write(path.join("logs.txt"), redacted_logs.join("\n"))?;

        let mut perf = self.perf.snapshot();
        perf.safe_mode_active = self.safe_mode_active;
        write_json(path.join("performance.json"), &perf)?;

        let persisted = self
            .persisted_crash_log_path
            .as_deref()
            .map(|path| read_persisted_crashes(path, PERSISTED_CRASH_LIMIT))
            .unwrap_or_default();
        let mut in_memory = self.crash_store.snapshot();
        for record in &mut in_memory {
            sanitize_crash_record(record);
        }
        let crashes = CrashReport {
            in_memory,
            persisted,
        };
        write_json(path.join("crashes.json"), &crashes)?;

        if let Some(repro) = self.options.reproduction {
            std::fs::write(path.join("repro.txt"), redact::redact_string(&repro))?;
        }

        if let Some(callback) = self.extra_attachments {
            if let Err(err) = callback(&path) {
                tracing::warn!(error = %err, "bugreport extra attachments failed");
            }
        }

        let archive = if self.create_archive {
            match create_zip_archive(&path) {
                Ok(path) => Some(path),
                Err(err) => {
                    tracing::warn!(error = %err, "bugreport archive creation failed");
                    None
                }
            }
        } else {
            None
        };

        Ok(BugReportBundle {
            directory: path,
            archive,
        })
    }
}

fn collect_env_snapshot() -> serde_json::Value {
    fn include_var(key: &str) -> bool {
        key.starts_with("NOVA_")
            || key.starts_with("VSCODE_")
            || matches!(key, "RUST_LOG" | "JAVA_HOME")
    }

    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| {
            let key = k.to_string_lossy().to_string();
            if !include_var(&key) {
                return None;
            }

            let value = if is_secret_key(&key) {
                "<redacted>".to_owned()
            } else {
                redact::redact_string(&v.to_string_lossy())
            };

            Some((key, value))
        })
        .collect();

    vars.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut map = serde_json::Map::new();
    for (k, v) in vars {
        map.insert(k, serde_json::Value::String(v));
    }
    serde_json::Value::Object(map)
}

fn total_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in meminfo.lines() {
            let line = line.trim_start();
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            let line = line.trim_start();
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn create_zip_archive(dir: &Path) -> Result<PathBuf, BugReportError> {
    let archive_path = dir.with_extension("zip");
    let file = std::fs::File::create(&archive_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::<()>::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let prefix = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("nova-bugreport");
    zip_dir_recursive(&mut zip, dir, dir, prefix, &options)?;

    zip.finish().map_err(std::io::Error::other)?;
    Ok(archive_path)
}

fn zip_dir_recursive(
    zip: &mut zip::ZipWriter<std::fs::File>,
    root: &Path,
    dir: &Path,
    prefix: &str,
    options: &zip::write::FileOptions<()>,
) -> Result<(), BugReportError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            zip_dir_recursive(zip, root, &path, prefix, options)?;
            continue;
        }
        if !ty.is_file() {
            continue;
        }

        let rel = path.strip_prefix(root).unwrap_or(path.as_path());
        let name = Path::new(prefix).join(rel);
        let name = name.to_string_lossy().replace('\\', "/");

        zip.start_file(name, *options)
            .map_err(std::io::Error::other)?;
        let mut file = std::fs::File::open(&path)?;
        std::io::copy(&mut file, zip)?;
    }
    Ok(())
}

pub fn create_bug_report_bundle(
    config: &NovaConfig,
    log_buffer: &LogBuffer,
    crash_store: &CrashStore,
    perf: &PerfStats,
    options: BugReportOptions,
) -> Result<BugReportBundle, BugReportError> {
    BugReportBuilder::new(config, log_buffer, crash_store, perf)
        .options(options)
        // Legacy API: preserve the previous default of emitting a directory on disk.
        .create_archive(false)
        .build()
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
        serde_json::Value::String(s) => serde_json::Value::String(redact::redact_string(&s)),
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

    #[test]
    fn config_sanitization_redacts_url_query_params_and_userinfo() {
        let input = serde_json::json!({
            "provider_url": "https://user:pass@example.com/path?token=abc123&foo=bar&api_key=sk-12345678901234567890"
        });

        let sanitized = sanitize_value(input);
        let out = serde_json::to_string(&sanitized).expect("json serialization should succeed");

        assert!(
            !out.contains("pass"),
            "userinfo password should be redacted"
        );
        assert!(!out.contains("abc123"), "query token should be redacted");
        assert!(
            !out.contains("sk-12345678901234567890"),
            "api key should be redacted"
        );
        assert!(
            out.contains("foo=bar"),
            "non-sensitive query params should remain"
        );
        assert!(
            out.contains("<redacted>@example.com"),
            "userinfo should be redacted"
        );
        assert!(
            out.contains("token=<redacted>") || out.contains("token=%3Credacted%3E"),
            "expected token query param to be redacted, got: {out}"
        );
    }

    #[test]
    fn logs_are_redacted_by_value_patterns() {
        let config = NovaConfig::default();

        let buffer = LogBuffer::new(10);
        buffer.push_line("Authorization: Bearer SUPERSECRET-TOKEN".to_owned());

        let crash_store = CrashStore::new(10);
        let perf = PerfStats::default();

        let bundle = create_bug_report_bundle(
            &config,
            &buffer,
            &crash_store,
            &perf,
            BugReportOptions::default(),
        )
        .expect("bundle creation failed");

        let logs =
            std::fs::read_to_string(bundle.path().join("logs.txt")).expect("logs read failed");
        assert!(!logs.contains("SUPERSECRET-TOKEN"));
        assert!(logs.contains("<redacted>"));
    }

    #[test]
    fn persisted_crashes_are_loaded_and_included_in_bundle() {
        let dir = tempfile::tempdir().expect("tempdir should succeed");
        let crash_log = dir.path().join("crashes.jsonl");

        let record = CrashRecord {
            timestamp_unix_ms: 123,
            message: "boom".to_owned(),
            location: None,
            backtrace: None,
        };
        append_crash_record(&crash_log, &record).expect("append crash record should succeed");

        let crash_store = CrashStore::new(10);
        crash_store.load_from_file(&crash_log);
        assert_eq!(crash_store.snapshot().len(), 1);

        let config = NovaConfig::default();
        let buffer = LogBuffer::new(1);
        let perf = PerfStats::default();

        let bundle = BugReportBuilder::new(&config, &buffer, &CrashStore::new(10), &perf)
            .persisted_crash_log_path(Some(crash_log))
            .create_archive(false)
            .build()
            .expect("bundle creation failed");

        let crashes_text = std::fs::read_to_string(bundle.path().join("crashes.json"))
            .expect("crashes read failed");
        let crashes: serde_json::Value =
            serde_json::from_str(&crashes_text).expect("crashes json should parse");
        let persisted = crashes
            .get("persisted")
            .and_then(|v| v.as_array())
            .expect("persisted crashes should be an array");
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].get("message").and_then(|v| v.as_str()),
            Some("boom")
        );
    }
}
