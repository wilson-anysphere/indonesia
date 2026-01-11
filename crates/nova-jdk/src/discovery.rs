use std::path::{Path, PathBuf};
use std::process::Command;

use nova_core::JdkConfig;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdkInstallation {
    root: PathBuf,
    jmods_dir: PathBuf,
}

impl JdkInstallation {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn jmods_dir(&self) -> &Path {
        &self.jmods_dir
    }

    pub fn from_root(root: impl AsRef<Path>) -> Result<Self, JdkDiscoveryError> {
        let root = root.as_ref().to_path_buf();
        let jmods_dir = root.join("jmods");
        if !jmods_dir.is_dir() {
            return Err(JdkDiscoveryError::MissingJmodsDir { root });
        }

        Ok(Self { root, jmods_dir })
    }

    /// Discover a JDK installation.
    ///
    /// When `JdkConfig.home` is set it is used as an explicit override.
    /// Otherwise discovery sources are tried in this order:
    /// 1. `JAVA_HOME`
    /// 2. `java` on `PATH` (via `java -XshowSettings:properties -version`, then symlink resolution)
    pub fn discover(config: Option<&JdkConfig>) -> Result<Self, JdkDiscoveryError> {
        // Optional config override: when present it should win regardless of environment.
        if let Some(override_home) = config.and_then(|c| c.home.as_deref()) {
            let candidate =
                coerce_to_jdk_root(override_home.to_path_buf()).unwrap_or_else(|| override_home.to_path_buf());
            return Self::from_root(candidate);
        }

        // Base discovery order: JAVA_HOME, then `java` on PATH.
        let discovered = discover_from_java_home()
            .and_then(|p| Self::from_root(p).ok())
            .or_else(|| discover_from_java_on_path().and_then(|p| Self::from_root(p).ok()));

        discovered.ok_or(JdkDiscoveryError::NotFound)
    }
}

#[derive(Debug, Error)]
pub enum JdkDiscoveryError {
    #[error("could not discover a JDK installation (tried JAVA_HOME and `java` on PATH)")]
    NotFound,

    #[error("JDK root `{root}` does not contain a `jmods/` directory")]
    MissingJmodsDir { root: PathBuf },
}

fn discover_from_java_home() -> Option<PathBuf> {
    std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .and_then(coerce_to_jdk_root)
}

fn discover_from_java_on_path() -> Option<PathBuf> {
    discover_from_java_command().or_else(discover_from_java_symlink)
}

fn discover_from_java_command() -> Option<PathBuf> {
    let output = Command::new("java")
        .args(["-XshowSettings:properties", "-version"])
        .output()
        .ok()?;

    // HotSpot prints settings to stderr, but we accept both.
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    combined.push_str(&String::from_utf8_lossy(&output.stderr));

    let java_home = combined.lines().find_map(|line| {
        let line = line.trim();
        let (k, v) = line.split_once('=')?;
        if k.trim() == "java.home" {
            Some(v.trim())
        } else {
            None
        }
    })?;

    coerce_to_jdk_root(PathBuf::from(java_home))
}

fn discover_from_java_symlink() -> Option<PathBuf> {
    let java_bin = find_java_on_path()?;
    let java_bin = java_bin.canonicalize().ok()?;
    let root = java_bin.parent()?.parent()?.to_path_buf();
    coerce_to_jdk_root(root)
}

fn find_java_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe_name = if cfg!(windows) { "java.exe" } else { "java" };

    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

fn coerce_to_jdk_root(mut candidate: PathBuf) -> Option<PathBuf> {
    // For older installations `java.home` might point at `$JDK/jre`. On modern
    // JPMS JDKs we need the directory containing `jmods/`.
    if candidate.join("jmods").is_dir() {
        return Some(candidate);
    }

    candidate.pop();
    if candidate.join("jmods").is_dir() {
        return Some(candidate);
    }

    None
}
