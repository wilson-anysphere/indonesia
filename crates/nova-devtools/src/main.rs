use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Context as _};

use nova_devtools::output::{print_diagnostics, print_human, print_json, JsonReport};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{}", sanitize_anyhow_error_message(&err));
            ExitCode::from(2)
        }
    }
}

fn sanitize_anyhow_error_message(err: &anyhow::Error) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (e.g.
    // `invalid type: string "..."`). When devtools parses JSON from tools like `cargo metadata`,
    // avoid echoing those values in stderr output.
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
    fn sanitize_anyhow_error_message_does_not_echo_string_values() {
        use anyhow::Context as _;

        let secret_suffix = "nova-devtools-super-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let err = Err::<(), _>(serde_err)
            .context("failed to parse JSON")
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
    fn sanitize_anyhow_error_message_does_not_echo_string_values_when_wrapped_in_io_error() {
        use anyhow::Context as _;

        let secret_suffix = "nova-devtools-anyhow-io-secret";
        let secret = format!("prefix\"{secret_suffix}");
        let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);

        let err = Err::<(), _>(io_err)
            .context("failed to parse JSON")
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
}

fn run() -> anyhow::Result<ExitCode> {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        print_help();
        return Ok(ExitCode::SUCCESS);
    };

    match cmd.as_str() {
        "check-deps" => {
            let opts = parse_common_layer_args(args, "check-deps")?;
            let report = nova_devtools::check_deps::check(
                &opts.config,
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
            )
            .with_context(|| format!("check-deps failed using config {}", opts.config.display()))?;

            emit_report("check-deps", opts.json, report.ok, report.diagnostics)?;
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "check-layers" => {
            let opts = parse_common_layer_args(args, "check-layers")?;
            let report = nova_devtools::check_layers::check(
                &opts.config,
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
            )
            .with_context(|| {
                format!("check-layers failed using config {}", opts.config.display())
            })?;

            emit_report("check-layers", opts.json, report.ok, report.diagnostics)?;
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "check-architecture-map" => {
            let opts = parse_check_arch_map_args(args)?;
            let report = nova_devtools::check_arch_map::check(
                &opts.doc,
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
                opts.strict,
            )
            .with_context(|| {
                format!("check-architecture-map failed using {}", opts.doc.display())
            })?;

            emit_report(
                "check-architecture-map",
                opts.json,
                report.ok,
                report.diagnostics,
            )?;
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "graph-deps" => {
            let opts = parse_graph_deps_args(args)?;
            let report = nova_devtools::graph::generate(
                &opts.config,
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
            )
            .with_context(|| format!("graph-deps failed using config {}", opts.config.display()))?;

            if opts.json {
                let output_path = opts
                    .output
                    .unwrap_or_else(nova_devtools::graph::default_output_path);
                nova_devtools::graph::write_dot(&output_path, &report.dot)?;
                #[derive(serde::Serialize)]
                struct GraphJson {
                    #[serde(flatten)]
                    base: JsonReport,
                    output_path: String,
                }

                let json = GraphJson {
                    base: JsonReport::new("graph-deps", report.ok, report.diagnostics),
                    output_path: output_path.display().to_string(),
                };
                let json = serde_json::to_string_pretty(&json)
                    .context("failed to serialize JSON output")?;
                println!("{json}");
            } else if let Some(path) = opts.output {
                nova_devtools::graph::write_dot(&path, &report.dot)?;
                print_human("graph-deps", report.ok, &report.diagnostics);
                println!("graph-deps: wrote {}", path.display());
            } else {
                // No `--output`: emit DOT to stdout.
                print_diagnostics(&report.diagnostics);
                println!("{}", report.dot);
            }

            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "check-protocol-extensions" => {
            let opts = parse_check_protocol_extensions_args(args)?;
            let report =
                nova_devtools::check_protocol_extensions::check(&opts.doc).with_context(|| {
                    format!(
                        "check-protocol-extensions failed using {}",
                        opts.doc.display()
                    )
                })?;

            emit_report(
                "check-protocol-extensions",
                opts.json,
                report.ok,
                report.diagnostics,
            )?;
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "check-test-layout" => {
            let opts = parse_common_workspace_args(args, "check-test-layout")?;
            let report = nova_devtools::check_test_layout::check(
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
            )
            .context("check-test-layout failed")?;

            emit_report(
                "check-test-layout",
                opts.json,
                report.ok,
                report.diagnostics,
            )?;
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "check-repo-invariants" => {
            let opts = parse_check_repo_invariants_args(args)?;

            let deps = nova_devtools::check_deps::check(
                &opts.config,
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
            )
            .with_context(|| format!("check-deps failed using config {}", opts.config.display()))?;
            let deps_ok = deps.ok;

            let layers = nova_devtools::check_layers::check(
                &opts.config,
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
            )
            .with_context(|| {
                format!("check-layers failed using config {}", opts.config.display())
            })?;
            let layers_ok = layers.ok;

            let arch = nova_devtools::check_arch_map::check(
                &opts.architecture_map,
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
                true,
            )
            .with_context(|| {
                format!(
                    "check-architecture-map failed using {}",
                    opts.architecture_map.display()
                )
            })?;
            let arch_ok = arch.ok;

            let proto = nova_devtools::check_protocol_extensions::check(&opts.protocol_extensions)
                .with_context(|| {
                    format!(
                        "check-protocol-extensions failed using {}",
                        opts.protocol_extensions.display()
                    )
                })?;
            let proto_ok = proto.ok;

            let test_layout = nova_devtools::check_test_layout::check(
                opts.manifest_path.as_deref(),
                opts.metadata_path.as_deref(),
            )
            .context("check-test-layout failed")?;
            let test_layout_ok = test_layout.ok;

            let overall_ok = deps_ok && layers_ok && arch_ok && proto_ok && test_layout_ok;

            if opts.json {
                let mut diagnostics = Vec::new();
                diagnostics.extend(deps.diagnostics);
                diagnostics.extend(layers.diagnostics);
                diagnostics.extend(arch.diagnostics);
                diagnostics.extend(proto.diagnostics);
                diagnostics.extend(test_layout.diagnostics);
                emit_report("check-repo-invariants", true, overall_ok, diagnostics)?;
            } else {
                print_human("check-deps", deps_ok, &deps.diagnostics);
                print_human("check-layers", layers_ok, &layers.diagnostics);
                print_human("check-architecture-map", arch_ok, &arch.diagnostics);
                print_human("check-protocol-extensions", proto_ok, &proto.diagnostics);
                print_human(
                    "check-test-layout",
                    test_layout_ok,
                    &test_layout.diagnostics,
                );

                if overall_ok {
                    println!("check-repo-invariants: ok");
                } else {
                    eprintln!("check-repo-invariants: failed");
                }
            }

            Ok(if overall_ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        "-h" | "--help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        other => Err(anyhow!(
            "unknown command {other:?}\n\nRun `nova-devtools --help` for usage."
        )),
    }
}

fn emit_report(
    command: &str,
    json: bool,
    ok: bool,
    diagnostics: Vec<nova_devtools::output::Diagnostic>,
) -> anyhow::Result<()> {
    if json {
        print_json(&JsonReport::new(command, ok, diagnostics))
    } else {
        print_human(command, ok, &diagnostics);
        Ok(())
    }
}

#[derive(Debug)]
struct CommonLayerArgs {
    config: PathBuf,
    manifest_path: Option<PathBuf>,
    metadata_path: Option<PathBuf>,
    json: bool,
}

#[derive(Debug)]
struct CommonWorkspaceArgs {
    manifest_path: Option<PathBuf>,
    metadata_path: Option<PathBuf>,
    json: bool,
}

fn parse_common_layer_args<I>(mut args: I, cmd: &str) -> anyhow::Result<CommonLayerArgs>
where
    I: Iterator<Item = String>,
{
    let mut config = PathBuf::from("crate-layers.toml");
    let mut manifest_path = None;
    let mut metadata_path = None;
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a value"))?;
                config = PathBuf::from(value);
            }
            "--manifest-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--manifest-path requires a value"))?;
                manifest_path = Some(PathBuf::from(value));
            }
            "--metadata-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--metadata-path requires a value"))?;
                metadata_path = Some(PathBuf::from(value));
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print_command_help(cmd);
                std::process::exit(0);
            }
            other => {
                return Err(anyhow!(
                    "unknown argument {other:?}\n\nRun `nova-devtools {cmd} --help` for usage."
                ));
            }
        }
    }

    Ok(CommonLayerArgs {
        config,
        manifest_path,
        metadata_path,
        json,
    })
}

