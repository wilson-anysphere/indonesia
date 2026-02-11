use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use nova_ai::{AiClient, NovaAi};
use nova_bugreport::{
    global_crash_store, install_panic_hook, BugReportBuilder, BugReportOptions, PanicHookConfig,
    PerfStats,
};
use nova_cache::{
    atomic_write, fetch_cache_package, install_cache_package, pack_cache_package, CacheConfig,
    CacheDir, CacheGcPolicy, CachePackageInstallOutcome,
};
use nova_config::{global_log_buffer, init_tracing_with_config, NovaConfig, NOVA_CONFIG_ENV_VAR};
use nova_core::{
    apply_text_edits as apply_core_text_edits, LineIndex, Position, Range,
    TextEdit as CoreTextEdit, TextSize,
};
mod diagnostics_output;
mod extensions;
mod refactor_apply;
use diagnostics_output::{print_github_annotations, print_sarif, write_sarif, DiagnosticsFormat};
use nova_deps_cache::DependencyIndexStore;
use nova_format::{edits_for_document_formatting, edits_for_range_formatting, FormatConfig};
use nova_perf::{
    compare_runs, compare_runtime_runs, load_criterion_directory, BenchRun, RuntimeRun,
    RuntimeThresholdConfig, ThresholdConfig,
};
use nova_refactor::{
    apply_text_edits as apply_refactor_text_edits, apply_workspace_edit,
    organize_imports as refactor_organize_imports, rename as refactor_rename, Conflict,
    FileId as RefactorFileId, FileOp, OrganizeImportsParams, RefactorJavaDatabase, RenameParams,
    SemanticRefactorError, TextDatabase, WorkspaceEdit, WorkspaceTextEdit as RefactorTextEdit,
};
use nova_router::{DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, WorkspaceLayout};
use nova_syntax::parse;
use nova_workspace::{
    CacheStatus, DiagnosticsReport, IndexReport, ParseResult, PerfMetrics, Workspace,
    WorkspaceSymbol,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(
    name = "nova",
    version,
    about = "Nova CLI (indexing, diagnostics, cache, perf, server launcher)"
)]
struct Cli {
    /// Optional path to a TOML config file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the Nova language server (LSP) by spawning `nova-lsp`.
    ///
    /// With no additional arguments, this defaults to `nova-lsp --stdio`.
    ///
    /// To see available `nova-lsp` flags (including distributed-mode flags), run
    /// `nova lsp -- --help` or `nova-lsp --help`.
    Lsp(LspLauncherArgs),
    /// Launch the Nova debug adapter (DAP) by spawning `nova-dap`.
    Dap(DapLauncherArgs),
    /// Load a project and build indexes/caches
    Index(IndexArgs),
    /// Run diagnostics for an entire project or a single file
    Diagnostics(DiagnosticsArgs),
    /// Workspace symbol search (defaults to current directory)
    Symbols(SymbolsArgs),
    /// Manage global dependency (JAR/JMOD) indexes
    Deps(DepsArgs),
    /// Manage persistent cache (defaults to `~/.nova/cache/<project-hash>/`, override with `NOVA_CACHE_DIR`)
    Cache(CacheArgs),
    /// Performance tools (cached perf report + benchmark comparison)
    Perf(PerfArgs),
    /// Print a debug parse tree / errors for a single file
    Parse(ParseArgs),
    /// Format a Java file using Nova's formatter
    Format(FormatArgs),
    /// Organize Java imports for a single file
    OrganizeImports(OrganizeImportsArgs),
    /// Semantic refactoring commands
    Refactor(RefactorArgs),
    /// Generate a diagnostic bundle (logs/config/crash reports) for troubleshooting.
    #[command(name = "bugreport")]
    BugReport(BugReportArgs),
    /// Inspect and validate WASM extension bundles.
    Extensions(extensions::ExtensionsArgs),
    /// Local AI utilities (Ollama / OpenAI-compatible endpoints)
    Ai(AiArgs),
}

#[derive(Args)]
struct AiArgs {
    #[command(subcommand)]
    command: AiCommand,
}

#[derive(Subcommand)]
enum AiCommand {
    /// List models (best effort) or validate backend connectivity.
    Models(AiModelsArgs),
    /// Print effective AI configuration + feature gating without starting the LSP.
    Status(AiStatusArgs),
    /// Review a unified diff (from stdin, a file, or `git diff`) using the configured AI backend.
    Review(AiReviewArgs),
    /// Run an offline semantic-search query against the configured semantic-search engine.
    #[command(name = "semantic-search")]
    SemanticSearch(AiSemanticSearchArgs),
}

