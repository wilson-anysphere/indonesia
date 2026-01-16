use crate::{
    NovaLspError, Result, BUG_REPORT_METHOD, BUILD_DIAGNOSTICS_METHOD, BUILD_FILE_CLASSPATH_METHOD,
    BUILD_PROJECT_METHOD, BUILD_STATUS_METHOD, BUILD_TARGET_CLASSPATH_METHOD,
    DEBUG_CONFIGURATIONS_METHOD, DEBUG_HOT_SWAP_METHOD, JAVA_CLASSPATH_METHOD,
    JAVA_GENERATED_SOURCES_METHOD, JAVA_RESOLVE_MAIN_CLASS_METHOD, JAVA_SOURCE_PATHS_METHOD,
    METRICS_METHOD, PROJECT_CONFIGURATION_METHOD, PROJECT_MODEL_METHOD, RELOAD_PROJECT_METHOD,
    RESET_METRICS_METHOD, RUN_ANNOTATION_PROCESSING_METHOD, SAFE_MODE_STATUS_METHOD,
    TEST_DEBUG_CONFIGURATION_METHOD, TEST_DISCOVER_METHOD, TEST_RUN_METHOD,
};
use nova_bugreport::{
    global_crash_store, install_panic_hook, BugReportBuilder, BugReportOptions, PanicHookConfig,
    PerfStats,
};
use nova_config::{global_log_buffer, init_tracing_with_config, NovaConfig};
use nova_scheduler::{CancellationToken, Watchdog, WatchdogError};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

static PERF: OnceLock<PerfStats> = OnceLock::new();
static CONFIG: OnceLock<NovaConfig> = OnceLock::new();
static SAFE_MODE: OnceLock<SafeModeState> = OnceLock::new();
static WATCHDOG: OnceLock<Watchdog> = OnceLock::new();

// Test-only safety valve: allow integration tests to force safe-mode in a spawned
// `nova-lsp` process without needing to trigger a real watchdog timeout/panic.
//
// This is compiled only in debug builds to avoid introducing an escape hatch in
// production binaries.
#[cfg(debug_assertions)]
const FORCE_SAFE_MODE_ENV: &str = "NOVA_LSP_TEST_FORCE_SAFE_MODE";

#[cfg(debug_assertions)]
fn force_safe_mode_from_env() -> bool {
    static FORCE: OnceLock<bool> = OnceLock::new();
    *FORCE.get_or_init(|| {
        matches!(
            std::env::var(FORCE_SAFE_MODE_ENV).as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        )
    })
}

#[derive(Debug, Default)]
struct SafeModeState {
    enabled: AtomicBool,
    until: Mutex<Option<Instant>>,
    reason: Mutex<Option<SafeModeReason>>,
}

fn perf() -> &'static PerfStats {
    PERF.get_or_init(PerfStats::default)
}

fn safe_mode() -> &'static SafeModeState {
    let safe_mode = SAFE_MODE.get_or_init(SafeModeState::default);

    // Best-effort test hook: allow integration tests to force safe-mode for a
    // full stdio server process via an env var. This helps validate that all
    // `nova/*` endpoints are consistently guarded, including those handled
    // directly by the stdio server (see `crates/nova-lsp/src/main.rs`).
    #[cfg(debug_assertions)]
    if force_safe_mode_from_env() {
        safe_mode.enabled.store(true, Ordering::Relaxed);
        *safe_mode
            .until
            .lock()
            .expect("SafeModeState mutex poisoned") = None;
        *safe_mode
            .reason
            .lock()
            .expect("SafeModeState mutex poisoned") = Some(SafeModeReason::Panic);
    }

    safe_mode
}

fn watchdog() -> &'static Watchdog {
    WATCHDOG.get_or_init(Watchdog::new)
}

fn config_snapshot() -> NovaConfig {
    CONFIG.get().cloned().unwrap_or_default()
}

pub fn record_request() {
    perf().record_request();
}

