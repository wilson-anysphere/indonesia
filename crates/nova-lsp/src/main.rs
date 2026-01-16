#[cfg(test)]
mod codec;
mod project_root;
mod rename_lsp;
mod rpc_out;
mod stdio_ai;
mod stdio_ai_code_edits;
mod stdio_ai_context;
mod stdio_ai_env;
mod stdio_ai_explain;
mod stdio_ai_privacy;
mod stdio_ai_snippets;
mod stdio_analysis;
mod stdio_apply_edit;
mod stdio_code_action;
mod stdio_code_lens;
mod stdio_completion;
mod stdio_config;
mod stdio_diagnostics;
mod stdio_distributed;
mod stdio_execute_command;
mod stdio_extensions;
mod stdio_extensions_db;
mod stdio_fs;
mod stdio_goto;
mod stdio_hierarchy;
mod stdio_incoming;
mod stdio_init;
mod stdio_io;
mod stdio_jsonrpc;
mod stdio_memory;
mod stdio_notifications;
mod stdio_organize_imports;
mod stdio_paths;
mod stdio_progress;
mod stdio_refactor_snapshot;
mod stdio_rename;
mod stdio_requests;
mod stdio_semantic_search;
mod stdio_semantic_tokens;
mod stdio_state;
mod stdio_text;
mod stdio_text_document;
mod stdio_transport;
mod stdio_workspace_symbol;
#[cfg(test)]
mod test_support;

pub(crate) use stdio_state::ServerState;

use lsp_server::Connection;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stdio_transport::{IncomingMessage, LspClient};