fn parse_common_workspace_args<I>(mut args: I, cmd: &str) -> anyhow::Result<CommonWorkspaceArgs>
where
    I: Iterator<Item = String>,
{
    let mut manifest_path = None;
    let mut metadata_path = None;
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--manifest-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--manifest-path requires a value"))?;
                manifest_path = Some(PathBuf::from(value));
            }
            "--metadata-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--metadata-path requires a value"))?;
                metadata_path = Some(PathBuf::from(value));
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print_command_help(cmd);
                std::process::exit(0);
            }
            other => {
                return Err(anyhow!(
                    "unknown argument {other:?}\n\nRun `nova-devtools {cmd} --help` for usage."
                ));
            }
        }
    }

    Ok(CommonWorkspaceArgs {
        manifest_path,
        metadata_path,
        json,
    })
}

#[derive(Debug)]
struct ArchMapArgs {
    doc: PathBuf,
    manifest_path: Option<PathBuf>,
    metadata_path: Option<PathBuf>,
    strict: bool,
    json: bool,
}

fn parse_check_arch_map_args<I>(mut args: I) -> anyhow::Result<ArchMapArgs>
where
    I: Iterator<Item = String>,
{
    let mut doc = PathBuf::from("docs/architecture-map.md");
    let mut manifest_path = None;
    let mut metadata_path = None;
    let mut strict = false;
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--doc" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--doc requires a value"))?;
                doc = PathBuf::from(value);
            }
            "--manifest-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--manifest-path requires a value"))?;
                manifest_path = Some(PathBuf::from(value));
            }
            "--metadata-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--metadata-path requires a value"))?;
                metadata_path = Some(PathBuf::from(value));
            }
            "--strict" => strict = true,
            "--json" => json = true,
            "-h" | "--help" => {
                print_command_help("check-architecture-map");
                std::process::exit(0);
            }
            other => {
                return Err(anyhow!(
                    "unknown argument {other:?}\n\nRun `nova-devtools check-architecture-map --help` for usage."
                ));
            }
        }
    }

    Ok(ArchMapArgs {
        doc,
        manifest_path,
        metadata_path,
        strict,
        json,
    })
}

