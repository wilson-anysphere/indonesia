use crate::cache::{BuildCache, BuildFileFingerprint};
use crate::{BuildError, BuildResult, BuildSystemKind, Classpath, JavaCompileConfig, Result};
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
        let pom_files = collect_maven_build_files(project_root)?;
        let fingerprint = BuildFileFingerprint::from_files(project_root, pom_files.clone())?;
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
            if let Some(cfg) = cached.java_compile_config {
                return Ok(Classpath::new(cfg.compile_classpath));
            }
        }

        // If the caller asked for the root classpath and we detect multiple
        // Maven modules, return a best-effort union of module classpaths. This
        // is more useful for language-server indexing than the aggregator POM's
        // own classpath (which is often empty for `<packaging>pom</packaging>`).
        if module_relative.is_none() {
            let modules = discover_maven_modules(project_root, &pom_files);
            if !modules.is_empty() {
                let mut entries = Vec::new();
                for module in modules {
                    let cp = self.classpath(project_root, Some(&module), cache)?;
                    entries.extend(cp.entries);
                }

                let mut seen = std::collections::HashSet::new();
                entries.retain(|p| seen.insert(p.clone()));

                cache.update_module(
                    project_root,
                    BuildSystemKind::Maven,
                    &fingerprint,
                    &module_key,
                    |m| {
                        m.classpath = Some(entries.clone());
                    },
                )?;

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

        cache.update_module(
            project_root,
            BuildSystemKind::Maven,
            &fingerprint,
            &module_key,
            |m| {
                m.classpath = Some(entries.clone());
            },
        )?;

        Ok(Classpath::new(entries))
    }

    pub fn java_compile_config(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        cache: &BuildCache,
    ) -> Result<JavaCompileConfig> {
        let pom_files = collect_maven_build_files(project_root)?;
        let fingerprint = BuildFileFingerprint::from_files(project_root, pom_files.clone())?;
        let module_key = module_relative
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<root>".to_string());

        if let Some(cached) = cache.get_module(
            project_root,
            BuildSystemKind::Maven,
            &fingerprint,
            &module_key,
        )? {
            if let Some(cfg) = cached.java_compile_config {
                return Ok(cfg);
            }
        }

        // Multi-module root: union module configs instead of using the aggregator POM.
        if module_relative.is_none() {
            let modules = discover_maven_modules(project_root, &pom_files);
            if !modules.is_empty() {
                let mut configs = Vec::new();
                for module in modules {
                    configs.push(self.java_compile_config(project_root, Some(&module), cache)?);
                }

                let cfg = JavaCompileConfig::union(configs);
                cache.update_module(
                    project_root,
                    BuildSystemKind::Maven,
                    &fingerprint,
                    &module_key,
                    |m| {
                        m.classpath = Some(cfg.compile_classpath.clone());
                        m.java_compile_config = Some(cfg.clone());
                    },
                )?;

                return Ok(cfg);
            }
        }

        let mut compile_classpath = self.evaluate_path_list(
            project_root,
            module_relative,
            "project.compileClasspathElements",
        )?;

        let mut test_classpath = self.evaluate_path_list(
            project_root,
            module_relative,
            "project.testClasspathElements",
        )?;

        let main_source_roots =
            self.evaluate_path_list(project_root, module_relative, "project.compileSourceRoots")?;

        let test_source_roots = {
            let roots = self.evaluate_path_list(
                project_root,
                module_relative,
                "project.testCompileSourceRoots",
            )?;
            if roots.is_empty() {
                self.evaluate_path_list(project_root, module_relative, "project.testSourceRoots")?
            } else {
                roots
            }
        };

        let main_output_dir = self
            .evaluate_scalar_best_effort(
                project_root,
                module_relative,
                "project.build.outputDirectory",
            )?
            .map(PathBuf::from)
            .or_else(|| {
                let rel = module_relative.unwrap_or_else(|| Path::new(""));
                if rel.as_os_str().is_empty() {
                    Some(project_root.join("target").join("classes"))
                } else {
                    Some(project_root.join(rel).join("target").join("classes"))
                }
            });

        let test_output_dir = self
            .evaluate_scalar_best_effort(
                project_root,
                module_relative,
                "project.build.testOutputDirectory",
            )?
            .map(PathBuf::from)
            .or_else(|| {
                let rel = module_relative.unwrap_or_else(|| Path::new(""));
                if rel.as_os_str().is_empty() {
                    Some(project_root.join("target").join("test-classes"))
                } else {
                    Some(project_root.join(rel).join("target").join("test-classes"))
                }
            });

        let release = self.evaluate_scalar_best_effort(
            project_root,
            module_relative,
            "maven.compiler.release",
        )?;
        let source = self.evaluate_scalar_best_effort(
            project_root,
            module_relative,
            "maven.compiler.source",
        )?;
        let target = self.evaluate_scalar_best_effort(
            project_root,
            module_relative,
            "maven.compiler.target",
        )?;

        let enable_preview = self.evaluate_contains_best_effort(
            project_root,
            module_relative,
            "maven.compiler.compilerArgs",
            "--enable-preview",
        )? || self.evaluate_contains_best_effort(
            project_root,
            module_relative,
            "maven.compiler.compilerArgument",
            "--enable-preview",
        )?;

        // Best-effort: ensure output dirs are represented on the appropriate classpaths.
        if let Some(out_dir) = &main_output_dir {
            if !compile_classpath.iter().any(|p| p == out_dir) {
                compile_classpath.insert(0, out_dir.clone());
            }
            if !test_classpath.iter().any(|p| p == out_dir) {
                test_classpath.insert(0, out_dir.clone());
            }
        }
        if let Some(out_dir) = &test_output_dir {
            if !test_classpath.iter().any(|p| p == out_dir) {
                test_classpath.insert(0, out_dir.clone());
            }
        }

        let cfg = JavaCompileConfig {
            compile_classpath,
            test_classpath,
            module_path: Vec::new(),
            main_source_roots,
            test_source_roots,
            main_output_dir,
            test_output_dir,
            source,
            target,
            release,
            enable_preview,
        };

        cache.update_module(
            project_root,
            BuildSystemKind::Maven,
            &fingerprint,
            &module_key,
            |m| {
                m.classpath = Some(cfg.compile_classpath.clone());
                m.java_compile_config = Some(cfg.clone());
            },
        )?;

        Ok(cfg)
    }

    pub fn build(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        cache: &BuildCache,
    ) -> Result<BuildResult> {
        let fingerprint = BuildFileFingerprint::from_files(
            project_root,
            collect_maven_build_files(project_root)?,
        )?;
        let module_key = module_relative
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<root>".to_string());

        let output = self.run(project_root, module_relative, &self.config.build_args)?;
        let combined = combine_output(&output);
        let diagnostics = crate::parse_javac_diagnostics(&combined, "maven");

        cache.update_module(
            project_root,
            BuildSystemKind::Maven,
            &fingerprint,
            &module_key,
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

    fn evaluate_path_list(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        expression: &str,
    ) -> Result<Vec<PathBuf>> {
        let output = self.run_help_evaluate_raw(project_root, module_relative, expression)?;
        Ok(parse_maven_classpath_output(&output))
    }

    fn evaluate_scalar_best_effort(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        expression: &str,
    ) -> Result<Option<String>> {
        match self.run_help_evaluate_raw(project_root, module_relative, expression) {
            Ok(output) => Ok(parse_maven_evaluate_scalar_output(&output)),
            Err(BuildError::CommandFailed { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn evaluate_contains_best_effort(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        expression: &str,
        needle: &str,
    ) -> Result<bool> {
        match self.run_help_evaluate_raw(project_root, module_relative, expression) {
            Ok(output) => Ok(output.contains(needle)),
            Err(BuildError::CommandFailed { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn run_help_evaluate_raw(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        expression: &str,
    ) -> Result<String> {
        let args = self.help_evaluate_args(expression);
        let output = self.run(project_root, module_relative, &args)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "maven",
                code: output.status.code(),
                output: combine_output(&output),
            });
        }
        Ok(combine_output(&output))
    }

    fn help_evaluate_args(&self, expression: &str) -> Vec<String> {
        let mut args = self.config.classpath_args.clone();

        let expr_positions: Vec<_> = args
            .iter()
            .enumerate()
            .filter_map(|(idx, arg)| arg.starts_with("-Dexpression=").then_some(idx))
            .collect();
        if let Some(&first) = expr_positions.first() {
            args[first] = format!("-Dexpression={expression}");
            for idx in expr_positions.iter().skip(1).rev() {
                args.remove(*idx);
            }
        } else if let Some(pos) = args.iter().position(|arg| arg == "help:evaluate") {
            args.insert(pos, format!("-Dexpression={expression}"));
        } else {
            args.push(format!("-Dexpression={expression}"));
        }

        if !args.iter().any(|arg| arg == "-q") {
            args.insert(0, "-q".to_string());
        }
        if !args.iter().any(|arg| arg == "-DforceStdout") {
            let pos = args.iter().position(|arg| arg == "-q").map_or(0, |i| i + 1);
            args.insert(pos, "-DforceStdout".to_string());
        }
        if !args.iter().any(|arg| arg == "help:evaluate") {
            args.push("help:evaluate".to_string());
        }

        args
    }
}

pub fn parse_maven_classpath_output(output: &str) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut bracket_accumulator: Option<String> = None;

    // `help:evaluate` output may be noisy even with `-q`; in practice we see
    // `[INFO]` preambles, warning lines, and downloads printed ahead of the actual
    // evaluated value.
    for line in output.lines() {
        let line = line.trim();
        if let Some(mut acc) = bracket_accumulator.take() {
            if !line.is_empty() && !is_maven_noise_line(line) {
                acc.push_str(line);
            }
            if line.ends_with(']') {
                entries.extend(parse_maven_bracket_list(&acc));
            } else {
                bracket_accumulator = Some(acc);
            }
            continue;
        }
        if line.is_empty() || is_maven_noise_line(line) || is_maven_null_value(line) {
            continue;
        }

        // `help:evaluate` often returns either a bracketed list or
        // newline-separated elements. Importantly, the bracketed list may appear
        // on a single line even when the overall output contains other lines.
        if line.starts_with('[') {
            if line.ends_with(']') && line.len() >= 2 {
                entries.extend(parse_maven_bracket_list(line));
            } else {
                bracket_accumulator = Some(line.to_string());
            }
            continue;
        }

        // Some Maven invocations print a single classpath line separated by the
        // platform-specific path separator.
        let split: Vec<_> = std::env::split_paths(line).collect();
        if split.len() > 1 {
            entries.extend(split);
        } else {
            entries.push(PathBuf::from(line));
        }
    }

    // Dedupe while preserving order.
    let mut seen = std::collections::HashSet::new();
    entries.retain(|p| seen.insert(p.clone()));
    entries
}

pub fn parse_maven_evaluate_scalar_output(output: &str) -> Option<String> {
    let mut last = None;
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || is_maven_noise_line(line) || is_maven_null_value(line) {
            continue;
        }
        last = Some(line.trim_matches('"').to_string());
    }
    last
}

fn is_maven_noise_line(line: &str) -> bool {
    line.starts_with("[INFO]")
        || line.starts_with("[WARNING]")
        || line.starts_with("[ERROR]")
        || line.starts_with("[DEBUG]")
        || line.starts_with("Downloading from")
        || line.starts_with("Downloaded from")
        || line.starts_with("Progress (")
        || line.starts_with("Picked up JAVA_TOOL_OPTIONS")
        || line.starts_with("Picked up _JAVA_OPTIONS")
}

fn is_maven_null_value(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower == "null"
        || lower == "[]"
        || lower.contains("null object")
        || lower.contains("invalid expression")
}

fn parse_maven_bracket_list(line: &str) -> Vec<PathBuf> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2) {
        return Vec::new();
    }

    let mut entries = Vec::new();
    let inner = &trimmed[1..trimmed.len() - 1];
    for part in inner.split(',') {
        let s = part.trim().trim_matches('"');
        if s.is_empty() || is_maven_null_value(s) {
            continue;
        }
        entries.push(PathBuf::from(s));
    }
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

pub fn collect_maven_build_files(root: &Path) -> Result<Vec<PathBuf>> {
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

fn discover_maven_modules(root: &Path, build_files: &[PathBuf]) -> Vec<PathBuf> {
    let root_pom = root.join("pom.xml");
    let mut modules = Vec::new();
    for file in build_files {
        if file.file_name().and_then(|s| s.to_str()) != Some("pom.xml") {
            continue;
        }
        if file == &root_pom {
            continue;
        }
        let Ok(rel) = file.strip_prefix(root) else {
            continue;
        };
        let Some(dir) = rel.parent() else {
            continue;
        };
        if dir.as_os_str().is_empty() {
            continue;
        }
        modules.push(dir.to_path_buf());
    }
    modules.sort();
    modules.dedup();
    modules
}

fn collect_maven_build_files_rec(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            if file_name == ".mvn" {
                let config = path.join("maven.config");
                if config.is_file() {
                    out.push(config);
                }

                let extensions = path.join("extensions.xml");
                if extensions.is_file() {
                    out.push(extensions);
                }

                let wrapper_props = path.join("wrapper").join("maven-wrapper.properties");
                if wrapper_props.is_file() {
                    out.push(wrapper_props);
                }

                continue;
            }

            if file_name == ".git"
                || file_name == "target"
                || file_name == "build"
                || file_name == ".gradle"
                || file_name == ".nova"
                || file_name == ".idea"
            {
                continue;
            }
            collect_maven_build_files_rec(root, &path, out)?;
            continue;
        }

        if file_name == "pom.xml" || file_name == "mvnw" || file_name == "mvnw.cmd" {
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

        let modules = discover_maven_modules(&root, &files);
        let rel_modules: Vec<_> = modules
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert_eq!(rel_modules, vec!["module-a", "module-b"]);
    }

    #[test]
    fn discover_maven_modules_ignores_wrapper_and_mvn_config_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(
            root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("module-a")).unwrap();
        std::fs::write(
            root.join("module-a").join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();

        std::fs::create_dir_all(root.join(".mvn").join("wrapper")).unwrap();
        std::fs::write(root.join(".mvn").join("maven.config"), "-DskipTests\n").unwrap();
        std::fs::write(
            root.join(".mvn")
                .join("wrapper")
                .join("maven-wrapper.properties"),
            "distributionUrl=https://example.invalid/maven.zip\n",
        )
        .unwrap();

        let build_files = collect_maven_build_files(root).unwrap();
        let modules = discover_maven_modules(root, &build_files);
        assert_eq!(modules, vec![PathBuf::from("module-a")]);
    }

    #[test]
    fn help_evaluate_args_replaces_expression_in_configured_args() {
        let build = MavenBuild::new(MavenConfig::default());
        let args = build.help_evaluate_args("project.testClasspathElements");
        assert!(args.iter().any(|a| a == "-q"));
        assert!(args.iter().any(|a| a == "-DforceStdout"));
        assert!(args.iter().any(|a| a == "help:evaluate"));
        assert!(args
            .iter()
            .any(|a| a == "-Dexpression=project.testClasspathElements"));
        assert!(!args
            .iter()
            .any(|a| a == "-Dexpression=project.compileClasspathElements"));
    }

    #[test]
    fn help_evaluate_args_injects_defaults_when_missing() {
        let mut cfg = MavenConfig::default();
        cfg.classpath_args = vec!["-Pdemo".into()];
        let build = MavenBuild::new(cfg);
        let args = build.help_evaluate_args("project.compileSourceRoots");
        assert_eq!(
            args,
            vec![
                "-q",
                "-DforceStdout",
                "-Pdemo",
                "-Dexpression=project.compileSourceRoots",
                "help:evaluate"
            ]
        );
    }

    #[test]
    fn help_evaluate_args_dedupes_multiple_expression_flags() {
        let mut cfg = MavenConfig::default();
        cfg.classpath_args = vec![
            "-q".into(),
            "-DforceStdout".into(),
            "-Dexpression=foo".into(),
            "-Dexpression=bar".into(),
            "help:evaluate".into(),
        ];
        let build = MavenBuild::new(cfg);
        let args = build.help_evaluate_args("project.build.outputDirectory");
        let expr_count = args
            .iter()
            .filter(|a| a.starts_with("-Dexpression="))
            .count();
        assert_eq!(expr_count, 1);
        assert!(args
            .iter()
            .any(|a| a == "-Dexpression=project.build.outputDirectory"));
    }
}