#[derive(Args)]
struct AiModelsArgs {
    /// Workspace root / target directory (defaults to current directory).
    ///
    /// This is used for config discovery.
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct AiReviewArgs {
    /// Workspace root / target directory (defaults to current directory).
    ///
    /// This is used for config discovery and as the working directory for `--git`.
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Read a unified diff from this file instead of stdin.
    ///
    /// If the path is relative, it is resolved against `--path`.
    #[arg(long, conflicts_with = "git")]
    diff_file: Option<PathBuf>,

    /// Use `git diff` to generate the diff input (best-effort).
    #[arg(long, conflicts_with = "diff_file")]
    git: bool,

    /// With `--git`, review staged changes (`git diff --staged`).
    #[arg(long, requires = "git")]
    staged: bool,

    /// Emit JSON suitable for scripting: `{ "review": "..." }`.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct AiStatusArgs {
    /// Workspace root / target directory (defaults to current directory).
    ///
    /// This is used for config discovery.
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct AiSemanticSearchArgs {
    /// Search query text.
    query: String,

    /// Workspace root / target directory to index (defaults to current directory).
    ///
    /// This is used for config discovery and as the root for scanning project files.
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Maximum number of results to return (clamped to 50).
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Emit machine-readable JSON: `{ "results": [...] }`.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct LspLauncherArgs {
    /// Optional path to the `nova-lsp` binary.
    ///
    /// If unset, `nova` will first try to resolve `nova-lsp` on $PATH, then fall back to looking
    /// for a `nova-lsp` binary adjacent to the running `nova` executable.
    #[arg(long = "nova-lsp", visible_alias = "path")]
    nova_lsp: Option<PathBuf>,

    /// Arguments to pass through to `nova-lsp`.
    ///
    /// Use `--` to disambiguate flags intended for `nova-lsp` from `nova lsp` flags:
    /// `nova lsp -- --config nova.toml`.
    ///
    /// Examples:
    /// `nova lsp -- --help`
    /// `nova lsp -- --distributed`
    /// `nova lsp -- --distributed-worker-command /path/to/nova-worker`
    #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct DapLauncherArgs {
    /// Optional path to the `nova-dap` binary.
    ///
    /// If unset, `nova` will first try to resolve `nova-dap` on $PATH, then fall back to looking
    /// for a `nova-dap` binary adjacent to the running `nova` executable.
    #[arg(long = "nova-dap", visible_alias = "path")]
    nova_dap: Option<PathBuf>,

    /// Arguments to pass through to `nova-dap`.
    ///
    /// Use `--` to disambiguate flags intended for `nova-dap` from `nova dap` flags:
    /// `nova dap -- --config nova.toml`.
    #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct IndexArgs {
    /// Path to a project directory (or a file within it)
    path: PathBuf,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct DiagnosticsArgs {
    /// Path to a project directory or a single file
    path: PathBuf,
    /// Output format (`human`, `json`, `github`, `sarif`)
    #[arg(long, value_enum)]
    format: Option<DiagnosticsFormat>,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
    /// Write SARIF v2.1.0 output to the given path.
    ///
    /// This is independent of `--format`: you can keep human output on stdout
    /// while capturing SARIF for GitHub code scanning.
    #[arg(long)]
    sarif_out: Option<PathBuf>,
}

#[derive(Args)]
struct SymbolsArgs {
    /// Substring query to match against indexed symbols
    query: String,
    /// Workspace root (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Maximum number of symbols to return
    #[arg(long, default_value_t = 200)]
    limit: usize,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
    /// Use the experimental distributed router/worker stack for indexing + symbol search.
    ///
    /// If distributed mode fails (e.g. `nova-worker` is unavailable), the CLI falls back to the
    /// default in-process implementation.
    #[arg(long)]
    distributed: bool,
    /// Path to the `nova-worker` binary when using `--distributed`.
    ///
    /// If unset, `nova` will first look for a `nova-worker` binary adjacent to the running `nova`
    /// executable, then fall back to resolving `nova-worker` on $PATH.
    #[arg(long)]
    distributed_worker_command: Option<PathBuf>,
}

#[derive(Args)]
struct CacheArgs {
    #[command(subcommand)]
    command: CacheCommand,
}

#[derive(Args)]
struct DepsArgs {
    #[command(subcommand)]
    command: DepsCommand,
}

#[derive(Subcommand)]
enum DepsCommand {
    /// Pre-build and store a dependency index bundle for a JAR/JMOD.
    Index { jar: PathBuf },
    /// Pack the global dependency index store into a .tar.gz archive.
    Pack { output: PathBuf },
    /// Install dependency index bundles from a .tar.gz archive.
    Install { archive: PathBuf },
}

#[derive(Subcommand)]
enum CacheCommand {
    Clean(WorkspaceArgs),
    Status(WorkspaceArgs),
    Warm(WorkspaceArgs),

    /// Garbage collect global per-project caches under `~/.nova/cache` (or `NOVA_CACHE_DIR`).
    ///
    /// This never touches `deps/` (the shared dependency cache).
    Gc(CacheGcArgs),

    /// List global per-project caches under `~/.nova/cache` (or `NOVA_CACHE_DIR`).
    ///
    /// This excludes `deps/` (the shared dependency cache).
    List(CacheListArgs),

    /// Package a project's persistent cache directory into a single tar.zst archive.
    Pack(CachePackArgs),
    /// Install a packaged cache archive for a project.
    Install(CacheInstallArgs),
    /// Fetch a cache package from a URL (http/https/file/s3) and install it.
    Fetch(CacheFetchArgs),
}

#[derive(Args)]
struct CacheGcArgs {
    /// Maximum total bytes for all per-project caches (excluding `deps/`).
    #[arg(long)]
    max_total_bytes: u64,

    /// Optional maximum age in milliseconds. Caches older than this are removed first.
    #[arg(long)]
    max_age_ms: Option<u64>,

    /// Number of most-recently-updated caches to always keep.
    #[arg(long, default_value_t = 1)]
    keep_latest_n: usize,

    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CacheListArgs {
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CachePackArgs {
    /// Workspace root (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Output archive path (.tar.zst recommended).
    #[arg(long)]
    out: PathBuf,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CacheInstallArgs {
    /// Workspace root (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Cache package file (.tar.zst).
    package: PathBuf,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CacheFetchArgs {
    /// Workspace root (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// URL to fetch (http(s)://..., file://..., s3://...).
    url: String,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct PerfArgs {
    #[command(subcommand)]
    command: PerfCommand,
}

#[derive(Subcommand)]
enum PerfCommand {
    /// Print cached perf metrics captured during indexing (`nova index ...`).
    Report(WorkspaceArgs),
    /// Convert a `criterion` output directory into a compact JSON summary.
    Capture {
        /// Path to `target/criterion`.
        #[arg(long)]
        criterion_dir: PathBuf,
        /// Path to write the output JSON file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Compare two benchmark runs and fail if configured regression thresholds are exceeded.
    Compare {
        /// Baseline run JSON file OR a `criterion` directory.
        #[arg(long)]
        baseline: PathBuf,
        /// Current run JSON file OR a `criterion` directory.
        #[arg(long)]
        current: PathBuf,
        /// Optional thresholds config (TOML).
        #[arg(long = "thresholds-config")]
        thresholds_config: Option<PathBuf>,
        /// Allow regressions for these benchmark IDs (repeatable).
        #[arg(long)]
        allow: Vec<String>,
        /// Optional path to write the markdown report.
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    /// Convert a `nova-workspace` `perf.json` file into a compact JSON snapshot suitable for CI diffing.
    CaptureRuntime {
        /// Path to a workspace cache root (directory containing `perf.json`) or a `perf.json` file.
        #[arg(long)]
        workspace_cache: PathBuf,
        /// Path to write the output JSON file.
        #[arg(long)]
        out: PathBuf,
        /// Optional path to a `nova-lsp` binary. When provided, `nova-cli` will query
        /// `nova/memoryStatus` and include memory/startup metrics in the snapshot.
        #[arg(long)]
        nova_lsp: Option<PathBuf>,
    },
    /// Compare two runtime runs and fail if configured regression thresholds are exceeded.
    CompareRuntime {
        /// Baseline runtime run JSON file.
        #[arg(long)]
        baseline: PathBuf,
        /// Current runtime run JSON file.
        #[arg(long)]
        current: PathBuf,
        /// Optional runtime thresholds config (TOML).
        #[arg(long = "thresholds-config")]
        thresholds_config: Option<PathBuf>,
        /// Allow regressions for these metric IDs (repeatable).
        #[arg(long)]
        allow: Vec<String>,
        /// Optional path to write the markdown report.
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
}

#[derive(Args, Clone)]
struct WorkspaceArgs {
    /// Workspace root (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ParseArgs {
    /// File to parse
    file: PathBuf,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct BugReportArgs {
    /// Maximum number of log lines to include in the bundle.
    #[arg(long, default_value_t = 500)]
    max_log_lines: usize,

    /// Path to a file containing reproduction steps (will be copied into the bundle).
    #[arg(long, conflicts_with = "repro_text")]
    repro: Option<PathBuf>,

    /// Reproduction steps as plain text (will be copied into the bundle).
    #[arg(long, conflicts_with = "repro")]
    repro_text: Option<String>,

    /// Output directory (defaults to a temp directory).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Also write a `.zip` archive alongside the output directory.
    #[arg(long)]
    archive: bool,

    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct FormatArgs {
    /// File to format
    file: PathBuf,
    /// Apply changes to the file on disk (atomic write).
    #[arg(long)]
    in_place: bool,
    /// Optional formatting range: `<startLine:startCol-endLine:endCol>` (1-based, UTF-16 columns).
    #[arg(long)]
    range: Option<String>,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct OrganizeImportsArgs {
    /// File to update
    file: PathBuf,
    /// Apply changes to the file on disk (atomic write).
    #[arg(long)]
    in_place: bool,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RefactorArgs {
    #[command(subcommand)]
    command: RefactorCommand,
}

#[derive(Subcommand)]
enum RefactorCommand {
    Rename(RenameArgs),
}

#[derive(Args)]
struct RenameArgs {
    /// File containing the symbol to rename
    file: PathBuf,
    /// 1-based line number (UTF-16 columns)
    #[arg(long)]
    line: u32,
    /// 1-based column number (UTF-16 columns)
    #[arg(long)]
    col: u32,
    /// New identifier name
    #[arg(long)]
    new_name: String,
    /// Apply changes to the file(s) on disk (atomic write per file).
    #[arg(long)]
    in_place: bool,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}
fn main() {
    let cli = Cli::parse();

    // `nova lsp` and `nova dap` are thin launchers intended for editors.
    //
    // Important: we intentionally do *not* load/validate the global config here. The underlying
    // servers already support `--config <path>` and have their own config error handling; eagerly
    // parsing config in the CLI wrapper would change behavior (e.g. failing fast instead of
    // continuing with defaults).
    match &cli.command {
        Command::Lsp(args) => {
            let exit_code = match run_lsp_launcher(args, cli.config.as_deref()) {
                Ok(code) => code,
                Err(err) => {
                    eprintln!("{}", sanitize_anyhow_error_message(&err));
                    2
                }
            };
            std::process::exit(exit_code);
        }
        Command::Dap(args) => {
            let exit_code = match run_dap_launcher(args, cli.config.as_deref()) {
                Ok(code) => code,
                Err(err) => {
                    eprintln!("{}", sanitize_anyhow_error_message(&err));
                    2
                }
            };
            std::process::exit(exit_code);
        }
        _ => {}
    }

    let config = load_config_from_cli(&cli);

    let _ = init_tracing_with_config(&config);
    install_panic_hook(
        PanicHookConfig {
            include_backtrace: config.logging.include_backtrace,
            ..Default::default()
        },
        Arc::new(|_| {}),
    );

    let exit_code = match run(cli, &config) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{}", sanitize_anyhow_error_message(&err));
            2
        }
    };

    std::process::exit(exit_code);
}

fn load_config_from_cli(cli: &Cli) -> NovaConfig {
    let mut config = if let Some(path) = cli.config.as_ref() {
        let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
        env::set_var(NOVA_CONFIG_ENV_VAR, &resolved);
        match NovaConfig::load_from_path(&resolved)
            .with_context(|| format!("load config from {}", resolved.display()))
        {
            Ok(config) => config,
            Err(err) => {
                eprintln!("{}", sanitize_anyhow_error_message(&err));
                std::process::exit(2);
            }
        }
    } else {
        let workspace_root = workspace_root_for_config_discovery(cli);
        match nova_config::load_for_workspace(&workspace_root)
            .with_context(|| format!("load config for workspace {}", workspace_root.display()))
        {
            Ok((config, path)) => {
                if let Some(path) = path {
                    env::set_var(NOVA_CONFIG_ENV_VAR, &path);
                }
                config
            }
            Err(err) => {
                eprintln!("{}", sanitize_anyhow_error_message(&err));
                std::process::exit(2);
            }
        }
    };

    apply_ai_env_overrides(&mut config);
    config
}

fn apply_ai_env_overrides(config: &mut NovaConfig) {
    fn env_truthy(name: &str) -> bool {
        matches!(
            std::env::var(name).as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        )
    }

    // Keep in sync with `nova-lsp` server-side AI overrides.
    let disable_ai = env_truthy("NOVA_DISABLE_AI");
    let disable_ai_completions = env_truthy("NOVA_DISABLE_AI_COMPLETIONS");
    let disable_ai_code_actions = env_truthy("NOVA_DISABLE_AI_CODE_ACTIONS");
    let disable_ai_code_review = env_truthy("NOVA_DISABLE_AI_CODE_REVIEW");

    if disable_ai {
        config.ai.enabled = false;
        config.ai.features.completion_ranking = false;
        config.ai.features.semantic_search = false;
        config.ai.features.multi_token_completion = false;
        config.ai.features.explain_errors = false;
        config.ai.features.code_actions = false;
        config.ai.features.code_review = false;
    } else if disable_ai_completions {
        config.ai.features.completion_ranking = false;
        config.ai.features.multi_token_completion = false;
    }

    if disable_ai_code_actions {
        config.ai.features.explain_errors = false;
        config.ai.features.code_actions = false;
    }

    if disable_ai_code_review {
        config.ai.features.code_review = false;
    }
}

fn workspace_root_for_config_discovery(cli: &Cli) -> PathBuf {
    fn root_with_explicit_config_env(start: &Path, stop_at: &Path) -> Option<PathBuf> {
        let value = env::var_os(NOVA_CONFIG_ENV_VAR)?;
        if value.is_empty() {
            return None;
        }
        let candidate = PathBuf::from(value);
        if candidate.is_absolute() {
            // Absolute `NOVA_CONFIG_PATH` doesn't depend on workspace-root resolution.
            return None;
        }

        let mut dir = start;
        loop {
            if dir.join(&candidate).is_file() {
                return Some(dir.to_path_buf());
            }

            if dir == stop_at {
                break;
            }

            let Some(parent) = dir.parent() else {
                break;
            };
            if parent == dir {
                break;
            }
            dir = parent;
        }

        None
    }

    fn root_with_config_marker(start: &Path, stop_at: &Path) -> Option<PathBuf> {
        const CANDIDATES: [&str; 4] = [
            "nova.toml",
            ".nova.toml",
            "nova.config.toml",
            // Legacy workspace-local config (kept for backwards compatibility).
            ".nova/config.toml",
        ];

        let mut dir = start;
        loop {
            if CANDIDATES.iter().any(|name| dir.join(name).is_file()) {
                return Some(dir.to_path_buf());
            }

            if dir == stop_at {
                break;
            }

            let Some(parent) = dir.parent() else {
                break;
            };
            if parent == dir {
                break;
            }
            dir = parent;
        }

        None
    }

    fn start_dir(path: &Path) -> PathBuf {
        match fs::metadata(path) {
            Ok(meta) => {
                if meta.is_dir() {
                    path.to_path_buf()
                } else {
                    path.parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from("."))
                }
            }
            Err(_) => {
                // Best effort: treat paths with an extension as files even if they don't exist yet.
                if path.extension().is_some() {
                    path.parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from("."))
                } else {
                    path.to_path_buf()
                }
            }
        }
    }

    let candidate_path: Option<&Path> = match &cli.command {
        Command::Index(args) => Some(args.path.as_path()),
        Command::Diagnostics(args) => Some(args.path.as_path()),
        Command::Symbols(args) => Some(args.path.as_path()),
        Command::Ai(args) => match &args.command {
            AiCommand::Models(args) => Some(args.path.as_path()),
            AiCommand::Status(args) => Some(args.path.as_path()),
            AiCommand::Review(args) => Some(args.path.as_path()),
            AiCommand::SemanticSearch(args) => Some(args.path.as_path()),
        },
        Command::Cache(args) => match &args.command {
            CacheCommand::Clean(args) | CacheCommand::Status(args) | CacheCommand::Warm(args) => {
                Some(args.path.as_path())
            }
            CacheCommand::Pack(args) => Some(args.path.as_path()),
            CacheCommand::Install(args) => Some(args.path.as_path()),
            CacheCommand::Fetch(args) => Some(args.path.as_path()),
            CacheCommand::Gc(_) | CacheCommand::List(_) => None,
        },
        Command::Perf(args) => match &args.command {
            PerfCommand::Report(args) => Some(args.path.as_path()),
            PerfCommand::CaptureRuntime { workspace_cache, .. } => Some(workspace_cache.as_path()),
            PerfCommand::Capture { .. }
            | PerfCommand::Compare { .. }
            | PerfCommand::CompareRuntime { .. } => None,
        },
        Command::Parse(args) => Some(args.file.as_path()),
        Command::Format(args) => Some(args.file.as_path()),
        Command::OrganizeImports(args) => Some(args.file.as_path()),
        Command::Refactor(args) => match &args.command {
            RefactorCommand::Rename(args) => Some(args.file.as_path()),
        },
        Command::Extensions(args) => match &args.command {
            extensions::ExtensionsCommand::List(args) => Some(args.root.as_path()),
            extensions::ExtensionsCommand::Validate(args) => Some(args.root.as_path()),
        },
        _ => None,
    };

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let start = candidate_path.map(start_dir).unwrap_or(cwd);
    let start = start.canonicalize().unwrap_or(start);
    let project_root_opt = nova_project::workspace_root(&start);
    let project_root = project_root_opt.clone().unwrap_or_else(|| start.clone());
    let stop_at = project_root_opt.as_deref().unwrap_or_else(|| {
        // No project-root markers were found (e.g. a tempdir with only `nova.toml`); allow config
        // discovery to walk all the way up to the filesystem root so we can still locate the
        // nearest config file.
        start.ancestors().last().unwrap_or(&start)
    });

    // If the user explicitly provided `NOVA_CONFIG_PATH` (relative to the workspace root),
    // prefer a workspace-root that actually contains that path. This avoids scenarios where a
    // nested `nova.toml` would otherwise "steal" the workspace-root and cause the explicit config
    // lookup to fail.
    if let Some(root) = root_with_explicit_config_env(&start, stop_at) {
        return root;
    }
    if env::var_os(NOVA_CONFIG_ENV_VAR).is_some() {
        return project_root;
    }
    if let Some(root) = root_with_config_marker(&start, stop_at) {
        return root;
    }
    project_root
}

fn run(cli: Cli, config: &NovaConfig) -> Result<i32> {
    match cli.command {
        Command::Lsp(_) | Command::Dap(_) => anyhow::bail!(
            "internal error: `nova lsp`/`nova dap` should have been handled before config init"
        ),
        Command::Index(args) => {
            let ws = Workspace::open_with_config(&args.path, config)?;
            let report = ws.index_and_write_cache()?;
            print_output(&report, args.json)?;
            Ok(0)
        }
        Command::Diagnostics(args) => {
            let ws = Workspace::open_with_config(&args.path, config)?;
            let report = ws.diagnostics(&args.path)?;
            let exit = if report.summary.errors > 0 { 1 } else { 0 };

            if let Some(path) = args.sarif_out.as_deref() {
                write_sarif(&report, path)?;
            }

            let format = if args.json {
                DiagnosticsFormat::Json
            } else {
                args.format.unwrap_or(DiagnosticsFormat::Human)
            };

            match format {
                DiagnosticsFormat::Human => print_output(&report, false)?,
                DiagnosticsFormat::Json => print_output(&report, true)?,
                DiagnosticsFormat::Github => print_github_annotations(&report),
                DiagnosticsFormat::Sarif => print_sarif(&report)?,
            }
            Ok(exit)
        }
        Command::Symbols(args) => {
            let results = if args.distributed {
                match workspace_symbols_distributed(&args) {
                    Ok(symbols) => symbols,
                    Err(err) => {
                        eprintln!(
                            "nova symbols: distributed mode failed; falling back: {}",
                            sanitize_anyhow_error_message(&err)
                        );
                        let ws = Workspace::open_with_config(&args.path, config)?;
                        ws.workspace_symbols(&args.query)?
                    }
                }
            } else {
                let ws = Workspace::open_with_config(&args.path, config)?;
                ws.workspace_symbols(&args.query)?
            };

            let results = results.into_iter().take(args.limit).collect::<Vec<_>>();
            print_output(&results, args.json)?;
            Ok(0)
        }
        Command::Deps(args) => match args.command {
            DepsCommand::Index { jar } => {
                let store = DependencyIndexStore::from_env()?;
                let stats = nova_classpath::IndexingStats::default();

                let entry = if jar
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jmod"))
                {
                    nova_classpath::ClasspathEntry::Jmod(jar.clone())
                } else {
                    nova_classpath::ClasspathEntry::Jar(jar.clone())
                };

                let index = nova_classpath::ClasspathIndex::build_with_deps_store(
                    &[entry],
                    None,
                    Some(&store),
                    Some(&stats),
                )?;

                let sha = nova_deps_cache::sha256_hex(&jar)?;
                println!(
                    "indexed {} ({} classes, sha256 {})",
                    jar.display(),
                    index.len(),
                    sha
                );
                println!(
                    "deps cache hits: {}, class parses: {}",
                    stats.deps_cache_hits(),
                    stats.classfiles_parsed()
                );
                Ok(0)
            }
            DepsCommand::Pack { output } => {
                let store = DependencyIndexStore::from_env()?;
                store.pack(&output)?;
                println!("packed dependency indexes to {}", output.display());
                Ok(0)
            }
            DepsCommand::Install { archive } => {
                let store = DependencyIndexStore::from_env()?;
                store.install(&archive)?;
                println!("installed dependency indexes from {}", archive.display());
                Ok(0)
            }
        },
        Command::Cache(args) => {
            match args.command {
                CacheCommand::Clean(args) => {
                    let ws = Workspace::open_with_config(&args.path, config)?;
                    let cache_root = ws.cache_root()?;
                    ws.cache_clean()?;
                    if !args.json {
                        println!("cache: cleaned {}", cache_root.display());
                    } else {
                        print_output(&serde_json::json!({ "ok": true }), true)?;
                    }
                }
                CacheCommand::Status(args) => {
                    let ws = Workspace::open_with_config(&args.path, config)?;
                    let status = ws.cache_status()?;
                    print_cache_status(&status, args.json)?;
                }
                CacheCommand::Warm(args) => {
                    let ws = Workspace::open_with_config(&args.path, config)?;
                    let report = ws.cache_warm()?;
                    print_output(&report, args.json)?;
                }
                CacheCommand::Gc(args) => {
                    let config = CacheConfig::from_env();
                    let root = nova_cache::cache_root(&config)?;
                    let report = nova_cache::gc_project_caches(
                        &root,
                        &CacheGcPolicy {
                            max_total_bytes: args.max_total_bytes,
                            max_age_ms: args.max_age_ms,
                            keep_latest_n: args.keep_latest_n,
                        },
                    )?;

                    if args.json {
                        print_output(
                            &serde_json::json!({ "cache_root": root, "report": report }),
                            true,
                        )?;
                    } else {
                        println!("cache gc: {}", root.display());
                        println!("  before_total_bytes: {}", report.before_total_bytes);
                        println!("  after_total_bytes: {}", report.after_total_bytes);
                        println!("  deleted: {}", report.deleted.len());
                        for cache in &report.deleted {
                            println!("    {} ({})", cache.path.display(), cache.size_bytes);
                        }
                        if !report.failed.is_empty() {
                            println!("  failed: {}", report.failed.len());
                            for failure in &report.failed {
                                println!("    {}: {}", failure.cache.path.display(), failure.error);
                            }
                        }
                    }
                }
                CacheCommand::List(args) => {
                    let config = CacheConfig::from_env();
                    let root = nova_cache::cache_root(&config)?;
                    let caches = nova_cache::enumerate_project_caches(&root)?;

                    if args.json {
                        print_output(
                            &serde_json::json!({ "cache_root": root, "caches": caches }),
                            true,
                        )?;
                    } else {
                        println!("cache root: {}", root.display());
                        for cache in caches {
                            let last_updated = cache
                                .last_updated_millis
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "(unknown)".to_string());
                            let nova_version = cache.nova_version.as_deref().unwrap_or("(unknown)");
                            let schema_version = cache
                                .schema_version
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "(unknown)".to_string());
                            println!(
                                "{}  bytes={}  last_updated_millis={}  nova_version={}  schema_version={}",
                                cache.name,
                                cache.size_bytes,
                                last_updated,
                                nova_version,
                                schema_version
                            );
                        }
                    }
                }
                CacheCommand::Pack(args) => {
                    let ws = Workspace::open_with_config(&args.path, config)?;
                    let cache_dir = CacheDir::new(ws.root(), CacheConfig::from_env())?;
                    pack_cache_package(&cache_dir, &args.out)?;
                    if args.json {
                        print_output(&serde_json::json!({ "ok": true, "out": args.out }), true)?;
                    } else {
                        println!(
                            "cache: packed {} -> {}",
                            cache_dir.root().display(),
                            args.out.display()
                        );
                    }
                }
                CacheCommand::Install(args) => {
                    let ws = Workspace::open_with_config(&args.path, config)?;
                    let cache_dir = CacheDir::new(ws.root(), CacheConfig::from_env())?;
                    let outcome = install_cache_package(&cache_dir, &args.package)?;
                    if args.json {
                        print_output(
                            &serde_json::json!({ "ok": true, "outcome": format!("{outcome:?}") }),
                            true,
                        )?;
                    } else {
                        match outcome {
                            CachePackageInstallOutcome::Full => {
                                println!("cache: installed full package")
                            }
                            CachePackageInstallOutcome::IndexesOnly { .. } => {
                                println!("cache: installed indexes only (fingerprint mismatch)")
                            }
                        }
                    }
                }
                CacheCommand::Fetch(args) => {
                    let ws = Workspace::open_with_config(&args.path, config)?;
                    let cache_dir = CacheDir::new(ws.root(), CacheConfig::from_env())?;
                    let outcome = fetch_cache_package(&cache_dir, &args.url)?;
                    if args.json {
                        print_output(
                            &serde_json::json!({ "ok": true, "outcome": format!("{outcome:?}") }),
                            true,
                        )?;
                    } else {
                        match outcome {
                            CachePackageInstallOutcome::Full => {
                                println!("cache: fetched and installed full package")
                            }
                            CachePackageInstallOutcome::IndexesOnly { .. } => {
                                println!("cache: fetched and installed indexes only (fingerprint mismatch)")
                            }
                        }
                    }
                }
            }
            Ok(0)
        }
        Command::Perf(args) => match args.command {
            PerfCommand::Report(args) => {
                let ws = Workspace::open_with_config(&args.path, config)?;
                let perf = ws.perf_report()?;
                if args.json {
                    print_output(&PerfEnvelope { perf }, true)?;
                } else if let Some(perf) = perf {
                    println!("perf:");
                    println!("  files_total: {}", perf.files_total);
                    println!("  files_invalidated: {}", perf.files_invalidated);
                    println!("  files_indexed: {}", perf.files_indexed);
                    println!("  bytes_indexed: {}", perf.bytes_indexed);
                    println!("  snapshot_ms: {}", perf.snapshot_ms);
                    println!("  index_ms: {}", perf.index_ms);
                    println!("  symbols_indexed: {}", perf.symbols_indexed);
                    println!("  elapsed_ms: {}", perf.elapsed_ms);
                    if let Some(rss) = perf.rss_bytes {
                        println!("  rss_bytes: {}", rss);
                    }
                } else {
                    println!(
                        "perf: no cached metrics found (run `nova index <path>` or `nova cache warm`)"
                    );
                }
                Ok(0)
            }
            PerfCommand::Capture { criterion_dir, out } => {
                let run = load_criterion_directory(&criterion_dir).with_context(|| {
                    format!("load criterion directory {}", criterion_dir.display())
                })?;
                run.write_json(&out)?;
                println!("wrote {}", out.display());
                Ok(0)
            }
            PerfCommand::Compare {
                baseline,
                current,
                thresholds_config,
                allow,
                markdown_out,
            } => {
                let baseline_run = load_run_from_path(&baseline)
                    .with_context(|| format!("load baseline run from {}", baseline.display()))?;
                let current_run = load_run_from_path(&current)
                    .with_context(|| format!("load current run from {}", current.display()))?;

                let thresholds = match thresholds_config {
                    Some(path) => ThresholdConfig::read_toml(&path)
                        .with_context(|| format!("load thresholds config {}", path.display()))?,
                    None => ThresholdConfig::default(),
                };

                let comparison = compare_runs(&baseline_run, &current_run, &thresholds, &allow);
                let markdown = comparison.to_markdown();

                if let Some(path) = markdown_out {
                    std::fs::write(&path, &markdown).with_context(|| {
                        format!("failed to write markdown report to {}", path.display())
                    })?;
                }

                print!("{markdown}");

                Ok(if comparison.has_failure { 1 } else { 0 })
            }
            PerfCommand::CaptureRuntime {
                workspace_cache,
                out,
                nova_lsp,
            } => {
                let perf = load_workspace_perf_metrics(&workspace_cache).with_context(|| {
                    format!(
                        "load workspace perf metrics from {}",
                        workspace_cache.display()
                    )
                })?;

                let mut run = runtime_run_from_workspace_perf(&perf);
                if let Some(nova_lsp) = nova_lsp {
                    add_lsp_runtime_metrics(&mut run, &nova_lsp).with_context(|| {
                        format!("capture runtime metrics from {}", nova_lsp.display())
                    })?;
                }
                run.write_json(&out)?;
                println!("wrote {}", out.display());
                Ok(0)
            }
            PerfCommand::CompareRuntime {
                baseline,
                current,
                thresholds_config,
                allow,
                markdown_out,
            } => {
                let baseline_run = RuntimeRun::read_json(&baseline).with_context(|| {
                    format!("load baseline runtime run from {}", baseline.display())
                })?;
                let current_run = RuntimeRun::read_json(&current).with_context(|| {
                    format!("load current runtime run from {}", current.display())
                })?;

                let config = match thresholds_config {
                    Some(path) => RuntimeThresholdConfig::read_toml(&path).with_context(|| {
                        format!("load runtime thresholds config {}", path.display())
                    })?,
                    None => RuntimeThresholdConfig::default(),
                };

                let comparison = compare_runtime_runs(&baseline_run, &current_run, &config, &allow);
                let markdown = comparison.to_markdown();

                if let Some(path) = markdown_out {
                    std::fs::write(&path, &markdown).with_context(|| {
                        format!("failed to write markdown report to {}", path.display())
                    })?;
                }

                print!("{markdown}");

                Ok(if comparison.has_failure { 1 } else { 0 })
            }
        },
        Command::Format(args) => handle_format(args),
        Command::OrganizeImports(args) => handle_organize_imports(args),
        Command::Refactor(args) => match args.command {
            RefactorCommand::Rename(args) => handle_rename(args),
        },
        Command::BugReport(args) => {
            tracing::info!(target = "nova.cli", "creating bug report bundle");

            let reproduction = if let Some(path) = args.repro.as_ref() {
                Some(
                    fs::read_to_string(path)
                        .with_context(|| format!("read reproduction file {}", path.display()))?,
                )
            } else {
                args.repro_text.clone()
            };

            let options = BugReportOptions {
                max_log_lines: args.max_log_lines,
                reproduction,
            };

            let perf = PerfStats::default();
            let log_buffer = global_log_buffer();
            let crash_store = global_crash_store();
            let bundle =
                BugReportBuilder::new(config, log_buffer.as_ref(), crash_store.as_ref(), &perf)
                    .options(options)
                    .create_archive(args.archive)
                    .build()
                    .map_err(|err| anyhow::anyhow!(err))
                    .context("failed to create bug report bundle")?;

            let mut bundle_path = bundle.path().to_path_buf();
            let mut archive_path = bundle.archive_path().map(|path| path.to_path_buf());
            if let Some(out) = args.out.as_ref() {
                let dest = resolve_bugreport_output_path(out, &bundle_path)?;
                move_dir(&bundle_path, &dest)?;
                bundle_path = dest;

                if let Some(old_archive) = archive_path.as_ref() {
                    let dest_archive = bundle_path.with_extension(
                        old_archive
                            .extension()
                            .and_then(|s| s.to_str())
                            .unwrap_or("zip"),
                    );
                    move_file(old_archive, &dest_archive)?;
                    archive_path = Some(dest_archive);
                }
            }

            if args.json {
                let mut payload = serde_json::json!({ "path": bundle_path });
                if let Some(path) = &archive_path {
                    payload["archive"] = serde_json::json!(path);
                }
                print_output(&payload, true)?;
            } else {
                println!("bugreport: {}", bundle_path.display());
                if let Some(path) = &archive_path {
                    println!("archive: {}", path.display());
                }
            }

            Ok(0)
        }
        Command::Extensions(args) => extensions::run(args),
        Command::Ai(args) => match args.command {
            AiCommand::Models(args) => {
                let client = AiClient::from_config(&config.ai)?;
                let rt = tokio::runtime::Runtime::new()?;
                let models = rt.block_on(client.list_models(CancellationToken::new()))?;

                if args.json {
                    print_output(&models, true)?;
                } else if models.is_empty() {
                    println!("No models returned by backend.");
                } else {
                    for model in models {
                        println!("{model}");
                    }
                }

                Ok(0)
            }
            AiCommand::Status(args) => {
                let env_truthy = |name: &str| {
                    matches!(
                        std::env::var(name).as_deref(),
                        Ok("1") | Ok("true") | Ok("TRUE")
                    )
                };

                let disable_ai = env_truthy("NOVA_DISABLE_AI");
                let disable_ai_completions = env_truthy("NOVA_DISABLE_AI_COMPLETIONS");
                let disable_ai_code_actions = env_truthy("NOVA_DISABLE_AI_CODE_ACTIONS");
                let disable_ai_code_review = env_truthy("NOVA_DISABLE_AI_CODE_REVIEW");

                let privacy = nova_ai::PrivacyMode::from_ai_privacy_config(&config.ai.privacy);
                let configured = config.ai.enabled && AiClient::from_config(&config.ai).is_ok();

                let payload = serde_json::json!({
                    "enabled": config.ai.enabled,
                    "configured": configured,
                    "providerKind": &config.ai.provider.kind,
                    "model": &config.ai.provider.model,
                    "privacy": {
                        "localOnly": config.ai.privacy.local_only,
                        "anonymizeIdentifiers": privacy.anonymize_identifiers,
                        "includeFilePaths": privacy.include_file_paths,
                        "excludedPathsCount": config.ai.privacy.excluded_paths.len(),
                    },
                    "features": {
                        "completion_ranking": config.ai.features.completion_ranking,
                        "semantic_search": config.ai.features.semantic_search,
                        "multi_token_completion": config.ai.features.multi_token_completion,
                        "explain_errors": config.ai.features.explain_errors,
                        "code_actions": config.ai.features.code_actions,
                        "code_review": config.ai.features.code_review,
                        "code_review_max_diff_chars": config.ai.features.code_review_max_diff_chars,
                    },
                    "cacheEnabled": config.ai.cache_enabled,
                    "auditLogEnabled": config.ai.audit_log.enabled,
                    "envOverrides": {
                        "disableAi": disable_ai,
                        "disableAiCompletions": disable_ai_completions,
                        "disableAiCodeActions": disable_ai_code_actions,
                        "disableAiCodeReview": disable_ai_code_review,
                    }
                });

                if args.json {
                    print_output(&payload, true)?;
                } else {
                    let provider_kind = serde_json::to_value(&config.ai.provider.kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| format!("{:?}", config.ai.provider.kind));

                    println!(
                        "AI: enabled={} configured={} providerKind={} model={}",
                        config.ai.enabled, configured, provider_kind, config.ai.provider.model
                    );
                    println!(
                        "Privacy: localOnly={} anonymizeIdentifiers={} includeFilePaths={} excludedPathsCount={}",
                        config.ai.privacy.local_only,
                        privacy.anonymize_identifiers,
                        privacy.include_file_paths,
                        config.ai.privacy.excluded_paths.len(),
                    );
                    println!(
                        "Features: completion_ranking={} semantic_search={} multi_token_completion={} explain_errors={} code_actions={} code_review={} code_review_max_diff_chars={}",
                        config.ai.features.completion_ranking,
                        config.ai.features.semantic_search,
                        config.ai.features.multi_token_completion,
                        config.ai.features.explain_errors,
                        config.ai.features.code_actions,
                        config.ai.features.code_review,
                        config.ai.features.code_review_max_diff_chars,
                    );
                    println!(
                        "Cache: enabled={} auditLogEnabled={}",
                        config.ai.cache_enabled, config.ai.audit_log.enabled
                    );
                    println!(
                        "Env overrides: disableAi={} disableAiCompletions={} disableAiCodeActions={} disableAiCodeReview={}",
                        disable_ai,
                        disable_ai_completions,
                        disable_ai_code_actions,
                        disable_ai_code_review
                    );
                    println!();
                    println!(
                        "Hint: edit nova.toml keys `ai.enabled`, `ai.provider.kind`, `ai.provider.model`, `ai.privacy.*`, `ai.features.*`."
                    );
                }

                Ok(0)
            }
            AiCommand::Review(args) => {
                let env_truthy = |name: &str| {
                    matches!(
                        std::env::var(name).as_deref(),
                        Ok("1") | Ok("true") | Ok("TRUE")
                    )
                };

                if !config.ai.enabled {
                    if env_truthy("NOVA_DISABLE_AI") {
                        anyhow::bail!(
                            "AI is disabled by NOVA_DISABLE_AI=1. Unset NOVA_DISABLE_AI (or set it to 0) \
                             and enable it by setting `[ai].enabled = true` in nova.toml \
                             (or pass `--config <path>` / set {config_env}).",
                            config_env = NOVA_CONFIG_ENV_VAR
                        );
                    }
                    anyhow::bail!(
                        "AI is disabled. Enable it by setting `[ai].enabled = true` in nova.toml \
                         (or pass `--config <path>` / set {config_env}).",
                        config_env = NOVA_CONFIG_ENV_VAR
                    );
                }
                if !config.ai.features.code_review {
                    if env_truthy("NOVA_DISABLE_AI") {
                        anyhow::bail!(
                            "AI code review is disabled because NOVA_DISABLE_AI=1 disables all AI features. \
                             Unset NOVA_DISABLE_AI (or set it to 0) and enable code review via \
                             `ai.features.code_review = true` in nova.toml \
                             (or pass `--config <path>` / set {config_env}).",
                            config_env = NOVA_CONFIG_ENV_VAR
                        );
                    }
                    if env_truthy("NOVA_DISABLE_AI_CODE_REVIEW") {
                        anyhow::bail!(
                            "AI code review is disabled by NOVA_DISABLE_AI_CODE_REVIEW=1. Unset \
                             NOVA_DISABLE_AI_CODE_REVIEW (or set it to 0) and enable it via \
                             `ai.features.code_review = true` in nova.toml \
                             (or pass `--config <path>` / set {config_env}).",
                            config_env = NOVA_CONFIG_ENV_VAR
                        );
                    }
                    anyhow::bail!(
                        "AI code review is disabled (ai.features.code_review=false). Enable it by setting \
                         `ai.features.code_review = true` in nova.toml (or pass `--config <path>` / set {config_env}).",
                        config_env = NOVA_CONFIG_ENV_VAR
                    );
                }

                let diff = load_ai_review_diff(&args)?;
                if diff.trim().is_empty() {
                    if args.git {
                        let which = if args.staged {
                            "git diff --staged"
                        } else {
                            "git diff"
                        };
                        anyhow::bail!("No diff content provided: `{which}` returned an empty diff.");
                    }
                    if let Some(path) = args.diff_file.as_ref() {
                        anyhow::bail!(
                            "No diff content provided: diff file {} was empty.",
                            path.display()
                        );
                    }
                    anyhow::bail!(
                        "No diff content provided: stdin was empty. Pass `--diff-file <path>` or use `--git`."
                    );
                }

                let ai = NovaAi::new(&config.ai)?;
                let rt = tokio::runtime::Runtime::new()?;
                let review = rt.block_on(ai.code_review(&diff, CancellationToken::new()))?;

                if args.json {
                    print_output(&serde_json::json!({ "review": review }), true)?;
                } else {
                    print!("{review}");
                    if !review.ends_with('\n') {
                        println!();
                    }
                }

                Ok(0)
            }
            AiCommand::SemanticSearch(args) => handle_ai_semantic_search(args, config),
        },
        Command::Parse(args) => {
            let ws = Workspace::open_with_config(&args.file, config)?;
            let result = ws.parse_file(&args.file)?;
            let exit = if result.errors.is_empty() { 0 } else { 1 };
            print_output(&result, args.json)?;
            Ok(exit)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CliSemanticSearchResult {
    path: String,
    kind: String,
    score: f32,
    snippet: String,
}

#[derive(Debug, Clone, Serialize)]
struct CliSemanticSearchResponse {
    results: Vec<CliSemanticSearchResult>,
}

fn semantic_search_extension_allowed(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };

    ext.eq_ignore_ascii_case("java")
        || ext.eq_ignore_ascii_case("kt")
        || ext.eq_ignore_ascii_case("kts")
        || ext.eq_ignore_ascii_case("gradle")
        || ext.eq_ignore_ascii_case("md")
}

fn semantic_search_display_path(workspace_root: &Path, path: &Path) -> String {
    match path.strip_prefix(workspace_root) {
        Ok(rel) if !rel.as_os_str().is_empty() => display_path(rel),
        _ => display_path(path),
    }
}

fn semantic_search_human_snippet(snippet: &str) -> String {
    snippet
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn handle_ai_semantic_search(args: AiSemanticSearchArgs, config: &NovaConfig) -> Result<i32> {
    let env_truthy = |name: &str| {
        matches!(
            std::env::var(name).as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        )
    };

    if !config.ai.enabled {
        if env_truthy("NOVA_DISABLE_AI") {
            anyhow::bail!(
                "AI is disabled by NOVA_DISABLE_AI=1. Unset NOVA_DISABLE_AI (or set it to 0) and enable it \
                 by setting `[ai].enabled = true` in nova.toml (or pass `--config <path>` / set {config_env}).",
                config_env = NOVA_CONFIG_ENV_VAR
            );
        }
        anyhow::bail!(
            "AI is disabled. Enable it by setting `[ai].enabled = true` in nova.toml (or pass `--config <path>` / set {config_env}).",
            config_env = NOVA_CONFIG_ENV_VAR
        );
    }

    if !config.ai.features.semantic_search {
        anyhow::bail!(
            "AI semantic search is disabled (ai.features.semantic_search=false). Enable it by setting \
             `ai.features.semantic_search = true` in nova.toml (or pass `--config <path>` / set {config_env}).",
            config_env = NOVA_CONFIG_ENV_VAR
        );
    }

    let workspace_root = resolve_path_workdir(&args.path);
    let workspace_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.clone());
    anyhow::ensure!(
        workspace_root.is_dir(),
        "--path must be a directory (got {})",
        workspace_root.display()
    );

    let excluded_matcher = nova_ai::ExcludedPathMatcher::from_config(&config.ai.privacy)?;

    let mut search = nova_ai::semantic_search_from_config(&config.ai)
        .with_context(|| "failed to initialize semantic search")?;

    const MAX_INDEXED_FILES: u64 = 2_000;
    const MAX_INDEXED_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB
    const MAX_FILE_BYTES: u64 = 256 * 1024; // 256 KiB

    let mut indexed_files = 0u64;
    let mut indexed_bytes = 0u64;

    let mut walk = WalkDir::new(&workspace_root).follow_links(false).into_iter();
    while let Some(entry) = walk.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | ".hg" | ".svn" | "target" | "build" | "out" | "node_modules"
            ) {
                walk.skip_current_dir();
                continue;
            }
        }

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path().to_path_buf();
        if !semantic_search_extension_allowed(&path) {
            continue;
        }

        if excluded_matcher.is_match(&path) {
            continue;
        }

        // Keep memory bounded (matches the LSP workspace-indexing limits).
        let meta_len = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);
        if meta_len > MAX_FILE_BYTES {
            continue;
        }
        if indexed_files >= MAX_INDEXED_FILES || indexed_bytes >= MAX_INDEXED_BYTES {
            break;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let len = text.len() as u64;
        if len > MAX_FILE_BYTES {
            continue;
        }
        if indexed_files + 1 > MAX_INDEXED_FILES || indexed_bytes.saturating_add(len) > MAX_INDEXED_BYTES
        {
            break;
        }

        search.index_file(path, text);
        indexed_files += 1;
        indexed_bytes = indexed_bytes.saturating_add(len);
    }

    search.finalize_indexing();

    let limit = args.limit.min(50);
    let mut results = if limit == 0 {
        Vec::new()
    } else {
        search.search(&args.query)
    };
    results.truncate(limit);

    let response = CliSemanticSearchResponse {
        results: results
            .into_iter()
            .map(|result| CliSemanticSearchResult {
                path: semantic_search_display_path(&workspace_root, &result.path),
                kind: result.kind,
                score: result.score,
                snippet: result.snippet,
            })
            .collect(),
    };

    if args.json {
        print_output(&response, true)?;
        return Ok(0);
    }

    if response.results.is_empty() {
        println!("No results.");
        return Ok(0);
    }

    for (idx, result) in response.results.iter().enumerate() {
        let snippet = semantic_search_human_snippet(&result.snippet);
        println!(
            "{}. {:.3} {} {}",
            idx + 1,
            result.score,
            result.path,
            snippet
        );
    }

    Ok(0)
}

fn load_ai_review_diff(args: &AiReviewArgs) -> Result<String> {
    use std::io::Read;

    let workdir = resolve_path_workdir(&args.path);

    if args.git {
        return load_git_diff(&workdir, args.staged);
    }

    if let Some(path) = args.diff_file.as_ref() {
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            workdir.join(path)
        };
        return fs::read_to_string(&resolved)
            .with_context(|| format!("failed to read diff file {}", resolved.display()));
    }

    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read diff from stdin")?;
    Ok(buf)
}

fn resolve_path_workdir(path: &Path) -> PathBuf {
    match fs::metadata(path) {
        Ok(meta) => {
            if meta.is_dir() {
                path.to_path_buf()
            } else {
                path.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            }
        }
        Err(_) => {
            if path.extension().is_some() {
                path.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            } else {
                path.to_path_buf()
            }
        }
    }
}

fn load_git_diff(workdir: &Path, staged: bool) -> Result<String> {
    use std::process::Command;

    let mut cmd = Command::new("git");
    cmd.arg("diff");
    // Produce stable, machine-consumable output regardless of user git config.
    cmd.arg("--no-color").arg("--no-ext-diff");
    if staged {
        cmd.arg("--staged");
    }
    cmd.current_dir(workdir);

    let output = cmd
        .output()
        .with_context(|| format!("failed to run `git diff` in {}", workdir.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "`git diff` failed in {} with exit code {:?}.\nstderr:\n{}",
            workdir.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_lsp_launcher(args: &LspLauncherArgs, config_path: Option<&Path>) -> Result<i32> {
    use std::process::{Command, Stdio};

    fn passthrough_has_flag(args: &[String], flag: &str) -> bool {
        args.iter()
            .any(|arg| arg == flag || arg.starts_with(&format!("{flag}=")))
    }

    fn build_lsp_command<S: AsRef<OsStr>>(
        program: S,
        args: &LspLauncherArgs,
        config_path: Option<&Path>,
    ) -> Command {
        let mut cmd = Command::new(program);

        // Important: no output on stdout except what the child writes (LSP frames).
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        // Always force stdio transport unless the caller explicitly opts in to a different mode.
        // Today `nova-lsp` ignores this flag, but future transports may require it.
        if !args.args.iter().any(|arg| arg == "--stdio") {
            cmd.arg("--stdio");
        }

        // Forward the CLI's global `--config <path>` to `nova-lsp` unless the user is already
        // explicitly passing `--config` in the passthrough args.
        if let Some(config_path) = config_path {
            if !passthrough_has_flag(&args.args, "--config") {
                cmd.arg("--config").arg(config_path);
            }
        }

        cmd.args(&args.args);
        cmd
    }

    let explicit_program = args.nova_lsp.as_deref().map(PathBuf::from);
    let adjacent_program = if explicit_program.is_none() {
        resolve_adjacent_binary("nova-lsp")
    } else {
        None
    };

    // Prefer an `exec()`-style handoff on Unix so `nova lsp` behaves exactly like
    // running `nova-lsp` directly (signal handling, process identity, etc.).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if let Some(program) = explicit_program {
            let mut cmd = build_lsp_command(program, args, config_path);
            let err = cmd.exec();
            return Err(err).with_context(|| "failed to exec nova-lsp");
        }

        let mut cmd = build_lsp_command("nova-lsp", args, config_path);
        let err = cmd.exec();
        if err.kind() == std::io::ErrorKind::NotFound {
            if let Some(program) = adjacent_program {
                let mut cmd = build_lsp_command(&program, args, config_path);
                let err = cmd.exec();
                return Err(err).with_context(|| format!("failed to exec {}", program.display()));
            }
        }
        return Err(err).with_context(|| "failed to exec nova-lsp");
    }

    #[cfg(not(unix))]
    {
        if let Some(program) = explicit_program {
            let status = build_lsp_command(program, args, config_path)
                .status()
                .with_context(|| "failed to spawn nova-lsp")?;
            return Ok(exit_code_from_status(status));
        }

        let status = match build_lsp_command("nova-lsp", args, config_path).status() {
            Ok(status) => status,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(program) = adjacent_program {
                    build_lsp_command(program, args, config_path)
                        .status()
                        .with_context(|| "failed to spawn nova-lsp")?
                } else {
                    return Err(err).with_context(|| "failed to spawn nova-lsp");
                }
            }
            Err(err) => return Err(err).with_context(|| "failed to spawn nova-lsp"),
        };
        Ok(exit_code_from_status(status))
    }
}

fn run_dap_launcher(args: &DapLauncherArgs, config_path: Option<&Path>) -> Result<i32> {
    use std::process::{Command, Stdio};

    fn passthrough_has_flag(args: &[String], flag: &str) -> bool {
        args.iter()
            .any(|arg| arg == flag || arg.starts_with(&format!("{flag}=")))
    }

    fn build_dap_command<S: AsRef<OsStr>>(
        program: S,
        args: &DapLauncherArgs,
        config_path: Option<&Path>,
    ) -> Command {
        let mut cmd = Command::new(program);
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        if let Some(config_path) = config_path {
            if !passthrough_has_flag(&args.args, "--config") {
                cmd.arg("--config").arg(config_path);
            }
        }

        cmd.args(&args.args);
        cmd
    }

    let explicit_program = args.nova_dap.as_deref().map(PathBuf::from);
    let adjacent_program = if explicit_program.is_none() {
        resolve_adjacent_binary("nova-dap")
    } else {
        None
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if let Some(program) = explicit_program {
            let mut cmd = build_dap_command(program, args, config_path);
            let err = cmd.exec();
            return Err(err).with_context(|| "failed to exec nova-dap");
        }

        let mut cmd = build_dap_command("nova-dap", args, config_path);
        let err = cmd.exec();
        if err.kind() == std::io::ErrorKind::NotFound {
            if let Some(program) = adjacent_program {
                let mut cmd = build_dap_command(&program, args, config_path);
                let err = cmd.exec();
                return Err(err).with_context(|| format!("failed to exec {}", program.display()));
            }
        }
        return Err(err).with_context(|| "failed to exec nova-dap");
    }

    #[cfg(not(unix))]
    {
        if let Some(program) = explicit_program {
            let status = build_dap_command(program, args, config_path)
                .status()
                .with_context(|| "failed to spawn nova-dap")?;
            return Ok(exit_code_from_status(status));
        }

        let status = match build_dap_command("nova-dap", args, config_path).status() {
            Ok(status) => status,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(program) = adjacent_program {
                    build_dap_command(program, args, config_path)
                        .status()
                        .with_context(|| "failed to spawn nova-dap")?
                } else {
                    return Err(err).with_context(|| "failed to spawn nova-dap");
                }
            }
            Err(err) => return Err(err).with_context(|| "failed to spawn nova-dap"),
        };

        Ok(exit_code_from_status(status))
    }
}

fn resolve_adjacent_binary(binary_name: &str) -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;

    fn candidate(dir: &Path, binary_name: &str) -> PathBuf {
        dir.join(format!("{binary_name}{}", std::env::consts::EXE_SUFFIX))
    }

    let direct = candidate(exe_dir, binary_name);
    if direct.is_file() {
        return Some(direct);
    }

    // In dev/test builds, `current_exe()` can resolve to `target/{profile}/deps/nova-<hash>`.
    // Prefer a sibling binary in the parent dir as well, so `cargo test` + launcher subcommands
    // work without requiring PATH hacks.
    if exe_dir.file_name() == Some(OsStr::new("deps")) {
        if let Some(parent) = exe_dir.parent() {
            let parent_candidate = candidate(parent, binary_name);
            if parent_candidate.is_file() {
                return Some(parent_candidate);
            }
        }
    }

    None
}
#[cfg(not(unix))]
fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    // If the process was killed by a signal, emulate the common Unix convention
    // of using 128+<signal> as the exit code.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }

    1
}

#[derive(Serialize)]
struct PerfEnvelope<T> {
    perf: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewlineStyle {
    Lf,
    CrLf,
}

fn detect_newline_style(text: &str) -> NewlineStyle {
    if text.contains("\r\n") {
        NewlineStyle::CrLf
    } else {
        NewlineStyle::Lf
    }
}

fn convert_newlines(text: &str, style: NewlineStyle) -> String {
    match style {
        NewlineStyle::Lf => text.replace("\r\n", "\n"),
        NewlineStyle::CrLf => text.replace("\r\n", "\n").replace('\n', "\r\n"),
    }
}

pub(crate) fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn parse_cli_position(line: u32, col: u32) -> Result<Position> {
    anyhow::ensure!(line > 0, "line is 1-based (got {line})");
    anyhow::ensure!(col > 0, "col is 1-based (got {col})");
    Ok(Position::new(line - 1, col - 1))
}

fn parse_cli_range(value: &str) -> Result<Range> {
    let (start, end) = value.split_once('-').with_context(|| {
        format!("range must be <startLine:startCol-endLine:endCol> (got {value:?})")
    })?;
    let start = parse_cli_pos_pair(start)?;
    let end = parse_cli_pos_pair(end)?;
    Ok(Range::new(start, end))
}

fn parse_cli_pos_pair(value: &str) -> Result<Position> {
    let (line, col) = value.split_once(':').with_context(|| {
        format!("position must be <line:col> with 1-based line/col (got {value:?})")
    })?;
    let line: u32 = line
        .parse()
        .with_context(|| format!("invalid line {line:?}"))?;
    let col: u32 = col
        .parse()
        .with_context(|| format!("invalid col {col:?}"))?;
    parse_cli_position(line, col)
}

#[derive(Debug, Clone, Serialize)]
struct CliJsonPosition {
    /// 1-based line number.
    line: u32,
    /// 1-based UTF-16 column number.
    col: u32,
}

#[derive(Debug, Clone, Serialize)]
struct CliJsonRange {
    start: CliJsonPosition,
    end: CliJsonPosition,
}

#[derive(Debug, Clone, Serialize)]
struct CliJsonEdit {
    range: CliJsonRange,
    start_byte: usize,
    end_byte: usize,
    replacement: String,
}

#[derive(Debug, Clone, Serialize)]
struct CliJsonFileEdits {
    file: String,
    edits: Vec<CliJsonEdit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
enum CliJsonFileOp {
    Rename { from: String, to: String },
    Create { file: String },
    Delete { file: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
enum CliJsonConflict {
    NameCollision {
        file: String,
        name: String,
        existing_symbol: String,
    },
    Shadowing {
        file: String,
        name: String,
        shadowed_symbol: String,
    },
    FieldShadowing {
        file: String,
        name: String,
        usage_range: CliJsonRange,
        start_byte: usize,
        end_byte: usize,
    },
    ReferenceWillChangeResolution {
        file: String,
        name: String,
        existing_symbol: String,
        usage_range: CliJsonRange,
        start_byte: usize,
        end_byte: usize,
    },
    VisibilityLoss {
        file: String,
        name: String,
        usage_range: CliJsonRange,
        start_byte: usize,
        end_byte: usize,
    },
    FileAlreadyExists {
        file: String,
    },
}

#[derive(Debug, Clone, Serialize)]
struct CliJsonError {
    kind: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct CliJsonOutput {
    ok: bool,
    files_changed: Vec<String>,
    edits: Vec<CliJsonFileEdits>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    file_ops: Vec<CliJsonFileOp>,
    conflicts: Vec<CliJsonConflict>,
    error: Option<CliJsonError>,
}

fn print_cli_json(value: &impl Serialize) -> Result<()> {
    let out = serde_json::to_string_pretty(value)?;
    println!("{out}");
    Ok(())
}

fn core_edits_to_json(file: String, source: &str, edits: &[CoreTextEdit]) -> CliJsonFileEdits {
    let index = LineIndex::new(source);
    let mut json_edits = edits
        .iter()
        .map(|edit| {
            let range = index.range(source, edit.range);
            let start_byte = u32::from(edit.range.start()) as usize;
            let end_byte = u32::from(edit.range.end()) as usize;
            CliJsonEdit {
                range: CliJsonRange {
                    start: CliJsonPosition {
                        line: range.start.line + 1,
                        col: range.start.character + 1,
                    },
                    end: CliJsonPosition {
                        line: range.end.line + 1,
                        col: range.end.character + 1,
                    },
                },
                start_byte,
                end_byte,
                replacement: edit.replacement.clone(),
            }
        })
        .collect::<Vec<_>>();
    json_edits.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then_with(|| a.end_byte.cmp(&b.end_byte))
            .then_with(|| a.replacement.cmp(&b.replacement))
    });
    CliJsonFileEdits {
        file,
        edits: json_edits,
    }
}

fn refactor_edits_to_json(
    file: String,
    source: &str,
    edits: &[RefactorTextEdit],
) -> CliJsonFileEdits {
    let index = LineIndex::new(source);
    let mut json_edits = edits
        .iter()
        .map(|edit| {
            let start = TextSize::from(edit.range.start as u32);
            let end = TextSize::from(edit.range.end as u32);
            let start_pos = index.position(source, start);
            let end_pos = index.position(source, end);
            CliJsonEdit {
                range: CliJsonRange {
                    start: CliJsonPosition {
                        line: start_pos.line + 1,
                        col: start_pos.character + 1,
                    },
                    end: CliJsonPosition {
                        line: end_pos.line + 1,
                        col: end_pos.character + 1,
                    },
                },
                start_byte: edit.range.start,
                end_byte: edit.range.end,
                replacement: edit.replacement.clone(),
            }
        })
        .collect::<Vec<_>>();
    json_edits.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then_with(|| a.end_byte.cmp(&b.end_byte))
            .then_with(|| a.replacement.cmp(&b.replacement))
    });
    CliJsonFileEdits {
        file,
        edits: json_edits,
    }
}

fn handle_format(args: FormatArgs) -> Result<i32> {
    let source = fs::read_to_string(&args.file)
        .with_context(|| format!("failed to read {}", args.file.display()))?;

    let config = FormatConfig::default();

    let mut edits: Vec<CoreTextEdit> = match args.range.as_deref() {
        Some(range) => {
            let range = parse_cli_range(range)?;
            let tree = parse(&source);
            edits_for_range_formatting(&tree, &source, range, &config)?
        }
        None => edits_for_document_formatting(&source, &config),
    };

    // Drop no-op edits to keep CLI output stable (and to defend against future
    // formatter/diff edge cases).
    edits.retain(|edit| {
        let start = u32::from(edit.range.start()) as usize;
        let end = u32::from(edit.range.end()) as usize;
        source
            .get(start..end)
            .map(|slice| slice != edit.replacement)
            .unwrap_or(true)
    });

    // Normalize edits for deterministic JSON output.
    edits.sort_by(|a, b| {
        a.range
            .start()
            .cmp(&b.range.start())
            .then_with(|| a.range.end().cmp(&b.range.end()))
            .then_with(|| a.replacement.cmp(&b.replacement))
    });

    let new_text = apply_core_text_edits(&source, &edits).map_err(|err| anyhow::anyhow!(err))?;
    let changed = new_text != source;

    if args.in_place && changed {
        atomic_write(&args.file, new_text.as_bytes())
            .with_context(|| format!("failed to write {}", args.file.display()))?;
    }

    let file = display_path(&args.file);
    let output = CliJsonOutput {
        ok: true,
        files_changed: if changed {
            vec![file.clone()]
        } else {
            Vec::new()
        },
        edits: if changed {
            vec![core_edits_to_json(file, &source, &edits)]
        } else {
            Vec::new()
        },
        file_ops: Vec::new(),
        conflicts: Vec::new(),
        error: None,
    };

    if args.json {
        print_cli_json(&output)?;
    } else if !args.in_place {
        print!("{new_text}");
    } else if changed {
        println!("formatted {}", args.file.display());
    }

    Ok(0)
}

fn handle_organize_imports(args: OrganizeImportsArgs) -> Result<i32> {
    let source = fs::read_to_string(&args.file)
        .with_context(|| format!("failed to read {}", args.file.display()))?;
    let newline_style = detect_newline_style(&source);

    let file_str = display_path(&args.file);
    let file_id = RefactorFileId::new(file_str.clone());
    let db = TextDatabase::new([(file_id.clone(), source.clone())]);

    let mut edit = refactor_organize_imports(
        &db,
        OrganizeImportsParams {
            file: file_id.clone(),
        },
    )?;

    for e in &mut edit.text_edits {
        e.replacement = convert_newlines(&e.replacement, newline_style);
    }

    let new_text =
        apply_refactor_text_edits(&source, &edit.text_edits).map_err(|err| anyhow::anyhow!(err))?;
    let changed = new_text != source;

    if args.in_place && changed {
        atomic_write(&args.file, new_text.as_bytes())
            .with_context(|| format!("failed to write {}", args.file.display()))?;
    }

    let output = CliJsonOutput {
        ok: true,
        files_changed: if changed {
            vec![file_str.clone()]
        } else {
            Vec::new()
        },
        edits: if changed {
            vec![refactor_edits_to_json(
                file_str.clone(),
                &source,
                &edit.text_edits,
            )]
        } else {
            Vec::new()
        },
        file_ops: Vec::new(),
        conflicts: Vec::new(),
        error: None,
    };

    if args.json {
        print_cli_json(&output)?;
    } else if !args.in_place {
        print!("{new_text}");
    } else if changed {
        println!("organized imports {}", args.file.display());
    }

    Ok(0)
}

fn workspace_symbols_distributed(args: &SymbolsArgs) -> Result<Vec<WorkspaceSymbol>> {
    let workspace_root = args
        .path
        .canonicalize()
        .unwrap_or_else(|_| args.path.clone());
    let worker_command = args
        .distributed_worker_command
        .clone()
        .unwrap_or_else(default_worker_command);

    let run_dir = distributed_run_dir();
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create distributed runtime dir {}", run_dir.display()))?;
    let cache_dir = run_dir.join("cache");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("create distributed cache dir {}", cache_dir.display()))?;
    let listen_addr = distributed_listen_addr(&run_dir);

    let cleanup_dir = run_dir.clone();
    let result = (|| -> Result<Vec<WorkspaceSymbol>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build tokio runtime for distributed mode")?;

        let symbols = rt.block_on(async {
            let layout = WorkspaceLayout {
                source_roots: vec![SourceRoot {
                    path: workspace_root.clone(),
                }],
            };
            let config = DistributedRouterConfig {
                listen_addr,
                worker_command,
                cache_dir,
                auth_token: None,
                allow_insecure_tcp: false,
                max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
                max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
                max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
                #[cfg(feature = "tls")]
                tls_client_cert_fingerprint_allowlist: Default::default(),
                spawn_workers: true,
            };

            let router = QueryRouter::new_distributed(config, layout).await?;

            let result = match router.index_workspace().await {
                Ok(()) => Ok(router.workspace_symbols(&args.query).await),
                Err(err) => Err(err),
            };

            let _ = router.shutdown().await;
            result
        })?;

        let mut out = Vec::new();
        for sym in symbols {
            let name = sym.name;
            if name.is_empty() {
                continue;
            }
            let path = sym.path;
            let file_path = PathBuf::from(path.as_str());
            let file =
                path_relative_to(&workspace_root, &file_path).unwrap_or_else(|_| path.clone());

            out.push(WorkspaceSymbol {
                qualified_name: name.clone(),
                name,
                kind: nova_index::IndexSymbolKind::Class,
                container_name: None,
                location: nova_index::SymbolLocation {
                    file,
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            });
        }

        Ok(out)
    })();

    // Best-effort cleanup: avoid leaving behind stale sockets/cache data if the CLI crashes.
    let _ = std::fs::remove_dir_all(&cleanup_dir);
    result
}

fn default_worker_command() -> PathBuf {
    let exe_name = if cfg!(windows) {
        "nova-worker.exe"
    } else {
        "nova-worker"
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    PathBuf::from(exe_name)
}

fn distributed_run_dir() -> PathBuf {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    base.join(format!("nova-distributed-{}-{ts}", std::process::id()))
}

#[cfg(unix)]
fn distributed_listen_addr(run_dir: &Path) -> ListenAddr {
    ListenAddr::Unix(run_dir.join("router.sock"))
}

#[cfg(windows)]
fn distributed_listen_addr(_run_dir: &Path) -> ListenAddr {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    ListenAddr::NamedPipe(format!("nova-router-{}-{ts}", std::process::id()))
}

pub(crate) fn path_relative_to(root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(root).with_context(|| {
        format!(
            "{} is not under workspace root {}",
            path.display(),
            root.display()
        )
    })?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn conflicts_to_json(
    files: &BTreeMap<RefactorFileId, String>,
    conflicts: Vec<Conflict>,
) -> Vec<CliJsonConflict> {
    let mut out = Vec::new();
    for conflict in conflicts {
        match conflict {
            Conflict::NameCollision {
                file,
                name,
                existing_symbol,
            } => out.push(CliJsonConflict::NameCollision {
                file: file.0,
                name,
                existing_symbol: format!("{existing_symbol:?}"),
            }),
            Conflict::Shadowing {
                file,
                name,
                shadowed_symbol,
            } => out.push(CliJsonConflict::Shadowing {
                file: file.0,
                name,
                shadowed_symbol: format!("{shadowed_symbol:?}"),
            }),
            Conflict::FieldShadowing {
                file,
                name,
                usage_range,
            } => {
                let text = files.get(&file).map(String::as_str).unwrap_or("");
                let index = LineIndex::new(text);
                let start = TextSize::from(usage_range.start as u32);
                let end = TextSize::from(usage_range.end as u32);
                let start_pos = index.position(text, start);
                let end_pos = index.position(text, end);
                out.push(CliJsonConflict::FieldShadowing {
                    file: file.0,
                    name,
                    usage_range: CliJsonRange {
                        start: CliJsonPosition {
                            line: start_pos.line + 1,
                            col: start_pos.character + 1,
                        },
                        end: CliJsonPosition {
                            line: end_pos.line + 1,
                            col: end_pos.character + 1,
                        },
                    },
                    start_byte: usage_range.start,
                    end_byte: usage_range.end,
                })
            }
            Conflict::ReferenceWillChangeResolution {
                file,
                usage_range,
                name,
                existing_symbol,
            } => {
                let text = files.get(&file).map(String::as_str).unwrap_or("");
                let index = LineIndex::new(text);
                let start = TextSize::from(usage_range.start as u32);
                let end = TextSize::from(usage_range.end as u32);
                let start_pos = index.position(text, start);
                let end_pos = index.position(text, end);
                out.push(CliJsonConflict::ReferenceWillChangeResolution {
                    file: file.0,
                    name,
                    existing_symbol: format!("{existing_symbol:?}"),
                    usage_range: CliJsonRange {
                        start: CliJsonPosition {
                            line: start_pos.line + 1,
                            col: start_pos.character + 1,
                        },
                        end: CliJsonPosition {
                            line: end_pos.line + 1,
                            col: end_pos.character + 1,
                        },
                    },
                    start_byte: usage_range.start,
                    end_byte: usage_range.end,
                })
            }
            Conflict::VisibilityLoss {
                file,
                usage_range,
                name,
            } => {
                let text = files.get(&file).map(String::as_str).unwrap_or("");
                let index = LineIndex::new(text);
                let start = TextSize::from(usage_range.start as u32);
                let end = TextSize::from(usage_range.end as u32);
                let start_pos = index.position(text, start);
                let end_pos = index.position(text, end);
                out.push(CliJsonConflict::VisibilityLoss {
                    file: file.0,
                    name,
                    usage_range: CliJsonRange {
                        start: CliJsonPosition {
                            line: start_pos.line + 1,
                            col: start_pos.character + 1,
                        },
                        end: CliJsonPosition {
                            line: end_pos.line + 1,
                            col: end_pos.character + 1,
                        },
                    },
                    start_byte: usage_range.start,
                    end_byte: usage_range.end,
                })
            }
            Conflict::FileAlreadyExists { file } => {
                out.push(CliJsonConflict::FileAlreadyExists { file: file.0 })
            }
        }
    }

    fn sort_key<'a>(
        conflict: &'a CliJsonConflict,
    ) -> (&'a str, u8, &'a str, usize, usize, &'a str) {
        match conflict {
            CliJsonConflict::NameCollision {
                file,
                name,
                existing_symbol,
            } => (file, 0, name, 0, 0, existing_symbol),
            CliJsonConflict::Shadowing {
                file,
                name,
                shadowed_symbol,
            } => (file, 1, name, 0, 0, shadowed_symbol),
            CliJsonConflict::FieldShadowing {
                file,
                name,
                start_byte,
                end_byte,
                ..
            } => (file, 2, name, *start_byte, *end_byte, ""),
            CliJsonConflict::ReferenceWillChangeResolution {
                file,
                name,
                start_byte,
                end_byte,
                existing_symbol,
                ..
            } => (file, 3, name, *start_byte, *end_byte, existing_symbol),
            CliJsonConflict::VisibilityLoss {
                file,
                name,
                start_byte,
                end_byte,
                ..
            } => (file, 4, name, *start_byte, *end_byte, ""),
            CliJsonConflict::FileAlreadyExists { file } => (file, 5, "", 0, 0, ""),
        }
    }

    out.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    out
}

fn handle_rename(args: RenameArgs) -> Result<i32> {
    let snapshot = match refactor_apply::build_java_workspace_snapshot(&args.file) {
        Ok(snapshot) => snapshot,
        Err(err) => return Ok(rename_error(args.json, err.to_string())?),
    };

    let project_root = snapshot.project_root;
    let file_texts = snapshot.files;
    let target_file = snapshot.focus_file;
    let Some(target_text) = file_texts.get(&target_file).map(String::as_str) else {
        return Ok(rename_error(
            args.json,
            format!(
                "focus file {} was not included in workspace snapshot",
                target_file.0
            ),
        )?);
    };

    // Build a multi-file database so rename can evolve beyond locals/parameters.
    let db = RefactorJavaDatabase::new(
        file_texts
            .iter()
            .map(|(file, text)| (file.clone(), text.clone())),
    );

    let pos = match parse_cli_position(args.line, args.col) {
        Ok(pos) => pos,
        Err(err) => return Ok(rename_error(args.json, err.to_string())?),
    };

    let index = LineIndex::new(target_text);
    let Some(offset) = index.offset_of_position(target_text, pos) else {
        return Ok(rename_error(
            args.json,
            format!("no offset for position line={} col={}", args.line, args.col),
        )?);
    };
    let offset = u32::from(offset) as usize;

    let symbol = db.symbol_at(&target_file, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|o| db.symbol_at(&target_file, o))
    });

    let Some(symbol) = symbol else {
        return Ok(rename_error(
            args.json,
            format!("no symbol at {}:{}:{}", target_file.0, args.line, args.col),
        )?);
    };

    let edit = match refactor_rename(
        &db,
        RenameParams {
            symbol,
            new_name: args.new_name.clone(),
        },
    ) {
        Ok(edit) => edit,
        Err(SemanticRefactorError::Conflicts(conflicts)) => {
            let conflicts = conflicts_to_json(&file_texts, conflicts);
            let output = CliJsonOutput {
                ok: false,
                files_changed: Vec::new(),
                edits: Vec::new(),
                file_ops: Vec::new(),
                conflicts,
                error: Some(CliJsonError {
                    kind: "Conflicts".to_string(),
                    message: "refactoring has conflicts".to_string(),
                }),
            };
            if args.json {
                print_cli_json(&output)?;
            } else {
                eprintln!("refactoring has conflicts");
            }
            return Ok(1);
        }
        Err(err @ SemanticRefactorError::RenameNotSupported { .. }) => {
            let output = CliJsonOutput {
                ok: false,
                files_changed: Vec::new(),
                edits: Vec::new(),
                file_ops: Vec::new(),
                conflicts: Vec::new(),
                error: Some(CliJsonError {
                    kind: "RenameNotSupported".to_string(),
                    message: err.to_string(),
                }),
            };
            if args.json {
                print_cli_json(&output)?;
            } else {
                eprintln!("{err}");
            }
            return Ok(1);
        }
        Err(err) => return Err(anyhow::anyhow!(err)),
    };

    let mut normalized_edit = edit.clone();
    normalized_edit
        .remap_text_edits_across_renames()
        .map_err(|err| anyhow::anyhow!(err))?;
    normalized_edit
        .normalize()
        .map_err(|err| anyhow::anyhow!(err))?;

    let file_ops = normalized_edit
        .file_ops
        .iter()
        .map(|op| match op {
            FileOp::Rename { from, to } => CliJsonFileOp::Rename {
                from: from.0.clone(),
                to: to.0.clone(),
            },
            FileOp::Create { file, .. } => CliJsonFileOp::Create {
                file: file.0.clone(),
            },
            FileOp::Delete { file } => CliJsonFileOp::Delete {
                file: file.0.clone(),
            },
        })
        .collect::<Vec<_>>();

    // Compute the pre/post workspace state in-memory (including file ops) before writing anything.
    let ops_only = WorkspaceEdit {
        file_ops: normalized_edit.file_ops.clone(),
        text_edits: Vec::new(),
    };
    let workspace_after_file_ops =
        apply_workspace_edit(&file_texts, &ops_only).map_err(|err| anyhow::anyhow!(err))?;
    let new_workspace =
        apply_workspace_edit(&file_texts, &normalized_edit).map_err(|err| anyhow::anyhow!(err))?;

    let by_file = normalized_edit.edits_by_file();
    let mut changed_files: Vec<String> = Vec::new();
    let mut outputs: Vec<CliJsonFileEdits> = Vec::new();
    let mut changed_texts: BTreeMap<RefactorFileId, String> = BTreeMap::new();

    for (file_id, edits) in by_file {
        let Some(original) = workspace_after_file_ops.get(file_id).map(String::as_str) else {
            return Err(anyhow::anyhow!(
                "refactoring produced edits for unknown file {:?}",
                file_id.0
            ));
        };
        let Some(updated) = new_workspace.get(file_id).cloned() else {
            return Err(anyhow::anyhow!(
                "refactoring produced edits for missing output file {:?}",
                file_id.0
            ));
        };

        if updated == original {
            continue;
        }

        let edits_owned = edits.into_iter().cloned().collect::<Vec<_>>();
        changed_files.push(file_id.0.clone());
        outputs.push(refactor_edits_to_json(
            file_id.0.clone(),
            original,
            &edits_owned,
        ));
        changed_texts.insert((*file_id).clone(), updated);
    }

    changed_files.sort();
    outputs.sort_by(|a, b| a.file.cmp(&b.file));

    if args.in_place {
        refactor_apply::apply_workspace_edit_to_disk(
            &project_root,
            &normalized_edit,
            &changed_texts,
        )
        .with_context(|| "failed to apply rename in-place")?;
    }

    let output = CliJsonOutput {
        ok: true,
        files_changed: changed_files,
        edits: outputs,
        file_ops,
        conflicts: Vec::new(),
        error: None,
    };

    if args.json {
        print_cli_json(&output)?;
    } else if args.in_place {
        for op in &normalized_edit.file_ops {
            match op {
                FileOp::Rename { from, to } => println!("renamed file {} -> {}", from.0, to.0),
                FileOp::Create { file, .. } => println!("created file {}", file.0),
                FileOp::Delete { file } => println!("deleted file {}", file.0),
            }
        }
        for file in &output.files_changed {
            println!("renamed occurrences in {}", file);
        }
    } else {
        for op in &normalized_edit.file_ops {
            match op {
                FileOp::Rename { from, to } => println!("would rename file {} -> {}", from.0, to.0),
                FileOp::Create { file, .. } => println!("would create file {}", file.0),
                FileOp::Delete { file } => println!("would delete file {}", file.0),
            }
        }
        for file in &output.files_changed {
            println!("would edit {}", file);
        }
    }

    Ok(0)
}

fn rename_error(json: bool, message: String) -> Result<i32> {
    if json {
        let output = CliJsonOutput {
            ok: false,
            files_changed: Vec::new(),
            edits: Vec::new(),
            file_ops: Vec::new(),
            conflicts: Vec::new(),
            error: Some(CliJsonError {
                kind: "SymbolResolutionFailed".to_string(),
                message,
            }),
        };
        print_cli_json(&output)?;
    } else {
        eprintln!("{message}");
    }
    Ok(1)
}

fn print_output<T: Serialize + 'static>(value: &T, json: bool) -> Result<()> {
    if json {
        let out = serde_json::to_string_pretty(value)?;
        println!("{out}");
    } else {
        // Human output for key types. Everything else falls back to pretty JSON.
        let any = value as &dyn std::any::Any;
        if let Some(report) = any.downcast_ref::<IndexReport>() {
            println!("indexed: {}", report.root.display());
            println!("  project_hash: {}", report.project_hash);
            println!("  cache_root: {}", report.cache_root.display());
            println!("  files_total: {}", report.metrics.files_total);
            println!("  files_invalidated: {}", report.metrics.files_invalidated);
            println!("  files_indexed: {}", report.metrics.files_indexed);
            println!("  bytes_indexed: {}", report.metrics.bytes_indexed);
            println!("  snapshot_ms: {}", report.metrics.snapshot_ms);
            println!("  index_ms: {}", report.metrics.index_ms);
            println!("  symbols_indexed: {}", report.metrics.symbols_indexed);
            println!("  elapsed_ms: {}", report.metrics.elapsed_ms);
            if let Some(rss) = report.metrics.rss_bytes {
                println!("  rss_bytes: {}", rss);
            }
        } else if let Some(report) = any.downcast_ref::<DiagnosticsReport>() {
            for d in &report.diagnostics {
                println!(
                    "{}:{}:{}: {}{} {}",
                    d.file.display(),
                    d.line,
                    d.column,
                    match d.severity {
                        nova_workspace::Severity::Error => "error",
                        nova_workspace::Severity::Warning => "warning",
                    },
                    d.code
                        .as_ref()
                        .map(|c| format!("[{c}]"))
                        .unwrap_or_default(),
                    d.message
                );
            }
            println!(
                "summary: {} errors, {} warnings",
                report.summary.errors, report.summary.warnings
            );
        } else if let Some(result) = any.downcast_ref::<ParseResult>() {
            print!("{}", result.tree);
            for e in &result.errors {
                println!("error:{}:{}: {}", e.line, e.column, e.message);
            }
        } else if let Some(symbols) = any.downcast_ref::<Vec<WorkspaceSymbol>>() {
            fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
                value.get(key).and_then(|v| v.as_str())
            }

            fn json_opt_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
                keys.iter()
                    .find_map(|key| json_string(value, key))
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty())
            }

            fn json_u32(value: &serde_json::Value, key: &str) -> Option<u32> {
                value
                    .get(key)
                    .and_then(|v| v.as_u64())
                    .and_then(|v| u32::try_from(v).ok())
            }

            fn json_location(value: &serde_json::Value) -> Option<(String, u32, u32)> {
                // Task 19: `WorkspaceSymbol` becomes flat and stores a single `location`.
                // For compatibility with older shape, also accept `locations[0]`.
                let loc = value
                    .get("location")
                    .or_else(|| value.get("locations").and_then(|v| v.get(0)))?;
                let file = json_string(loc, "file")?.to_string();
                let line = json_u32(loc, "line").unwrap_or(0);
                let column = json_u32(loc, "column").unwrap_or(0);
                Some((file, line, column))
            }

            fn kind_display(kind: Option<&serde_json::Value>) -> Option<String> {
                let kind = kind?;
                match kind {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => match n.as_u64() {
                        Some(1) => Some("file".to_string()),
                        Some(2) => Some("module".to_string()),
                        Some(3) => Some("namespace".to_string()),
                        Some(4) => Some("package".to_string()),
                        Some(5) => Some("class".to_string()),
                        Some(6) => Some("method".to_string()),
                        Some(7) => Some("property".to_string()),
                        Some(8) => Some("field".to_string()),
                        Some(9) => Some("constructor".to_string()),
                        Some(10) => Some("enum".to_string()),
                        Some(11) => Some("interface".to_string()),
                        Some(12) => Some("function".to_string()),
                        Some(13) => Some("variable".to_string()),
                        Some(14) => Some("constant".to_string()),
                        Some(23) => Some("struct".to_string()),
                        Some(19) => Some("object".to_string()),
                        _ => Some(n.to_string()),
                    },
                    _ => None,
                }
            }

            for sym in symbols {
                let value = serde_json::to_value(sym)?;
                let name = json_opt_string(&value, &["qualified_name", "qualifiedName"])
                    .or_else(|| json_opt_string(&value, &["name"]))
                    .unwrap_or_else(|| "<symbol>".to_string());
                let kind = kind_display(value.get("kind"));
                let container_name = json_opt_string(&value, &["container_name", "containerName"]);

                let Some((file, line, column)) = json_location(&value) else {
                    // Best-effort output if symbol has no location.
                    if let Some(kind) = kind {
                        println!("{name} [{kind}]");
                    } else {
                        println!("{name}");
                    }
                    continue;
                };

                let mut prefix = name;
                if let Some(kind) = kind {
                    prefix = format!("{prefix} [{kind}]");
                }
                if let Some(container) = container_name {
                    prefix = format!("{prefix} in {container}");
                }
                println!("{prefix} {file}:{line}:{column}");
            }
        } else {
            let out = serde_json::to_string_pretty(value)?;
            println!("{out}");
        }
    }
    Ok(())
}

