use crate::cache::{BuildCache, BuildFileFingerprint};
use crate::{BuildError, BuildResult, BuildSystemKind, Classpath, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct GradleConfig {
    /// Path to the `gradle` executable used when a project wrapper (`gradlew`)
    /// is not present.
    pub gradle_path: PathBuf,
    /// Prefer using the Gradle wrapper (`./gradlew`) when present.
    pub prefer_wrapper: bool,
}

impl Default for GradleConfig {
    fn default() -> Self {
        Self {
            gradle_path: PathBuf::from("gradle"),
            prefer_wrapper: true,
        }
    }
}

#[derive(Debug)]
pub struct GradleBuild {
    config: GradleConfig,
}

impl GradleBuild {
    pub fn new(config: GradleConfig) -> Self {
        Self { config }
    }

    pub fn classpath(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<Classpath> {
        let build_files = collect_gradle_build_files(project_root)?;
        let fingerprint = BuildFileFingerprint::from_files(project_root, build_files)?;
        let module_key = project_path.unwrap_or("<root>");

        if let Some(cached) = cache.get_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
        )? {
            if let Some(entries) = cached.classpath {
                return Ok(Classpath::new(entries));
            }
        }

        let output = self.run_print_classpath(project_root, project_path)?;
        if !output.status.success() {
            let combined = combine_output(&output);
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                code: output.status.code(),
                output: combined,
            });
        }

        let combined = combine_output(&output);
        let mut entries = parse_gradle_classpath_output(&combined);

        // Best-effort: include the project's compiled classes output directory.
        //
        // Gradle's `compileClasspath` typically only includes dependency artifacts
        // and does not include the output directory for the current project.
        // For language-server classpath resolution we want the directory that
        // `compileJava` emits classes into.
        let output_dir = gradle_output_dir(project_root, project_path);
        if !entries.iter().any(|p| p == &output_dir) {
            entries.insert(0, output_dir);
        }

        cache.update_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
            |m| {
                m.classpath = Some(entries.clone());
            },
        )?;

        Ok(Classpath::new(entries))
    }

    pub fn build(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<BuildResult> {
        let build_files = collect_gradle_build_files(project_root)?;
        let fingerprint = BuildFileFingerprint::from_files(project_root, build_files)?;
        let module_key = project_path.unwrap_or("<root>");

        let output = self.run_compile(project_root, project_path)?;
        let combined = combine_output(&output);
        let diagnostics = crate::parse_javac_diagnostics(&combined, "gradle");

        cache.update_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
            |m| {
                m.diagnostics =
                    Some(diagnostics.iter().map(crate::cache::CachedDiagnostic::from).collect());
            },
        )?;

        if output.status.success() || !diagnostics.is_empty() {
            return Ok(BuildResult { diagnostics });
        }

        Err(BuildError::CommandFailed {
            tool: "gradle",
            code: output.status.code(),
            output: combined,
        })
    }

    fn run_print_classpath(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<std::process::Output> {
        let gradle = self.gradle_executable(project_root);
        let init_script = write_init_script(project_root)?;

        let mut cmd = Command::new(gradle);
        cmd.current_dir(project_root);
        cmd.arg("--no-daemon");
        cmd.arg("--console=plain");
        cmd.arg("-q");
        cmd.arg("--init-script").arg(&init_script);

        let task = match project_path {
            Some(p) => format!("{p}:printNovaClasspath"),
            None => "printNovaClasspath".to_string(),
        };
        cmd.arg(task);

        let output = cmd.output()?;
        let _ = std::fs::remove_file(&init_script);
        Ok(output)
    }

    fn run_compile(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<std::process::Output> {
        let gradle = self.gradle_executable(project_root);
        let mut cmd = Command::new(gradle);
        cmd.current_dir(project_root);
        cmd.arg("--no-daemon");
        cmd.arg("--console=plain");

        let task = match project_path {
            Some(p) => format!("{p}:compileJava"),
            None => "compileJava".to_string(),
        };
        cmd.arg(task);
        Ok(cmd.output()?)
    }

    fn gradle_executable(&self, project_root: &Path) -> PathBuf {
        if self.config.prefer_wrapper {
            let wrapper = project_root.join("gradlew");
            if wrapper.exists() {
                return wrapper;
            }
        }
        self.config.gradle_path.clone()
    }
}

fn gradle_output_dir(project_root: &Path, project_path: Option<&str>) -> PathBuf {
    // Best-effort mapping from Gradle project paths to directories.
    //
    // For standard Gradle layouts, a project path like `:app` corresponds to an
    // `app/` directory under the workspace root. More complex setups can change
    // this mapping using `settings.gradle`, but we keep the heuristic small and
    // predictable.
    let mut rel = PathBuf::new();
    if let Some(path) = project_path {
        let trimmed = path.trim_matches(':');
        for part in trimmed.split(':').filter(|p| !p.is_empty()) {
            rel.push(part);
        }
    }

    project_root
        .join(rel)
        .join("build")
        .join("classes")
        .join("java")
        .join("main")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradle_output_dir_maps_project_path_to_directory() {
        let root = Path::new("/workspace");
        assert_eq!(
            gradle_output_dir(root, None),
            PathBuf::from("/workspace/build/classes/java/main")
        );
        assert_eq!(
            gradle_output_dir(root, Some(":app")),
            PathBuf::from("/workspace/app/build/classes/java/main")
        );
        assert_eq!(
            gradle_output_dir(root, Some(":lib:core")),
            PathBuf::from("/workspace/lib/core/build/classes/java/main")
        );
    }
}

pub fn parse_gradle_classpath_output(output: &str) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('>') || line.starts_with("FAILURE") || line.starts_with("BUILD FAILED")
        {
            continue;
        }
        // If a tool prints a single platform-separated classpath string, fall
        // back to `split_paths`.
        let split: Vec<_> = std::env::split_paths(line).collect();
        if split.len() > 1 {
            entries.extend(split);
        } else {
            entries.push(PathBuf::from(line));
        }
    }
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

fn write_init_script(project_root: &Path) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("nova_gradle_init_{token}.gradle"));

    // Best-effort init script that registers a task printing the resolved
    // `compileClasspath` configuration.
    let script = r#"
allprojects { proj ->
    proj.tasks.register("printNovaClasspath") {
        doLast {
            def cfg = proj.configurations.findByName("compileClasspath")
            if (cfg == null) {
                cfg = proj.configurations.findByName("runtimeClasspath")
            }
            if (cfg == null) {
                return
            }
            cfg.resolve().each { f ->
                println(f.absolutePath)
            }
        }
    }
}
"#;

    std::fs::write(&path, script)?;

    // Make sure the temp file is unique within the project (e.g. when running
    // with restrictive tmpfs setups).
    if !path.exists() {
        return Err(BuildError::Unsupported(format!(
            "failed to create init script under {}",
            project_root.display()
        )));
    }

    Ok(path)
}

fn collect_gradle_build_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_gradle_build_files_rec(root, root, &mut out)?;
    Ok(out)
}

fn collect_gradle_build_files_rec(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            if file_name == ".git"
                || file_name == ".gradle"
                || file_name == "build"
                || file_name == "target"
                || file_name == ".nova"
            {
                continue;
            }
            collect_gradle_build_files_rec(root, &path, out)?;
            continue;
        }

        match file_name.as_ref() {
            "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "settings.gradle.kts"
            | "gradle.properties" => {
                out.push(path);
            }
            _ => {}
        }
    }
    // Ensure stable order for hashing.
    out.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });
    Ok(())
}
