#[cfg(target_os = "linux")]
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Parsed cgroup paths from `/proc/self/cgroup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSelfCgroup {
    /// cgroup v2 unified hierarchy entry (e.g. `0::/some/path`).
    pub v2_path: Option<String>,
    /// cgroup v1 memory controller entry (e.g. `5:memory:/some/path`).
    pub v1_memory_path: Option<String>,
}

/// Parse `/proc/self/cgroup` contents and extract relevant cgroup paths.
///
/// This is a pure helper intended for unit testing; it does not touch the
/// filesystem.
pub fn parse_proc_self_cgroup(contents: &str) -> ProcSelfCgroup {
    let mut v2_path = None;
    let mut v1_memory_path = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.splitn(3, ':');
        let hierarchy_id = parts.next();
        let controllers = parts.next();
        let path = parts.next();

        let (Some(hierarchy_id), Some(controllers), Some(path)) = (hierarchy_id, controllers, path)
        else {
            continue;
        };

        // cgroup v2 unified hierarchy is represented by `0::/path`.
        let path = path.trim();

        if v2_path.is_none() && hierarchy_id == "0" && controllers.is_empty() && !path.is_empty() {
            v2_path = Some(path.to_string());
        }

        // cgroup v1 exposes controller names in the middle field.
        if v1_memory_path.is_none()
            && controllers
                .split(',')
                .any(|controller| controller.trim() == "memory")
        {
            v1_memory_path = Some(path.to_string());
        }
    }

    ProcSelfCgroup {
        v2_path,
        v1_memory_path,
    }
}

const UNLIMITED_THRESHOLD_BYTES: u64 = 1 << 60; // 1 EiB; above this is treated as "unlimited".

/// Parse a cgroup memory limit value.
///
/// - cgroup v2: `memory.max` is either `max` or a byte count.
/// - cgroup v1: `memory.limit_in_bytes` is a byte count, with very large values
///   commonly used to represent "no limit".
///
/// Returns `None` for unlimited/unknown values.
pub fn parse_cgroup_memory_limit_bytes(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "max" {
        return None;
    }

    let value = match raw.parse::<u64>() {
        Ok(value) => value,
        Err(err) => {
            // Best-effort: cgroup file formats should be stable, but container runtimes and
            // sandbox environments can expose unexpected values.
            static REPORTED_CGROUP_LIMIT_PARSE_ERROR: OnceLock<()> = OnceLock::new();
            if REPORTED_CGROUP_LIMIT_PARSE_ERROR.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.memory",
                    raw,
                    error = %err,
                    "failed to parse cgroup memory limit value"
                );
            }
            return None;
        }
    };
    if value >= UNLIMITED_THRESHOLD_BYTES {
        return None;
    }

    Some(value)
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text.trim().to_string()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            // Best-effort: some environments restrict cgroup files; avoid log spam by reporting
            // only the first unexpected read failure.
            static REPORTED_CGROUP_READ_ERROR: OnceLock<()> = OnceLock::new();
            if REPORTED_CGROUP_READ_ERROR.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.memory",
                    path = %path.display(),
                    error = %err,
                    "failed to read cgroup file"
                );
            }
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn relative_cgroup_path(path: &str) -> PathBuf {
    PathBuf::from(path.trim_start_matches('/'))
}

#[cfg(target_os = "linux")]
fn effective_limit_from_ancestors(
    mount: &Path,
    cgroup_path: &str,
    limit_filename: &str,
) -> Option<u64> {
    let mut rel = relative_cgroup_path(cgroup_path);
    let mut best: Option<u64> = None;

    loop {
        let candidate = mount.join(&rel).join(limit_filename);
        if let Some(raw) = read_trimmed(&candidate) {
            if let Some(limit) = parse_cgroup_memory_limit_bytes(&raw) {
                best = Some(best.map_or(limit, |best| best.min(limit)));
            }
        }

        if !rel.pop() {
            break;
        }
    }

    best
}

#[cfg(target_os = "linux")]
fn cgroup_v2_memory_limit_bytes(cgroup_path: &str) -> Option<u64> {
    effective_limit_from_ancestors(Path::new("/sys/fs/cgroup"), cgroup_path, "memory.max")
}

#[cfg(target_os = "linux")]
fn cgroup_v1_memory_limit_bytes(cgroup_path: &str) -> Option<u64> {
    effective_limit_from_ancestors(
        Path::new("/sys/fs/cgroup/memory"),
        cgroup_path,
        "memory.limit_in_bytes",
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn cgroup_memory_limit_bytes() -> Option<u64> {
    let proc_contents = match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(contents) => contents,
        Err(err) => {
            // `/proc` may not be available in some sandboxed environments; treat it as best-effort.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.memory",
                    error = %err,
                    "failed to read /proc/self/cgroup while probing cgroup memory limit"
                );
            }
            return None;
        }
    };
    let parsed = parse_proc_self_cgroup(&proc_contents);

    if let Some(path) = &parsed.v2_path {
        if let Some(limit) = cgroup_v2_memory_limit_bytes(path) {
            return Some(limit);
        }
    }

    if let Some(path) = &parsed.v1_memory_path {
        if let Some(limit) = cgroup_v1_memory_limit_bytes(path) {
            return Some(limit);
        }
    }

    None
}

/// Best-effort: sample current cgroup memory usage in bytes.
///
/// This reads:
/// - cgroup v2: `memory.current`
/// - cgroup v1: `memory.usage_in_bytes`
pub fn cgroup_memory_current_bytes() -> Option<u64> {
    cgroup_memory_current_bytes_impl()
}

#[cfg(target_os = "linux")]
fn cgroup_memory_current_bytes_impl() -> Option<u64> {
    let proc_contents = match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(contents) => contents,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.memory",
                    error = %err,
                    "failed to read /proc/self/cgroup while sampling cgroup memory usage"
                );
            }
            return None;
        }
    };
    let parsed = parse_proc_self_cgroup(&proc_contents);
    static REPORTED_CGROUP_USAGE_PARSE_ERROR: OnceLock<()> = OnceLock::new();

    if let Some(path) = &parsed.v2_path {
        let rel = relative_cgroup_path(path);
        let usage_path = Path::new("/sys/fs/cgroup").join(rel).join("memory.current");
        if let Some(raw) = read_trimmed(&usage_path) {
            match raw.parse::<u64>() {
                Ok(value) => return Some(value),
                Err(err) => {
                    if REPORTED_CGROUP_USAGE_PARSE_ERROR.set(()).is_ok() {
                        tracing::debug!(
                            target = "nova.memory",
                            path = %usage_path.display(),
                            raw,
                            error = %err,
                            "failed to parse cgroup memory usage value"
                        );
                    }
                }
            }
        }
    }

    if let Some(path) = &parsed.v1_memory_path {
        let rel = relative_cgroup_path(path);
        let usage_path = Path::new("/sys/fs/cgroup/memory")
            .join(rel)
            .join("memory.usage_in_bytes");
        if let Some(raw) = read_trimmed(&usage_path) {
            match raw.parse::<u64>() {
                Ok(value) => return Some(value),
                Err(err) => {
                    if REPORTED_CGROUP_USAGE_PARSE_ERROR.set(()).is_ok() {
                        tracing::debug!(
                            target = "nova.memory",
                            path = %usage_path.display(),
                            raw,
                            error = %err,
                            "failed to parse cgroup memory usage value"
                        );
                    }
                }
            }
        }
    }

    None
}

#[cfg(not(target_os = "linux"))]
fn cgroup_memory_current_bytes_impl() -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cgroup_memory_limit_bytes() -> Option<u64> {
    None
}