/// Initializes structured logging and installs the global panic hook used by
/// the LSP process.
///
/// This is safe to call multiple times; only the first call installs the global
/// subscriber and stores the config snapshot.
pub fn init(config: &NovaConfig, notifier: Arc<dyn Fn(&str) + Send + Sync + 'static>) {
    let _ = init_tracing_with_config(config);
    let _ = CONFIG.set(config.clone());
    install_panic_hook(
        PanicHookConfig {
            include_backtrace: config.logging.include_backtrace,
            ..Default::default()
        },
        notifier,
    );
}

pub fn guard_method(method: &str) -> Result<()> {
    if matches!(
        method,
        BUG_REPORT_METHOD | METRICS_METHOD | RESET_METRICS_METHOD | SAFE_MODE_STATUS_METHOD
    ) {
        return Ok(());
    }

    let safe_mode = safe_mode();
    if safe_mode.enabled.load(Ordering::Relaxed) {
        if let Some(until) = safe_mode
            .until
            .lock()
            .expect("SafeModeState mutex poisoned")
            .as_ref()
            .copied()
        {
            if Instant::now() >= until {
                safe_mode.enabled.store(false, Ordering::Relaxed);
                *safe_mode
                    .until
                    .lock()
                    .expect("SafeModeState mutex poisoned") = None;
                *safe_mode
                    .reason
                    .lock()
                    .expect("SafeModeState mutex poisoned") = None;
                return Ok(());
            }
        }

        return Err(NovaLspError::Internal(
            "Nova is running in safe-mode (previous request crashed or timed out). Only `nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` are available for now."
                .to_owned(),
        ));
    }

    Ok(())
}

fn enter_safe_mode(reason: SafeModeReason) {
    perf().record_safe_mode_entry();
    let safe_mode = safe_mode();
    safe_mode.enabled.store(true, Ordering::Relaxed);
    let mut until = safe_mode
        .until
        .lock()
        .expect("SafeModeState mutex poisoned");
    let duration = match reason {
        SafeModeReason::Panic => Duration::from_secs(60),
        SafeModeReason::WatchdogTimeout => Duration::from_secs(30),
    };
    *until = Some(Instant::now() + duration);
    *safe_mode
        .reason
        .lock()
        .expect("SafeModeState mutex poisoned") = Some(reason);
}

#[derive(Debug, Clone, Copy)]
enum SafeModeReason {
    Panic,
    WatchdogTimeout,
}

/// Snapshot the current safe-mode state.
///
/// Returns `(enabled, reason)` where `reason` is a stable string identifier intended for clients.
pub fn safe_mode_snapshot() -> (bool, Option<&'static str>) {
    let safe_mode = safe_mode();
    if !safe_mode.enabled.load(Ordering::Relaxed) {
        return (false, None);
    }

    if let Some(until) = safe_mode
        .until
        .lock()
        .expect("SafeModeState mutex poisoned")
        .as_ref()
        .copied()
    {
        if Instant::now() >= until {
            safe_mode.enabled.store(false, Ordering::Relaxed);
            *safe_mode
                .until
                .lock()
                .expect("SafeModeState mutex poisoned") = None;
            *safe_mode
                .reason
                .lock()
                .expect("SafeModeState mutex poisoned") = None;
            return (false, None);
        }
    }

    let reason = safe_mode
        .reason
        .lock()
        .expect("SafeModeState mutex poisoned")
        .as_ref()
        .map(|reason| match reason {
            SafeModeReason::Panic => "panic",
            SafeModeReason::WatchdogTimeout => "watchdog_timeout",
        });
    (true, reason)
}

pub fn run_with_watchdog(
    method: &str,
    params: serde_json::Value,
    handler: fn(serde_json::Value) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    run_with_watchdog_cancelable(method, params, CancellationToken::new(), handler)
}

pub fn run_with_watchdog_cancelable(
    method: &str,
    params: serde_json::Value,
    cancel: CancellationToken,
    handler: fn(serde_json::Value) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let deadline = deadline_for_method(method);
    let watchdog = watchdog();
    let cancel = cancel.child_token();

    match watchdog.run_with_deadline(deadline, cancel, move |_token| handler(params)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(err),
        Err(WatchdogError::DeadlineExceeded(duration)) => {
            perf().record_timeout();
            nova_metrics::MetricsRegistry::global().record_timeout(method);
            if timeout_enters_safe_mode(method) {
                enter_safe_mode(SafeModeReason::WatchdogTimeout);
            }
            Err(NovaLspError::Internal(format!(
                "{method} exceeded its time budget of {duration:?}"
            )))
        }
        Err(WatchdogError::Panicked) => {
            perf().record_panic();
            nova_metrics::MetricsRegistry::global().record_panic(method);
            enter_safe_mode(SafeModeReason::Panic);
            Err(NovaLspError::Internal(format!(
                "{method} panicked; entering safe-mode"
            )))
        }
        Err(WatchdogError::Cancelled) => {
            Err(NovaLspError::Internal(format!("{method} was cancelled")))
        }
    }
}

