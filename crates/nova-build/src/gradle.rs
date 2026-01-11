use crate::cache::{BuildCache, BuildFileFingerprint, CachedProjectInfo};
use crate::{BuildError, BuildResult, BuildSystemKind, Classpath, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

const NOVA_NO_CLASSPATH_MARKER: &str = "NOVA_NO_CLASSPATH";
const NOVA_PROJECTS_BEGIN: &str = "NOVA_PROJECTS_BEGIN";
const NOVA_PROJECTS_END: &str = "NOVA_PROJECTS_END";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GradleProjectInfo {
    pub path: String,
    pub dir: PathBuf,
}

impl GradleBuild {
    pub fn new(config: GradleConfig) -> Self {
        Self { config }
    }

    pub fn projects(
        &self,
        project_root: &Path,
        cache: &BuildCache,
    ) -> Result<Vec<GradleProjectInfo>> {
        let fingerprint = gradle_build_fingerprint(project_root)?;

        if let Some(cached) = cache.load(project_root, BuildSystemKind::Gradle, &fingerprint)? {
            if let Some(projects) = cached.projects {
                return Ok(projects
                    .into_iter()
                    .map(|p| GradleProjectInfo {
                        path: p.path,
                        dir: p.dir,
                    })
                    .collect());
            }
        }

        let output = self.run_print_projects(project_root)?;
        if !output.status.success() {
            let combined = combine_output(&output);
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                code: output.status.code(),
                output: combined,
            });
        }

        let combined = combine_output(&output);
        let projects = parse_gradle_projects_output(&combined)?;

        let cached_projects: Vec<CachedProjectInfo> = projects
            .iter()
            .map(|p| CachedProjectInfo {
                path: p.path.clone(),
                dir: p.dir.clone(),
            })
            .collect();

        let mut data = cache
            .load(project_root, BuildSystemKind::Gradle, &fingerprint)?
            .unwrap_or_default();
        data.projects = Some(cached_projects);
        cache.store(project_root, BuildSystemKind::Gradle, &fingerprint, &data)?;

        Ok(projects)
    }

    pub fn classpath(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<Classpath> {
        let project_path = project_path.filter(|p| *p != ":");
        let fingerprint = gradle_build_fingerprint(project_root)?;
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
        let has_classpath = !combined
            .lines()
            .any(|line| line.trim() == NOVA_NO_CLASSPATH_MARKER);

        // Aggregator roots often don't apply the Java plugin and thus do not
        // expose `compileClasspath`. When querying the workspace-level
        // classpath (project path == None), fall back to unioning all
        // subprojects.
        if project_path.is_none() && !has_classpath {
            let projects = self.projects(project_root, cache)?;

            let mut classpaths = Vec::new();
            for project in projects.into_iter().filter(|p| p.path != ":") {
                classpaths.push(
                    self.classpath(project_root, Some(project.path.as_str()), cache)?
                        .entries,
                );
            }
            let entries = union_classpath_entries(classpaths);

            cache.update_module(
                project_root,
                BuildSystemKind::Gradle,
                &fingerprint,
                module_key,
                |m| {
                    m.classpath = Some(entries.clone());
                },
            )?;

            return Ok(Classpath::new(entries));
        }

        let mut entries = parse_gradle_classpath_output(&combined);

        // Best-effort: include the project's compiled classes output directory.
        //
        // Gradle's `compileClasspath` typically only includes dependency artifacts
        // and does not include the output directory for the current project.
        // For language-server classpath resolution we want the directory that
        // `compileJava` emits classes into.
        if has_classpath {
            let output_dir =
                gradle_output_dir_cached(project_root, project_path, cache, &fingerprint)?;
            if !entries.iter().any(|p| p == &output_dir) {
                entries.insert(0, output_dir);
            }
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
        let project_path = project_path.filter(|p| *p != ":");
        let fingerprint = gradle_build_fingerprint(project_root)?;
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
                m.diagnostics = Some(
                    diagnostics
                        .iter()
                        .map(crate::cache::CachedDiagnostic::from)
                        .collect(),
                );
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

    fn run_print_projects(&self, project_root: &Path) -> Result<std::process::Output> {
        let gradle = self.gradle_executable(project_root);
        let init_script = write_init_script(project_root)?;

        let mut cmd = Command::new(gradle);
        cmd.current_dir(project_root);
        cmd.arg("--no-daemon");
        cmd.arg("--console=plain");
        cmd.arg("-q");
        cmd.arg("--init-script").arg(&init_script);
        cmd.arg("printNovaProjects");

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

fn gradle_build_fingerprint(project_root: &Path) -> Result<BuildFileFingerprint> {
    let build_files = collect_gradle_build_files(project_root)?;
    BuildFileFingerprint::from_files(project_root, build_files)
}

fn gradle_output_dir_cached(
    project_root: &Path,
    project_path: Option<&str>,
    cache: &BuildCache,
    fingerprint: &BuildFileFingerprint,
) -> Result<PathBuf> {
    let Some(project_path) = project_path else {
        return Ok(gradle_output_dir(project_root, None));
    };

    // Root project path can't be used as a task prefix (it would produce
    // `::printNovaClasspath`), so callers use `None` instead.
    if project_path == ":" {
        return Ok(gradle_output_dir(project_root, None));
    }

    if let Some(data) = cache.load(project_root, BuildSystemKind::Gradle, fingerprint)? {
        if let Some(projects) = data.projects {
            if let Some(found) = projects.into_iter().find(|p| p.path == project_path) {
                return Ok(found
                    .dir
                    .join("build")
                    .join("classes")
                    .join("java")
                    .join("main"));
            }
        }
    }

    Ok(gradle_output_dir(project_root, Some(project_path)))
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

    #[test]
    fn parses_gradle_projects_json_from_noisy_output() {
        let out = r#"
> Task :printNovaProjects
Deprecated feature warning
NOVA_PROJECTS_BEGIN
{"projects":[{"path":":app","projectDir":"/workspace/app"},{"path":":","projectDir":"/workspace"}]}
NOVA_PROJECTS_END
BUILD SUCCESSFUL
"#;
        let projects = parse_gradle_projects_output(out).unwrap();
        assert_eq!(
            projects,
            vec![
                GradleProjectInfo {
                    path: ":".into(),
                    dir: PathBuf::from("/workspace"),
                },
                GradleProjectInfo {
                    path: ":app".into(),
                    dir: PathBuf::from("/workspace/app"),
                }
            ]
        );
    }

    #[test]
    fn union_classpath_preserves_order_and_dedupes() {
        let union = union_classpath_entries([
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
            ],
            vec![PathBuf::from("/b"), PathBuf::from("/d")],
            vec![PathBuf::from("/a"), PathBuf::from("/e")],
        ]);
        assert_eq!(
            union,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
                PathBuf::from("/d"),
                PathBuf::from("/e")
            ]
        );
    }

    #[test]
    fn parse_gradle_classpath_ignores_nova_markers() {
        let out = "NOVA_NO_CLASSPATH\n/a/b/c.jar\n";
        let cp = parse_gradle_classpath_output(out);
        assert_eq!(cp, vec![PathBuf::from("/a/b/c.jar")]);
    }
}

pub fn parse_gradle_classpath_output(output: &str) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("NOVA_") {
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

pub fn parse_gradle_projects_output(output: &str) -> Result<Vec<GradleProjectInfo>> {
    let json = extract_sentinel_block(output, NOVA_PROJECTS_BEGIN, NOVA_PROJECTS_END)
        .ok_or_else(|| BuildError::Parse("failed to locate Gradle project JSON block".into()))?;

    let parsed: GradleProjectsJson =
        serde_json::from_str(json.trim()).map_err(|e| BuildError::Parse(e.to_string()))?;

    let mut projects: Vec<GradleProjectInfo> = parsed
        .projects
        .into_iter()
        .map(|p| GradleProjectInfo {
            path: p.path,
            dir: PathBuf::from(p.project_dir),
        })
        .collect();
    projects.sort_by(|a, b| a.path.cmp(&b.path));
    projects.dedup_by(|a, b| a.path == b.path);
    Ok(projects)
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

fn union_classpath_entries<I, T>(classpaths: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = T>,
    T: IntoIterator<Item = PathBuf>,
{
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for cp in classpaths {
        for path in cp {
            if seen.insert(path.clone()) {
                entries.push(path);
            }
        }
    }
    entries
}

fn extract_sentinel_block(output: &str, begin: &str, end: &str) -> Option<String> {
    let mut in_block = false;
    let mut lines = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == begin {
                in_block = true;
            }
            continue;
        }

        if trimmed == end {
            return Some(lines.join("\n"));
        }
        lines.push(line);
    }
    None
}

#[derive(Debug, Deserialize)]
struct GradleProjectsJson {
    projects: Vec<GradleProjectJson>,
}

#[derive(Debug, Deserialize)]
struct GradleProjectJson {
    path: String,
    #[serde(rename = "projectDir")]
    project_dir: String,
}

fn write_init_script(project_root: &Path) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("nova_gradle_init_{token}.gradle"));

    // Best-effort init script that registers tasks for emitting:
    // - resolved `compileClasspath` configuration entries per project
    // - Gradle project list + directories for multi-module discovery
    let script = r#"
import groovy.json.JsonOutput

allprojects { proj ->
    proj.tasks.register("printNovaClasspath") {
        doLast {
            def cfg = proj.configurations.findByName("compileClasspath")
            if (cfg == null) {
                cfg = proj.configurations.findByName("runtimeClasspath")
            }
            if (cfg == null) {
                println("NOVA_NO_CLASSPATH")
                return
            }
            cfg.resolve().each { f ->
                println(f.absolutePath)
            }
        }
    }

    if (proj == proj.rootProject) {
        proj.tasks.register("printNovaProjects") {
            doLast {
                def projects = proj.rootProject.allprojects.collect { p ->
                    [path: p.path, projectDir: p.projectDir.absolutePath]
                }
                projects.sort { a, b -> a.path <=> b.path }
                def json = JsonOutput.toJson([projects: projects])
                println("NOVA_PROJECTS_BEGIN")
                println(json)
                println("NOVA_PROJECTS_END")
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

pub fn collect_gradle_build_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_gradle_build_files_rec(root, root, &mut out)?;
    // Stable sort for hashing.
    out.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });
    out.dedup();
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
                || file_name == ".idea"
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
            | "gradle.properties"
            | "gradlew"
            | "gradlew.bat" => {
                out.push(path);
            }
            "gradle-wrapper.properties" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties")) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
    Ok(())
}
