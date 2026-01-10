use crate::breakpoints::map_line_breakpoints;
use crate::dap::codec::{read_json_message, write_json_message};
use crate::dap::messages::{Event, Request, Response};
use crate::jdwp::{JdwpClient, TcpJdwpClient};
use anyhow::Context;
use nova_db::RootDatabase;
use nova_project::{AttachConfig, LaunchConfig};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;

pub struct DapServer<C: JdwpClient> {
    next_seq: u64,
    db: RootDatabase,
    jdwp: C,
    breakpoints: HashMap<PathBuf, Vec<u32>>,
    should_exit: bool,
}

impl Default for DapServer<TcpJdwpClient> {
    fn default() -> Self {
        Self::new(TcpJdwpClient::new())
    }
}

impl<C: JdwpClient> DapServer<C> {
    pub fn new(jdwp: C) -> Self {
        Self {
            next_seq: 1,
            db: RootDatabase::new(),
            jdwp,
            breakpoints: HashMap::new(),
            should_exit: false,
        }
    }

    pub fn run_stdio(mut self) -> anyhow::Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = BufReader::new(stdin.lock());
        let mut writer = BufWriter::new(stdout.lock());

        while let Some(request) = read_json_message::<_, Request>(&mut reader)? {
            let outgoing = match self.handle_request(&request) {
                Ok(outgoing) => outgoing,
                Err(err) => vec![serde_json::to_value(Response::error(
                    self.alloc_seq(),
                    &request,
                    err.to_string(),
                ))?],
            };

            for msg in outgoing {
                write_json_message(&mut writer, &msg)?;
            }
            writer.flush()?;

            if self.should_exit {
                break;
            }
        }

        Ok(())
    }

    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    fn handle_request(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        match request.command.as_str() {
            "initialize" => self.initialize(request),
            "launch" => self.launch(request),
            "attach" => self.attach(request),
            "setBreakpoints" => self.set_breakpoints(request),
            "configurationDone" => self.simple_ok(request, None),
            "threads" => self.threads(request),
            "stackTrace" => self.stack_trace(request),
            "scopes" => self.scopes(request),
            "variables" => self.variables(request),
            "evaluate" => self.evaluate(request),
            "continue" | "next" | "stepIn" | "stepOut" | "pause" => self.execution_control(request),
            "disconnect" => self.disconnect(request),
            _ => Ok(vec![serde_json::to_value(Response::error(
                self.alloc_seq(),
                request,
                format!("Unknown command: {}", request.command),
            ))?]),
        }
    }

    fn initialize(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let capabilities = json!({
            "supportsConfigurationDoneRequest": true,
            "supportsEvaluateForHovers": true,
            "supportsStepBack": false,
            "supportsDataBreakpoints": false,
            "supportsTerminateRequest": true,
            "supportsRestartRequest": false,
        });

        let response = Response::success(self.alloc_seq(), request, Some(capabilities));
        let initialized_event = Event::new(self.alloc_seq(), "initialized", None);

        Ok(vec![serde_json::to_value(response)?, serde_json::to_value(initialized_event)?])
    }

    fn launch(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let args: LaunchConfig = request
            .arguments
            .clone()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();

        if let Some(port) = args.port {
            let host = args.host.as_deref().unwrap_or("127.0.0.1");
            let _ = self.jdwp.connect(host, port);
        }

        self.simple_ok(request, None)
    }

    fn attach(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let args: AttachConfig = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("attach requires arguments")?,
        )?;

        let host = args.host.as_deref().unwrap_or("127.0.0.1");
        let _ = self.jdwp.connect(host, args.port);
        self.simple_ok(request, None)
    }

    fn set_breakpoints(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Source {
            path: Option<String>,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SourceBreakpoint {
            line: u32,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            source: Source,
            #[serde(default)]
            breakpoints: Vec<SourceBreakpoint>,
        }

        let args: Args = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("setBreakpoints requires arguments")?,
        )?;

        let Some(path) = args.source.path else {
            return self.simple_ok(request, Some(json!({ "breakpoints": [] })));
        };

        let path_buf = PathBuf::from(&path);

        // Load file text. DAP clients typically provide absolute paths.
        let text = std::fs::read_to_string(&path_buf).unwrap_or_default();
        let file_id = self.db.file_id_for_path(&path_buf);
        self.db.set_file_text(file_id, text);

        let requested: Vec<u32> = args.breakpoints.iter().map(|bp| bp.line).collect();
        let resolved = map_line_breakpoints(&self.db, file_id, &requested);

        let mut dap_breakpoints = Vec::new();
        for bp in resolved {
            if bp.verified {
                if let Some(class) = &bp.enclosing_class {
                let _ = self
                    .jdwp
                    .set_line_breakpoint(class, bp.enclosing_method.as_deref(), bp.resolved_line);
                }
            }

            dap_breakpoints.push(json!({
                "verified": bp.verified,
                "line": bp.resolved_line,
            }));
        }

        self.breakpoints.insert(path_buf, requested);

        self.simple_ok(request, Some(json!({ "breakpoints": dap_breakpoints })))
    }

    fn threads(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let threads = json!({
            "threads": [
                {"id": 1, "name": "Main Thread"}
            ]
        });
        self.simple_ok(request, Some(threads))
    }

    fn stack_trace(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let body = json!({
            "stackFrames": [],
            "totalFrames": 0
        });
        self.simple_ok(request, Some(body))
    }

    fn scopes(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let body = json!({
            "scopes": [
                {"name": "Locals", "presentationHint": "locals", "variablesReference": 1, "expensive": false}
            ]
        });
        self.simple_ok(request, Some(body))
    }

    fn variables(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let body = json!({ "variables": [] });
        self.simple_ok(request, Some(body))
    }

    fn evaluate(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        let body = json!({
            "result": "Evaluation is not implemented yet",
            "variablesReference": 0
        });
        self.simple_ok(request, Some(body))
    }

    fn execution_control(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        // Best-effort: acknowledge the request so the client can continue
        // interacting with the session.
        let body = match request.command.as_str() {
            "continue" => Some(json!({ "allThreadsContinued": true })),
            _ => None,
        };
        self.simple_ok(request, body)
    }

    fn disconnect(&mut self, request: &Request) -> anyhow::Result<Vec<serde_json::Value>> {
        self.should_exit = true;
        self.simple_ok(request, None)
    }

    fn simple_ok(&mut self, request: &Request, body: Option<serde_json::Value>) -> anyhow::Result<Vec<serde_json::Value>> {
        let response = Response::success(self.alloc_seq(), request, body);
        Ok(vec![serde_json::to_value(response)?])
    }
}
