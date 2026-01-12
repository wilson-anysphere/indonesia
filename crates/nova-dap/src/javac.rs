use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use nova_build::{BuildManager, DefaultCommandRunner};
use nova_jdwp::wire::JdwpClient;
use nova_scheduler::CancellationToken;
use tokio::process::Command;

use crate::hot_swap::{CompileError, CompiledClass};

static HOT_SWAP_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default)]
pub(crate) struct HotSwapJavacConfig {
    pub(crate) javac: String,
    pub(crate) classpath: Option<std::ffi::OsString>,
    pub(crate) module_path: Option<std::ffi::OsString>,
    pub(crate) release: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) enable_preview: bool,
}

pub(crate) async fn resolve_hot_swap_javac_config(
    cancel: &CancellationToken,
    jdwp: &JdwpClient,
    project_root: Option<&Path>,
) -> HotSwapJavacConfig {
    let mut config = HotSwapJavacConfig {
        javac: "javac".to_string(),
        ..HotSwapJavacConfig::default()
    };

    if let Some(project_root) = project_root {
        if let Some(build_cfg) = resolve_build_java_compile_config(cancel, project_root).await {
            config.release = build_cfg.release.clone();
            config.source = build_cfg.source.clone();
            config.target = build_cfg.target.clone();
            config.enable_preview = build_cfg.enable_preview;

            if !build_cfg.compile_classpath.is_empty() {
                config.classpath = std::env::join_paths(build_cfg.compile_classpath.iter()).ok();
            }
            if !build_cfg.module_path.is_empty() {
                config.module_path = std::env::join_paths(build_cfg.module_path.iter()).ok();
            }
        }
    }

    if config.classpath.is_none() {
        config.classpath = resolve_vm_classpath(cancel, jdwp).await;
    }

    config
}

/// Apply stream-eval-specific defaults to a base `javac` configuration.
///
/// Stream debug compiles and injects a helper class into the debuggee JVM. When we're attaching
/// without a resolved build configuration (no `projectRoot` or no build tool metadata), `javac`
/// defaults to the host toolchain language level and can emit classfiles that the debuggee JVM
/// cannot load (e.g. compiling with JDK 21 against a JDK 8 debuggee).
///
/// Streams require Java 8+, so we default to `--release 8` only when no explicit language level is
/// available (no `--release` / `--source` / `--target` and preview is disabled).
pub(crate) fn apply_stream_eval_defaults(base: &HotSwapJavacConfig) -> HotSwapJavacConfig {
    let mut config = base.clone();
    let has_explicit_language_level =
        config.release.is_some() || config.source.is_some() || config.target.is_some();
    if !has_explicit_language_level && !config.enable_preview {
        config.release = Some("8".to_string());
    }
    config
}

pub(crate) async fn resolve_vm_classpath(
    cancel: &CancellationToken,
    jdwp: &JdwpClient,
) -> Option<std::ffi::OsString> {
    let paths = tokio::select! {
        _ = cancel.cancelled() => return None,
        res = tokio::time::timeout(Duration::from_secs(2), jdwp.virtual_machine_class_paths()) => match res {
            Ok(Ok(paths)) => paths,
            _ => return None,
        }
    };

    let base_dir = PathBuf::from(paths.base_dir);
    let entries: Vec<PathBuf> = paths
        .classpaths
        .into_iter()
        .map(PathBuf::from)
        .map(|entry| {
            if entry.is_absolute() {
                entry
            } else {
                base_dir.join(entry)
            }
        })
        .collect();
    std::env::join_paths(entries.iter()).ok()
}

async fn resolve_build_java_compile_config(
    cancel: &CancellationToken,
    project_root: &Path,
) -> Option<nova_build::JavaCompileConfig> {
    let project_root = project_root.to_path_buf();
    if !project_root.join("pom.xml").is_file()
        && !project_root.join("build.gradle").is_file()
        && !project_root.join("build.gradle.kts").is_file()
    {
        return None;
    }

    let cache_dir = std::env::temp_dir().join("nova-build-cache");
    let build_cancel = CancellationToken::new();
    let build_cancel_runner = build_cancel.clone();

    let mut handle = tokio::task::spawn_blocking(move || {
        let runner = std::sync::Arc::new(DefaultCommandRunner {
            timeout: Some(Duration::from_secs(10)),
            cancellation: Some(build_cancel_runner),
        });
        let manager = BuildManager::with_runner(cache_dir, runner);
        if project_root.join("pom.xml").is_file() {
            manager.java_compile_config_maven(&project_root, None)
        } else {
            manager.java_compile_config_gradle(&project_root, None)
        }
    });

    let cfg = tokio::select! {
        _ = cancel.cancelled() => {
            build_cancel.cancel();
            handle.abort();
            return None;
        }
        _ = tokio::time::sleep(Duration::from_secs(10)) => {
            build_cancel.cancel();
            handle.abort();
            return None;
        }
        res = &mut handle => match res {
            Ok(Ok(cfg)) => cfg,
            _ => return None,
        }
    };

    Some(cfg)
}

pub(crate) async fn compile_java_for_hot_swap(
    cancel: &CancellationToken,
    javac: &HotSwapJavacConfig,
    source_file: &Path,
) -> std::result::Result<Vec<CompiledClass>, CompileError> {
    if !source_file.is_file() {
        return Err(CompileError::new(format!(
            "file does not exist: {}",
            source_file.display()
        )));
    }

    let output_dir = match hot_swap_temp_dir() {
        Ok(dir) => dir,
        Err(err) => {
            return Err(CompileError::new(format!(
                "failed to create hot swap output directory: {err}"
            )))
        }
    };

    let compile_result = compile_java_to_dir(cancel, javac, source_file, &output_dir).await;
    let compiled = match compile_result {
        Ok(classes) => Ok(classes),
        Err(err) => Err(err),
    };

    let _ = std::fs::remove_dir_all(&output_dir);
    compiled
}

