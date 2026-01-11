use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use nova_ai::AiClient;
use nova_bugreport::{global_crash_store, BugReportBuilder, BugReportOptions, PerfStats};
use nova_cache::{
    atomic_write, fetch_cache_package, install_cache_package, pack_cache_package, CacheConfig,
    CacheDir, CachePackageInstallOutcome,
};
use nova_config::{global_log_buffer, init_tracing_with_config, NovaConfig, NOVA_CONFIG_ENV_VAR};
use nova_core::{
    apply_text_edits as apply_core_text_edits, LineIndex, Position, Range,
    TextEdit as CoreTextEdit, TextSize,
};
mod diagnostics_output;
use diagnostics_output::{print_github_annotations, print_sarif, write_sarif, DiagnosticsFormat};
use nova_deps_cache::DependencyIndexStore;
use nova_format::{edits_for_range_formatting, format_java, minimal_text_edits, FormatConfig};
use nova_perf::{compare_runs, load_criterion_directory, BenchRun, ThresholdConfig};
use nova_refactor::{
    apply_text_edits as apply_refactor_text_edits, organize_imports as refactor_organize_imports,
    rename as refactor_rename, Conflict, FileId as RefactorFileId, InMemoryJavaDatabase,
    OrganizeImportsParams, RenameParams, SemanticRefactorError,
    WorkspaceTextEdit as RefactorTextEdit,
};
use nova_syntax::parse;
use nova_workspace::{
    CacheStatus, DiagnosticsReport, IndexReport, ParseResult, Workspace, WorkspaceSymbol,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "nova",
    version,
    about = "Nova CLI (indexing, diagnostics, cache, perf)"
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
        #[arg(long = "thresholds-config")]
        thresholds_config: Option<PathBuf>,
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

    let config = load_config_from_cli(&cli);

    let _ = init_tracing_with_config(&config);

    let exit_code = match run(cli, &config) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{:#}", err);
            2
        }
    };

    std::process::exit(exit_code);
}

fn load_config_from_cli(cli: &Cli) -> NovaConfig {
    // Prefer explicit `--config` (and propagate it to nested loaders).
    if let Some(path) = cli.config.as_ref() {
        let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
        env::set_var(NOVA_CONFIG_ENV_VAR, &resolved);
        match NovaConfig::load_from_path(&resolved)
            .with_context(|| format!("load config from {}", resolved.display()))
        {
            Ok(config) => return config,
            Err(err) => {
                eprintln!("{:#}", err);
                std::process::exit(2);
            }
        }
    }

    let root = config_root_from_command(&cli.command)
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let root = if root.is_file() {
        root.parent().map(Path::to_path_buf).unwrap_or(root)
    } else {
        root
    };

    match nova_config::load_for_workspace(&root) {
        Ok((config, _path)) => config,
        Err(err) => {
            eprintln!(
                "nova-cli: failed to load workspace config from {}: {err}",
                root.display()
            );
            NovaConfig::default()
        }
    }
}

fn config_root_from_command(command: &Command) -> Option<PathBuf> {
    match command {
        Command::Index(args) => Some(args.path.clone()),
        Command::Diagnostics(args) => args.path.is_dir().then(|| args.path.clone()),
        Command::Symbols(args) => Some(args.path.clone()),
        Command::Cache(args) => match &args.command {
            CacheCommand::Clean(args) | CacheCommand::Status(args) | CacheCommand::Warm(args) => {
                Some(args.path.clone())
            }
            CacheCommand::Pack(args) => Some(args.path.clone()),
            CacheCommand::Install(args) => Some(args.path.clone()),
            CacheCommand::Fetch(args) => Some(args.path.clone()),
        },
        // File-centric commands assume the current working directory is the workspace root unless
        // `--config` / `NOVA_CONFIG_PATH` is provided.
        Command::Parse(_)
        | Command::Format(_)
        | Command::OrganizeImports(_)
        | Command::Refactor(_) => None,
        // Other commands are not tied to a workspace (deps cache, perf tooling, etc).
        Command::Deps(_) | Command::Perf(_) | Command::BugReport(_) | Command::Ai(_) => None,
    }
}

