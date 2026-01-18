use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use nova_core::JdkConfig;
use nova_process::{run_command, RunOptions};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdkInstallation {
    root: PathBuf,
    jmods_dir: Option<PathBuf>,
    java_home: PathBuf,
}

impl JdkInstallation {
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the JPMS `jmods/` directory if present (JDK 9+ layout).
    pub fn jmods_dir(&self) -> Option<&Path> {
        self.jmods_dir.as_deref()
    }

    /// Returns the Java runtime home directory (i.e. `java.home`).
    ///
    /// - JPMS JDKs (9+): `<root>`
    /// - Legacy JDK 8: `<root>/jre`
    /// - Legacy JRE 8: `<root>`
    pub fn java_home(&self) -> &Path {
        &self.java_home
    }

    /// Best-effort path to the `java` launcher for this installation.
    ///
    /// For legacy JDK8 installations, prefer `$JDK/bin/java` when present and
    /// fall back to `$JAVA_HOME/bin/java` (where `$JAVA_HOME` might be `$JDK/jre`).
    pub fn java_bin(&self) -> PathBuf {
        let exe_name = if cfg!(windows) { "java.exe" } else { "java" };
        let root_java = self.root.join("bin").join(exe_name);
        if root_java.is_file() {
            return root_java;
        }

        self.java_home.join("bin").join(exe_name)
    }

    /// Returns the path to the JDK `src.zip` if it exists.
    ///
    /// This checks common JDK layouts:
    /// - `$JDK/src.zip` (e.g. some Linux distributions)
    /// - `$JDK/lib/src.zip` (e.g. macOS / SDKMAN)
    pub fn src_zip(&self) -> Option<PathBuf> {
        src_zip_from_root(&self.root)
    }

    /// Best-effort Java feature/spec release for this installation (e.g. `8`, `17`).
    ///
    /// For JPMS JDKs (9+) this is read from the `$JDK/release` properties-style file.
    ///
    /// The parser prefers `JAVA_SPEC_VERSION` and falls back to `JAVA_VERSION`.
    /// It supports both legacy (`"1.8"`) and modern (`"17"`, `"17.0.10"`) formats.
    pub fn spec_release(&self) -> Option<u16> {
        spec_release_from_root(&self.root)
    }

    pub fn from_root(root: impl AsRef<Path>) -> Result<Self, JdkDiscoveryError> {
        let root = root.as_ref().to_path_buf();
        let jmods_dir = root.join("jmods");
        if jmods_dir.is_dir() {
            return Ok(Self {
                root: root.clone(),
                jmods_dir: Some(jmods_dir),
                java_home: root,
            });
        }

        // Legacy JDK 8 layout: `$JDK/jre/lib/rt.jar`.
        let java_home = root.join("jre");
        if java_home.join("lib").join("rt.jar").is_file() {
            return Ok(Self {
                root,
                jmods_dir: None,
                java_home,
            });
        }

        // Legacy JRE 8 layout: `$JRE/lib/rt.jar`.
        if root.join("lib").join("rt.jar").is_file() {
            return Ok(Self {
                root: root.clone(),
                jmods_dir: None,
                java_home: root,
            });
        }

        Err(JdkDiscoveryError::InvalidJdkRoot { root })
    }

    /// Discover a JDK installation.
    ///
    /// When `JdkConfig.home` is set it is used as an explicit override.
    /// When `JdkConfig.release` is set and a matching `JdkConfig.toolchains` entry exists, the
    /// toolchain home is preferred over `home`.
    /// Otherwise discovery sources are tried in this order:
    /// 1. `JAVA_HOME`
    /// 2. `java` on `PATH` (via `java -XshowSettings:properties -version`, then symlink resolution)
    pub fn discover(config: Option<&JdkConfig>) -> Result<Self, JdkDiscoveryError> {
        Self::discover_for_release(config, None)
    }

    /// Like [`Self::discover`] but allows callers to request a specific Java feature release.
    ///
    /// When `requested_release` is `Some`, a matching `config.toolchains` entry is preferred over
    /// `config.home`. When it is `None`, Nova falls back to `config.release`.
    pub fn discover_for_release(
        config: Option<&JdkConfig>,
        requested_release: Option<u16>,
    ) -> Result<Self, JdkDiscoveryError> {
        // Optional config override: when present it should win regardless of environment.
        if let Some(override_home) = config.and_then(|c| c.preferred_home(requested_release)) {
            let candidate = coerce_to_jdk_root(override_home.to_path_buf())
                .unwrap_or_else(|| override_home.to_path_buf());
            return Self::from_root(candidate);
        }

        // Base discovery order: JAVA_HOME, then `java` on PATH.
        let discovered = discover_from_java_home()
            .and_then(|p| Self::from_root(p).ok())
            .or_else(|| discover_from_java_on_path().and_then(|p| Self::from_root(p).ok()));

        discovered.ok_or(JdkDiscoveryError::NotFound)
    }
}

pub(crate) fn src_zip_from_root(root: &Path) -> Option<PathBuf> {
    let root_src = root.join("src.zip");
    if root_src.is_file() {
        return Some(root_src);
    }

    let lib_src = root.join("lib").join("src.zip");
    if lib_src.is_file() {
        return Some(lib_src);
    }

    None
}