pub fn run_with_watchdog_cancelable_with_token(
    method: &str,
    params: serde_json::Value,
    cancel: CancellationToken,
    handler: fn(serde_json::Value, CancellationToken) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let deadline = deadline_for_method(method);
    let watchdog = watchdog();
    let cancel = cancel.child_token();

    match watchdog.run_with_deadline(deadline, cancel, move |token| handler(params, token)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(err),
        Err(WatchdogError::DeadlineExceeded(duration)) => {
            perf().record_timeout();
            nova_metrics::MetricsRegistry::global().record_timeout(method);
            if timeout_enters_safe_mode(method) {
                enter_safe_mode(SafeModeReason::WatchdogTimeout);
            }
            Err(NovaLspError::Internal(format!(
                "{method} exceeded its time budget of {duration:?}"
            )))
        }
        Err(WatchdogError::Panicked) => {
            perf().record_panic();
            nova_metrics::MetricsRegistry::global().record_panic(method);
            enter_safe_mode(SafeModeReason::Panic);
            Err(NovaLspError::Internal(format!(
                "{method} panicked; entering safe-mode"
            )))
        }
        Err(WatchdogError::Cancelled) => {
            Err(NovaLspError::Internal(format!("{method} was cancelled")))
        }
    }
}

pub fn run_with_watchdog_cancel(
    method: &str,
    params: serde_json::Value,
    handler: fn(serde_json::Value, CancellationToken) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    run_with_watchdog_cancelable_with_token(method, params, CancellationToken::new(), handler)
}

fn deadline_for_method(method: &str) -> Duration {
    match method {
        TEST_DISCOVER_METHOD => Duration::from_secs(30),
        TEST_RUN_METHOD => Duration::from_secs(300),
        TEST_DEBUG_CONFIGURATION_METHOD => Duration::from_secs(30),
        // Build integration can legitimately take longer; keep it bounded but
        // don't treat timeouts as a signal to enter safe-mode.
        BUILD_PROJECT_METHOD => Duration::from_secs(120),
        JAVA_CLASSPATH_METHOD => Duration::from_secs(60),
        PROJECT_CONFIGURATION_METHOD => Duration::from_secs(60),
        JAVA_SOURCE_PATHS_METHOD => Duration::from_secs(30),
        JAVA_RESOLVE_MAIN_CLASS_METHOD => Duration::from_secs(60),
        JAVA_GENERATED_SOURCES_METHOD => Duration::from_secs(60),
        RUN_ANNOTATION_PROCESSING_METHOD => Duration::from_secs(300),
        RELOAD_PROJECT_METHOD => Duration::from_secs(60),
        DEBUG_CONFIGURATIONS_METHOD => Duration::from_secs(30),
        DEBUG_HOT_SWAP_METHOD => Duration::from_secs(120),
        BUILD_TARGET_CLASSPATH_METHOD => Duration::from_secs(60),
        BUILD_FILE_CLASSPATH_METHOD => Duration::from_secs(60),
        BUILD_STATUS_METHOD => Duration::from_secs(5),
        BUILD_DIAGNOSTICS_METHOD => Duration::from_secs(120),
        PROJECT_MODEL_METHOD => Duration::from_secs(120),
        _ => Duration::from_secs(2),
    }
}

