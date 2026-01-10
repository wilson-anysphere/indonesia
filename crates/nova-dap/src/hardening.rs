use nova_bugreport::{install_panic_hook, PanicHookConfig};
use nova_config::{init_tracing_with_config, NovaConfig};
use std::sync::Arc;

/// Initialize structured logging and install a global panic hook for the DAP
/// process.
///
/// DAP request handlers should still use local panic isolation (`catch_unwind`)
/// when possible; the panic hook is a last-resort safety net that records crash
/// diagnostics and emits a user-facing notification.
pub fn init(config: &NovaConfig, notifier: Arc<dyn Fn(&str) + Send + Sync + 'static>) {
    let _ = init_tracing_with_config(config);
    install_panic_hook(
        PanicHookConfig {
            include_backtrace: config.logging.include_backtrace,
        },
        notifier,
    );
}

