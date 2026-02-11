use crate::outcome::{ProviderError, ProviderErrorKind, ProviderResult};
use crate::traits::{
    CodeActionParams, CodeActionProvider, CompletionParams, CompletionProvider, DiagnosticParams,
    DiagnosticProvider, InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider,
};
use crate::types::{CodeAction, InlayHint, NavigationTarget, Symbol};
use crate::{ExtensionContext, ExtensionRegistry};
use nova_core::FileId;
use nova_core::WasmHostDb;
use nova_types::{CompletionItem, Diagnostic, Severity, Span};
use std::sync::OnceLock;
use std::time::Duration;
use wasmtime::{Engine, Instance, Linker, Module, Store, StoreLimitsBuilder, TypedFunc};

use nova_ext_abi::v1::capabilities as abi_caps;
use nova_ext_abi::v1::exports as abi_exports;
use nova_ext_abi::v1::{
    CodeActionV1, CodeActionsRequestV1, CompletionItemV1, CompletionsRequestV1, DiagnosticV1,
    DiagnosticsRequestV1, InlayHintV1, InlayHintsRequestV1, NavigationRequestV1,
    NavigationTargetV1, SeverityV1, SpanV1, SymbolV1,
};
use nova_ext_abi::ABI_V1;

const EXPORT_ABI_VERSION: &str = abi_exports::ABI_VERSION;
const EXPORT_CAPABILITIES: &str = abi_exports::CAPABILITIES;
const EXPORT_MEMORY: &str = abi_exports::MEMORY;
const EXPORT_ALLOC: &str = abi_exports::ALLOC;
const EXPORT_FREE: &str = abi_exports::FREE;

const EXPORT_DIAGNOSTICS: &str = abi_exports::DIAGNOSTICS;
const EXPORT_COMPLETIONS: &str = abi_exports::COMPLETIONS;
const EXPORT_CODE_ACTIONS: &str = abi_exports::CODE_ACTIONS;
const EXPORT_NAVIGATION: &str = abi_exports::NAVIGATION;
const EXPORT_INLAY_HINTS: &str = abi_exports::INLAY_HINTS;

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(50);
const DEFAULT_MAX_MEMORY_BYTES: u64 = 64 * 1024 * 1024; // 64MiB
const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024; // 1MiB
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024; // 1MiB

const EPOCH_TICK: Duration = Duration::from_millis(1);

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        // The default Wasmtime guard regions reserve multiple GiB of virtual address space per
        // instance. Nova runs under a relatively small RLIMIT_AS during CI/agent execution, so we
        // configure the engine to use smaller dynamic memory reservations. Memory usage is still
        // enforced per-plugin via `StoreLimits`.
        config.static_memory_maximum_size(0);
        config.static_memory_guard_size(0);
        config.dynamic_memory_guard_size(0);
        config.dynamic_memory_reserved_for_growth(DEFAULT_MAX_MEMORY_BYTES);

        let engine = Engine::new(&config).expect("wasmtime Engine construction should not fail");

        // A single global epoch-ticker thread is sufficient to support timeouts for all stores
        // created by this engine.
        let ticker_engine = engine.clone();
        std::thread::Builder::new()
            .name("nova-ext-wasm-epoch".to_string())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                ticker_engine.increment_epoch();
            })
            .expect("spawning wasmtime epoch thread should not fail");

        engine
    })
}