#[derive(Debug)]
struct GraphArgs {
    config: PathBuf,
    manifest_path: Option<PathBuf>,
    metadata_path: Option<PathBuf>,
    output: Option<PathBuf>,
    json: bool,
}

fn parse_graph_deps_args<I>(mut args: I) -> anyhow::Result<GraphArgs>
where
    I: Iterator<Item = String>,
{
    let mut config = PathBuf::from("crate-layers.toml");
    let mut manifest_path = None;
    let mut metadata_path = None;
    let mut output = None;
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a value"))?;
                config = PathBuf::from(value);
            }
            "--manifest-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--manifest-path requires a value"))?;
                manifest_path = Some(PathBuf::from(value));
            }
            "--metadata-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--metadata-path requires a value"))?;
                metadata_path = Some(PathBuf::from(value));
            }
            "--output" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--output requires a value"))?;
                output = Some(PathBuf::from(value));
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print_command_help("graph-deps");
                std::process::exit(0);
            }
            other => {
                return Err(anyhow!(
                    "unknown argument {other:?}\n\nRun `nova-devtools graph-deps --help` for usage."
                ));
            }
        }
    }

    Ok(GraphArgs {
        config,
        manifest_path,
        metadata_path,
        output,
        json,
    })
}

#[derive(Debug)]
struct ProtocolExtensionsArgs {
    doc: PathBuf,
    json: bool,
}

