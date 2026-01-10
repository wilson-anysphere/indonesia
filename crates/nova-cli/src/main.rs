use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use nova_workspace::{CacheStatus, DiagnosticsReport, IndexReport, ParseResult, Workspace};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nova", version, about = "Nova CLI (indexing, diagnostics, cache, perf)")]
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
    /// Manage persistent cache stored in `.nova-cache`
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
                    ws.cache_clean()?;
                    if !args.json {
                        println!("cache: cleaned {}", ws.cache_dir().display());
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
                        println!("  files_scanned: {}", perf.files_scanned);
                        println!("  bytes_scanned: {}", perf.bytes_scanned);
                        println!("  symbols_indexed: {}", perf.symbols_indexed);
                        println!("  elapsed_ms: {}", perf.elapsed_ms);
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
            println!("  files_scanned: {}", report.metrics.files_scanned);
            println!("  bytes_scanned: {}", report.metrics.bytes_scanned);
            println!("  symbols_indexed: {}", report.metrics.symbols_indexed);
            println!("  elapsed_ms: {}", report.metrics.elapsed_ms);
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
    println!("  dir: {}", status.cache_dir.display());
    println!("  exists: {}", status.exists);
    println!("  index: {}", status.index_path.display());
    if let Some(bytes) = status.index_bytes {
        println!("    bytes: {bytes}");
    } else {
        println!("    bytes: (missing)");
    }
    if let Some(symbols) = status.symbols_indexed {
        println!("    symbols_indexed: {symbols}");
    }
    println!("  perf: {}", status.perf_path.display());
    if let Some(bytes) = status.perf_bytes {
        println!("    bytes: {bytes}");
    } else {
        println!("    bytes: (missing)");
    }
    if let Some(perf) = &status.last_perf {
        println!("    files_scanned: {}", perf.files_scanned);
        println!("    bytes_scanned: {}", perf.bytes_scanned);
        println!("    symbols_indexed: {}", perf.symbols_indexed);
        println!("    elapsed_ms: {}", perf.elapsed_ms);
    }

    Ok(())
}
