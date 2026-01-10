use crate::traits::{DiagnosticParams, DiagnosticProvider};
use crate::ExtensionContext;
use nova_types::{Diagnostic, Severity, Span};
use serde::Deserialize;
use wasmtime::{Engine, Instance, Linker, Module, Store, TypedFunc};

#[derive(Debug)]
pub enum WasmLoadError {
    Compile(String),
    Instantiate(String),
    MissingExport(&'static str),
    Utf8(String),
    Json(String),
}

impl std::fmt::Display for WasmLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmLoadError::Compile(msg) => write!(f, "failed to compile wasm module: {msg}"),
            WasmLoadError::Instantiate(msg) => write!(f, "failed to instantiate wasm module: {msg}"),
            WasmLoadError::MissingExport(name) => write!(f, "missing required wasm export: {name}"),
            WasmLoadError::Utf8(msg) => write!(f, "invalid utf-8 from wasm module: {msg}"),
            WasmLoadError::Json(msg) => write!(f, "invalid json from wasm module: {msg}"),
        }
    }
}

impl std::error::Error for WasmLoadError {}

/// Minimal runtime-loaded diagnostic provider backed by a WebAssembly module.
///
/// The current ABI is intentionally tiny: the module must export:
/// - `memory`: linear memory
/// - `diagnostics_ptr() -> i32`: pointer to a NUL-terminated UTF-8 JSON array of diagnostics
///
/// This is sufficient to demonstrate runtime loading while keeping the surface area small.
pub struct WasmDiagnosticProvider {
    id: String,
    engine: Engine,
    module: Module,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginDiagnostic {
    message: String,
    #[serde(default)]
    severity: Option<PluginSeverity>,
    #[serde(default)]
    span: Option<PluginSpan>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PluginSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Deserialize)]
struct PluginSpan {
    start: usize,
    end: usize,
}

impl WasmDiagnosticProvider {
    pub fn from_wasm_bytes(id: impl Into<String>, bytes: &[u8]) -> Result<Self, WasmLoadError> {
        let engine = Engine::default();
        let module = Module::new(&engine, bytes).map_err(|e| WasmLoadError::Compile(e.to_string()))?;
        Ok(Self {
            id: id.into(),
            engine,
            module,
        })
    }

    pub fn from_wat(id: impl Into<String>, wat: &str) -> Result<Self, WasmLoadError> {
        let bytes = wat::parse_str(wat).map_err(|e| WasmLoadError::Compile(e.to_string()))?;
        Self::from_wasm_bytes(id, &bytes)
    }

    fn instantiate(&self) -> Result<(Store<()>, Instance), WasmLoadError> {
        let mut store = Store::new(&self.engine, ());
        let mut linker = Linker::new(&self.engine);
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| WasmLoadError::Instantiate(e.to_string()))?;
        Ok((store, instance))
    }

    fn load_diagnostics(&self) -> Result<Vec<Diagnostic>, WasmLoadError> {
        let (mut store, instance) = self.instantiate()?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(WasmLoadError::MissingExport("memory"))?;

        let ptr_func: TypedFunc<(), i32> = instance
            .get_typed_func(&mut store, "diagnostics_ptr")
            .map_err(|_| WasmLoadError::MissingExport("diagnostics_ptr"))?;
        let ptr = ptr_func.call(&mut store, ()).map_err(|e| WasmLoadError::Instantiate(e.to_string()))? as usize;

        // Read a NUL-terminated string with a conservative cap.
        const MAX_BYTES: usize = 1024 * 1024;
        let data = memory.data(&store);
        if ptr >= data.len() {
            return Ok(Vec::new());
        }

        let mut end = ptr;
        while end < data.len() && end - ptr < MAX_BYTES {
            if data[end] == 0 {
                break;
            }
            end += 1;
        }

        let bytes = &data[ptr..end];
        let json = std::str::from_utf8(bytes).map_err(|e| WasmLoadError::Utf8(e.to_string()))?;
        let parsed = serde_json::from_str::<Vec<PluginDiagnostic>>(json)
            .map_err(|e| WasmLoadError::Json(e.to_string()))?;

        Ok(parsed
            .into_iter()
            .map(|diag| {
                let severity = match diag.severity {
                    Some(PluginSeverity::Error) => Severity::Error,
                    Some(PluginSeverity::Warning) => Severity::Warning,
                    Some(PluginSeverity::Info) => Severity::Info,
                    None => Severity::Warning,
                };

                Diagnostic {
                    severity,
                    code: "WASM_EXT",
                    message: diag.message,
                    span: diag.span.map(|span| Span::new(span.start, span.end)),
                }
            })
            .collect())
    }
}

impl<DB: ?Sized + Send + Sync> DiagnosticProvider<DB> for WasmDiagnosticProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn provide_diagnostics(&self, _ctx: ExtensionContext<DB>, _params: DiagnosticParams) -> Vec<Diagnostic> {
        self.load_diagnostics().unwrap_or_default()
    }
}
