use nova_process::RunOptions;
use std::time::Duration;

/// Options controlling `bazel build` execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BazelBuildOptions {
    /// Wall-clock timeout for the build.
    pub timeout: Option<Duration>,
    /// Maximum bytes to capture per stream (stdout and stderr).
    ///
    /// This is clamped to at least 1MiB to ensure we keep enough context for diagnostics.
    pub max_bytes: usize,
}

impl Default for BazelBuildOptions {
    fn default() -> Self {
        Self {
            // Bazel builds can be slow, especially on cold caches or large monorepos.
            timeout: Some(Duration::from_secs(15 * 60)),
            max_bytes: 16 * 1024 * 1024,
        }
    }
}

impl BazelBuildOptions {
    pub(crate) fn to_run_options(self) -> RunOptions {
        const MIN_MAX_BYTES: usize = 1_048_576; // 1 MiB

        RunOptions {
            timeout: self.timeout,
            max_bytes: self.max_bytes.max(MIN_MAX_BYTES),
            ..RunOptions::default()
        }
    }
}

pub(crate) fn bazel_build_args<T: AsRef<str>, A: AsRef<str>>(
    targets: &[T],
    extra_args: &[A],
) -> Vec<String> {
    let has_color = extra_args
        .iter()
        .any(|arg| bazel_flag_is_present(arg.as_ref(), "--color"));
    let has_curses = extra_args
        .iter()
        .any(|arg| bazel_flag_is_present(arg.as_ref(), "--curses"));
    let has_progress = extra_args.iter().any(|arg| {
        let arg = arg.as_ref();
        arg == "--noshow_progress"
            || arg == "--show_progress"
            || arg.starts_with("--show_progress")
            || arg.starts_with("--noshow_progress")
    });

    let mut args = Vec::with_capacity(1 + 3 + extra_args.len() + targets.len());
    args.push("build".to_string());

    // Prefer deterministic output by default unless the caller explicitly asked for otherwise.
    if !has_color {
        args.push("--color=no".to_string());
    }
    if !has_curses {
        args.push("--curses=no".to_string());
    }
    if !has_progress {
        args.push("--noshow_progress".to_string());
    }

    args.extend(extra_args.iter().map(|arg| arg.as_ref().to_string()));
    args.extend(targets.iter().map(|target| target.as_ref().to_string()));
    args
}

fn bazel_flag_is_present(arg: &str, flag: &str) -> bool {
    if arg == flag {
        return true;
    }

    // Bazel flags are usually `--flag=value`.
    arg.strip_prefix(flag)
        .is_some_and(|rest| rest.starts_with('='))
}