pub(crate) fn spec_release_from_root(root: &Path) -> Option<u16> {
    let release_path = root.join("release");
    let contents = match std::fs::read_to_string(&release_path) {
        Ok(contents) => contents,
        Err(err) => {
            // Candidate roots may not be real JDKs; only log unexpected filesystem errors.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.jdk",
                    release_path = %release_path.display(),
                    error = %err,
                    "failed to read JDK release file"
                );
            }
            return None;
        }
    };

    let spec = parse_release_property(&contents, "JAVA_SPEC_VERSION")
        .and_then(|v| parse_java_release(&v))
        .or_else(|| {
            parse_release_property(&contents, "JAVA_VERSION").and_then(|v| parse_java_release(&v))
        })?;

    Some(spec)
}

fn parse_release_property(contents: &str, key: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (k, v) = line.split_once('=')?;
        if k.trim() != key {
            continue;
        }

        let mut v = v.trim();
        if let Some(stripped) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            v = stripped;
        }
        if let Some(stripped) = v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            v = stripped;
        }

        return Some(v.to_owned());
    }

    None
}

fn parse_java_release(raw: &str) -> Option<u16> {
    static PARSE_JAVA_RELEASE_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    let raw = raw.trim();
    let raw = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);

    // Legacy `JAVA_VERSION` / `JAVA_SPEC_VERSION` values use the "1.x" format.
    if let Some(rest) = raw.strip_prefix("1.") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        return match digits.parse::<u16>() {
            Ok(v) => Some(v),
            Err(err) => {
                if PARSE_JAVA_RELEASE_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.jdk",
                        raw = %raw,
                        digits = %digits,
                        error = %err,
                        "failed to parse legacy java release"
                    );
                }
                None
            }
        };
    }

    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.parse::<u16>() {
        Ok(v) => Some(v),
        Err(err) => {
            if PARSE_JAVA_RELEASE_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.jdk",
                    raw = %raw,
                    digits = %digits,
                    error = %err,
                    "failed to parse java release"
                );
            }
            None
        }
    }
}

#[derive(Debug, Error)]
pub enum JdkDiscoveryError {
    #[error("could not discover a JDK installation (tried JAVA_HOME and `java` on PATH)")]
    NotFound,

    #[error("JDK root `{root}` does not contain `jmods/` or an `rt.jar` runtime")]
    InvalidJdkRoot { root: PathBuf },
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
    let args: Vec<String> = vec![
        "-XshowSettings:properties".to_string(),
        "-version".to_string(),
    ];
    let opts = RunOptions {
        // JDK discovery should not hang the language server. `java -version` should return nearly
        // immediately on a healthy install; if it doesn't, skip this probe.
        timeout: Some(Duration::from_secs(5)),
        // HotSpot prints a modest amount of config data; keep a conservative cap anyway.
        max_bytes: 1024 * 1024,
        ..RunOptions::default()
    };
    let output = match run_command(Path::new("."), Path::new("java"), &args, opts) {
        Ok(output) => output,
        Err(err) => {
            tracing::debug!(
                target = "nova.jdk",
                error = %err,
                "failed to run `java` for JDK discovery"
            );
            return None;
        }
    };
    if output.timed_out {
        tracing::debug!(
            target = "nova.jdk",
            "timed out running `java` for JDK discovery"
        );
        return None;
    }

    let combined = output.output.combined();

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
    let java_bin = match java_bin.canonicalize() {
        Ok(java_bin) => java_bin,
        Err(err) => {
            tracing::debug!(
                target = "nova.jdk",
                java_bin = %java_bin.display(),
                error = %err,
                "failed to canonicalize java binary for JDK discovery"
            );
            return None;
        }
    };
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
    // Accept JPMS JDK roots.
    if is_jpms_root(&candidate) || is_legacy_jdk_root(&candidate) || is_legacy_jre_root(&candidate)
    {
        return prefer_parent_if_jre(candidate);
    }

    // Some probes (e.g. symlink resolution) may return `$JAVA_HOME/bin`; accept one level up.
    candidate.pop();
    if is_jpms_root(&candidate) || is_legacy_jdk_root(&candidate) || is_legacy_jre_root(&candidate)
    {
        return prefer_parent_if_jre(candidate);
    }

    None
}

fn is_jpms_root(root: &Path) -> bool {
    root.join("jmods").is_dir()
}

fn is_legacy_jdk_root(root: &Path) -> bool {
    root.join("jre").join("lib").join("rt.jar").is_file()
}

fn is_legacy_jre_root(root: &Path) -> bool {
    root.join("lib").join("rt.jar").is_file()
}

fn prefer_parent_if_jre(candidate: PathBuf) -> Option<PathBuf> {
    // For older installations `java.home` often points at `$JDK/jre`. Prefer the
    // parent directory when it looks like a full JDK root.
    if candidate
        .file_name()
        .is_some_and(|name| name == std::ffi::OsStr::new("jre"))
    {
        if let Some(parent) = candidate.parent() {
            if is_jpms_root(parent) || is_legacy_jdk_root(parent) {
                return Some(parent.to_path_buf());
            }
        }
    }

    Some(candidate)
}
