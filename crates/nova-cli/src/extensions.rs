use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[cfg(feature = "wasm-extensions")]
use anyhow::Context;
#[cfg(feature = "wasm-extensions")]
use nova_config::NovaConfig;
#[cfg(feature = "wasm-extensions")]
use serde::Serialize;
#[cfg(feature = "wasm-extensions")]
use std::collections::BTreeSet;
#[cfg(feature = "wasm-extensions")]
use std::path::Path;

#[derive(Args)]
pub(crate) struct ExtensionsArgs {
    #[command(subcommand)]
    pub(crate) command: ExtensionsCommand,
}

#[derive(Subcommand)]
pub(crate) enum ExtensionsCommand {
    /// List configured WASM extension bundles.
    List(ExtensionsListArgs),
    /// Validate (compile/probe) configured WASM extension bundles.
    Validate(ExtensionsValidateArgs),
}

#[derive(Args)]
pub(crate) struct ExtensionsListArgs {
    /// Workspace root (defaults to current directory).
    #[arg(long, default_value = ".")]
    pub(crate) root: PathBuf,
    /// Emit JSON suitable for CI.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct ExtensionsValidateArgs {
    /// Workspace root (defaults to current directory).
    #[arg(long, default_value = ".")]
    pub(crate) root: PathBuf,
}

pub(crate) fn run(args: ExtensionsArgs) -> Result<i32> {
    match args.command {
        ExtensionsCommand::List(args) => list_impl(args),
        ExtensionsCommand::Validate(args) => validate_impl(args),
    }
}

#[cfg(feature = "wasm-extensions")]
struct Discovery {
    workspace_root: PathBuf,
    config_path: Option<PathBuf>,
    search_paths: Vec<PathBuf>,
    loaded: Vec<nova_ext::LoadedExtension>,
    errors: Vec<String>,
}

#[cfg(feature = "wasm-extensions")]
fn load_workspace_config(root: &Path) -> Result<(PathBuf, NovaConfig, Option<PathBuf>)> {
    let ws = nova_workspace::Workspace::open(root)
        .with_context(|| format!("failed to open workspace at {}", root.display()))?;
    let workspace_root = ws.root().to_path_buf();
    let (config, config_path) = nova_config::load_for_workspace(&workspace_root)
        .with_context(|| format!("failed to load config for {}", workspace_root.display()))?;
    Ok((workspace_root, config, config_path))
}

#[cfg(feature = "wasm-extensions")]
fn resolve_wasm_paths(workspace_root: &Path, config: &NovaConfig) -> Vec<PathBuf> {
    config
        .extensions
        .wasm_paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                workspace_root.join(path)
            }
        })
        .collect()
}

#[cfg(feature = "wasm-extensions")]
fn discover(root: &Path) -> Result<Discovery> {
    let (workspace_root, config, config_path) = load_workspace_config(root)?;

    let search_paths = if config.extensions.enabled {
        resolve_wasm_paths(&workspace_root, &config)
    } else {
        Vec::new()
    };

    if search_paths.is_empty() {
        return Ok(Discovery {
            workspace_root,
            config_path,
            search_paths,
            loaded: Vec::new(),
            errors: Vec::new(),
        });
    }

    let (mut loaded, errors) = nova_ext::ExtensionManager::load_all(&search_paths);
    let errors = errors.into_iter().map(|err| err.to_string()).collect();

    let deny: BTreeSet<String> = config.extensions.deny.into_iter().collect();
    let allow: Option<BTreeSet<String>> = config
        .extensions
        .allow
        .map(|allow| allow.into_iter().collect());

    loaded.retain(|ext| {
        let id = ext.id();
        if deny.contains(id) {
            return false;
        }
        if let Some(allow) = allow.as_ref() {
            return allow.contains(id);
        }
        true
    });
    loaded.sort_by(|a, b| a.id().cmp(b.id()));

    Ok(Discovery {
        workspace_root,
        config_path,
        search_paths,
        loaded,
        errors,
    })
}

#[cfg(feature = "wasm-extensions")]
#[derive(Debug, Serialize)]
struct ExtensionsListOutput {
    workspace_root: String,
    config_path: Option<String>,
    search_paths: Vec<String>,
    extensions: Vec<ExtensionRow>,
    errors: Vec<String>,
}

#[cfg(feature = "wasm-extensions")]
#[derive(Debug, Serialize)]
struct ExtensionRow {
    id: String,
    version: String,
    capabilities: Vec<String>,
    dir: String,
}