pub(crate) async fn compile_java_to_dir(
    cancel: &CancellationToken,
    javac: &HotSwapJavacConfig,
    source_file: &Path,
    output_dir: &Path,
) -> std::result::Result<Vec<CompiledClass>, CompileError> {
    let mut cmd = Command::new(&javac.javac);
    // Keep memory usage low so hot-swap compilation remains reliable under the
    // `cargo_agent` RLIMIT_AS cap used in CI.
    cmd.arg("-J-Xms16m");
    cmd.arg("-J-Xmx256m");
    cmd.arg("-J-XX:CompressedClassSpaceSize=64m");
    cmd.arg("-g");
    cmd.arg("-encoding");
    cmd.arg("UTF-8");
    cmd.arg("-d");
    cmd.arg(output_dir);
    if let Some(classpath) = javac.classpath.as_ref() {
        cmd.arg("-classpath");
        cmd.arg(classpath);
    }
    if let Some(module_path) = javac.module_path.as_ref() {
        cmd.arg("--module-path");
        cmd.arg(module_path);
    }
    if let Some(release) = javac.release.as_deref() {
        cmd.arg("--release");
        cmd.arg(release);
    } else {
        if let Some(source) = javac.source.as_deref() {
            cmd.arg("--source");
            cmd.arg(source);
        }
        if let Some(target) = javac.target.as_deref() {
            cmd.arg("--target");
            cmd.arg(target);
        }
    }
    if javac.enable_preview {
        cmd.arg("--enable-preview");
    }
    cmd.arg(source_file);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|err| {
        CompileError::new(format!("failed to spawn {}: {err}", javac.javac.as_str()))
    })?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| CompileError::new("javac stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| CompileError::new("javac stderr unavailable"))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
        buf
    });

    let timeout = Duration::from_secs(30);
    let status = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(CompileError::new("cancelled"));
        }
        res = tokio::time::timeout(timeout, child.wait()) => match res {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => {
                stdout_task.abort();
                stderr_task.abort();
                return Err(CompileError::new(format!("javac failed: {err}")));
            }
            Err(_elapsed) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(CompileError::new(format!("javac timed out after {timeout:?}")));
            }
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() {
        let output = format_javac_failure(&stdout, &stderr);
        return Err(CompileError::new(output));
    }

    let class_files = match collect_class_files(output_dir) {
        Ok(files) => files,
        Err(err) => {
            return Err(CompileError::new(format!(
                "failed to read compiled classes: {err}"
            )))
        }
    };

    let mut classes = Vec::new();
    for class_file in class_files {
        let Some(class_name) = class_name_from_class_file(output_dir, &class_file) else {
            continue;
        };
        match std::fs::read(&class_file) {
            Ok(bytecode) => classes.push(CompiledClass {
                class_name,
                bytecode,
            }),
            Err(err) => {
                return Err(CompileError::new(format!(
                    "failed to read class file {}: {err}",
                    class_file.display()
                )))
            }
        }
    }

    if classes.is_empty() {
        return Err(CompileError::new("javac produced no class files"));
    }

    Ok(classes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_stream_eval_defaults_sets_release_8_when_no_build_level() {
        let base = HotSwapJavacConfig {
            javac: "javac".to_string(),
            ..HotSwapJavacConfig::default()
        };
        let cfg = apply_stream_eval_defaults(&base);
        assert_eq!(cfg.release.as_deref(), Some("8"));
    }

    #[test]
    fn apply_stream_eval_defaults_does_not_override_explicit_release() {
        let base = HotSwapJavacConfig {
            javac: "javac".to_string(),
            release: Some("21".to_string()),
            ..HotSwapJavacConfig::default()
        };
        let cfg = apply_stream_eval_defaults(&base);
        assert_eq!(cfg.release.as_deref(), Some("21"));
    }
}

pub(crate) fn hot_swap_temp_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir().join("nova-dap-hot-swap");
    std::fs::create_dir_all(&base)?;
    let id = HOT_SWAP_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("compile-{id}-{}", std::process::id()));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

fn collect_class_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_class_files_inner(dir, &mut out)?;
    Ok(out)
}

fn collect_class_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_class_files_inner(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("class") {
            out.push(path);
        }
    }
    Ok(())
}

fn class_name_from_class_file(output_dir: &Path, class_file: &Path) -> Option<String> {
    let rel = class_file.strip_prefix(output_dir).ok()?;
    let mut components: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(os) => Some(os.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    let last = components.pop()?;
    let last = last.strip_suffix(".class").unwrap_or(&last).to_string();
    components.push(last);
    Some(components.join("."))
}

pub(crate) fn format_javac_failure(stdout: &[u8], stderr: &[u8]) -> String {
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&String::from_utf8_lossy(stdout));
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(stderr));
    }
    let combined = combined.trim().to_string();

    let diagnostics = nova_build::parse_javac_diagnostics(&combined, "javac");
    let message = if diagnostics.is_empty() {
        combined
    } else {
        diagnostics
            .into_iter()
            .map(|diag| {
                let line = diag.range.start.line + 1;
                let col = diag.range.start.character + 1;
                format!("{}:{line}:{col}: {}", diag.file.display(), diag.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    truncate_message(message, 8 * 1024)
}

fn truncate_message(mut message: String, max_len: usize) -> String {
    if message.len() <= max_len {
        return message;
    }

    message.truncate(max_len);
    message.push_str("\n<output truncated>");
    message
}
