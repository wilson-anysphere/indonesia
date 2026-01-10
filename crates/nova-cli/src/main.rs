use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use nova_ai::AiClient;
use nova_config::NovaConfig;
use nova_cache::{
    fetch_cache_package, install_cache_package, pack_cache_package, CacheConfig, CacheDir,
    CachePackageInstallOutcome,
};
use nova_perf::{compare_runs, load_criterion_directory, BenchRun, ThresholdConfig};
use nova_workspace::{
    CacheStatus, DiagnosticsReport, IndexReport, ParseResult, Workspace, WorkspaceSymbol,
};
use serde::Serialize;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "nova",
    version,
    about = "Nova CLI (indexing, diagnostics, cache, perf)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load a project and build indexes/caches
    Index(IndexArgs),
    /// Run diagnostics for an entire project or a single file
    Diagnostics(DiagnosticsArgs),
    /// Workspace symbol search (defaults to current directory)
    Symbols(SymbolsArgs),
    /// Manage persistent cache (defaults to `~/.nova/cache/<project-hash>/`, override with `NOVA_CACHE_DIR`)
    Cache(CacheArgs),
    /// Performance tools (cached perf report + benchmark comparison)
    Perf(PerfArgs),
    /// Print a debug parse tree / errors for a single file
    Parse(ParseArgs),
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
}

#[derive(Args)]
struct AiModelsArgs {
    /// Optional path to a TOML config file (defaults to in-memory defaults).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
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
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
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
}

#[derive(Args)]
struct CacheArgs {
    #[command(subcommand)]
    command: CacheCommand,
}

#[derive(Subcommand)]
enum CacheCommand {
    Clean(WorkspaceArgs),
    Status(WorkspaceArgs),
    Warm(WorkspaceArgs),

    /// Package a project's persistent cache directory into a single tar.zst archive.
    Pack(CachePackArgs),
    /// Install a packaged cache archive for a project.
    Install(CacheInstallArgs),
    /// Fetch a cache package from a URL (http/https/file/s3) and install it.
    Fetch(CacheFetchArgs),
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
        #[arg(long)]
        config: Option<PathBuf>,
        /// Allow regressions for these benchmark IDs (repeatable).
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

fn main() {
    let cli = Cli::parse();
    let exit_code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{:#}", err);
            2
        }
    };

    std::process::exit(exit_code);
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Index(args) => {
            let ws = Workspace::open(&args.path)?;
            let report = ws.index_and_write_cache()?;
            print_output(&report, args.json)?;
            Ok(0)
        }
        Command::Diagnostics(args) => {
            let ws = Workspace::open(&args.path)?;
            let report = ws.diagnostics(&args.path)?;
            let exit = if report.summary.errors > 0 { 1 } else { 0 };
            print_output(&report, args.json)?;
            Ok(exit)
        }
        Command::Symbols(args) => {
            let ws = Workspace::open(&args.path)?;
            let results = ws
                .workspace_symbols(&args.query)?
                .into_iter()
                .take(args.limit)
                .collect::<Vec<_>>();
            print_output(&results, args.json)?;
            Ok(0)
        }
        Command::Cache(args) => {
            match args.command {
                CacheCommand::Clean(args) => {
                    let ws = Workspace::open(&args.path)?;
                    let cache_root = ws.cache_root()?;
                    ws.cache_clean()?;
                    if !args.json {
                        println!("cache: cleaned {}", cache_root.display());
                    } else {
                        print_output(&serde_json::json!({ "ok": true }), true)?;
                    }
                }
                CacheCommand::Status(args) => {
                    let ws = Workspace::open(&args.path)?;
                    let status = ws.cache_status()?;
                    print_cache_status(&status, args.json)?;
                }
                CacheCommand::Warm(args) => {
                    let ws = Workspace::open(&args.path)?;
                    let report = ws.cache_warm()?;
                    print_output(&report, args.json)?;
                }
                CacheCommand::Pack(args) => {
                    let ws = Workspace::open(&args.path)?;
                    let cache_dir = CacheDir::new(ws.root(), CacheConfig::from_env())?;
                    pack_cache_package(&cache_dir, &args.out)?;
                    if args.json {
                        print_output(
                            &serde_json::json!({ "ok": true, "out": args.out }),
                            true,
                        )?;
                    } else {
                        println!(
                            "cache: packed {} -> {}",
                            cache_dir.root().display(),
                            args.out.display()
                        );
                    }
                }
                CacheCommand::Install(args) => {
                    let ws = Workspace::open(&args.path)?;
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
                    let ws = Workspace::open(&args.path)?;
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
                let ws = Workspace::open(&args.path)?;
                let perf = ws.perf_report()?;
                if args.json {
                    print_output(&PerfEnvelope { perf }, true)?;
                } else if let Some(perf) = perf {
                    println!("perf:");
                    println!("  files_total: {}", perf.files_total);
                    println!("  files_indexed: {}", perf.files_indexed);
                    println!("  bytes_indexed: {}", perf.bytes_indexed);
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
                config,
                allow,
                markdown_out,
            } => {
                let baseline_run = load_run_from_path(&baseline)
                    .with_context(|| format!("load baseline run from {}", baseline.display()))?;
                let current_run = load_run_from_path(&current)
                    .with_context(|| format!("load current run from {}", current.display()))?;

                let config = match config {
                    Some(path) => ThresholdConfig::read_toml(&path)
                        .with_context(|| format!("load thresholds config {}", path.display()))?,
                    None => ThresholdConfig::default(),
                };

                let comparison = compare_runs(&baseline_run, &current_run, &config, &allow);
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
        Command::Ai(args) => match args.command {
            AiCommand::Models(args) => {
                let cfg = match args.config.as_ref() {
                    Some(path) => NovaConfig::load_from_path(path)?,
                    None => NovaConfig::default(),
                };

                let client = AiClient::from_config(&cfg.ai)?;
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
        },
        Command::Parse(args) => {
            let ws = Workspace::open(&args.file)?;
            let result = ws.parse_file(&args.file)?;
            let exit = if result.errors.is_empty() { 0 } else { 1 };
            print_output(&result, args.json)?;
            Ok(exit)
        }
    }
}

#[derive(Serialize)]
struct PerfEnvelope<T> {
    perf: T,
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
            println!("  files_indexed: {}", report.metrics.files_indexed);
            println!("  bytes_indexed: {}", report.metrics.bytes_indexed);
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
            for sym in symbols {
                if sym.locations.is_empty() {
                    println!("{}", sym.name);
                    continue;
                }
                for loc in &sym.locations {
                    println!("{} {}:{}:{}", sym.name, loc.file, loc.line, loc.column);
                }
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
        println!("    files_indexed: {}", perf.files_indexed);
        println!("    bytes_indexed: {}", perf.bytes_indexed);
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