#[cfg(feature = "wasm-extensions")]
fn list_impl(args: ExtensionsListArgs) -> Result<i32> {
    let discovery = discover(&args.root)?;

    let rows = discovery
        .loaded
        .iter()
        .map(|ext| ExtensionRow {
            id: ext.id().to_string(),
            version: ext.manifest().version.to_string(),
            capabilities: ext
                .manifest()
                .capabilities
                .iter()
                .map(|cap| cap.as_str().to_string())
                .collect(),
            dir: ext.dir().display().to_string(),
        })
        .collect::<Vec<_>>();

    if args.json {
        let output = ExtensionsListOutput {
            workspace_root: discovery.workspace_root.display().to_string(),
            config_path: discovery
                .config_path
                .as_ref()
                .map(|path| path.display().to_string()),
            search_paths: discovery
                .search_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            extensions: rows,
            errors: discovery.errors,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(0);
    }

    let mut table_rows: Vec<[String; 4]> = Vec::new();
    for row in rows {
        table_rows.push([
            row.id,
            row.version,
            if row.capabilities.is_empty() {
                "-".to_string()
            } else {
                row.capabilities.join(",")
            },
            row.dir,
        ]);
    }

    print_table(&table_rows, ["id", "version", "capabilities", "dir"]);

    if !discovery.errors.is_empty() {
        eprintln!();
        eprintln!("errors:");
        for err in discovery.errors {
            eprintln!("  - {err}");
        }
    }

    Ok(0)
}

#[cfg(not(feature = "wasm-extensions"))]
fn list_impl(_args: ExtensionsListArgs) -> Result<i32> {
    eprintln!("nova built without wasm extension support");
    Ok(2)
}

#[cfg(feature = "wasm-extensions")]
fn validate_impl(args: ExtensionsValidateArgs) -> Result<i32> {
    let discovery = discover(&args.root)?;

    if discovery.search_paths.is_empty() {
        // No configured extension paths (or extensions are disabled).
        return Ok(0);
    }

    let mut ok = true;

    for err in &discovery.errors {
        eprintln!("error: {err}");
        ok = false;
    }

    for ext in &discovery.loaded {
        match validate_one(ext) {
            Ok(()) => {
                println!("ok: {}", ext.id());
            }
            Err(err) => {
                eprintln!("error: {}", err);
                ok = false;
            }
        }
    }

    Ok(if ok { 0 } else { 1 })
}

#[cfg(feature = "wasm-extensions")]
fn validate_one(ext: &nova_ext::LoadedExtension) -> Result<()> {
    use nova_ext::wasm::{WasmCapabilities, WasmPlugin, WasmPluginConfig};
    use nova_ext::ExtensionCapability;

    let plugin = WasmPlugin::from_wasm_bytes(ext.id(), ext.entry_bytes(), WasmPluginConfig::default())
        .map_err(|err| {
            anyhow::anyhow!(
                "extension {} at {}: {}",
                ext.id(),
                ext.dir().display(),
                err
            )
        })?;

    let caps = plugin.capabilities();
    for cap in &ext.manifest().capabilities {
        let required = match cap {
            ExtensionCapability::Diagnostics => WasmCapabilities::DIAGNOSTICS,
            ExtensionCapability::Completion => WasmCapabilities::COMPLETIONS,
            ExtensionCapability::CodeAction => WasmCapabilities::CODE_ACTIONS,
            ExtensionCapability::Navigation => WasmCapabilities::NAVIGATION,
            ExtensionCapability::InlayHint => WasmCapabilities::INLAY_HINTS,
        };

        if !caps.contains(required) {
            anyhow::bail!(
                "extension {} at {}: manifest capability '{}' not supported by wasm module",
                ext.id(),
                ext.dir().display(),
                cap.as_str()
            );
        }
    }

    Ok(())
}

#[cfg(not(feature = "wasm-extensions"))]
fn validate_impl(_args: ExtensionsValidateArgs) -> Result<i32> {
    eprintln!("nova built without wasm extension support");
    Ok(2)
}

#[cfg(feature = "wasm-extensions")]
fn print_table<const N: usize>(rows: &[[String; N]], headers: [&str; N]) {
    let mut widths = [0_usize; N];
    for (idx, header) in headers.iter().enumerate() {
        widths[idx] = header.len();
    }
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(cell.len());
        }
    }

    for (idx, header) in headers.iter().enumerate() {
        if idx > 0 {
            print!("  ");
        }
        print!("{header:<width$}", width = widths[idx]);
    }
    println!();

    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            if idx > 0 {
                print!("  ");
            }
            print!("{cell:<width$}", width = widths[idx]);
        }
        println!();
    }
}
