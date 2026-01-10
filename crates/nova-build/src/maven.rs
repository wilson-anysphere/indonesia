use crate::cache::{BuildCache, BuildFileFingerprint};
use crate::{BuildError, BuildResult, BuildSystemKind, Classpath, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct MavenConfig {
    /// Path to the Maven executable (defaults to `mvn` in `PATH`).
    pub mvn_path: PathBuf,
    /// Arguments used to compute a module's compile classpath.
    ///
    /// Defaults to `help:evaluate` on `project.compileClasspathElements`.
    pub classpath_args: Vec<String>,
    /// Arguments used to trigger compilation (and thus produce diagnostics).
    pub build_args: Vec<String>,
    /// Whether to pass `-am` (also make) when targeting a specific module.
    pub also_make: bool,
}

impl Default for MavenConfig {
    fn default() -> Self {
        Self {
            mvn_path: PathBuf::from("mvn"),
            classpath_args: vec![
                "-q".into(),
                "-DforceStdout".into(),
                "-Dexpression=project.compileClasspathElements".into(),
                "help:evaluate".into(),
            ],
            build_args: vec!["-q".into(), "-DskipTests".into(), "compile".into()],
            also_make: true,
        }
    }
}

#[derive(Debug)]
pub struct MavenBuild {
    config: MavenConfig,
}

impl MavenBuild {
    pub fn new(config: MavenConfig) -> Self {
        Self { config }
    }

    pub fn classpath(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        cache: &BuildCache,
    ) -> Result<Classpath> {
        let fingerprint =
            BuildFileFingerprint::from_files(project_root, collect_maven_build_files(project_root)?)?;
        let module_key = module_relative
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<root>".to_string());

        if let Some(cached) = cache.get_module(
            project_root,
            BuildSystemKind::Maven,
            &fingerprint,
            &module_key,
        )? {
            if let Some(entries) = cached.classpath {
                return Ok(Classpath::new(entries));
            }
        }

        let output = self.run(project_root, module_relative, &self.config.classpath_args)?;
        if !output.status.success() {
            let combined = combine_output(&output);
            return Err(BuildError::CommandFailed {
                tool: "maven",
                code: output.status.code(),
                output: combined,
            });
        }

        let combined = combine_output(&output);
        let mut entries = parse_maven_classpath_output(&combined);

        // Best-effort: ensure the module output dir is present even if the chosen
        // classpath strategy omits it.
        if let Some(module_rel) = module_relative {
            let out_dir = project_root.join(module_rel).join("target").join("classes");
            if !entries.iter().any(|p| p == &out_dir) {
                entries.insert(0, out_dir);
            }
        } else {
            let out_dir = project_root.join("target").join("classes");
            if !entries.iter().any(|p| p == &out_dir) {
                entries.insert(0, out_dir);
            }
        }

        cache.update_module(project_root, BuildSystemKind::Maven, &fingerprint, &module_key, |m| {
            m.classpath = Some(entries.clone());
        })?;

        Ok(Classpath::new(entries))
    }

    pub fn build(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        cache: &BuildCache,
    ) -> Result<BuildResult> {
        let fingerprint =
            BuildFileFingerprint::from_files(project_root, collect_maven_build_files(project_root)?)?;
        let module_key = module_relative
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<root>".to_string());

        let output = self.run(project_root, module_relative, &self.config.build_args)?;
        let combined = combine_output(&output);
        let diagnostics = crate::parse_javac_diagnostics(&combined, "maven");

        cache.update_module(project_root, BuildSystemKind::Maven, &fingerprint, &module_key, |m| {
            m.diagnostics = Some(diagnostics.iter().map(crate::cache::CachedDiagnostic::from).collect());
        })?;

        if output.status.success() || !diagnostics.is_empty() {
            return Ok(BuildResult { diagnostics });
        }

        Err(BuildError::CommandFailed {
            tool: "maven",
            code: output.status.code(),
            output: combined,
        })
    }

    fn run(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        args: &[String],
    ) -> Result<std::process::Output> {
        let mut cmd = Command::new(&self.config.mvn_path);
        cmd.current_dir(project_root);

        if let Some(module) = module_relative {
            cmd.arg("-pl").arg(module);
            if self.config.also_make {
                cmd.arg("-am");
            }
        }

        cmd.args(args);
        Ok(cmd.output()?)
    }
}

pub fn parse_maven_classpath_output(output: &str) -> Vec<PathBuf> {
    let trimmed = output.trim();
    let mut entries: Vec<PathBuf> = Vec::new();

    // `help:evaluate` often returns either a bracketed list or newline-separated
    // elements.
    if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        for part in inner.split(',') {
            let s = part.trim().trim_matches('"');
            if !s.is_empty() {
                entries.push(PathBuf::from(s));
            }
        }
    } else {
        for line in trimmed.lines() {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with("[INFO]")
                || line.starts_with("[WARNING]")
                || line.starts_with("[ERROR]")
            {
                continue;
            }

            // Some Maven invocations print a single classpath line separated by
            // the platform-specific path separator.
            let split: Vec<_> = std::env::split_paths(line).collect();
            if split.len() > 1 {
                entries.extend(split);
            } else {
                entries.push(PathBuf::from(line));
            }
        }
    }

    // Dedupe while preserving order.
    let mut seen = std::collections::HashSet::new();
    entries.retain(|p| seen.insert(p.clone()));
    entries
}

fn combine_output(output: &std::process::Output) -> String {
    let mut s = String::new();
    s.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    s
}

fn collect_maven_build_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_maven_build_files_rec(root, root, &mut out)?;
    // Stable sort for hashing.
    out.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });
    out.dedup();
    Ok(out)
}

fn collect_maven_build_files_rec(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            if file_name == ".git"
                || file_name == "target"
                || file_name == ".nova"
                || file_name == ".idea"
                || file_name == ".mvn"
            {
                continue;
            }
            collect_maven_build_files_rec(root, &path, out)?;
            continue;
        }

        if file_name == "pom.xml" {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_poms_for_multi_module_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/maven-multi");
        let files = collect_maven_build_files(&root).unwrap();
        let mut rel: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().to_string())
            .collect();
        rel.sort();
        assert_eq!(rel, vec!["module-a/pom.xml", "module-b/pom.xml", "pom.xml"]);
    }
}