fn print_cache_status(status: &CacheStatus, json: bool) -> Result<()> {
    if json {
        print_output(status, true)?;
        return Ok(());
    }

    println!("cache:");
    println!("  project_root: {}", status.project_root.display());
    println!("  project_hash: {}", status.project_hash);
    println!("  root: {}", status.cache_root.display());
    println!("  metadata: {}", status.metadata_path.display());
    println!("    present: {}", status.metadata.is_some());
    if let Some(meta) = &status.metadata {
        println!("    compatible: {}", meta.is_compatible());
        println!("    last_updated_millis: {}", meta.last_updated_millis);
    }

    println!("  indexes:");
    for idx in &status.indexes {
        let bytes = idx
            .bytes
            .map(|b| b.to_string())
            .unwrap_or_else(|| "(missing)".to_string());
        println!("    {}: {} ({})", idx.name, idx.path.display(), bytes);
    }

    println!("  perf: {}", status.perf_path.display());
    if let Some(bytes) = status.perf_bytes {
        println!("    bytes: {bytes}");
    } else {
        println!("    bytes: (missing)");
    }
    if let Some(perf) = &status.last_perf {
        println!("    files_total: {}", perf.files_total);
        println!("    files_invalidated: {}", perf.files_invalidated);
        println!("    files_indexed: {}", perf.files_indexed);
        println!("    bytes_indexed: {}", perf.bytes_indexed);
        println!("    snapshot_ms: {}", perf.snapshot_ms);
        println!("    index_ms: {}", perf.index_ms);
        println!("    symbols_indexed: {}", perf.symbols_indexed);
        println!("    elapsed_ms: {}", perf.elapsed_ms);
        if let Some(rss) = perf.rss_bytes {
            println!("    rss_bytes: {}", rss);
        }
    }

    Ok(())
}