fn timeout_to_epoch_deadline(timeout: Duration) -> u64 {
    // `Store::set_epoch_deadline` takes a "tick budget", decremented each time the engine epoch
    // is incremented. With `EPOCH_TICK` configured at 1ms, this approximates a wall-clock timeout.
    let timeout_ms = timeout.as_millis();
    let tick_ms = EPOCH_TICK.as_millis().max(1);
    let ticks = timeout_ms.div_ceil(tick_ms).max(1);
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

fn unpack_ptr_len(v: u64) -> (u32, u32) {
    let ptr = (v & 0xFFFF_FFFF) as u32;
    let len = (v >> 32) as u32;
    (ptr, len)
}

/// Capability bitset exported by a guest module via `nova_ext_capabilities()`.
///
/// # Bit assignments (ABI v1)
///
/// - bit 0 (`1 << 0`): diagnostics (`nova_ext_diagnostics`)
/// - bit 1 (`1 << 1`): completions (`nova_ext_completions`)
/// - bit 2 (`1 << 2`): code actions (`nova_ext_code_actions`)
/// - bit 3 (`1 << 3`): navigation (`nova_ext_navigation`)
/// - bit 4 (`1 << 4`): inlay hints (`nova_ext_inlay_hints`)
///
/// Unknown bits are currently ignored by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WasmCapabilities(u32);

impl WasmCapabilities {
    const KNOWN_MASK: u32 = abi_caps::KNOWN_MASK;

    pub const NONE: Self = Self(0);
    pub const DIAGNOSTICS: Self = Self(abi_caps::DIAGNOSTICS);
    pub const COMPLETIONS: Self = Self(abi_caps::COMPLETIONS);
    pub const CODE_ACTIONS: Self = Self(abi_caps::CODE_ACTIONS);
    pub const NAVIGATION: Self = Self(abi_caps::NAVIGATION);
    pub const INLAY_HINTS: Self = Self(abi_caps::INLAY_HINTS);

    pub const fn from_bits(bits: u32) -> Self {
        Self(bits & Self::KNOWN_MASK)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn iter_known() -> impl Iterator<Item = (Self, &'static str)> {
        [
            (Self::DIAGNOSTICS, EXPORT_DIAGNOSTICS),
            (Self::COMPLETIONS, EXPORT_COMPLETIONS),
            (Self::CODE_ACTIONS, EXPORT_CODE_ACTIONS),
            (Self::NAVIGATION, EXPORT_NAVIGATION),
            (Self::INLAY_HINTS, EXPORT_INLAY_HINTS),
        ]
        .into_iter()
    }
}

#[derive(Clone, Debug)]
pub struct WasmPluginConfig {
    pub timeout: Duration,
    pub max_memory_bytes: u64,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
}

impl Default for WasmPluginConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

#[derive(Debug)]
pub enum WasmLoadError {
    Compile(String),
    Instantiate(String),
    MissingExport(&'static str),
    AbiVersionMismatch { expected: u32, found: u32 },
}

impl std::fmt::Display for WasmLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmLoadError::Compile(msg) => write!(f, "failed to compile wasm module: {msg}"),
            WasmLoadError::Instantiate(msg) => {
                write!(f, "failed to instantiate wasm module: {msg}")
            }
            WasmLoadError::MissingExport(name) => write!(f, "missing required wasm export: {name}"),
            WasmLoadError::AbiVersionMismatch { expected, found } => write!(
                f,
                "unsupported nova-ext wasm ABI version {found} (expected {expected})"
            ),
        }
    }
}

impl std::error::Error for WasmLoadError {}

#[derive(Debug)]
pub enum WasmCallError {
    Instantiate(String),
    MissingExport(&'static str),
    RequestTooLarge {
        len: usize,
        max: usize,
    },
    ResponseTooLarge {
        len: usize,
        max: usize,
    },
    MemoryOutOfBounds {
        ptr: usize,
        len: usize,
        memory_len: usize,
    },
    Timeout(String),
    Trap(String),
    Json(String),
}

impl std::fmt::Display for WasmCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmCallError::Instantiate(msg) => {
                write!(f, "failed to instantiate wasm module: {msg}")
            }
            WasmCallError::MissingExport(name) => write!(f, "missing required wasm export: {name}"),
            WasmCallError::RequestTooLarge { len, max } => {
                write!(f, "wasm request too large ({len} bytes > {max} bytes)")
            }
            WasmCallError::ResponseTooLarge { len, max } => {
                write!(f, "wasm response too large ({len} bytes > {max} bytes)")
            }
            WasmCallError::MemoryOutOfBounds {
                ptr,
                len,
                memory_len,
            } => write!(
                f,
                "wasm response out of bounds (ptr={ptr}, len={len}, memory_len={memory_len})"
            ),
            WasmCallError::Timeout(msg) => write!(f, "wasm execution timed out: {msg}"),
            WasmCallError::Trap(msg) => write!(f, "wasm trap: {msg}"),
            WasmCallError::Json(msg) => write!(f, "wasm invalid json: {msg}"),
        }
    }
}

