use anyhow::{anyhow, Context, Result};
use nova_build_bazel::bsp::{
    BuildTarget, BuildTargetIdentifier, CompileParams, InitializeBuildResult, InverseSourcesParams,
    JavacOptionsItem, JavacOptionsParams, PublishDiagnosticsParams, WorkspaceBuildTargetsResult,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

#[derive(Debug)]
struct PipeState {
    buf: VecDeque<u8>,
    closed: bool,
}

#[derive(Debug)]
struct PipeInner {
    state: Mutex<PipeState>,
    available: Condvar,
}

#[derive(Debug)]
pub struct PipeReader {
    inner: Arc<PipeInner>,
}

#[derive(Debug)]
pub struct PipeWriter {
    inner: Arc<PipeInner>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut guard = self
            .inner
            .state
            .lock()
            .map_err(|_| std::io::ErrorKind::Other)?;
        while guard.buf.is_empty() && !guard.closed {
            guard = self
                .inner
                .available
                .wait(guard)
                .map_err(|_| std::io::ErrorKind::Other)?;
        }

        if guard.buf.is_empty() && guard.closed {
            return Ok(0);
        }

        let mut n = 0;
        while n < buf.len() {
            let Some(b) = guard.buf.pop_front() else {
                break;
            };
            buf[n] = b;
            n += 1;
        }
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .inner
            .state
            .lock()
            .map_err(|_| std::io::ErrorKind::Other)?;
        guard.buf.extend(buf);
        drop(guard);
        self.inner.available.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.state.lock() {
            guard.closed = true;
        }
        self.inner.available.notify_all();
    }
}

fn pipe() -> (PipeReader, PipeWriter) {
    let inner = Arc::new(PipeInner {
        state: Mutex::new(PipeState {
            buf: VecDeque::new(),
            closed: false,
        }),
        available: Condvar::new(),
    });
    (
        PipeReader {
            inner: inner.clone(),
        },
        PipeWriter { inner },
    )
}

#[derive(Debug)]
struct Endpoint {
    read: PipeReader,
    write: PipeWriter,
}

fn duplex() -> (Endpoint, Endpoint) {
    let (client_to_server_read, client_to_server_write) = pipe();
    let (server_to_client_read, server_to_client_write) = pipe();
    (
        Endpoint {
            read: server_to_client_read,
            write: client_to_server_write,
        },
        Endpoint {
            read: client_to_server_read,
            write: server_to_client_write,
        },
    )
}

#[derive(Debug, Clone)]
pub struct FakeBspServerConfig {
    pub initialize: InitializeBuildResult,
    pub targets: Vec<BuildTarget>,
    pub inverse_sources: BTreeMap<String, Vec<BuildTargetIdentifier>>,
    pub javac_options: Vec<JavacOptionsItem>,
    pub compile_status_code: i32,
    pub diagnostics: Vec<PublishDiagnosticsParams>,
    /// If true, the server will send an unsolicited JSON-RPC request (method + id) before
    /// responding to `build/initialize`. This exercises client handling of server->client
    /// requests interleaved with normal request/response traffic.
    pub send_server_request_before_initialize_response: bool,
}

#[derive(Debug)]
pub struct FakeBspServer {
    handle: JoinHandle<()>,
    #[allow(dead_code)]
    requests: Arc<Mutex<Vec<Value>>>,
}

impl FakeBspServer {
    pub fn join(self) {
        self.handle.join().unwrap();
    }

    #[allow(dead_code)]
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

pub fn spawn_fake_bsp_server(
    config: FakeBspServerConfig,
) -> Result<(nova_build_bazel::BspClient, FakeBspServer)> {
    let (client, server) = duplex();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_thread = Arc::clone(&requests);

    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(server.read);
        let mut writer = server.write;

        while let Ok(Some(msg)) = read_message(&mut reader) {
            requests_for_thread.lock().unwrap().push(msg.clone());
            let method = msg.get("method").and_then(Value::as_str);
            let id = msg.get("id").and_then(Value::as_i64);

            match (method, id) {
                (Some("build/initialize"), Some(id)) => {
                    if config.send_server_request_before_initialize_response {
                        let _ = write_message(
                            &mut writer,
                            &serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "method": "server/request",
                                "params": Value::Null,
                            }),
                        );
                    }
                    let result = serde_json::to_value(&config.initialize).unwrap();
                    let _ = write_message(
                        &mut writer,
                        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                    );
                }
                (Some("build/initialized"), None) => {
                    // No-op
                }
                (Some("workspace/buildTargets"), Some(id)) => {
                    let result = serde_json::to_value(&WorkspaceBuildTargetsResult {
                        targets: config.targets.clone(),
                    })
                    .unwrap();
                    let _ = write_message(
                        &mut writer,
                        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                    );
                }
                (Some("buildTarget/inverseSources"), Some(id)) => {
                    let params: InverseSourcesParams = msg
                        .get("params")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or(InverseSourcesParams {
                            text_document: nova_build_bazel::bsp::TextDocumentIdentifier {
                                uri: String::new(),
                            },
                        });
                    let targets = config
                        .inverse_sources
                        .get(&params.text_document.uri)
                        .cloned()
                        .unwrap_or_default();
                    let result = serde_json::json!({ "targets": targets });
                    let _ = write_message(
                        &mut writer,
                        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                    );
                }
                (Some("buildTarget/javacOptions"), Some(id)) => {
                    let params: JavacOptionsParams = msg
                        .get("params")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or(JavacOptionsParams {
                            targets: Vec::new(),
                        });
                    let requested: Vec<String> =
                        params.targets.into_iter().map(|t| t.uri).collect();
                    let items: Vec<JavacOptionsItem> = config
                        .javac_options
                        .iter()
                        .filter(|item| requested.contains(&item.target.uri))
                        .cloned()
                        .collect();
                    let result = serde_json::json!({ "items": items });
                    let _ = write_message(
                        &mut writer,
                        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                    );
                }
                (Some("buildTarget/compile"), Some(id)) => {
                    let params: CompileParams = msg
                        .get("params")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or(CompileParams {
                            targets: Vec::new(),
                        });
                    let _ = params;

                    // Interleave diagnostics notifications before the compile response to
                    // exercise client-side JSON-RPC handling.
                    for diag in &config.diagnostics {
                        let params = serde_json::to_value(diag).unwrap();
                        let _ = write_message(
                            &mut writer,
                            &serde_json::json!({"jsonrpc":"2.0","method":"build/publishDiagnostics","params":params}),
                        );
                    }

                    let result = serde_json::json!({ "statusCode": config.compile_status_code });
                    let _ = write_message(
                        &mut writer,
                        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                    );
                }
                (Some(method), Some(id)) => {
                    let _ = write_message(
                        &mut writer,
                        &serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("method not found: {method}")}}),
                    );
                }
                _ => {}
            }
        }
    });

    let client = nova_build_bazel::BspClient::from_streams(client.read, client.write);
    Ok((client, FakeBspServer { handle, requests }))
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .context("failed to read framed JSON-RPC message")?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

fn write_message(writer: &mut impl Write, msg: &Value) -> Result<()> {
    let json = serde_json::to_vec(msg)?;
    write!(writer, "Content-Length: {}\r\n\r\n", json.len())?;
    writer.write_all(&json)?;
    writer.flush()?;
    Ok(())
}