fn load_run_from_path(path: &PathBuf) -> Result<BenchRun> {
    if path.is_dir() {
        return load_criterion_directory(path);
    }
    BenchRun::read_json(path)
}

fn resolve_bugreport_output_path(out: &PathBuf, bundle_path: &PathBuf) -> Result<PathBuf> {
    if out.exists() {
        if out.is_dir() {
            let name = bundle_path
                .file_name()
                .context("bugreport bundle path has no file name")?;
            Ok(out.join(name))
        } else {
            anyhow::bail!("bugreport output path {} already exists", out.display());
        }
    } else {
        Ok(out.clone())
    }
}

fn move_dir(src: &PathBuf, dest: &PathBuf) -> Result<()> {
    if dest.exists() {
        anyhow::bail!("bugreport destination {} already exists", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_dir_all(src, dest)?;
            std::fs::remove_dir_all(src)
                .with_context(|| format!("failed to remove {}", src.display()))?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to move bugreport bundle from {} to {}",
                src.display(),
                dest.display()
            )
        }),
    }
}

fn move_file(src: &PathBuf, dest: &PathBuf) -> Result<()> {
    if dest.exists() {
        anyhow::bail!("bugreport destination {} already exists", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            std::fs::copy(src, dest).with_context(|| {
                format!("failed to copy {} to {}", src.display(), dest.display())
            })?;
            std::fs::remove_file(src)
                .with_context(|| format!("failed to remove {}", src.display()))?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to move bugreport archive from {} to {}",
                src.display(),
                dest.display()
            )
        }),
    }
}

