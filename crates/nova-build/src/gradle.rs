use crate::cache::{BuildCache, BuildFileFingerprint, CachedProjectInfo};
use crate::command::format_command;
use crate::{
    BuildError, BuildResult, BuildSystemKind, Classpath, CommandOutput, CommandRunner,
    DefaultCommandRunner, JavaCompileConfig, Result,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NOVA_JSON_BEGIN: &str = "NOVA_JSON_BEGIN";
const NOVA_JSON_END: &str = "NOVA_JSON_END";
const NOVA_GRADLE_TASK: &str = "printNovaJavaCompileConfig";

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
    runner: Arc<dyn CommandRunner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GradleProjectInfo {
    pub path: String,
    pub dir: PathBuf,
}

impl GradleBuild {
    pub fn new(config: GradleConfig) -> Self {
        Self::with_runner(config, Arc::new(DefaultCommandRunner::default()))
    }

    pub fn with_runner(config: GradleConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self { config, runner }
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
                    .map(|p| GradleProjectInfo { path: p.path, dir: p.dir })
                    .collect());
            }
        }

        let (program, args, output) = self.run_print_projects(project_root)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }

        let combined = output.combined();
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
        let cfg = self.java_compile_config(project_root, project_path, cache)?;
        Ok(Classpath::new(cfg.compile_classpath))
    }

    pub fn java_compile_config(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
        cache: &BuildCache,
    ) -> Result<JavaCompileConfig> {
        let project_path = project_path.filter(|p| *p != ":");
        let fingerprint = gradle_build_fingerprint(project_root)?;
        let module_key = project_path.unwrap_or("<root>");

        if let Some(cached) = cache.get_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
        )? {
            if let Some(cfg) = cached.java_compile_config {
                return Ok(cfg);
            }
            // Backwards-compat: older cache entries may contain only classpath.
            if let Some(entries) = cached.classpath {
                return Ok(JavaCompileConfig {
                    compile_classpath: entries,
                    ..JavaCompileConfig::default()
                });
            }
        }

        let (program, args, output) =
            self.run_print_java_compile_config(project_root, project_path)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "gradle",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }

        let combined = output.combined();
        let json = parse_gradle_java_compile_config_json(&combined)?;

        // Aggregator roots often don't apply the Java plugin and thus do not
        // expose `compileClasspath`. When querying the workspace-level config
        // (project path == None), fall back to unioning all subprojects.
        if project_path.is_none() && json.compile_classpath.is_none() {
            let projects = self.projects(project_root, cache)?;

            let mut configs = Vec::new();
            for project in projects.into_iter().filter(|p| p.path != ":") {
                configs.push(self.java_compile_config(
                    project_root,
                    Some(project.path.as_str()),
                    cache,
                )?);
            }
            let union = JavaCompileConfig::union(configs);

            cache.update_module(
                project_root,
                BuildSystemKind::Gradle,
                &fingerprint,
                module_key,
                |m| {
                    m.java_compile_config = Some(union.clone());
                    m.classpath = Some(union.compile_classpath.clone());
                },
            )?;

            return Ok(union);
        }

        let main_output_fallback =
            gradle_output_dir_cached(project_root, project_path, cache, &fingerprint)?;
        let test_output_fallback = gradle_test_output_dir_from_main(&main_output_fallback);
        let config =
            normalize_gradle_java_compile_config(json, main_output_fallback, test_output_fallback);

        cache.update_module(
            project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            module_key,
            |m| {
                m.java_compile_config = Some(config.clone());
                // Keep populating the legacy classpath field for older readers.
                m.classpath = Some(config.compile_classpath.clone());
            },
        )?;

        Ok(config)
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

        let (program, args, output) = self.run_compile(project_root, project_path)?;
        let combined = output.combined();
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
            command: format_command(&program, &args),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn run_print_java_compile_config(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let gradle = self.gradle_executable(project_root);
        let init_script = write_init_script(project_root)?;

        let mut args: Vec<String> = Vec::new();
        args.push("--no-daemon".into());
        args.push("--console=plain".into());
        args.push("-q".into());
        args.push("--init-script".into());
        args.push(init_script.to_string_lossy().to_string());

        let task = match project_path {
            Some(p) => format!("{p}:{NOVA_GRADLE_TASK}"),
            None => NOVA_GRADLE_TASK.to_string(),
        };
        args.push(task);

        let output = self.runner.run(project_root, &gradle, &args);
        let _ = std::fs::remove_file(&init_script);
        Ok((gradle, args, output?))
    }

    fn run_print_projects(
        &self,
        project_root: &Path,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let gradle = self.gradle_executable(project_root);
        let init_script = write_init_script(project_root)?;

        let mut args: Vec<String> = Vec::new();
        args.push("--no-daemon".into());
        args.push("--console=plain".into());
        args.push("-q".into());
        args.push("--init-script".into());
        args.push(init_script.to_string_lossy().to_string());
        args.push("printNovaProjects".into());

        let output = self.runner.run(project_root, &gradle, &args);
        let _ = std::fs::remove_file(&init_script);
        Ok((gradle, args, output?))
    }

    fn run_compile(
        &self,
        project_root: &Path,
        project_path: Option<&str>,
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let gradle = self.gradle_executable(project_root);
        let mut args: Vec<String> = Vec::new();
        args.push("--no-daemon".into());
        args.push("--console=plain".into());

        match project_path {
            Some(p) => {
                args.push(format!("{p}:compileJava"));
                let output = self.runner.run(project_root, &gradle, &args)?;
                Ok((gradle, args, output))
            }
            None => {
                let init_script = write_compile_all_java_init_script(project_root)?;
                args.push("--init-script".into());
                args.push(init_script.to_string_lossy().to_string());
                args.push("novaCompileAllJava".into());

                let output = self.runner.run(project_root, &gradle, &args);
                let _ = std::fs::remove_file(&init_script);
                Ok((gradle, args, output?))
            }
        }
    }

    fn gradle_executable(&self, project_root: &Path) -> PathBuf {
        if self.config.prefer_wrapper {
            let wrapper_candidates = if cfg!(windows) {
                ["gradlew.bat", "gradlew"]
            } else {
                ["gradlew", "gradlew.bat"]
            };
            for name in wrapper_candidates {
                let wrapper = project_root.join(name);
                if wrapper.exists() {
                    return wrapper;
                }
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
    // `::printNovaJavaCompileConfig`), so callers use `None` instead.
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

fn gradle_test_output_dir_from_main(main_output_dir: &Path) -> PathBuf {
    let mut path = main_output_dir.to_path_buf();
    path.pop();
    path.push("test");
    path
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
        let union = JavaCompileConfig::union([
            JavaCompileConfig {
                compile_classpath: vec![
                    PathBuf::from("/a"),
                    PathBuf::from("/b"),
                    PathBuf::from("/c"),
                ],
                ..JavaCompileConfig::default()
            },
            JavaCompileConfig {
                compile_classpath: vec![PathBuf::from("/b"), PathBuf::from("/d")],
                ..JavaCompileConfig::default()
            },
            JavaCompileConfig {
                compile_classpath: vec![PathBuf::from("/a"), PathBuf::from("/e")],
                ..JavaCompileConfig::default()
            },
        ]);
        assert_eq!(
            union.compile_classpath,
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
        let out = "NOVA_JSON_BEGIN\n/a/b/c.jar\n";
        let cp = parse_gradle_classpath_output(out);
        assert_eq!(cp, vec![PathBuf::from("/a/b/c.jar")]);
    }

    #[test]
    fn extracts_nova_json_block_from_gradle_noise() {
        let out = r#"
> Task :someTask

NOVA_JSON_BEGIN
{"compileClasspath":["/a.jar"]}
NOVA_JSON_END

BUILD SUCCESSFUL in 1s
"#;
        let json = extract_nova_json_block(out).unwrap();
        assert_eq!(json.trim(), r#"{"compileClasspath":["/a.jar"]}"#);
    }

    #[test]
    fn parses_gradle_java_compile_config_and_dedupes_paths() {
        let out = r#"
NOVA_JSON_BEGIN
{
  "compileClasspath": ["/dep/a.jar", "/dep/a.jar", "/dep/b.jar"],
  "testCompileClasspath": ["/dep/test.jar"],
  "mainSourceRoots": ["/src/main/java"],
  "testSourceRoots": ["/src/test/java"],
  "mainOutputDirs": ["/out/main", "/out/main"],
  "testOutputDirs": ["/out/test"],
  "sourceCompatibility": "17",
  "targetCompatibility": "17",
  "toolchainLanguageVersion": "21"
}
NOVA_JSON_END
"#;
        let parsed = parse_gradle_java_compile_config_json(out).expect("parse json");
        let cfg = normalize_gradle_java_compile_config(
            parsed,
            PathBuf::from("/fallback/main"),
            PathBuf::from("/fallback/test"),
        );
        assert_eq!(cfg.main_source_roots, vec![PathBuf::from("/src/main/java")]);
        assert_eq!(cfg.test_source_roots, vec![PathBuf::from("/src/test/java")]);
        assert_eq!(cfg.main_output_dir, Some(PathBuf::from("/out/main")));
        assert_eq!(cfg.test_output_dir, Some(PathBuf::from("/out/test")));
        assert_eq!(
            cfg.compile_classpath,
            vec![
                PathBuf::from("/out/main"),
                PathBuf::from("/dep/a.jar"),
                PathBuf::from("/dep/b.jar")
            ]
        );
        assert_eq!(
            cfg.test_classpath,
            vec![
                PathBuf::from("/out/test"),
                PathBuf::from("/out/main"),
                PathBuf::from("/dep/test.jar")
            ]
        );
        assert_eq!(cfg.source.as_deref(), Some("17"));
        assert_eq!(cfg.target.as_deref(), Some("17"));
        assert_eq!(cfg.release.as_deref(), Some("21"));
    }

    #[test]
    fn parses_gradle_java_compile_config_with_null_fields() {
        let out = r#"
some warning
NOVA_JSON_BEGIN
{"compileClasspath":null,"testCompileClasspath":null,"mainOutputDirs":null,"testOutputDirs":null}
NOVA_JSON_END
"#;
        let parsed = parse_gradle_java_compile_config_json(out).expect("parse json");
        let main_output_fallback = gradle_output_dir(Path::new("/workspace"), Some(":app"));
        let test_output_fallback = gradle_test_output_dir_from_main(&main_output_fallback);
        let cfg = normalize_gradle_java_compile_config(
            parsed,
            main_output_fallback,
            test_output_fallback,
        );
        assert_eq!(
            cfg.main_output_dir,
            Some(PathBuf::from("/workspace/app/build/classes/java/main"))
        );
        assert_eq!(
            cfg.test_output_dir,
            Some(PathBuf::from("/workspace/app/build/classes/java/test"))
        );
        assert_eq!(
            cfg.compile_classpath,
            vec![PathBuf::from("/workspace/app/build/classes/java/main")]
        );
        assert_eq!(
            cfg.test_classpath,
            vec![
                PathBuf::from("/workspace/app/build/classes/java/test"),
                PathBuf::from("/workspace/app/build/classes/java/main")
            ]
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GradleJavaCompileConfigJson {
    #[serde(default)]
    compile_classpath: Option<Vec<String>>,
    #[serde(default)]
    test_compile_classpath: Option<Vec<String>>,
    #[serde(default)]
    main_source_roots: Option<Vec<String>>,
    #[serde(default)]
    test_source_roots: Option<Vec<String>>,
    #[serde(default)]
    main_output_dirs: Option<Vec<String>>,
    #[serde(default)]
    test_output_dirs: Option<Vec<String>>,
    #[serde(default)]
    source_compatibility: Option<String>,
    #[serde(default)]
    target_compatibility: Option<String>,
    #[serde(default)]
    toolchain_language_version: Option<String>,
}

fn parse_gradle_java_compile_config_json(output: &str) -> Result<GradleJavaCompileConfigJson> {
    let json = extract_nova_json_block(output)?;
    serde_json::from_str(json.trim()).map_err(|e| BuildError::Parse(e.to_string()))
}

fn normalize_gradle_java_compile_config(
    parsed: GradleJavaCompileConfigJson,
    main_output_fallback: PathBuf,
    test_output_fallback: PathBuf,
) -> JavaCompileConfig {
    let mut main_output_dirs = strings_to_paths(parsed.main_output_dirs);
    dedupe_paths(&mut main_output_dirs);
    if main_output_dirs.is_empty() {
        main_output_dirs.push(main_output_fallback);
    }

    let mut test_output_dirs = strings_to_paths(parsed.test_output_dirs);
    dedupe_paths(&mut test_output_dirs);
    if test_output_dirs.is_empty() {
        test_output_dirs.push(test_output_fallback);
    }

    let main_output_dir = main_output_dirs.first().cloned();
    let test_output_dir = test_output_dirs.first().cloned();

    let mut compile_classpath = Vec::new();
    compile_classpath.extend(main_output_dirs.clone());
    compile_classpath.extend(strings_to_paths(parsed.compile_classpath));
    dedupe_paths(&mut compile_classpath);

    let mut test_classpath = Vec::new();
    test_classpath.extend(test_output_dirs);
    test_classpath.extend(main_output_dirs);
    test_classpath.extend(strings_to_paths(parsed.test_compile_classpath));
    dedupe_paths(&mut test_classpath);

    let mut main_source_roots = strings_to_paths(parsed.main_source_roots);
    let mut test_source_roots = strings_to_paths(parsed.test_source_roots);
    dedupe_paths(&mut main_source_roots);
    dedupe_paths(&mut test_source_roots);

    JavaCompileConfig {
        compile_classpath,
        test_classpath,
        module_path: Vec::new(),
        main_source_roots,
        test_source_roots,
        main_output_dir,
        test_output_dir,
        source: parsed.source_compatibility,
        target: parsed.target_compatibility,
        release: parsed.toolchain_language_version,
        enable_preview: false,
    }
}

fn extract_nova_json_block(output: &str) -> Result<String> {
    extract_sentinel_block(output, NOVA_JSON_BEGIN, NOVA_JSON_END)
        .ok_or_else(|| BuildError::Parse("failed to locate Gradle JSON block".into()))
}

fn strings_to_paths(value: Option<Vec<String>>) -> Vec<PathBuf> {
    value
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(PathBuf::from(s))
            }
        })
        .collect()
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
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
    proj.tasks.register("printNovaJavaCompileConfig") {
        doLast {
            def payload = [:]

            def cfg = proj.configurations.findByName("compileClasspath")
            if (cfg == null) {
                cfg = proj.configurations.findByName("runtimeClasspath")
            }

            def testCfg = proj.configurations.findByName("testCompileClasspath")
            if (testCfg == null) {
                testCfg = proj.configurations.findByName("testRuntimeClasspath")
            }
            if (testCfg == null) {
                testCfg = proj.configurations.findByName("runtimeClasspath")
            }

            payload.compileClasspath = (cfg != null) ? cfg.resolve().collect { it.absolutePath } : null
            payload.testCompileClasspath = (testCfg != null) ? testCfg.resolve().collect { it.absolutePath } : null

            def sourceSets = null
            try {
                sourceSets = proj.extensions.findByName("sourceSets")
            } catch (Exception ignored) {}

            if (sourceSets != null) {
                def main = sourceSets.findByName("main")
                def test = sourceSets.findByName("test")
                payload.mainSourceRoots = (main != null) ? main.java.srcDirs.collect { it.absolutePath } : null
                payload.testSourceRoots = (test != null) ? test.java.srcDirs.collect { it.absolutePath } : null
                payload.mainOutputDirs = (main != null) ? main.output.classesDirs.files.collect { it.absolutePath } : null
                payload.testOutputDirs = (test != null) ? test.output.classesDirs.files.collect { it.absolutePath } : null
            } else {
                payload.mainSourceRoots = null
                payload.testSourceRoots = null
                payload.mainOutputDirs = null
                payload.testOutputDirs = null
            }

            def sourceCompat = null
            def targetCompat = null
            def toolchainLang = null

            def javaExt = null
            try {
                javaExt = proj.extensions.findByName("java")
            } catch (Exception ignored) {}

            if (javaExt != null) {
                try {
                    sourceCompat = javaExt.sourceCompatibility?.toString()
                } catch (Exception ignored) {}
                try {
                    targetCompat = javaExt.targetCompatibility?.toString()
                } catch (Exception ignored) {}
                try {
                    def lv = javaExt.toolchain?.languageVersion
                    if (lv != null && lv.isPresent()) {
                        toolchainLang = lv.get().asInt().toString()
                    }
                } catch (Exception ignored) {}
            } else {
                try {
                    sourceCompat = proj.sourceCompatibility?.toString()
                } catch (Exception ignored) {}
                try {
                    targetCompat = proj.targetCompatibility?.toString()
                } catch (Exception ignored) {}
            }

            payload.sourceCompatibility = sourceCompat
            payload.targetCompatibility = targetCompat
            payload.toolchainLanguageVersion = toolchainLang

            println("NOVA_JSON_BEGIN")
            println(JsonOutput.toJson(payload))
            println("NOVA_JSON_END")
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

fn write_compile_all_java_init_script(project_root: &Path) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("nova_gradle_compile_all_{token}.gradle"));

    // Register a root task that depends on all `compileJava` tasks we can find.
    //
    // This is necessary for multi-project Gradle workspaces where the root
    // project is just an aggregator and does not apply the Java plugin.
    let script = r#"
gradle.rootProject { root ->
    def novaTaskProvider = root.tasks.register("novaCompileAllJava") {
        group = "build"
        description = "Compiles all Java sources across all projects (Nova helper task)"
    }

    gradle.projectsEvaluated {
        def compileTasks = []
        root.allprojects { proj ->
            def t = proj.tasks.findByName("compileJava")
            if (t != null) {
                compileTasks.add(t)
            }
        }
        novaTaskProvider.configure {
            dependsOn compileTasks
        }
    }
}
"#;

    std::fs::write(&path, script)?;

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
            | "gradle.properties" => {
                // NOTE: we intentionally exclude the wrapper scripts (`gradlew*`) from
                // the fingerprint. They are execution helpers rather than build
                // configuration. Keeping them out of the fingerprint lets Nova reuse
                // cached build metadata even if a workspace is missing the wrapper
                // scripts (e.g. sparse checkouts).
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