fn flush_after_message(client: &LspClient, state: &mut ServerState) -> std::io::Result<()> {
    stdio_notifications::flush_memory_status_notifications(client, state)?;
    stdio_notifications::flush_safe_mode_notifications(client, state)?;
    stdio_diagnostics::flush_publish_diagnostics(client, state)?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!(
            "nova-lsp {version}\n\nUsage:\n  nova-lsp [--stdio] [--config <path>] [--distributed] [--distributed-worker-command <path>]\n\nFlags:\n  --stdio\n      Use stdio transport (default; only supported transport).\n\n  --config <path>\n      Path to the nova.toml configuration file.\n      If omitted, uses NOVA_CONFIG/NOVA_CONFIG_PATH or discovers nova.toml/.nova.toml in the workspace.\n\n  --distributed\n      Enable local distributed indexing/search via nova-router + nova-worker.\n\n  --distributed-worker-command <path>\n      Path to the nova-worker binary (only used with --distributed).\n      Defaults to a sibling nova-worker next to nova-lsp if present; otherwise falls back to nova-worker on PATH.\n\n  -h, --help\n      Print help.\n\n  -V, --version\n      Print version.\n",
            version = env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }

    // Load AI config early so audit logging can be wired up before we install
    // the global tracing subscriber.
    let ai_env = match stdio_ai_env::load_ai_config_from_env() {
        Ok(value) => value,
        Err(err) => {
            eprintln!("failed to configure AI: {err}");
            None
        }
    };

    // Install panic hook + structured logging early. The stdio transport does
    // not currently emit `window/showMessage` notifications on panic, but
    // `nova/bugReport` can be used to generate a diagnostic bundle.
    let mut config = stdio_config::load_config_from_args(&args);
    let privacy_override = ServerState::apply_ai_overrides_from_env(&mut config, ai_env.as_ref());
    nova_lsp::hardening::init(&config, Arc::new(|message| eprintln!("{message}")));
    stdio_fs::gc_decompiled_document_store_best_effort();

    // Accept `--stdio` for compatibility with editor templates. For now we only
    // support stdio transport, and ignore any other args.

    let metrics = nova_metrics::MetricsRegistry::global();

    let (connection, io_threads) = Connection::stdio();

    let distributed_cli = stdio_distributed::parse_distributed_cli(&args);

    let config_memory_overrides = config.memory_budget_overrides();
    let mut state = ServerState::new(config, privacy_override, config_memory_overrides);
    state.distributed_cli = distributed_cli;

    let request_cancellation =
        nova_lsp::RequestCancellation::new(nova_scheduler::Scheduler::new({
            // The request-cancellation registry only needs a progress channel; keep the
            // scheduler pools tiny so multiple `nova-lsp` processes can run in constrained
            // environments (e.g. tests, CI sandboxes) without exhausting thread quotas.
            let mut cfg = nova_scheduler::SchedulerConfig::default();
            cfg.compute_threads = 1;
            cfg.background_threads = 1;
            cfg.io_threads = 1;
            cfg
        }));

    // ---------------------------------------------------------------------
    // Initialize handshake
    // ---------------------------------------------------------------------
    stdio_init::perform_initialize_handshake(&connection, &mut state, metrics)?;

    // ---------------------------------------------------------------------
    // Main message loop (with cancellation router)
    // ---------------------------------------------------------------------
    let Connection { sender, receiver } = connection;
    let client = LspClient::new(sender);
    let (incoming_tx, incoming_rx) = crossbeam_channel::unbounded::<IncomingMessage>();
    std::thread::spawn({
        let incoming_tx = incoming_tx.clone();
        let request_cancellation = request_cancellation.clone();
        let salsa = state.analysis.salsa.clone();
        move || {
            stdio_transport::message_router(
                receiver,
                incoming_tx,
                request_cancellation,
                Some(salsa),
            )
        }
    });
    drop(incoming_tx);

    let mut exit_code: Option<i32> = None;
    for msg in incoming_rx {
        match msg {
            IncomingMessage::Request {
                request,
                cancel_id,
                cancel_token,
            } => {
                let method = request.method.clone();
                let request_id = request.id.clone();
                let start = Instant::now();
                let mut did_panic = false;

                let response = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    nova_db::catch_cancelled(|| {
                        stdio_requests::handle_request(request, cancel_token, &mut state, &client)
                    })
                })) {
                    Ok(Ok(Ok(response))) => response,
                    Ok(Ok(Err(err))) => {
                        request_cancellation.finish(cancel_id);
                        metrics.record_request(&method, start.elapsed());
                        metrics.record_error(&method);
                        return Err(err);
                    }
                    Ok(Err(_cancelled)) => {
                        stdio_jsonrpc::response_error(request_id, -32800, "Request cancelled")
                    }
                    Err(_) => {
                        did_panic = true;
                        tracing::error!(
                            target = "nova.lsp",
                            method,
                            "panic while handling request"
                        );
                        stdio_jsonrpc::response_error(request_id, -32603, "Internal error (panic)")
                    }
                };
                let response_is_error = response.error.is_some();

                request_cancellation.finish(cancel_id);

                if let Err(err) = client.respond(response) {
                    metrics.record_request(&method, start.elapsed());
                    metrics.record_error(&method);
                    if did_panic {
                        metrics.record_panic(&method);
                    }
                    return Err(err);
                }

                metrics.record_request(&method, start.elapsed());
                if response_is_error {
                    metrics.record_error(&method);
                }
                if did_panic {
                    metrics.record_panic(&method);
                }
                flush_after_message(&client, &mut state)?;
            }
            IncomingMessage::Notification(notification) => {
                let method = notification.method.clone();
                let start = Instant::now();
                if method == "exit" {
                    // Best-effort: shut down the distributed router before exiting so any
                    // spawned workers terminate and any IPC sockets are cleaned up.
                    state.shutdown_distributed_router(Duration::from_secs(2));
                    metrics.record_request(&method, start.elapsed());
                    exit_code = Some(if state.shutdown_requested { 0 } else { 1 });
                    break;
                }

                let mut did_panic = false;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    stdio_incoming::handle_notification(
                        &method,
                        notification.params,
                        &mut state,
                        &client,
                    )
                }));

                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        metrics.record_request(&method, start.elapsed());
                        metrics.record_error(&method);
                        return Err(err);
                    }
                    Err(_) => {
                        did_panic = true;
                        tracing::error!(
                            target = "nova.lsp",
                            method,
                            "panic while handling notification"
                        );
                    }
                }

                metrics.record_request(&method, start.elapsed());
                if did_panic {
                    metrics.record_error(&method);
                    metrics.record_panic(&method);
                }
                flush_after_message(&client, &mut state)?;
            }
            IncomingMessage::Response(_response) => {
                // Best-effort: ignore server->client responses (we do not await them today).
            }
        }
    }

    if let Some(exit_code) = exit_code {
        // Best-effort: shut down `lsp-server` I/O threads (especially the stdout writer) before
        // terminating the process. Some clients send `exit` and keep the pipes open briefly, so
        // this is intentionally bounded and will fall back to `process::exit`.
        drop(client);
        if state.shutdown_requested {
            stdio_io::join_io_threads_with_timeout(io_threads, Duration::from_millis(250));
        }
        std::process::exit(exit_code);
    }

    io_threads.join()?;
    Ok(())
}

// Core server state lives in `stdio_state`.

// Leaf `textDocument/*` handlers live in `stdio_text_document`.

// code-action helpers live in `stdio_code_action`
