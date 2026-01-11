use std::path::PathBuf;

use anyhow::{anyhow, Context as _};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        print_help();
        return Ok(());
    };

    match cmd.as_str() {
        "check-deps" => {
            let (config, manifest_path, metadata_path) = parse_check_deps_args(args)?;
            nova_devtools::check_deps::run(
                &config,
                manifest_path.as_deref(),
                metadata_path.as_deref(),
            )
            .with_context(|| format!("check-deps failed using config {}", config.display()))
        }
        "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => Err(anyhow!(
            "unknown command {other:?}\n\nRun `nova-devtools --help` for usage."
        )),
    }
}

fn parse_check_deps_args<I>(
    mut args: I,
) -> anyhow::Result<(PathBuf, Option<PathBuf>, Option<PathBuf>)>
where
    I: Iterator<Item = String>,
{
    let mut config = PathBuf::from("crate-layers.toml");
    let mut manifest_path = None;
    let mut metadata_path = None;

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
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                return Err(anyhow!(
                    "unknown argument {other:?}\n\nRun `nova-devtools check-deps --help` for usage."
                ));
            }
        }
    }

    Ok((config, manifest_path, metadata_path))
}

fn print_help() {
    println!(
        "\
nova-devtools

USAGE:
  nova-devtools check-deps [--config <path>] [--manifest-path <path>] [--metadata-path <path>]

COMMANDS:
  check-deps    Validate workspace crate dependencies against ADR 0007 layering rules

OPTIONS:
  --config <path>         Path to crate-layers.toml (default: crate-layers.toml)
  --manifest-path <path>  Optional workspace Cargo.toml to run `cargo metadata` against
  --metadata-path <path>  Pre-generated `cargo metadata --format-version=1 --no-deps` JSON to read instead of spawning cargo
  -h, --help              Print help
"
    );
}