fn copy_dir_all(src: &PathBuf, dest: &PathBuf) -> Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).with_context(|| {
                format!("failed to copy {} to {}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

fn load_workspace_perf_metrics(path: &PathBuf) -> Result<PerfMetrics> {
    let perf_path = if path.is_dir() {
        path.join("perf.json")
    } else {
        path.clone()
    };

    let content = std::fs::read_to_string(&perf_path)
        .with_context(|| format!("failed to read {}", perf_path.display()))?;
    Ok(serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", perf_path.display()))?)
}

fn runtime_run_from_workspace_perf(perf: &PerfMetrics) -> RuntimeRun {
    fn usize_to_u64(value: usize) -> u64 {
        u64::try_from(value).unwrap_or(u64::MAX)
    }

    let mut run = RuntimeRun::default();
    run.metrics.insert(
        "workspace/index.files_total".to_string(),
        nova_perf::RuntimeMetric(usize_to_u64(perf.files_total)),
    );
    run.metrics.insert(
        "workspace/index.files_indexed".to_string(),
        nova_perf::RuntimeMetric(usize_to_u64(perf.files_indexed)),
    );
    run.metrics.insert(
        "workspace/index.bytes_indexed".to_string(),
        nova_perf::RuntimeMetric(perf.bytes_indexed),
    );
    run.metrics.insert(
        "workspace/index.symbols_indexed".to_string(),
        nova_perf::RuntimeMetric(usize_to_u64(perf.symbols_indexed)),
    );
    run.metrics.insert(
        "workspace/index.elapsed_ms".to_string(),
        nova_perf::RuntimeMetric(u64::try_from(perf.elapsed_ms).unwrap_or(u64::MAX)),
    );

    if let Some(rss) = perf.rss_bytes {
        run.metrics.insert(
            "workspace/index.rss_bytes".to_string(),
            nova_perf::RuntimeMetric(rss),
        );
    }

    run
}

fn add_lsp_runtime_metrics(run: &mut RuntimeRun, nova_lsp: &PathBuf) -> Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const MAX_LSP_MESSAGE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB
    const MAX_LSP_HEADER_LINE_BYTES: usize = 8 * 1024; // 8 KiB

    fn read_line_limited<R: BufRead>(reader: &mut R, max_len: usize) -> Result<Option<String>> {
        let mut buf = Vec::<u8>::new();
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }

            let newline_pos = available.iter().position(|&b| b == b'\n');
            let take = newline_pos.map(|pos| pos + 1).unwrap_or(available.len());
            if buf.len() + take > max_len {
                anyhow::bail!("LSP header line exceeds maximum size ({max_len} bytes)");
            }

            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            if newline_pos.is_some() {
                break;
            }
        }

        Ok(Some(String::from_utf8(buf)?))
    }

    fn write_lsp_message<W: Write>(writer: &mut W, value: &serde_json::Value) -> Result<()> {
        let payload = serde_json::to_vec(value)
            .map_err(|err| anyhow::Error::msg(sanitize_serde_json_error(&err)))?;
        write!(writer, "Content-Length: {}\r\n\r\n", payload.len())?;
        writer.write_all(&payload)?;
        writer.flush()?;
        Ok(())
    }

    fn read_lsp_message<R: Read>(reader: &mut BufReader<R>) -> Result<Option<serde_json::Value>> {
        let mut content_length: Option<usize> = None;

        loop {
            let Some(line) = read_line_limited(reader, MAX_LSP_HEADER_LINE_BYTES)? else {
                return Ok(None);
            };

            let header = line.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                break;
            }

            if let Some(rest) = header.strip_prefix("Content-Length:") {
                let value = rest.trim();
                content_length =
                    Some(value.parse::<usize>().with_context(|| {
                        format!("invalid Content-Length header value {value:?}")
                    })?);
            }
        }

        let len = content_length.context("missing Content-Length header")?;
        anyhow::ensure!(
            len <= MAX_LSP_MESSAGE_BYTES,
            "LSP message Content-Length {len} exceeds maximum allowed size {MAX_LSP_MESSAGE_BYTES}"
        );
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        let value = serde_json::from_slice(&payload)
            .map_err(|err| anyhow::Error::msg(sanitize_serde_json_error(&err)))?;
        Ok(Some(value))
    }

    fn read_lsp_response<R: Read>(
        reader: &mut BufReader<R>,
        expected_id: i32,
    ) -> Result<serde_json::Value> {
        let expected = serde_json::Value::from(expected_id);
        loop {
            let Some(message) = read_lsp_message(reader)? else {
                return Err(anyhow::anyhow!(
                    "unexpected EOF while waiting for LSP response"
                ));
            };

            // Ignore notifications.
            let Some(id) = message.get("id") else {
                continue;
            };

            if id != &expected {
                continue;
            }

            if let Some(err) = message.get("error") {
                return Err(anyhow::anyhow!("LSP request failed: {}", err));
            }

            return Ok(message);
        }
    }

    let mut child = Command::new(nova_lsp)
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", nova_lsp.display()))?;

    let mut stdin = child.stdin.take().context("nova-lsp stdin unavailable")?;
    let stdout = child.stdout.take().context("nova-lsp stdout unavailable")?;
    let mut reader = BufReader::new(stdout);

    let start = Instant::now();
    write_lsp_message(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootUri": null,
                "capabilities": {},
            },
        }),
    )?;
    let _ = read_lsp_response(&mut reader, 1)?;
    run.metrics.insert(
        "lsp/startup.elapsed_ms".to_string(),
        nova_perf::RuntimeMetric(u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)),
    );

    write_lsp_message(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/memoryStatus",
            "params": null,
        }),
    )?;
    let response = read_lsp_response(&mut reader, 2)?;

    let usage = response
        .pointer("/result/report/usage")
        .context("missing memoryStatus result.report.usage")?;
    let total = usage
        .get("query_cache")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .saturating_add(
            usage
                .get("syntax_trees")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        )
        .saturating_add(usage.get("indexes").and_then(|v| v.as_u64()).unwrap_or(0))
        .saturating_add(usage.get("type_info").and_then(|v| v.as_u64()).unwrap_or(0))
        .saturating_add(usage.get("other").and_then(|v| v.as_u64()).unwrap_or(0));
    run.metrics.insert(
        "lsp/memory.usage_total_bytes".to_string(),
        nova_perf::RuntimeMetric(total),
    );

    write_lsp_message(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "shutdown",
            "params": null,
        }),
    )?;
    let _ = read_lsp_response(&mut reader, 3)?;

    // `exit` is a notification (no response).
    let _ = write_lsp_message(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null,
        }),
    );

    let _ = child.wait();

    Ok(())
}