fn run(cli: Cli, config: &NovaConfig) -> Result<i32> {
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
            let ws = Workspace::open(&args.path)?;
            let results = ws
                .workspace_symbols(&args.query)?
                .into_iter()
                .take(args.limit)
                .collect::<Vec<_>>();
            print_output(&results, args.json)?;
            Ok(0)
        }
        Command::Deps(args) => match args.command {
            DepsCommand::Index { jar } => {
                let store = DependencyIndexStore::from_env()?;
                let stats = nova_classpath::IndexingStats::default();

                let entry = if jar.extension().and_then(|e| e.to_str()) == Some("jmod") {
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

fn display_path(path: &Path) -> String {
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
    VisibilityLoss {
        file: String,
        name: String,
        usage_range: CliJsonRange,
        start_byte: usize,
        end_byte: usize,
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
    let newline_style = detect_newline_style(&source);

    let tree = parse(&source);
    let config = FormatConfig::default();

    let mut edits: Vec<CoreTextEdit> = match args.range.as_deref() {
        Some(range) => {
            let range = parse_cli_range(range)?;
            let mut edits = edits_for_range_formatting(&tree, &source, range, &config)?;
            for edit in &mut edits {
                edit.replacement = convert_newlines(&edit.replacement, newline_style);
            }
            // Drop no-op edits (commonly caused by newline normalization).
            edits.retain(|edit| {
                let start = u32::from(edit.range.start()) as usize;
                let end = u32::from(edit.range.end()) as usize;
                source
                    .get(start..end)
                    .map(|slice| slice != edit.replacement)
                    .unwrap_or(true)
            });
            edits
        }
        None => {
            let formatted = format_java(&tree, &source, &config);
            let formatted = convert_newlines(&formatted, newline_style);
            minimal_text_edits(&source, &formatted)
        }
    };

    // Normalize edits for deterministic JSON output.
    edits.sort_by_key(|e| (e.range.start(), e.range.end(), e.replacement.clone()));

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
    let db = InMemoryJavaDatabase::new([(file_id.clone(), source.clone())]);

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

fn collect_java_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn should_skip_dir(path: &Path) -> bool {
        matches!(
            path.file_name().and_then(|s| s.to_str()),
            Some(".git" | "target" | ".nova" | "out" | "build")
        )
    }

    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if should_skip_dir(&dir) && dir != root {
            continue;
        }
        for entry in fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() && path.extension().and_then(|e| e.to_str()) == Some("java") {
                out.push(path);
            }
        }
    }

    out.sort();
    Ok(out)
}

fn path_relative_to(root: &Path, path: &Path) -> Result<String> {
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
    files: &BTreeMap<String, String>,
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
            Conflict::VisibilityLoss {
                file,
                usage_range,
                name,
            } => {
                let text = files.get(&file.0).map(String::as_str).unwrap_or("");
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
            CliJsonConflict::VisibilityLoss {
                file,
                name,
                start_byte,
                end_byte,
                ..
            } => (file, 2, name, *start_byte, *end_byte, ""),
        }
    }

    out.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    out
}

fn handle_rename(args: RenameArgs) -> Result<i32> {
    let ws = Workspace::open(&args.file)?;
    let root = ws.root().to_path_buf();

    let java_files = collect_java_files(&root)?;
    anyhow::ensure!(
        !java_files.is_empty(),
        "no .java files found under {}",
        root.display()
    );

    let mut file_texts: BTreeMap<String, String> = BTreeMap::new();
    let mut db_files = Vec::with_capacity(java_files.len());
    for path in java_files {
        let file_id = path_relative_to(&root, &path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        file_texts.insert(file_id.clone(), text.clone());
        db_files.push((RefactorFileId::new(file_id), text));
    }

    let db = InMemoryJavaDatabase::new(db_files);
    let target_path = fs::canonicalize(&args.file)
        .with_context(|| format!("canonicalize {}", args.file.display()))?;
    let target_file_id_str = path_relative_to(&root, &target_path)?;
    let target_file = RefactorFileId::new(target_file_id_str.clone());
    let Some(target_text) = file_texts.get(&target_file_id_str) else {
        // Should be impossible because we loaded the workspace file list.
        return Ok(rename_error(
            args.json,
            format!("file {target_file_id_str:?} was not loaded from workspace"),
        )?);
    };

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
            format!(
                "no symbol at {}:{}:{}",
                target_file_id_str, args.line, args.col
            ),
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
        Err(err) => return Err(anyhow::anyhow!(err)),
    };

    let by_file = edit.edits_by_file();
    let mut changed_files: Vec<String> = Vec::new();
    let mut outputs: Vec<CliJsonFileEdits> = Vec::new();

    // Compute the new file contents before writing anything.
    let mut new_texts: BTreeMap<String, String> = BTreeMap::new();

    for (file_id, edits) in by_file {
        let file_str = file_id.0.clone();
        let Some(original) = file_texts.get(&file_str) else {
            return Err(anyhow::anyhow!(
                "refactoring produced edits for unknown file {file_str:?}"
            ));
        };

        let edits_owned = edits.into_iter().cloned().collect::<Vec<_>>();
        let updated = apply_refactor_text_edits(original, &edits_owned)
            .map_err(|err| anyhow::anyhow!(err))?;
        if updated == *original {
            continue;
        }

        changed_files.push(file_str.clone());
        outputs.push(refactor_edits_to_json(
            file_str.clone(),
            original,
            &edits_owned,
        ));
        new_texts.insert(file_str, updated);
    }

    changed_files.sort();
    outputs.sort_by(|a, b| a.file.cmp(&b.file));

    if args.in_place {
        for (file, text) in &new_texts {
            let path = root.join(Path::new(file));
            atomic_write(&path, text.as_bytes())
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }

    let output = CliJsonOutput {
        ok: true,
        files_changed: changed_files,
        edits: outputs,
        conflicts: Vec::new(),
        error: None,
    };

    if args.json {
        print_cli_json(&output)?;
    } else if args.in_place {
        for file in &output.files_changed {
            println!("renamed occurrences in {}", file);
        }
    } else {
        // Human output: brief summary.
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