fn parse_check_protocol_extensions_args<I>(mut args: I) -> anyhow::Result<ProtocolExtensionsArgs>
where
    I: Iterator<Item = String>,
{
    let mut doc = PathBuf::from("docs/protocol-extensions.md");
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--doc" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--doc requires a value"))?;
                doc = PathBuf::from(value);
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print_command_help("check-protocol-extensions");
                std::process::exit(0);
            }
            other => {
                return Err(anyhow!(
                    "unknown argument {other:?}\n\nRun `nova-devtools check-protocol-extensions --help` for usage."
                ));
            }
        }
    }

    Ok(ProtocolExtensionsArgs { doc, json })
}

#[derive(Debug)]
struct RepoInvariantsArgs {
    config: PathBuf,
    manifest_path: Option<PathBuf>,
    metadata_path: Option<PathBuf>,
    architecture_map: PathBuf,
    protocol_extensions: PathBuf,
    json: bool,
}

fn parse_check_repo_invariants_args<I>(mut args: I) -> anyhow::Result<RepoInvariantsArgs>
where
    I: Iterator<Item = String>,
{
    let mut config = PathBuf::from("crate-layers.toml");
    let mut manifest_path = None;
    let mut metadata_path = None;
    let mut architecture_map = PathBuf::from("docs/architecture-map.md");
    let mut protocol_extensions = PathBuf::from("docs/protocol-extensions.md");
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a value"))?;
                config = PathBuf::from(value);
            }
            "--manifest-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--manifest-path requires a value"))?;
                manifest_path = Some(PathBuf::from(value));
            }
            "--metadata-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--metadata-path requires a value"))?;
                metadata_path = Some(PathBuf::from(value));
            }
            "--architecture-map" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--architecture-map requires a value"))?;
                architecture_map = PathBuf::from(value);
            }
            "--protocol-extensions" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--protocol-extensions requires a value"))?;
                protocol_extensions = PathBuf::from(value);
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print_command_help("check-repo-invariants");
                std::process::exit(0);
            }
            other => {
                return Err(anyhow!(
                    "unknown argument {other:?}\n\nRun `nova-devtools check-repo-invariants --help` for usage."
                ));
            }
        }
    }

    Ok(RepoInvariantsArgs {
        config,
        manifest_path,
        metadata_path,
        architecture_map,
        protocol_extensions,
        json,
    })
}

fn print_help() {
    println!(
        "\
nova-devtools

USAGE:
  nova-devtools <command> [options]

COMMANDS:
  check-deps             Validate workspace crate dependency edges against ADR 0007 layering rules
  check-layers           Validate crate-layers.toml integrity (workspace coverage, unknown crates, layer refs)
  check-architecture-map Validate docs/architecture-map.md coverage for workspace crates
  check-protocol-extensions Validate docs/protocol-extensions.md coverage for `nova/*` method constants and VS Code client usage
  check-test-layout      Validate integration test layout (warn at 2 root `tests/*.rs`, error at >2)
  check-repo-invariants  Run all nova-devtools repo invariants (deps, layers, architecture-map --strict, protocol-extensions, test-layout)
  graph-deps             Emit a GraphViz/DOT dependency graph annotated by layer (see --help)

OPTIONS:
  -h, --help  Print help

Run `nova-devtools <command> --help` for command-specific options.
"
    );
}