impl std::error::Error for WasmCallError {}

fn classify_call_error(err: wasmtime::Error) -> WasmCallError {
    // Wasmtime doesn't currently expose a stable, crate-local "timeout" error type. Timeouts via
    // epoch interruption surface as traps in Wasmtime, but the exact error *display* string can vary
    // depending on how much context Wasmtime attaches.
    //
    // To keep this robust across Wasmtime versions, we scan both the Display and Debug
    // representations for "interrupt"/"epoch"/"deadline"/"timeout" indicators.
    let msg = err.to_string();
    let mut haystack = msg.to_ascii_lowercase();
    haystack.push('\n');
    haystack.push_str(&format!("{err:?}").to_ascii_lowercase());

    if haystack.contains("interrupt")
        || haystack.contains("epoch")
        || haystack.contains("deadline")
        || haystack.contains("timeout")
    {
        WasmCallError::Timeout(msg)
    } else {
        WasmCallError::Trap(msg)
    }
}

fn provider_error_from_wasm_call_error(err: WasmCallError) -> ProviderError {
    let kind = match err {
        WasmCallError::Timeout(_) => ProviderErrorKind::Timeout,
        WasmCallError::Trap(_) => ProviderErrorKind::Trap,
        WasmCallError::Json(_)
        | WasmCallError::MissingExport(_)
        | WasmCallError::MemoryOutOfBounds { .. }
        | WasmCallError::RequestTooLarge { .. }
        | WasmCallError::ResponseTooLarge { .. }
        | WasmCallError::Instantiate(_) => ProviderErrorKind::InvalidResponse,
    };

    ProviderError::new(kind, err.to_string())
}

fn sanitize_serde_json_error(err: &serde_json::Error) -> String {
    sanitize_json_error_message(&err.to_string())
}