fn sanitize_serde_json_error(err: &serde_json::Error) -> String {
    sanitize_json_error_message(&err.to_string())
}

fn sanitize_anyhow_error_message(err: &anyhow::Error) -> String {
    // Many Nova subsystems use `anyhow` and error chains. If a `serde_json::Error` appears anywhere
    // in the chain, sanitize the formatted output before printing it to stderr so user-controlled
    // scalar values don't leak into logs.
    if err.chain().any(contains_serde_json_error) {
        sanitize_json_error_message(&format!("{err:#}"))
    } else {
        format!("{err:#}")
    }
}

fn contains_serde_json_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = current {
        if err.is::<serde_json::Error>() {
            return true;
        }

        if let Some(build_err) = err.downcast_ref::<nova_build::BuildError>() {
            match build_err {
                nova_build::BuildError::Io(io_err) => {
                    if contains_serde_json_error(io_err) {
                        return true;
                    }
                }
                nova_build::BuildError::Cache(cache_err) => {
                    if contains_serde_json_error(cache_err) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io_err.get_ref() {
                let inner: &(dyn std::error::Error + 'static) = inner;
                if contains_serde_json_error(inner) {
                    return true;
                }
            }
        }

        current = err.source();
    }
    false
}

fn sanitize_json_error_message(message: &str) -> String {
    nova_core::sanitize_json_error_message(message)
}

#[cfg(test)]
mod json_error_sanitization_tests {
    use super::*;

    #[test]
    fn sanitize_serde_json_error_does_not_echo_string_values() {
        let secret_suffix = "nova-cli-super-secret-token";
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
    fn sanitize_serde_json_error_does_not_echo_backticked_values() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-cli-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let err = serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");

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

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values() {
        use anyhow::Context as _;

        let secret_suffix = "nova-cli-anyhow-super-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON input")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_backticked_values() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-cli-anyhow-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let raw_message = serde_err.to_string();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw serde_json error string to include the backticked value so this test catches leaks: {raw_message}"
        );

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON input")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        let secret_suffix = "nova-cli-anyhow-io-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);

        let err = Err::<(), _>(io_err)
            .context("failed to parse JSON input")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_backticked_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-cli-anyhow-io-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);

        let err = Err::<(), _>(io_err)
            .context("failed to parse JSON input")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_string_values_when_wrapped_in_build_error() {
        use anyhow::Context as _;

        let secret_suffix = "nova-cli-anyhow-build-error-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
        let build_err: nova_build::BuildError = io_err.into();

        let err = Err::<(), _>(build_err)
            .context("failed to parse JSON input")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_anyhow_error_message_does_not_echo_backticked_values_when_wrapped_in_build_error() {
        use anyhow::Context as _;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-cli-anyhow-build-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let serde_err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
        let build_err: nova_build::BuildError = io_err.into();

        let err = Err::<(), _>(build_err)
            .context("failed to parse JSON input")
            .expect_err("expected anyhow error");

        let message = sanitize_anyhow_error_message(&err);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized anyhow error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized anyhow error message to include redaction marker: {message}"
        );
    }
}
