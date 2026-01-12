use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use nova_build::{BuildManager, DefaultCommandRunner};
use nova_jdwp::wire::JdwpClient;
use nova_scheduler::CancellationToken;
use tempfile::TempDir;
use tokio::process::Command;

use crate::hot_swap::{CompileError, CompiledClass};

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
/// available (no `--release` / `-source` / `-target` and preview is disabled).
pub(crate) fn apply_stream_eval_defaults(base: &HotSwapJavacConfig) -> HotSwapJavacConfig {
    let mut config = base.clone();
    let has_explicit_language_level =
        config.release.is_some() || config.source.is_some() || config.target.is_some();
    if !has_explicit_language_level && !config.enable_preview {
        config.release = Some("8".to_string());
    }
    config
}

pub(crate) fn javac_error_is_release_flag_unsupported(err: &CompileError) -> bool {
    let msg = err.to_string().to_lowercase();
    // JDK 8's `javac` does not support `--release`. Error messages vary by distribution:
    // - "error: invalid flag: --release"
    // - "javac: invalid flag: --release"
    // - "Unrecognized option: --release"
    msg.contains("invalid flag: --release") || msg.contains("unrecognized option: --release")
}

fn normalize_legacy_javac_source_target(value: &str) -> Cow<'_, str> {
    // `javac` 8 expects "1.8" rather than "8" for `-source/-target`, while newer toolchains accept
    // both. Normalize "8" (and lower) to the legacy "1.x" form so we can safely retry when
    // compiling with a JDK 8 toolchain.
    let trimmed = value.trim();
    if trimmed.starts_with("1.") {
        return Cow::Borrowed(trimmed);
    }
    let Ok(version) = trimmed.parse::<u32>() else {
        return Cow::Borrowed(trimmed);
    };
    if version <= 8 {
        Cow::Owned(format!("1.{version}"))
    } else {
        Cow::Borrowed(trimmed)
    }
}

fn normalize_javac_release(value: &str) -> Cow<'_, str> {
    // `javac --release` expects a feature release number (8, 11, 17, ...). Some build tools may
    // still report legacy `1.x` values (e.g. `1.8`). Normalize those to the expected form.
    let trimmed = value.trim();
    let Some(rest) = trimmed.strip_prefix("1.") else {
        return Cow::Borrowed(trimmed);
    };

    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    let Ok(version) = digits.parse::<u32>() else {
        return Cow::Borrowed(trimmed);
    };

    Cow::Owned(version.to_string())
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

    let temp_dir = hot_swap_temp_dir().map_err(|err| {
        CompileError::new(format!("failed to create hot swap output directory: {err}"))
    })?;

    let result = match compile_java_to_dir(cancel, javac, source_file, temp_dir.path()).await {
        Ok(classes) => Ok(classes),
        Err(err) => {
            // JDK 8's `javac` doesn't support `--release`. If the build config specifies a
            // release level and the configured toolchain is JDK 8, retry with `-source/-target`.
            let should_retry_without_release =
                javac.release.is_some() && javac_error_is_release_flag_unsupported(&err);
            if !should_retry_without_release {
                Err(err)
            } else {
                let mut fallback = javac.clone();
                if let Some(release) = fallback.release.take() {
                    if fallback.source.is_none() {
                        fallback.source = Some(release.clone());
                    }
                    if fallback.target.is_none() {
                        fallback.target = Some(release);
                    }
                }

                // Ensure a clean output directory so we don't accidentally pick up stale classes.
                let _ = std::fs::remove_dir_all(temp_dir.path());
                if let Err(err) = std::fs::create_dir_all(temp_dir.path()) {
                    Err(CompileError::new(format!(
                        "failed to recreate hot swap output directory {}: {err}",
                        temp_dir.path().display()
                    )))
                } else {
                    match compile_java_to_dir(cancel, &fallback, source_file, temp_dir.path()).await
                    {
                        Ok(classes) => Ok(classes),
                        Err(err2) => Err(CompileError::new(format!(
                            "{err}\n\nretry without `--release` failed:\n{err2}"
                        ))),
                    }
                }
            }
        }
    };

    if keep_hot_swap_temp_dir() {
        let path = temp_dir.keep();
        tracing::info!(
            path = %path.display(),
            "{} is set; keeping hot-swap compilation temp directory",
            KEEP_HOT_SWAP_TEMP_ENV,
        );
    }

    result
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
        let normalized = normalize_javac_release(release);
        cmd.arg(normalized.as_ref());
    } else {
        if let Some(source) = javac.source.as_deref() {
            cmd.arg("-source");
            let normalized = normalize_legacy_javac_source_target(source);
            cmd.arg(normalized.as_ref());
        }
        if let Some(target) = javac.target.as_deref() {
            cmd.arg("-target");
            let normalized = normalize_legacy_javac_source_target(target);
            cmd.arg(normalized.as_ref());
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

    #[test]
    fn javac_error_is_release_flag_unsupported_matches_common_messages() {
        for msg in [
            "error: invalid flag: --release",
            "javac: invalid flag: --release",
            "Unrecognized option: --release",
        ] {
            let err = CompileError::new(msg);
            assert!(javac_error_is_release_flag_unsupported(&err), "msg={msg:?}");
        }
    }

    #[test]
    fn javac_error_is_release_flag_unsupported_does_not_match_other_errors() {
        let err = CompileError::new("some other javac error");
        assert!(!javac_error_is_release_flag_unsupported(&err));
    }

    #[test]
    fn normalize_legacy_javac_source_target_converts_numeric_8_to_1_8() {
        assert_eq!(
            normalize_legacy_javac_source_target("8").as_ref(),
            "1.8",
            "expected legacy `javac`-compatible version string"
        );
    }

    #[test]
    fn normalize_legacy_javac_source_target_keeps_existing_legacy_form() {
        assert_eq!(normalize_legacy_javac_source_target("1.8").as_ref(), "1.8");
    }

    #[test]
    fn normalize_legacy_javac_source_target_does_not_rewrite_modern_versions() {
        assert_eq!(normalize_legacy_javac_source_target("17").as_ref(), "17");
    }

    #[test]
    fn normalize_javac_release_converts_legacy_1_8_to_8() {
        assert_eq!(normalize_javac_release("1.8").as_ref(), "8");
    }

    #[test]
    fn normalize_javac_release_preserves_modern_versions() {
        assert_eq!(normalize_javac_release("21").as_ref(), "21");
    }
}

const KEEP_HOT_SWAP_TEMP_ENV: &str = "NOVA_DAP_KEEP_HOT_SWAP_TEMP";

fn keep_hot_swap_temp_dir() -> bool {
    let Some(value) = std::env::var_os(KEEP_HOT_SWAP_TEMP_ENV) else {
        return false;
    };
    let value = value.to_string_lossy();
    !value.is_empty() && value != "0"
}

pub(crate) fn hot_swap_temp_dir() -> std::io::Result<TempDir> {
    // Use a per-process base directory to avoid cross-process interference in tests
    // and when multiple Nova instances are running concurrently on the same machine.
    let base = std::env::temp_dir().join(format!("nova-dap-hot-swap-{}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    tempfile::Builder::new().prefix("compile-").tempdir_in(base)
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