fn print_command_help(cmd: &str) {
    match cmd {
        "check-deps" | "check-layers" => {
            println!(
                "\
USAGE:
  nova-devtools {cmd} [--config <path>] [--manifest-path <path>] [--metadata-path <path>] [--json]

OPTIONS:
  --config <path>         Path to crate-layers.toml (default: crate-layers.toml)
  --manifest-path <path>  Optional workspace Cargo.toml to run `cargo metadata` against
  --metadata-path <path>  Pre-generated `cargo metadata --format-version=1 --no-deps --locked` JSON to read instead of spawning cargo
  --json                  Emit machine-readable JSON output
  -h, --help              Print help
"
            );
        }
        "check-architecture-map" => {
            println!(
                "\
USAGE:
  nova-devtools check-architecture-map [--doc <path>] [--manifest-path <path>] [--metadata-path <path>] [--strict] [--json]

OPTIONS:
  --doc <path>            Path to docs/architecture-map.md (default: docs/architecture-map.md)
  --manifest-path <path>  Optional workspace Cargo.toml to run `cargo metadata` against
  --metadata-path <path>  Pre-generated `cargo metadata --format-version=1 --no-deps --locked` JSON to read instead of spawning cargo
  --strict                Require Purpose / Key entry points / Maturity / Known gaps bullets for each crate section
  --json                  Emit machine-readable JSON output
  -h, --help              Print help
"
            );
        }
        "graph-deps" => {
            println!(
                "\
USAGE:
  nova-devtools graph-deps [--config <path>] [--manifest-path <path>] [--metadata-path <path>] [--output <path>] [--json]

By default, emits DOT to stdout. Use `--output` to write to a file.

OPTIONS:
  --config <path>         Path to crate-layers.toml (default: crate-layers.toml)
  --manifest-path <path>  Optional workspace Cargo.toml to run `cargo metadata` against
  --metadata-path <path>  Pre-generated `cargo metadata --format-version=1 --no-deps --locked` JSON to read instead of spawning cargo
  --output <path>         Write DOT to a file instead of stdout
  --json                  Write DOT to `--output` (or target/nova-deps.dot) and emit a JSON report
  -h, --help              Print help
"
            );
        }
        "check-protocol-extensions" => {
            println!(
                "\
USAGE:
  nova-devtools check-protocol-extensions [--doc <path>] [--json]

OPTIONS:
  --doc <path>   Path to docs/protocol-extensions.md (default: docs/protocol-extensions.md)
  --json         Emit machine-readable JSON output
  -h, --help     Print help
"
            );
        }
        "check-test-layout" => {
            println!(
                "\
USAGE:
  nova-devtools check-test-layout [--manifest-path <path>] [--metadata-path <path>] [--json]

This enforces the AGENTS.md integration test layout rule: each workspace crate may have at most
one `tests/*.rs` file (each file becomes a separate integration-test binary).

OPTIONS:
  --manifest-path <path>  Optional workspace Cargo.toml to run `cargo metadata` against
  --metadata-path <path>  Pre-generated `cargo metadata --format-version=1 --no-deps --locked` JSON to read instead of spawning cargo
  --json                  Emit machine-readable JSON output
  -h, --help              Print help
"
            );
        }
        "check-repo-invariants" => {
            println!(
                "\
USAGE:
  nova-devtools check-repo-invariants [options]

This runs:
  - check-deps
  - check-layers
  - check-architecture-map --strict
  - check-protocol-extensions
  - check-test-layout

OPTIONS:
  --config <path>               Path to crate-layers.toml (default: crate-layers.toml)
  --manifest-path <path>        Optional workspace Cargo.toml to run `cargo metadata` against
  --metadata-path <path>        Pre-generated `cargo metadata --format-version=1 --no-deps --locked` JSON to read instead of spawning cargo
  --architecture-map <path>     Path to docs/architecture-map.md (default: docs/architecture-map.md)
  --protocol-extensions <path>  Path to docs/protocol-extensions.md (default: docs/protocol-extensions.md)
  --json                        Emit machine-readable JSON output
  -h, --help                    Print help
"
            );
        }
        _ => print_help(),
    }
}