fn sanitize_json_error_message(message: &str) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (for example:
    // `invalid type: string "..."`). WASM extension request/response payloads can include source
    // text, diagnostics, etc. Conservatively redact quoted substrings to avoid leaking those values
    // through provider errors.
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'"' {
                continue;
            }

            // Treat quotes preceded by an odd number of backslashes as escaped.
            let mut backslashes = 0usize;
            let mut k = idx;
            while k > 0 && bytes[k - 1] == b'\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                end = Some(idx);
                break;
            }
        }

        let Some(end) = end else {
            // Unterminated quote: redact the remainder and stop.
            out.push_str("<redacted>");
            rest = "";
            break;
        };
        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.rfind("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.rfind('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

#[derive(Clone)]
pub struct WasmPlugin {
    id: String,
    module: Module,
    capabilities: WasmCapabilities,
    config: WasmPluginConfig,
}

impl WasmPlugin {
    pub fn from_wasm_bytes(
        id: impl Into<String>,
        bytes: &[u8],
        config: WasmPluginConfig,
    ) -> Result<Self, WasmLoadError> {
        let id = id.into();
        let module = Module::new(engine(), bytes).map_err(|e| {
            tracing::warn!(plugin_id = %id, error = %e, "failed to compile wasm extension");
            WasmLoadError::Compile(e.to_string())
        })?;

        let capabilities = probe_module(&id, &module, &config)?;

        Ok(Self {
            id,
            module,
            capabilities,
            config,
        })
    }

    pub fn from_wat(
        id: impl Into<String>,
        wat_src: &str,
        config: WasmPluginConfig,
    ) -> Result<Self, WasmLoadError> {
        let id = id.into();
        let bytes = wat::parse_str(wat_src).map_err(|e| {
            tracing::warn!(plugin_id = %id, error = %e, "failed to parse WAT for wasm extension");
            WasmLoadError::Compile(e.to_string())
        })?;
        Self::from_wasm_bytes(id, &bytes, config)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn capabilities(&self) -> WasmCapabilities {
        self.capabilities
    }

    pub fn config(&self) -> &WasmPluginConfig {
        &self.config
    }

    fn config_for_ctx<DB: ?Sized + Send + Sync>(
        &self,
        ctx: &ExtensionContext<DB>,
    ) -> WasmPluginConfig {
        let mut config = self.config.clone();
        if let Some(timeout_ms) = ctx.config.extensions.wasm_timeout_ms {
            // Treat `NovaConfig.extensions.wasm_timeout_ms` as a sandbox *upper bound*.
            // This allows tightening, but avoids accidentally relaxing the per-plugin timeout.
            config.timeout = config.timeout.min(Duration::from_millis(timeout_ms));
        }
        if let Some(max_bytes) = ctx.config.extensions.wasm_memory_limit_bytes {
            // Treat `NovaConfig.extensions.wasm_memory_limit_bytes` as a sandbox *upper bound*.
            // This allows tightening, but avoids accidentally relaxing the per-plugin memory cap.
            config.max_memory_bytes = config.max_memory_bytes.min(max_bytes);
        }
        config
    }

    /// Register all implemented capabilities with an [`ExtensionRegistry`].
    pub fn register<DB>(
        self: &std::sync::Arc<Self>,
        registry: &mut ExtensionRegistry<DB>,
    ) -> Result<(), crate::RegisterError>
    where
        DB: ?Sized + Send + Sync + WasmHostDb + 'static,
    {
        if self.capabilities.contains(WasmCapabilities::DIAGNOSTICS) {
            registry.register_diagnostic_provider(
                std::sync::Arc::clone(self) as std::sync::Arc<dyn DiagnosticProvider<DB>>
            )?;
        }
        if self.capabilities.contains(WasmCapabilities::COMPLETIONS) {
            registry.register_completion_provider(
                std::sync::Arc::clone(self) as std::sync::Arc<dyn CompletionProvider<DB>>
            )?;
        }
        if self.capabilities.contains(WasmCapabilities::CODE_ACTIONS) {
            registry.register_code_action_provider(
                std::sync::Arc::clone(self) as std::sync::Arc<dyn CodeActionProvider<DB>>
            )?;
        }
        if self.capabilities.contains(WasmCapabilities::NAVIGATION) {
            registry.register_navigation_provider(
                std::sync::Arc::clone(self) as std::sync::Arc<dyn NavigationProvider<DB>>
            )?;
        }
        if self.capabilities.contains(WasmCapabilities::INLAY_HINTS) {
            registry.register_inlay_hint_provider(
                std::sync::Arc::clone(self) as std::sync::Arc<dyn InlayHintProvider<DB>>
            )?;
        }
        Ok(())
    }

    fn call_vec<Req, Out>(
        &self,
        config: &WasmPluginConfig,
        export: &'static str,
        request: &Req,
    ) -> Result<Vec<Out>, WasmCallError>
    where
        Req: serde::Serialize,
        Out: for<'de> serde::Deserialize<'de>,
    {
        let req_bytes =
            serde_json::to_vec(request).map_err(|e| WasmCallError::Json(sanitize_serde_json_error(&e)))?;
        if req_bytes.len() > config.max_request_bytes {
            return Err(WasmCallError::RequestTooLarge {
                len: req_bytes.len(),
                max: config.max_request_bytes,
            });
        }

        let mut store = new_store(config);
        let instance = instantiate(&mut store, &self.module)
            .map_err(|e| WasmCallError::Instantiate(e.to_string()))?;

        let memory = instance
            .get_memory(&mut store, EXPORT_MEMORY)
            .ok_or(WasmCallError::MissingExport(EXPORT_MEMORY))?;

        let alloc: TypedFunc<i32, i32> = instance
            .get_typed_func(&mut store, EXPORT_ALLOC)
            .map_err(|_| WasmCallError::MissingExport(EXPORT_ALLOC))?;
        let free: TypedFunc<(i32, i32), ()> = instance
            .get_typed_func(&mut store, EXPORT_FREE)
            .map_err(|_| WasmCallError::MissingExport(EXPORT_FREE))?;

        let func: TypedFunc<(i32, i32), i64> = instance
            .get_typed_func(&mut store, export)
            .map_err(|_| WasmCallError::MissingExport(export))?;

        let req_len_i32 = i32::try_from(req_bytes.len()).unwrap_or(i32::MAX);
        let req_ptr_i32 = alloc
            .call(&mut store, req_len_i32)
            .map_err(classify_call_error)?;
        if req_ptr_i32 == 0 && req_len_i32 != 0 {
            return Err(WasmCallError::Trap(
                "nova_ext_alloc returned a null pointer".to_string(),
            ));
        }
        let req_ptr = req_ptr_i32 as u32 as usize;

        memory
            .write(&mut store, req_ptr, &req_bytes)
            .map_err(|e| WasmCallError::Trap(e.to_string()))?;

        let ret = func
            .call(&mut store, (req_ptr as i32, req_len_i32))
            .map_err(classify_call_error)?;

        // Always attempt to free the request buffer.
        let _ = free.call(&mut store, (req_ptr_i32, req_len_i32));

        let (resp_ptr, resp_len) = unpack_ptr_len(ret as u64);
        if resp_len == 0 {
            return Ok(Vec::new());
        }
        if resp_ptr == 0 {
            return Err(WasmCallError::Trap(
                "wasm returned a null response pointer with non-zero length".to_string(),
            ));
        }

        let resp_len_usize = usize::try_from(resp_len).unwrap_or(usize::MAX);
        if resp_len_usize > config.max_response_bytes {
            return Err(WasmCallError::ResponseTooLarge {
                len: resp_len_usize,
                max: config.max_response_bytes,
            });
        }

        let resp_ptr_usize = resp_ptr as usize;
        let data = memory.data(&store);
        let end = resp_ptr_usize.saturating_add(resp_len_usize);
        let bytes = data
            .get(resp_ptr_usize..end)
            .ok_or(WasmCallError::MemoryOutOfBounds {
                ptr: resp_ptr_usize,
                len: resp_len_usize,
                memory_len: data.len(),
            })?
            .to_vec();

        // Free response memory according to the ABI contract.
        let _ = free.call(&mut store, (resp_ptr as i32, resp_len as i32));

        serde_json::from_slice::<Vec<Out>>(&bytes)
            .map_err(|e| WasmCallError::Json(sanitize_serde_json_error(&e)))
    }

    fn call_diagnostics_v1(
        &self,
        config: &WasmPluginConfig,
        req: DiagnosticsRequestV1,
    ) -> Result<Vec<DiagnosticV1>, WasmCallError> {
        self.call_vec(config, EXPORT_DIAGNOSTICS, &req)
    }

    fn call_completions_v1(
        &self,
        config: &WasmPluginConfig,
        req: CompletionsRequestV1,
    ) -> Result<Vec<CompletionItemV1>, WasmCallError> {
        self.call_vec(config, EXPORT_COMPLETIONS, &req)
    }

    fn call_code_actions_v1(
        &self,
        config: &WasmPluginConfig,
        req: CodeActionsRequestV1,
    ) -> Result<Vec<CodeActionV1>, WasmCallError> {
        self.call_vec(config, EXPORT_CODE_ACTIONS, &req)
    }

    fn call_navigation_v1(
        &self,
        config: &WasmPluginConfig,
        req: NavigationRequestV1,
    ) -> Result<Vec<NavigationTargetV1>, WasmCallError> {
        self.call_vec(config, EXPORT_NAVIGATION, &req)
    }

    fn call_inlay_hints_v1(
        &self,
        config: &WasmPluginConfig,
        req: InlayHintsRequestV1,
    ) -> Result<Vec<InlayHintV1>, WasmCallError> {
        self.call_vec(config, EXPORT_INLAY_HINTS, &req)
    }
}

fn new_store(config: &WasmPluginConfig) -> Store<StoreState> {
    let mut store = Store::new(engine(), StoreState::new(config));
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(timeout_to_epoch_deadline(config.timeout));
    store
}

fn instantiate(
    store: &mut Store<StoreState>,
    module: &Module,
) -> Result<Instance, wasmtime::Error> {
    // No WASI, no host functions by default.
    let linker = Linker::new(engine());
    linker.instantiate(store, module)
}

struct StoreState {
    limits: wasmtime::StoreLimits,
}

impl StoreState {
    fn new(config: &WasmPluginConfig) -> Self {
        let max_memory_bytes = usize::try_from(config.max_memory_bytes).unwrap_or(usize::MAX);
        let limits = StoreLimitsBuilder::new()
            .memory_size(max_memory_bytes)
            .build();
        Self { limits }
    }
}

fn probe_module(
    id: &str,
    module: &Module,
    config: &WasmPluginConfig,
) -> Result<WasmCapabilities, WasmLoadError> {
    let mut store = new_store(config);
    let instance = instantiate(&mut store, module).map_err(|e| {
        tracing::warn!(plugin_id = %id, error = %e, "failed to instantiate wasm extension for probing");
        WasmLoadError::Instantiate(e.to_string())
    })?;

    // Required exports for all modules.
    instance
        .get_memory(&mut store, EXPORT_MEMORY)
        .ok_or(WasmLoadError::MissingExport(EXPORT_MEMORY))?;
    instance
        .get_typed_func::<i32, i32>(&mut store, EXPORT_ALLOC)
        .map_err(|_| WasmLoadError::MissingExport(EXPORT_ALLOC))?;
    instance
        .get_typed_func::<(i32, i32), ()>(&mut store, EXPORT_FREE)
        .map_err(|_| WasmLoadError::MissingExport(EXPORT_FREE))?;

    let abi_version_func: TypedFunc<(), i32> = instance
        .get_typed_func(&mut store, EXPORT_ABI_VERSION)
        .map_err(|_| WasmLoadError::MissingExport(EXPORT_ABI_VERSION))?;
    let found_version = abi_version_func
        .call(&mut store, ())
        .map_err(|e| WasmLoadError::Instantiate(e.to_string()))? as u32;
    if found_version != ABI_V1 {
        tracing::warn!(
            plugin_id = %id,
            expected = ABI_V1,
            found = found_version,
            "wasm extension ABI version mismatch"
        );
        return Err(WasmLoadError::AbiVersionMismatch {
            expected: ABI_V1,
            found: found_version,
        });
    }

    let caps_func: TypedFunc<(), i32> = instance
        .get_typed_func(&mut store, EXPORT_CAPABILITIES)
        .map_err(|_| WasmLoadError::MissingExport(EXPORT_CAPABILITIES))?;
    let caps_bits = caps_func
        .call(&mut store, ())
        .map_err(|e| WasmLoadError::Instantiate(e.to_string()))? as u32;
    let capabilities = WasmCapabilities::from_bits(caps_bits);

    // Validate declared capability exports.
    for (cap, export) in WasmCapabilities::iter_known() {
        if !capabilities.contains(cap) {
            continue;
        }
        instance
            .get_typed_func::<(i32, i32), i64>(&mut store, export)
            .map_err(|_| WasmLoadError::MissingExport(export))?;
    }

    Ok(capabilities)
}

fn span_to_v1(span: Span) -> SpanV1 {
    SpanV1 {
        start: span.start,
        end: span.end,
    }
}

fn severity_from_v1(sev: Option<SeverityV1>) -> Severity {
    match sev {
        Some(SeverityV1::Error) => Severity::Error,
        Some(SeverityV1::Warning) | None => Severity::Warning,
        Some(SeverityV1::Info) => Severity::Info,
    }
}

fn symbol_to_v1(symbol: Symbol) -> SymbolV1 {
    match symbol {
        Symbol::File(file) => SymbolV1::File(file.to_raw()),
        Symbol::Class(class) => SymbolV1::Class(class.to_raw()),
    }
}

impl<DB> DiagnosticProvider<DB> for WasmPlugin
where
    DB: ?Sized + Send + Sync + WasmHostDb,
{
    fn id(&self) -> &str {
        self.id()
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        self.try_provide_diagnostics(ctx, params)
            .unwrap_or_else(|_| Vec::new())
    }

    fn try_provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> ProviderResult<Vec<Diagnostic>> {
        if !self.capabilities.contains(WasmCapabilities::DIAGNOSTICS) {
            return Ok(Vec::new());
        }

        let file_path = ctx
            .db
            .file_path(params.file)
            .map(|p| p.to_string_lossy().into_owned());
        let req = DiagnosticsRequestV1 {
            project_id: ctx.project.to_raw(),
            file_id: params.file.to_raw(),
            file_path,
            text: ctx.db.file_text(params.file).to_string(),
        };

        let config = self.config_for_ctx(&ctx);
        let diags = self
            .call_diagnostics_v1(&config, req)
            .map_err(provider_error_from_wasm_call_error)?;

        Ok(diags
            .into_iter()
            .map(|diag| Diagnostic {
                severity: severity_from_v1(diag.severity),
                code: diag.code.map_or_else(|| "WASM_EXT".into(), Into::into),
                message: diag.message,
                span: diag.span.map(|s| Span::new(s.start, s.end)),
            })
            .collect())
    }
}

impl<DB> CompletionProvider<DB> for WasmPlugin
where
    DB: ?Sized + Send + Sync + WasmHostDb,
{
    fn id(&self) -> &str {
        self.id()
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        self.try_provide_completions(ctx, params)
            .unwrap_or_else(|_| Vec::new())
    }

    fn try_provide_completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> ProviderResult<Vec<CompletionItem>> {
        if !self.capabilities.contains(WasmCapabilities::COMPLETIONS) {
            return Ok(Vec::new());
        }

        let req = CompletionsRequestV1 {
            project_id: ctx.project.to_raw(),
            file_id: params.file.to_raw(),
            file_path: ctx
                .db
                .file_path(params.file)
                .map(|path| path.to_string_lossy().into_owned()),
            offset: params.offset,
            text: ctx.db.file_text(params.file).to_string(),
        };

        let config = self.config_for_ctx(&ctx);
        let items = self
            .call_completions_v1(&config, req)
            .map_err(provider_error_from_wasm_call_error)?;

        Ok(items
            .into_iter()
            .map(|item| CompletionItem {
                label: item.label,
                detail: item.detail,
                replace_span: None,
            })
            .collect())
    }
}

impl<DB> CodeActionProvider<DB> for WasmPlugin
where
    DB: ?Sized + Send + Sync + WasmHostDb,
{
    fn id(&self) -> &str {
        self.id()
    }

    fn provide_code_actions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CodeActionParams,
    ) -> Vec<CodeAction> {
        self.try_provide_code_actions(ctx, params)
            .unwrap_or_else(|_| Vec::new())
    }

    fn try_provide_code_actions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CodeActionParams,
    ) -> ProviderResult<Vec<CodeAction>> {
        if !self.capabilities.contains(WasmCapabilities::CODE_ACTIONS) {
            return Ok(Vec::new());
        }

        let req = CodeActionsRequestV1 {
            project_id: ctx.project.to_raw(),
            file_id: params.file.to_raw(),
            file_path: ctx
                .db
                .file_path(params.file)
                .map(|path| path.to_string_lossy().into_owned()),
            span: params.span.map(span_to_v1),
            text: ctx.db.file_text(params.file).to_string(),
        };

        let config = self.config_for_ctx(&ctx);
        let actions = self
            .call_code_actions_v1(&config, req)
            .map_err(provider_error_from_wasm_call_error)?;

        Ok(actions
            .into_iter()
            .map(|action| CodeAction {
                title: action.title,
                kind: action.kind,
            })
            .collect())
    }
}