fn timeout_enters_safe_mode(method: &str) -> bool {
    !matches!(
        method,
        BUILD_PROJECT_METHOD
            | JAVA_CLASSPATH_METHOD
            | PROJECT_CONFIGURATION_METHOD
            | JAVA_SOURCE_PATHS_METHOD
            | JAVA_RESOLVE_MAIN_CLASS_METHOD
            | JAVA_GENERATED_SOURCES_METHOD
            | RUN_ANNOTATION_PROCESSING_METHOD
            | RELOAD_PROJECT_METHOD
            | TEST_DISCOVER_METHOD
            | TEST_RUN_METHOD
            | TEST_DEBUG_CONFIGURATION_METHOD
            | DEBUG_CONFIGURATIONS_METHOD
            | DEBUG_HOT_SWAP_METHOD
            | BUILD_TARGET_CLASSPATH_METHOD
            | BUILD_FILE_CLASSPATH_METHOD
            | BUILD_DIAGNOSTICS_METHOD
            | PROJECT_MODEL_METHOD
    )
}

pub fn handle_bug_report(params: serde_json::Value) -> Result<serde_json::Value> {
    // Best-effort: if the embedding application didn't call `init`, still set up
    // logging/panic recording so the bundle contains something useful.
    init(
        &config_snapshot(),
        Arc::new(|_| {
            // No-op notifier; transports can install a real one via `init`.
        }),
    );

    let (max_log_lines, reproduction) = match params {
        Value::Null => (None, None),
        Value::Object(obj) => {
            let max_log_lines = obj
                .get("maxLogLines")
                .and_then(|v| v.as_u64())
                .and_then(|n| usize::try_from(n).ok());
            let reproduction = obj
                .get("reproduction")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (max_log_lines, reproduction)
        }
        _ => {
            return Err(NovaLspError::InvalidParams(
                "bug report params must be an object".to_string(),
            ))
        }
    };

    let options = BugReportOptions {
        max_log_lines: max_log_lines.unwrap_or(500),
        reproduction,
    };

    let config = config_snapshot();
    let safe_mode_active = safe_mode().enabled.load(Ordering::Relaxed);
    let bundle =
        BugReportBuilder::new(&config, &global_log_buffer(), &global_crash_store(), perf())
            .options(options)
            .safe_mode_active(Some(safe_mode_active))
            .extra_attachments(|dir| {
                // Best-effort: attach the runtime request metrics snapshot. This is useful when
                // diagnosing hangs/timeouts/panics because it captures per-method latencies and
                // error rates.
                if let Ok(metrics_json) = serde_json::to_string_pretty(
                    &nova_metrics::MetricsRegistry::global().snapshot(),
                ) {
                    let _ = std::fs::write(dir.join("metrics.json"), metrics_json);
                }
                Ok(())
            })
            .build()
            .map_err(|err| NovaLspError::Internal(err.to_string()))?;

    let mut response = Map::new();
    response.insert(
        "path".to_string(),
        Value::String(bundle.path().to_string_lossy().to_string()),
    );
    if let Some(path) = bundle.archive_path() {
        response.insert(
            "archivePath".to_string(),
            Value::String(path.to_string_lossy().to_string()),
        );
    }
    Ok(Value::Object(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn panic_is_isolated_and_bug_report_is_generated() {
        global_crash_store().clear();
        safe_mode().enabled.store(false, Ordering::Relaxed);
        *safe_mode()
            .until
            .lock()
            .expect("SafeModeState mutex poisoned") = None;

        let notifications: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let notify = {
            let notifications = notifications.clone();
            Arc::new(move |msg: &str| {
                notifications
                    .lock()
                    .expect("notifications mutex poisoned")
                    .push(msg.to_owned());
            })
        };

        init(&NovaConfig::default(), notify);

        let _ = std::panic::catch_unwind(|| panic!("boom"));

        let bundle = handle_bug_report(serde_json::Value::Null).expect("bug report should succeed");
        let path = bundle
            .get("path")
            .and_then(|v| v.as_str())
            .expect("bundle should return a path");
        let crashes = std::fs::read_to_string(std::path::Path::new(path).join("crashes.json"))
            .expect("crashes.json should exist");
        assert!(crashes.contains("boom"));

        let notifications = notifications
            .lock()
            .expect("notifications mutex poisoned")
            .clone();
        assert!(
            notifications
                .iter()
                .any(|n| n.contains("Nova hit an internal error")),
            "expected panic hook notification, got: {notifications:?}"
        );
    }
}
