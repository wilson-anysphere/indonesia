use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use nova_workspace::{CacheStatus, DiagnosticsReport, IndexReport, ParseResult, Workspace};
use serde::Serialize;
use std::path::PathBuf;

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
    /// Manage persistent cache (defaults to `~/.nova/cache`, override with `NOVA_CACHE_DIR`)
    Cache(CacheArgs),
    /// Print performance metrics captured during indexing
    Perf(PerfArgs),
    /// Print a debug parse tree / errors for a single file
    Parse(ParseArgs),
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
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CacheArgs {
    #[command(subcommand)]
    command: CacheCommand,
    /// Workspace root (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum CacheCommand {
    Clean,
    Status,
    Warm,
}

#[derive(Args)]
struct PerfArgs {
    #[command(subcommand)]
    command: PerfCommand,
    /// Workspace root (defaults to current directory)
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Emit JSON suitable for CI
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum PerfCommand {
    Report,
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
            let results = ws.workspace_symbols(&args.query)?;
            print_output(&results, args.json)?;
            Ok(0)
        }
        Command::Cache(args) => {
            let ws = Workspace::open(&args.path)?;
            match args.command {
                CacheCommand::Clean => {
                    let cache_root = ws.cache_root()?;
                    ws.cache_clean()?;
                    if !args.json {
                        println!("cache: cleaned {}", cache_root.display());
                    } else {
                        print_output(&serde_json::json!({ "ok": true }), true)?;
                    }
                }
                CacheCommand::Status => {
                    let status = ws.cache_status()?;
                    print_cache_status(&status, args.json)?;
                }
                CacheCommand::Warm => {
                    let report = ws.cache_warm()?;
                    print_output(&report, args.json)?;
                }
            }
            Ok(0)
        }
        Command::Perf(args) => {
            let ws = Workspace::open(&args.path)?;
            match args.command {
                PerfCommand::Report => {
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
                        println!("perf: no cached metrics found (run `nova index <path>` or `nova cache warm`)");
                    }
                }
            }
            Ok(0)
        }
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