impl<DB> NavigationProvider<DB> for WasmPlugin
where
    DB: ?Sized + Send + Sync + WasmHostDb,
{
    fn id(&self) -> &str {
        self.id()
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<DB>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        self.try_provide_navigation(ctx, params)
            .unwrap_or_else(|_| Vec::new())
    }

    fn try_provide_navigation(
        &self,
        ctx: ExtensionContext<DB>,
        params: NavigationParams,
    ) -> ProviderResult<Vec<NavigationTarget>> {
        if !self.capabilities.contains(WasmCapabilities::NAVIGATION) {
            return Ok(Vec::new());
        }

        let req = NavigationRequestV1 {
            project_id: ctx.project.to_raw(),
            symbol: symbol_to_v1(params.symbol),
        };

        let config = self.config_for_ctx(&ctx);
        let targets = self
            .call_navigation_v1(&config, req)
            .map_err(provider_error_from_wasm_call_error)?;

        Ok(targets
            .into_iter()
            .map(|target| NavigationTarget {
                file: FileId::from_raw(target.file_id),
                span: target.span.map(|s| Span::new(s.start, s.end)),
                label: target.label,
            })
            .collect())
    }
}

impl<DB> InlayHintProvider<DB> for WasmPlugin
where
    DB: ?Sized + Send + Sync + WasmHostDb,
{
    fn id(&self) -> &str {
        self.id()
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<DB>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        self.try_provide_inlay_hints(ctx, params)
            .unwrap_or_else(|_| Vec::new())
    }

    fn try_provide_inlay_hints(
        &self,
        ctx: ExtensionContext<DB>,
        params: InlayHintParams,
    ) -> ProviderResult<Vec<InlayHint>> {
        if !self.capabilities.contains(WasmCapabilities::INLAY_HINTS) {
            return Ok(Vec::new());
        }

        let req = InlayHintsRequestV1 {
            project_id: ctx.project.to_raw(),
            file_id: params.file.to_raw(),
            file_path: ctx
                .db
                .file_path(params.file)
                .map(|path| path.to_string_lossy().into_owned()),
            text: ctx.db.file_text(params.file).to_string(),
        };

        let config = self.config_for_ctx(&ctx);
        let hints = self
            .call_inlay_hints_v1(&config, req)
            .map_err(provider_error_from_wasm_call_error)?;

        Ok(hints
            .into_iter()
            .map(|hint| InlayHint {
                span: hint.span.map(|s| Span::new(s.start, s.end)),
                label: hint.label,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_call_error_json_does_not_echo_string_values() {
        let secret_suffix = "nova-wasm-secret-token";
        let secret = format!("prefix\"{secret_suffix}");
        let err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let message = sanitize_serde_json_error(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized serde_json error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized serde_json error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn wasm_call_error_json_does_not_echo_backticked_values() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-wasm-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");

        let message = sanitize_serde_json_error(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized serde_json error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized serde_json error message to include redaction marker: {message}"
        );
    }
}
