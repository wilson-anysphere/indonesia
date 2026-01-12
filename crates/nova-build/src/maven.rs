use crate::cache::{BuildCache, BuildFileFingerprint};
use crate::command::format_command;
use crate::jpms::{
    compiler_arg_looks_like_jpms, infer_module_path_for_compile_config,
    main_source_roots_have_module_info,
};
use crate::{
    BuildError, BuildResult, BuildSystemKind, Classpath, CommandOutput, CommandRunner,
    DefaultCommandRunner, JavaCompileConfig, MavenBuildGoal, Result,
};
use nova_build_model::{AnnotationProcessing, AnnotationProcessingConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn maven_compiler_arg_contains_enable_preview(arg: &str) -> bool {
    let arg = arg.trim();
    arg == "--enable-preview" || arg.split_whitespace().any(|tok| tok == "--enable-preview")
}

fn maven_compiler_arg_looks_like_jpms(arg: &str) -> bool {
    compiler_arg_looks_like_jpms(arg) || arg.split_whitespace().any(compiler_arg_looks_like_jpms)
}

#[derive(Debug, Clone)]
pub struct MavenConfig {
    /// Path to the Maven executable (defaults to `mvn` in `PATH`).
    pub mvn_path: PathBuf,
    /// Prefer using the Maven wrapper (`./mvnw`) when present.
    pub prefer_wrapper: bool,
    /// Arguments used to compute a module's compile classpath.
    ///
    /// Defaults to `help:evaluate` on `project.compileClasspathElements`.
    pub classpath_args: Vec<String>,
    /// Arguments used to trigger compilation (and thus produce diagnostics).
    pub build_args: Vec<String>,
    /// Arguments used to trigger test compilation (and thus annotation processing for test sources).
    pub test_build_args: Vec<String>,
    /// Whether to pass `-am` (also make) when targeting a specific module.
    pub also_make: bool,
}

impl Default for MavenConfig {
    fn default() -> Self {
        Self {
            mvn_path: PathBuf::from("mvn"),
            prefer_wrapper: true,
            classpath_args: vec![
                "-q".into(),
                "-DforceStdout".into(),
                "-Dexpression=project.compileClasspathElements".into(),
                "help:evaluate".into(),
            ],
            build_args: vec!["-q".into(), "-DskipTests".into(), "compile".into()],
            test_build_args: vec!["-q".into(), "-DskipTests".into(), "test-compile".into()],
            also_make: true,
        }
    }
}

#[derive(Debug)]
pub struct MavenBuild {
    config: MavenConfig,
    runner: Arc<dyn CommandRunner>,
}

impl MavenBuild {
    pub fn new(config: MavenConfig) -> Self {
        Self::with_runner(config, Arc::new(DefaultCommandRunner::default()))
    }

    pub fn with_runner(config: MavenConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self { config, runner }
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

        let (program, args, output) =
            self.run(project_root, module_relative, &self.config.classpath_args)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "maven",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        let module_dir = module_dir(project_root, module_relative);
        let stdout_entries = parse_maven_classpath_output(&output.stdout);
        let mut entries = if stdout_entries.is_empty() {
            parse_maven_classpath_output(&output.combined())
        } else {
            stdout_entries
        };
        entries = absolutize_paths(&module_dir, entries);

        // Best-effort: ensure the module output dir is present even if the chosen
        // classpath strategy omits it.
        let out_dir = module_dir.join("target").join("classes");
        if !entries.iter().any(|p| p == &out_dir) {
            entries.insert(0, out_dir);
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

        let module_dir = module_dir(project_root, module_relative);

        let mut compile_classpath = absolutize_paths(
            &module_dir,
            self.evaluate_path_list(
                project_root,
                module_relative,
                "project.compileClasspathElements",
            )?,
        );

        let mut test_classpath = absolutize_paths(
            &module_dir,
            self.evaluate_path_list(
                project_root,
                module_relative,
                "project.testClasspathElements",
            )?,
        );

        let mut main_source_roots = absolutize_paths(
            &module_dir,
            self.evaluate_path_list(project_root, module_relative, "project.compileSourceRoots")?,
        );

        let mut test_source_roots = {
            let roots = self.evaluate_path_list(
                project_root,
                module_relative,
                "project.testCompileSourceRoots",
            )?;
            let roots = if roots.is_empty() {
                self.evaluate_path_list(project_root, module_relative, "project.testSourceRoots")?
            } else {
                roots
            };
            absolutize_paths(&module_dir, roots)
        };

        // Maven should normally report conventional source roots via
        // `project.compileSourceRoots` / `project.testCompileSourceRoots`, but keep the config
        // resilient when the expressions are unsupported or return empty.
        if main_source_roots.is_empty() {
            let conventional = module_dir.join("src").join("main").join("java");
            if conventional.is_dir() {
                main_source_roots.push(conventional);
            }
        }
        if test_source_roots.is_empty() {
            let conventional = module_dir.join("src").join("test").join("java");
            if conventional.is_dir() {
                test_source_roots.push(conventional);
            }
        }

        let build_dir = self
            .evaluate_scalar_best_effort(project_root, module_relative, "project.build.directory")?
            .filter(|value| !value.contains("${"))
            .map(PathBuf::from)
            .map(|p| absolutize_path(&module_dir, p))
            .unwrap_or_else(|| module_dir.join("target"));

        // Best-effort heuristic: Maven codegen plugins typically write generated sources to
        // `${project.build.directory}/generated-sources/**` but these roots may not be present in
        // `project.compileSourceRoots` / `project.testCompileSourceRoots` until after the first
        // successful build.
        for root in [
            build_dir.join("generated-sources"),
            build_dir.join("generated-sources").join("annotations"),
        ] {
            if !main_source_roots.iter().any(|p| p == &root) {
                main_source_roots.push(root);
            }
        }
        for root in [
            build_dir.join("generated-test-sources"),
            build_dir
                .join("generated-test-sources")
                .join("test-annotations"),
        ] {
            if !test_source_roots.iter().any(|p| p == &root) {
                test_source_roots.push(root);
            }
        }

        let main_output_dir = self
            .evaluate_scalar_best_effort(
                project_root,
                module_relative,
                "project.build.outputDirectory",
            )?
            .map(PathBuf::from)
            .map(|p| absolutize_path(&module_dir, p))
            .or_else(|| Some(module_dir.join("target").join("classes")));

        let test_output_dir = self
            .evaluate_scalar_best_effort(
                project_root,
                module_relative,
                "project.build.testOutputDirectory",
            )?
            .map(PathBuf::from)
            .map(|p| absolutize_path(&module_dir, p))
            .or_else(|| Some(module_dir.join("target").join("test-classes")));

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

        let compiler_args_raw = self.evaluate_raw_best_effort(
            project_root,
            module_relative,
            "maven.compiler.compilerArgs",
        )?;
        let (mut enable_preview, mut compiler_args_looks_like_jpms) = compiler_args_raw
            .as_deref()
            .map(parse_maven_string_list_output)
            .map(|args| {
                let enable_preview = args
                    .iter()
                    .any(|arg| maven_compiler_arg_contains_enable_preview(arg));
                let looks_like_jpms = args
                    .iter()
                    .any(|arg| maven_compiler_arg_looks_like_jpms(arg));
                (enable_preview, looks_like_jpms)
            })
            .unwrap_or((false, false));

        if !(enable_preview && compiler_args_looks_like_jpms) {
            let compiler_argument_raw = self.evaluate_raw_best_effort(
                project_root,
                module_relative,
                "maven.compiler.compilerArgument",
            )?;
            if let Some(output) = compiler_argument_raw.as_deref() {
                let args = parse_maven_string_list_output(output);
                enable_preview |= args
                    .iter()
                    .any(|arg| maven_compiler_arg_contains_enable_preview(arg));
                compiler_args_looks_like_jpms |= args
                    .iter()
                    .any(|arg| maven_compiler_arg_looks_like_jpms(arg));
            }
        }

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

        // Match Gradle behavior by considering only stable modules
        // (`module-info.class` or `Automatic-Module-Name`) from the resolved compile classpath.
        //
        // We only evaluate Maven's `*ModulePathElements` expressions as a JPMS *signal* when we
        // can't already determine JPMS via `module-info.java` or compiler flags. This avoids extra
        // `mvn help:evaluate` invocations for the common JPMS cases.
        let module_path = if compiler_args_looks_like_jpms
            || main_source_roots_have_module_info(&main_source_roots)
        {
            infer_module_path_for_compile_config(
                &compile_classpath,
                &main_source_roots,
                main_output_dir.as_ref(),
                compiler_args_looks_like_jpms,
            )
        } else {
            // Best-effort JPMS module-path detection: Maven computes these properties when
            // configured for module-path compilation; however, older versions and some plugin
            // configurations may not expose them. We never fail the overall request if these
            // expressions are unsupported.
            let mut main_module_path = self.evaluate_path_list_best_effort(
                project_root,
                module_relative,
                "project.compileModulePathElements",
            )?;
            if main_module_path.is_empty() {
                // Some Maven versions expose `compileModulepathElements` (lowercase "p").
                main_module_path = self.evaluate_path_list_best_effort(
                    project_root,
                    module_relative,
                    "project.compileModulepathElements",
                )?;
            }

            let test_module_path = self.evaluate_path_list_best_effort(
                project_root,
                module_relative,
                "project.testCompileModulePathElements",
            )?;

            let maven_reports_module_path_elements =
                !main_module_path.is_empty() || !test_module_path.is_empty();

            if maven_reports_module_path_elements {
                infer_module_path_for_compile_config(
                    &compile_classpath,
                    &main_source_roots,
                    main_output_dir.as_ref(),
                    true,
                )
            } else {
                // Heuristic fallback: when Maven doesn't expose module-path expressions, approximate
                // the module-path with the compile classpath for JPMS projects (e.g. when a
                // `module-info.java` is present). This keeps the request resilient while still
                // allowing downstream consumers to enable module-path compilation.
                let should_infer_module_path = compiler_args_looks_like_jpms
                    || main_source_roots_have_module_info(&main_source_roots);
                if should_infer_module_path {
                    infer_module_path_for_compile_config(
                        &compile_classpath,
                        &main_source_roots,
                        main_output_dir.as_ref(),
                        compiler_args_looks_like_jpms,
                    )
                } else {
                    Vec::new()
                }
            }
        };

        let cfg = JavaCompileConfig {
            compile_classpath,
            test_classpath,
            module_path,
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
        self.build_with_goal(
            project_root,
            module_relative,
            MavenBuildGoal::Compile,
            cache,
        )
    }

    pub fn build_with_goal(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        goal: MavenBuildGoal,
        cache: &BuildCache,
    ) -> Result<BuildResult> {
        let fingerprint = BuildFileFingerprint::from_files(
            project_root,
            collect_maven_build_files(project_root)?,
        )?;
        let module_key = module_relative
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<root>".to_string());

        let goal_args = match goal {
            MavenBuildGoal::Compile => &self.config.build_args,
            MavenBuildGoal::TestCompile => &self.config.test_build_args,
        };
        let (program, args, output) = self.run(project_root, module_relative, goal_args)?;
        let command = format_command(&program, &args);
        let combined = output.combined();
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
            return Ok(BuildResult {
                diagnostics,
                tool: Some("maven".to_string()),
                command: Some(command),
                exit_code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        Err(BuildError::CommandFailed {
            tool: "maven",
            command,
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
            output_truncated: output.truncated,
        })
    }

    pub fn annotation_processing(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        cache: &BuildCache,
    ) -> Result<AnnotationProcessing> {
        let pom_files = collect_maven_build_files(project_root)?;
        let fingerprint = BuildFileFingerprint::from_files(project_root, pom_files)?;
        let module_key = module_relative
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<root>".to_string());

        if let Some(cached) = cache.get_module(
            project_root,
            BuildSystemKind::Maven,
            &fingerprint,
            &module_key,
        )? {
            if let Some(config) = cached.annotation_processing {
                return Ok(config);
            }
        }

        let module_root = module_relative
            .map(|p| project_root.join(p))
            .unwrap_or_else(|| project_root.to_path_buf());

        let effective_pom = write_temp_effective_pom_path();
        let effective_pom_args = vec![
            "-q".to_string(),
            format!("-Doutput={}", effective_pom.display()),
            "help:effective-pom".to_string(),
        ];
        let (program, args, output) =
            self.run(project_root, module_relative, &effective_pom_args)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "maven",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }

        let pom_xml = std::fs::read_to_string(&effective_pom)?;
        let _ = std::fs::remove_file(&effective_pom);

        let maven_repo = self
            .evaluate_scalar_best_effort(project_root, module_relative, "settings.localRepository")?
            .and_then(|value| resolve_maven_repo_path_best_effort(&value))
            .unwrap_or_else(default_maven_repo);

        let mut config = parse_maven_effective_pom_annotation_processing_with_repo(
            &pom_xml,
            &module_root,
            &maven_repo,
        )?;

        // Ensure defaults for generated source directories even when the effective POM does not
        // contain explicit configuration.
        if let Some(main) = config.main.as_mut() {
            if main.generated_sources_dir.is_none() {
                main.generated_sources_dir =
                    Some(module_root.join("target/generated-sources/annotations"));
            }
        }
        if let Some(test) = config.test.as_mut() {
            if test.generated_sources_dir.is_none() {
                test.generated_sources_dir =
                    Some(module_root.join("target/generated-test-sources/test-annotations"));
            }
        }

        cache.update_module(
            project_root,
            BuildSystemKind::Maven,
            &fingerprint,
            &module_key,
            |m| {
                m.annotation_processing = Some(config.clone());
            },
        )?;

        Ok(config)
    }

    fn run(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        args: &[String],
    ) -> Result<(PathBuf, Vec<String>, CommandOutput)> {
        let program = self.mvn_executable(project_root);
        let mut cmd_args: Vec<String> = Vec::new();

        if let Some(module) = module_relative {
            cmd_args.push("-pl".into());
            cmd_args.push(module.to_string_lossy().to_string());
            if self.config.also_make {
                cmd_args.push("-am".into());
            }
        }

        cmd_args.extend(args.iter().cloned());
        let output = self.runner.run(project_root, &program, &cmd_args)?;
        Ok((program, cmd_args, output))
    }

    fn mvn_executable(&self, project_root: &Path) -> PathBuf {
        if self.config.prefer_wrapper {
            #[cfg(windows)]
            {
                let wrapper = project_root.join("mvnw.cmd");
                if wrapper.exists() {
                    return wrapper;
                }
            }

            #[cfg(not(windows))]
            {
                let wrapper = project_root.join("mvnw");
                if wrapper.exists() {
                    return wrapper;
                }
            }
        }

        self.config.mvn_path.clone()
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

    fn evaluate_path_list_best_effort(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        expression: &str,
    ) -> Result<Vec<PathBuf>> {
        match self.run_help_evaluate_raw(project_root, module_relative, expression) {
            Ok(output) => Ok(parse_maven_classpath_output(&output)),
            Err(BuildError::CommandFailed { .. }) => Ok(Vec::new()),
            Err(err) => Err(err),
        }
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

    fn evaluate_raw_best_effort(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        expression: &str,
    ) -> Result<Option<String>> {
        match self.run_help_evaluate_raw(project_root, module_relative, expression) {
            Ok(output) => Ok(Some(output)),
            Err(BuildError::CommandFailed { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn run_help_evaluate_raw(
        &self,
        project_root: &Path,
        module_relative: Option<&Path>,
        expression: &str,
    ) -> Result<String> {
        let eval_args = self.help_evaluate_args(expression);
        let (program, args, output) = self.run(project_root, module_relative, &eval_args)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "maven",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                output_truncated: output.truncated,
            });
        }
        Ok(output.combined())
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

fn module_dir(project_root: &Path, module_relative: Option<&Path>) -> PathBuf {
    match module_relative {
        Some(rel) => project_root.join(rel),
        None => project_root.to_path_buf(),
    }
}

fn absolutize_paths(base_dir: &Path, paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|p| absolutize_path(base_dir, p))
        .collect()
}

fn absolutize_path(base_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}
pub fn parse_maven_effective_pom_annotation_processing(
    effective_pom_xml: &str,
    module_root: &Path,
) -> Result<AnnotationProcessing> {
    let repo = default_maven_repo();
    parse_maven_effective_pom_annotation_processing_with_repo(effective_pom_xml, module_root, &repo)
}

pub fn parse_maven_effective_pom_annotation_processing_with_repo(
    effective_pom_xml: &str,
    module_root: &Path,
    maven_repo: &Path,
) -> Result<AnnotationProcessing> {
    let doc = roxmltree::Document::parse(effective_pom_xml)
        .map_err(|err| BuildError::Parse(err.to_string()))?;
    let project = doc.root_element();

    let Some(build) = child_element(&project, "build") else {
        return Ok(AnnotationProcessing::default());
    };

    let Some(plugin) = find_maven_compiler_plugin(build) else {
        return Ok(AnnotationProcessing::default());
    };

    let mut main = AnnotationProcessingConfig {
        enabled: true,
        generated_sources_dir: Some(module_root.join("target/generated-sources/annotations")),
        ..Default::default()
    };
    let mut test = AnnotationProcessingConfig {
        enabled: true,
        generated_sources_dir: Some(
            module_root.join("target/generated-test-sources/test-annotations"),
        ),
        ..Default::default()
    };

    // Apply plugin-level config.
    if let Some(config) = child_element(&plugin, "configuration") {
        apply_maven_compiler_config(&config, module_root, maven_repo, &mut main, &mut test);
    }

    // Apply execution-level overrides.
    if let Some(executions) = child_element(&plugin, "executions") {
        for exec in executions
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("execution"))
        {
            let goals = child_element(&exec, "goals")
                .map(|goals| {
                    goals
                        .children()
                        .filter(|n| n.is_element() && n.has_tag_name("goal"))
                        .filter_map(|n| n.text())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let Some(config) = child_element(&exec, "configuration") else {
                continue;
            };

            if goals.iter().any(|g| g == "compile") {
                apply_maven_compiler_config(&config, module_root, maven_repo, &mut main, &mut test);
            }
            if goals.iter().any(|g| g == "testCompile") {
                // Execution config uses the same keys, but we treat it as test-specific.
                // Use a dummy "main" config to avoid double-borrowing `test`.
                let mut dummy_main = AnnotationProcessingConfig::default();
                apply_maven_compiler_config(
                    &config,
                    module_root,
                    maven_repo,
                    &mut dummy_main,
                    &mut test,
                );
            }
        }
    }

    augment_from_compiler_args(&mut main);
    augment_from_compiler_args(&mut test);

    Ok(AnnotationProcessing {
        main: Some(main),
        test: Some(test),
    })
}

fn write_temp_effective_pom_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("nova_maven_effective_{token}.xml"));
    path
}

fn apply_maven_compiler_config(
    config: &roxmltree::Node<'_, '_>,
    module_root: &Path,
    maven_repo: &Path,
    main: &mut AnnotationProcessingConfig,
    test: &mut AnnotationProcessingConfig,
) {
    if let Some(proc_mode) = child_text(config, "proc") {
        let mode = proc_mode.trim().to_ascii_lowercase();
        if mode == "none" {
            main.enabled = false;
            test.enabled = false;
        } else {
            main.enabled = true;
            test.enabled = true;
        }
    }

    if let Some(dir) = child_text(config, "generatedSourcesDirectory")
        .and_then(|v| resolve_maven_path(&v, module_root))
    {
        main.generated_sources_dir = Some(dir);
    }
    if let Some(dir) = child_text(config, "generatedTestSourcesDirectory")
        .and_then(|v| resolve_maven_path(&v, module_root))
    {
        test.generated_sources_dir = Some(dir);
    }

    if let Some(args) = child_element(config, "compilerArgs") {
        for arg in args
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("arg"))
            .filter_map(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            main.compiler_args.push(arg.to_string());
            test.compiler_args.push(arg.to_string());
        }
    }

    if let Some(processors) = child_element(config, "annotationProcessors") {
        // Maven's compiler plugin supports either a comma-separated string or nested elements.
        let mut extracted = Vec::new();
        for child in processors.children().filter(|n| n.is_element()) {
            if let Some(text) = child.text().map(str::trim).filter(|s| !s.is_empty()) {
                extracted.push(text.to_string());
            }
        }
        if extracted.is_empty() {
            if let Some(text) = processors.text().map(str::trim).filter(|s| !s.is_empty()) {
                extracted.extend(
                    text.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
        }
        main.processors.extend(extracted.iter().cloned());
        test.processors.extend(extracted);
    }

    if let Some(paths) = child_element(config, "annotationProcessorPaths") {
        for path in paths
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("path"))
        {
            let Some(group_id) = child_text(&path, "groupId") else {
                continue;
            };
            let Some(artifact_id) = child_text(&path, "artifactId") else {
                continue;
            };
            let Some(version) = child_text(&path, "version") else {
                continue;
            };
            if version.contains("${") {
                continue;
            }
            let classifier = child_text(&path, "classifier");
            let type_ = child_text(&path, "type").unwrap_or_else(|| "jar".to_string());
            if type_ != "jar" {
                continue;
            }

            if let Some(jar) = maven_jar_path(
                maven_repo,
                &group_id,
                &artifact_id,
                &version,
                classifier.as_deref(),
            ) {
                main.processor_path.push(jar.clone());
                test.processor_path.push(jar);
            }
        }
    }
}

fn augment_from_compiler_args(config: &mut AnnotationProcessingConfig) {
    let mut proc_mode = None::<String>;
    let mut it = config.compiler_args.clone().into_iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-processor" => {
                if let Some(value) = it.next() {
                    config.processors.extend(
                        value
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                }
            }
            "-processorpath" | "--processor-path" => {
                if let Some(value) = it.next() {
                    config.processor_path.extend(split_path_list(&value));
                }
            }
            "-s" => {
                if let Some(value) = it.next() {
                    config.generated_sources_dir = Some(PathBuf::from(value));
                }
            }
            other if other.starts_with("-proc:") => {
                proc_mode = Some(other.trim_start_matches("-proc:").to_string());
            }
            other if other.starts_with("-A") => {
                let rest = other.trim_start_matches("-A");
                let (k, v) = rest.split_once('=').unwrap_or((rest, ""));
                if !k.is_empty() {
                    config.options.insert(k.to_string(), v.to_string());
                }
            }
            _ => {}
        }
    }

    config.enabled = match proc_mode.as_deref() {
        Some("none") => false,
        Some(_) => true,
        None => config.enabled,
    };

    let mut seen_processors = std::collections::HashSet::new();
    config
        .processors
        .retain(|p| seen_processors.insert(p.clone()));

    let mut seen_paths = std::collections::HashSet::new();
    config
        .processor_path
        .retain(|p| seen_paths.insert(p.clone()));
}

fn resolve_maven_path(value: &str, module_root: &Path) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() || value.contains("${") {
        return None;
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(module_root.join(path))
    }
}

fn find_maven_compiler_plugin<'a, 'i>(
    build: roxmltree::Node<'a, 'i>,
) -> Option<roxmltree::Node<'a, 'i>> {
    // Prefer `<build><plugins>`; fall back to pluginManagement if needed.
    if let Some(plugins) = child_element(&build, "plugins") {
        if let Some(plugin) = plugins.children().find(|n| {
            n.is_element()
                && n.has_tag_name("plugin")
                && child_text(n, "artifactId").as_deref() == Some("maven-compiler-plugin")
        }) {
            return Some(plugin);
        }
    }

    if let Some(pm) = child_element(&build, "pluginManagement") {
        if let Some(plugins) = child_element(&pm, "plugins") {
            if let Some(plugin) = plugins.children().find(|n| {
                n.is_element()
                    && n.has_tag_name("plugin")
                    && child_text(n, "artifactId").as_deref() == Some("maven-compiler-plugin")
            }) {
                return Some(plugin);
            }
        }
    }

    None
}

fn default_maven_repo() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".m2/repository")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn resolve_maven_repo_path_best_effort(value: &str) -> Option<PathBuf> {
    let value = value.trim().trim_matches(|c| matches!(c, '"' | '\'')).trim();
    if value.is_empty() {
        return None;
    }

    if let Some(path) = expand_maven_tilde_home(value) {
        return Some(path);
    }
    if let Some(path) = expand_maven_env_placeholder(value) {
        return Some(path);
    }
    if let Some(path) = expand_maven_user_home_placeholder(value) {
        return Some(path);
    }

    // If there are any remaining placeholders, bail out rather than guessing.
    if value.contains("${") {
        return None;
    }

    Some(PathBuf::from(value))
}

fn expand_maven_tilde_home(value: &str) -> Option<PathBuf> {
    let rest = value.strip_prefix('~')?;
    let home = home_dir()?;

    if rest.is_empty() {
        return Some(home);
    }

    // Only expand `~/...` (or `~\\...` on Windows). Don't guess for `~user/...`.
    let rest = rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\'))?;
    if rest.contains("${") {
        return None;
    }

    Some(home.join(rest))
}

fn expand_maven_user_home_placeholder(value: &str) -> Option<PathBuf> {
    const USER_HOME: &str = "${user.home}";
    let rest = value.strip_prefix(USER_HOME)?;

    let home = home_dir()?;
    if rest.is_empty() {
        return Some(home);
    }

    // Accept both separators so configs remain portable.
    let rest = rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\')).unwrap_or(rest);
    if rest.contains("${") {
        // If there are any remaining placeholders, bail out rather than guessing.
        return None;
    }

    Some(home.join(rest))
}

fn expand_maven_env_placeholder(value: &str) -> Option<PathBuf> {
    const PREFIX: &str = "${env.";
    let rest = value.strip_prefix(PREFIX)?;
    let (raw_key, rest) = rest.split_once('}')?;
    let key = raw_key.trim();
    if key.is_empty() {
        return None;
    }

    let base = PathBuf::from(std::env::var_os(key)?);
    if rest.is_empty() {
        return Some(base);
    }

    // Accept both separators so configs remain portable.
    let rest = rest
        .strip_prefix('/')
        .or_else(|| rest.strip_prefix('\\'))
        .unwrap_or(rest);
    if rest.contains("${") {
        return None;
    }

    Some(base.join(rest))
}

pub fn maven_jar_path(
    repo: &Path,
    group_id: &str,
    artifact_id: &str,
    version: &str,
    classifier: Option<&str>,
) -> Option<PathBuf> {
    let group_path = group_id.replace('.', "/");
    let base = repo.join(group_path).join(artifact_id).join(version);

    let classifier = classifier.map(str::trim).filter(|c| !c.is_empty());

    if version.ends_with("-SNAPSHOT") {
        if let Some(path) = resolve_snapshot_maven_jar_path(&base, artifact_id, classifier) {
            return Some(path);
        }
    }

    Some(base.join(maven_jar_file_name(artifact_id, version, classifier)))
}

fn maven_jar_file_name(artifact_id: &str, version: &str, classifier: Option<&str>) -> String {
    if let Some(classifier) = classifier {
        format!("{artifact_id}-{version}-{classifier}.jar")
    } else {
        format!("{artifact_id}-{version}.jar")
    }
}

fn resolve_snapshot_maven_jar_path(
    version_dir: &Path,
    artifact_id: &str,
    classifier: Option<&str>,
) -> Option<PathBuf> {
    let value = snapshot_jar_value_from_local_metadata(version_dir, classifier)?;
    let file_name = if let Some(classifier) = classifier {
        format!("{artifact_id}-{value}-{classifier}.jar")
    } else {
        format!("{artifact_id}-{value}.jar")
    };
    let path = version_dir.join(file_name);

    // Best-effort: ensure we don't hand out paths to jars that aren't present.
    path.is_file().then_some(path)
}

fn snapshot_jar_value_from_local_metadata(
    version_dir: &Path,
    classifier: Option<&str>,
) -> Option<String> {
    let mut best: Option<String> = None;

    // Maven writes metadata files like:
    // - `maven-metadata-local.xml` (installed snapshots)
    // - `maven-metadata.xml` (downloaded snapshots)
    for file in ["maven-metadata-local.xml", "maven-metadata.xml"] {
        let path = version_dir.join(file);
        let Ok(xml) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = roxmltree::Document::parse(&xml) else {
            continue;
        };

        for sv in doc
            .descendants()
            .filter(|n| n.is_element() && n.has_tag_name("snapshotVersion"))
        {
            let ext = child_text(&sv, "extension");
            if ext.as_deref() != Some("jar") {
                continue;
            }

            let sv_classifier = child_text(&sv, "classifier");
            let sv_classifier = sv_classifier
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty());
            if sv_classifier != classifier {
                continue;
            }

            let Some(value) = child_text(&sv, "value") else {
                continue;
            };
            match &best {
                None => best = Some(value),
                Some(current) if value > *current => best = Some(value),
                _ => {}
            }
        }
    }

    best
}

fn split_path_list(value: &str) -> Vec<PathBuf> {
    let sep = if value.contains(';') { ';' } else { ':' };
    value
        .split(sep)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn child_element<'a, 'i>(
    node: &roxmltree::Node<'a, 'i>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

fn child_text<'a, 'i>(node: &roxmltree::Node<'a, 'i>, name: &str) -> Option<String> {
    child_element(node, name)
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

pub fn parse_maven_classpath_output(output: &str) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut bracket_accumulator: Option<String> = None;

    // `help:evaluate` output may be noisy even with `-q`; in practice we see
    // `[INFO]` preambles, warning lines, and downloads printed ahead of the actual
    // evaluated value.
    for line in output.lines() {
        let line = line.trim();
        let line = line.trim_matches(|c| matches!(c, '"' | '\'')).trim();
        if let Some(mut acc) = bracket_accumulator.take() {
            if !line.is_empty() && !is_maven_noise_line(line) && !is_maven_null_value(line) {
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
        let candidate = line.trim_matches(|c| matches!(c, '"' | '\'')).trim();
        // `help:evaluate` may print a bracketed list when the expression resolves to a list.
        // Treat that as invalid for scalar extraction so callers can fall back to defaults.
        if candidate.starts_with('[') {
            continue;
        }
        last = Some(candidate.to_string());
    }
    last
}

fn parse_maven_string_list_output(output: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut bracket_accumulator: Option<String> = None;

    // `help:evaluate` output may be noisy even with `-q`; in practice we see
    // `[INFO]` preambles, warning lines, and downloads printed ahead of the actual
    // evaluated value.
    for line in output.lines() {
        let line = line.trim();
        let line = line.trim_matches(|c| matches!(c, '"' | '\'')).trim();
        if let Some(mut acc) = bracket_accumulator.take() {
            if !line.is_empty() && !is_maven_noise_line(line) && !is_maven_null_value(line) {
                acc.push_str(line);
            }
            if line.ends_with(']') {
                entries.extend(parse_maven_bracket_string_list(&acc));
            } else {
                bracket_accumulator = Some(acc);
            }
            continue;
        }

        if line.is_empty() || is_maven_noise_line(line) || is_maven_null_value(line) {
            continue;
        }

        if line.starts_with('[') {
            if line.ends_with(']') && line.len() >= 2 {
                entries.extend(parse_maven_bracket_string_list(line));
            } else {
                bracket_accumulator = Some(line.to_string());
            }
            continue;
        }

        let s = line.trim_matches('"').trim_matches('\'');
        if s.is_empty() || is_maven_null_value(s) {
            continue;
        }
        entries.push(s.to_string());
    }

    // Dedupe while preserving order.
    let mut seen = std::collections::HashSet::new();
    entries.retain(|s| seen.insert(s.clone()));
    entries
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

fn parse_maven_bracket_string_list(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2) {
        return Vec::new();
    }

    let mut entries = Vec::new();
    let inner = &trimmed[1..trimmed.len() - 1];
    for part in inner.split(',') {
        let s = part.trim().trim_matches('"').trim_matches('\'');
        if s.is_empty() || is_maven_null_value(s) {
            continue;
        }
        entries.push(s.to_string());
    }
    entries
}

fn parse_maven_bracket_list(line: &str) -> Vec<PathBuf> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2) {
        return Vec::new();
    }

    let mut entries = Vec::new();
    let inner = &trimmed[1..trimmed.len() - 1];
    for part in inner.split(',') {
        let s = part.trim().trim_matches(|c| matches!(c, '"' | '\'')).trim();
        if s.is_empty() || is_maven_null_value(s) {
            continue;
        }
        entries.push(PathBuf::from(s));
    }
    entries
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

fn discover_maven_modules(root: &Path, _build_files: &[PathBuf]) -> Vec<PathBuf> {
    fn parse_modules(pom_xml: &str) -> Vec<PathBuf> {
        let Ok(doc) = roxmltree::Document::parse(pom_xml) else {
            return Vec::new();
        };
        let project = doc.root_element();
        let Some(modules) = child_element(&project, "modules") else {
            return Vec::new();
        };

        modules
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "module")
            .filter_map(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            // Best-effort: skip module paths that require property interpolation.
            .filter(|s| !s.contains("${"))
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty() && !p.is_absolute())
            .collect()
    }

    let root_pom = root.join("pom.xml");
    let Ok(root_pom_xml) = std::fs::read_to_string(&root_pom) else {
        return Vec::new();
    };
    let root_modules = parse_modules(&root_pom_xml);
    if root_modules.is_empty() {
        return Vec::new();
    }

    // Recursively walk aggregator modules by reading each module's `pom.xml` and parsing
    // its `<modules>` list. Missing/invalid child POMs are ignored best-effort.
    let mut out = Vec::new();
    let mut stack = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for module in root_modules {
        if seen.insert(module.clone()) {
            stack.push(module.clone());
            out.push(module);
        }
    }

    while let Some(parent_rel) = stack.pop() {
        let child_pom = root.join(&parent_rel).join("pom.xml");
        let Ok(child_xml) = std::fs::read_to_string(&child_pom) else {
            continue;
        };
        let child_modules = parse_modules(&child_xml);
        for child in child_modules {
            let rel = parent_rel.join(child);
            if rel.as_os_str().is_empty() {
                continue;
            }
            if seen.insert(rel.clone()) {
                stack.push(rel.clone());
                out.push(rel);
            }
        }
    }

    // Stable sort for deterministic cache keys / tests.
    out.sort();
    out.dedup();
    out
}

fn collect_maven_build_files_rec(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            // Avoid scanning huge non-source directories that commonly show up in mono-repos.
            // These trees can contain many files that look like build files but should not
            // influence Nova's build fingerprint (e.g. vendored JS dependencies).
            if file_name == "node_modules" {
                continue;
            }
            // Bazel output trees are typically created at the workspace root and can be enormous.
            // Skip any `bazel-*` entries (`bazel-out`, `bazel-bin`, `bazel-testlogs`,
            // `bazel-<workspace>`, etc).
            if file_name.starts_with("bazel-") {
                continue;
            }

            if file_name == ".mvn" {
                let config = path.join("maven.config");
                if config.is_file() {
                    out.push(config);
                }

                let jvm_config = path.join("jvm.config");
                if jvm_config.is_file() {
                    out.push(jvm_config);
                }

                let extensions = path.join("extensions.xml");
                if extensions.is_file() {
                    out.push(extensions);
                }

                let wrapper_props = path.join("wrapper").join("maven-wrapper.properties");
                if wrapper_props.is_file() {
                    out.push(wrapper_props);
                }

                let wrapper_jar = path.join("wrapper").join("maven-wrapper.jar");
                if wrapper_jar.is_file() {
                    out.push(wrapper_jar);
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
    use std::ffi::OsString;
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::process::ExitStatus;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvVarGuard {
        key: &'static str,
        prior: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: Option<&std::path::Path>) -> Self {
            let prior = std::env::var_os(key);
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            Self { key, prior }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn with_home_dir<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned");

        // Set both to behave deterministically on Windows/Linux.
        let _home = EnvVarGuard::set("HOME", Some(home));
        let _userprofile = EnvVarGuard::set("USERPROFILE", Some(home));

        f()
    }

    #[test]
    fn parse_maven_classpath_output_strips_quotes_from_bracket_list_entries() {
        let output = "['a.jar', 'b.jar']\n";
        let parsed = parse_maven_classpath_output(output);
        assert_eq!(parsed, vec![PathBuf::from("a.jar"), PathBuf::from("b.jar")]);
    }

    #[test]
    fn parse_maven_classpath_output_strips_quotes_from_path_separated_list() {
        let expected = vec![PathBuf::from("a.jar"), PathBuf::from("b.jar")];
        let joined = std::env::join_paths(&expected)
            .expect("join paths")
            .to_string_lossy()
            .to_string();
        let output = format!("\"{}\"\n", joined);
        let parsed = parse_maven_classpath_output(&output);
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_maven_evaluate_scalar_output_strips_single_quotes() {
        let output = "[INFO] noise\n'/tmp/repo'\n";
        assert_eq!(
            parse_maven_evaluate_scalar_output(output),
            Some("/tmp/repo".to_string())
        );
    }

    #[test]
    fn resolve_maven_repo_path_best_effort_expands_user_home_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");

        with_home_dir(&home, || {
            let repo = resolve_maven_repo_path_best_effort("${user.home}/.m2/custom-repo")
                .expect("repo");
            assert_eq!(repo, home.join(".m2").join("custom-repo"));
        });
    }

    #[test]
    fn resolve_maven_repo_path_best_effort_expands_tilde_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");

        with_home_dir(&home, || {
            let repo =
                resolve_maven_repo_path_best_effort("~/.m2/custom-repo").expect("repo");
            assert_eq!(repo, home.join(".m2").join("custom-repo"));
        });
    }

    #[test]
    fn resolve_maven_repo_path_best_effort_expands_env_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let repo_root = tmp.path().join("repo-root");
        std::fs::create_dir_all(&repo_root).expect("create repo root");

        with_home_dir(&home, || {
            let _guard = EnvVarGuard::set("M2_REPO_ROOT", Some(&repo_root));
            let repo = resolve_maven_repo_path_best_effort("${env.M2_REPO_ROOT}/m2")
                .expect("repo");
            assert_eq!(repo, repo_root.join("m2"));
        });
    }

    #[test]
    fn resolve_maven_repo_path_best_effort_rejects_unknown_placeholders() {
        assert!(
            resolve_maven_repo_path_best_effort("${something}/repo").is_none(),
            "unknown placeholders should be rejected so callers can fall back to defaults"
        );
    }

    #[test]
    fn parse_maven_string_list_output_accepts_quoted_bracket_list() {
        let output = "\"[--enable-preview, -Xlint:all]\"\n";
        assert_eq!(
            parse_maven_string_list_output(output),
            vec!["--enable-preview".to_string(), "-Xlint:all".to_string()]
        );
    }

    #[test]
    fn collects_poms_for_multi_module_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/maven-multi");
        let files = collect_maven_build_files(&root).unwrap();
        let mut rel: Vec<_> = files
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        rel.sort();
        assert_eq!(rel, vec!["module-a/pom.xml", "module-b/pom.xml", "pom.xml"]);

        let modules = discover_maven_modules(&root, &files);
        let rel_modules: Vec<_> = modules
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(rel_modules, vec!["module-a", "module-b"]);
    }

    #[test]
    fn collect_maven_build_files_ignores_bazel_dirs_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Real build files.
        std::fs::write(root.join("pom.xml"), "<project></project>").unwrap();
        std::fs::write(root.join("mvnw"), "echo mvnw").unwrap();
        std::fs::write(root.join("mvnw.cmd"), "@echo mvnw").unwrap();
        std::fs::create_dir_all(root.join("module-a")).unwrap();
        std::fs::write(root.join("module-a").join("pom.xml"), "<project></project>").unwrap();

        // Bazel output trees can contain files that look like build markers, but they should not
        // influence Maven fingerprinting.
        std::fs::create_dir_all(root.join("bazel-out")).unwrap();
        std::fs::write(
            root.join("bazel-out").join("pom.xml"),
            "<project></project>",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("nested").join("bazel-out")).unwrap();
        std::fs::write(
            root.join("nested").join("bazel-out").join("pom.xml"),
            "<project></project>",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("nested").join("bazel-myworkspace")).unwrap();
        std::fs::write(
            root.join("nested")
                .join("bazel-myworkspace")
                .join("pom.xml"),
            "<project></project>",
        )
        .unwrap();

        let files = collect_maven_build_files(root).unwrap();
        let rel: BTreeSet<PathBuf> = files
            .into_iter()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        let expected: BTreeSet<PathBuf> = [
            PathBuf::from("module-a/pom.xml"),
            PathBuf::from("mvnw"),
            PathBuf::from("mvnw.cmd"),
            PathBuf::from("pom.xml"),
        ]
        .into_iter()
        .collect();

        assert_eq!(rel, expected);
    }

    #[test]
    fn discover_maven_modules_ignores_wrapper_and_mvn_config_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(
            root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion><modules><module>module-a</module></modules></project>",
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
    fn discover_maven_modules_ignores_unlisted_poms() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(
            root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion><modules><module>module-a</module></modules></project>",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("module-a")).unwrap();
        std::fs::write(
            root.join("module-a").join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();

        // Not listed in `<modules>`, should not be treated as part of the reactor.
        std::fs::create_dir_all(root.join("unrelated")).unwrap();
        std::fs::write(
            root.join("unrelated").join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
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
        let cfg = MavenConfig {
            classpath_args: vec!["-Pdemo".into()],
            ..MavenConfig::default()
        };
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
        let cfg = MavenConfig {
            classpath_args: vec![
                "-q".into(),
                "-DforceStdout".into(),
                "-Dexpression=foo".into(),
                "-Dexpression=bar".into(),
                "help:evaluate".into(),
            ],
            ..MavenConfig::default()
        };
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

    #[test]
    fn mvn_executable_prefers_wrapper_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let wrapper_name = if cfg!(windows) { "mvnw.cmd" } else { "mvnw" };
        std::fs::write(root.join(wrapper_name), "echo mvn").unwrap();

        let build = MavenBuild::new(MavenConfig::default());
        assert_eq!(build.mvn_executable(root), root.join(wrapper_name));
    }

    #[test]
    fn mvn_executable_falls_back_to_mvn_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let cfg = MavenConfig {
            mvn_path: PathBuf::from("/custom/mvn"),
            ..MavenConfig::default()
        };

        let build = MavenBuild::new(cfg.clone());
        assert_eq!(build.mvn_executable(root), cfg.mvn_path);
    }

    #[test]
    fn infer_module_path_includes_only_stable_modules_and_excludes_output_dir() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let tmp = tempfile::tempdir().unwrap();
        let main_src_root = tmp.path().join("src/main/java");
        std::fs::create_dir_all(&main_src_root).unwrap();
        std::fs::write(
            main_src_root.join("module-info.java"),
            "module example.mod {}",
        )
        .unwrap();

        // Simulate a stable module output dir. Even for JPMS projects, output directories should
        // live on the compile classpath (not the inferred module-path).
        let out_dir = tmp.path().join("target/classes");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("module-info.class"), b"").unwrap();

        let resolved_compile_classpath = vec![
            out_dir.clone(),
            named.clone(),
            automatic.clone(),
            dep.clone(),
        ];
        let module_path = infer_module_path_for_compile_config(
            &resolved_compile_classpath,
            &[main_src_root],
            Some(&out_dir),
            false,
        );

        assert_eq!(module_path, vec![named.clone(), automatic.clone()]);
        assert!(
            !module_path.contains(&out_dir),
            "output directories should not be included on the inferred module-path"
        );
        assert!(
            !module_path.contains(&dep),
            "classpath-only dependencies without module metadata should stay off the module-path"
        );
    }

    #[test]
    fn infer_module_path_is_empty_without_module_info_or_jpms_args() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");

        let tmp = tempfile::tempdir().unwrap();
        let main_src_root = tmp.path().join("src/main/java");
        std::fs::create_dir_all(&main_src_root).unwrap();

        let resolved_compile_classpath = vec![named, automatic];
        let module_path = infer_module_path_for_compile_config(
            &resolved_compile_classpath,
            &[main_src_root],
            None,
            false,
        );

        assert!(module_path.is_empty());
    }

    #[test]
    fn infer_module_path_is_enabled_by_jpms_args() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let tmp = tempfile::tempdir().unwrap();
        let main_src_root = tmp.path().join("src/main/java");
        std::fs::create_dir_all(&main_src_root).unwrap();

        let resolved_compile_classpath = vec![named.clone(), dep, automatic.clone()];
        let module_path = infer_module_path_for_compile_config(
            &resolved_compile_classpath,
            &[main_src_root],
            None,
            true,
        );

        assert_eq!(module_path, vec![named, automatic]);
    }

    #[derive(Debug)]
    struct StaticMavenRunner {
        invocations: Mutex<Vec<Vec<String>>>,
        outputs_by_expression: HashMap<String, String>,
    }

    impl StaticMavenRunner {
        fn new(outputs_by_expression: HashMap<String, String>) -> Self {
            Self {
                invocations: Mutex::new(Vec::new()),
                outputs_by_expression,
            }
        }

        fn invocations(&self) -> Vec<Vec<String>> {
            self.invocations.lock().expect("lock poisoned").clone()
        }
    }

    fn exit_status(code: i32) -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(code << 8)
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code as u32)
        }
    }

    impl CommandRunner for StaticMavenRunner {
        fn run(
            &self,
            _cwd: &Path,
            _program: &Path,
            args: &[String],
        ) -> std::io::Result<CommandOutput> {
            self.invocations
                .lock()
                .expect("lock poisoned")
                .push(args.to_vec());

            let expression = args
                .iter()
                .find_map(|arg| arg.strip_prefix("-Dexpression="))
                .unwrap_or("");
            let stdout = self
                .outputs_by_expression
                .get(expression)
                .cloned()
                .unwrap_or_else(|| "null\n".to_string());

            Ok(CommandOutput {
                status: exit_status(0),
                stdout,
                stderr: String::new(),
                truncated: false,
            })
        }
    }

    #[test]
    fn java_compile_config_infers_module_path_when_maven_reports_module_path_elements() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        // Minimal POM so we can fingerprint and cache.
        std::fs::write(
            project_root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();

        // Create conventional source roots, but do NOT create `module-info.java` so the primary
        // JPMS heuristics don't trigger.
        std::fs::create_dir_all(project_root.join("src/main/java")).unwrap();
        std::fs::create_dir_all(project_root.join("src/test/java")).unwrap();

        // Create an output dir that *looks* like a named module, and ensure we still exclude it
        // from the module-path.
        let out_dir = project_root.join("target/classes");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("module-info.class"), b"cafebabe").unwrap();

        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let mut outputs = HashMap::new();
        outputs.insert(
            "project.compileClasspathElements".to_string(),
            format!(
                "[{},{},{}]\n",
                named.display(),
                automatic.display(),
                dep.display()
            ),
        );
        outputs.insert(
            "project.testClasspathElements".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.compileSourceRoots".to_string(), "[]\n".to_string());
        outputs.insert(
            "project.testCompileSourceRoots".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.testSourceRoots".to_string(), "[]\n".to_string());

        // JPMS signal: Maven exposes `*ModulePathElements` even though we have no module-info.java
        // and no compiler flags.
        outputs.insert(
            "project.compileModulePathElements".to_string(),
            format!("[{}]\n", named.display()),
        );
        outputs.insert(
            "project.testCompileModulePathElements".to_string(),
            "[]\n".to_string(),
        );

        let runner = Arc::new(StaticMavenRunner::new(outputs));
        let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());
        let cache = BuildCache::new(tmp.path().join("cache"));

        let cfg = build
            .java_compile_config(&project_root, None, &cache)
            .unwrap();

        assert_eq!(cfg.module_path, vec![named, automatic]);
        assert!(!cfg.module_path.contains(&out_dir));

        // Sanity check: we evaluated `compileModulePathElements` (since module-info and compiler
        // args didn't indicate JPMS).
        assert!(runner.invocations().iter().any(|args| args
            .iter()
            .any(|a| a == "-Dexpression=project.compileModulePathElements")));
    }

    #[test]
    fn java_compile_config_is_cached_including_module_path() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        std::fs::write(
            project_root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();

        std::fs::create_dir_all(project_root.join("src/main/java")).unwrap();
        std::fs::create_dir_all(project_root.join("src/test/java")).unwrap();

        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let mut outputs = HashMap::new();
        outputs.insert(
            "project.compileClasspathElements".to_string(),
            format!(
                "[{},{},{}]\n",
                named.display(),
                automatic.display(),
                dep.display()
            ),
        );
        outputs.insert(
            "project.testClasspathElements".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.compileSourceRoots".to_string(), "[]\n".to_string());
        outputs.insert(
            "project.testCompileSourceRoots".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.testSourceRoots".to_string(), "[]\n".to_string());

        outputs.insert(
            "project.compileModulePathElements".to_string(),
            format!("[{}]\n", named.display()),
        );
        outputs.insert(
            "project.testCompileModulePathElements".to_string(),
            "[]\n".to_string(),
        );

        let runner = Arc::new(StaticMavenRunner::new(outputs));
        let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());
        let cache = BuildCache::new(tmp.path().join("cache"));

        let cfg1 = build
            .java_compile_config(&project_root, None, &cache)
            .unwrap();
        assert_eq!(cfg1.module_path, vec![named, automatic]);

        let invocations_first = runner.invocations();
        assert!(!invocations_first.is_empty());

        let cfg2 = build
            .java_compile_config(&project_root, None, &cache)
            .unwrap();
        assert_eq!(cfg2, cfg1);

        let invocations_second = runner.invocations();
        assert_eq!(invocations_second.len(), invocations_first.len());
    }

    #[test]
    fn java_compile_config_skips_module_path_elements_when_module_info_present() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        std::fs::write(
            project_root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();

        let src_main = project_root.join("src/main/java");
        std::fs::create_dir_all(&src_main).unwrap();
        std::fs::write(src_main.join("module-info.java"), "module example.mod {}").unwrap();

        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let mut outputs = HashMap::new();
        outputs.insert(
            "project.compileClasspathElements".to_string(),
            format!(
                "[{},{},{}]\n",
                named.display(),
                automatic.display(),
                dep.display()
            ),
        );
        outputs.insert(
            "project.testClasspathElements".to_string(),
            "[]\n".to_string(),
        );

        // Make `project.compileSourceRoots` empty so Nova falls back to conventional `src/main/java`.
        outputs.insert("project.compileSourceRoots".to_string(), "[]\n".to_string());
        outputs.insert(
            "project.testCompileSourceRoots".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.testSourceRoots".to_string(), "[]\n".to_string());

        let runner = Arc::new(StaticMavenRunner::new(outputs));
        let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());
        let cache = BuildCache::new(tmp.path().join("cache"));

        let cfg = build
            .java_compile_config(&project_root, None, &cache)
            .unwrap();

        assert_eq!(cfg.module_path, vec![named, automatic]);

        // Since module-info.java is present, we should not need to evaluate Maven's module path
        // element expressions.
        assert!(!runner.invocations().iter().any(|args| {
            args.iter()
                .any(|a| a == "-Dexpression=project.compileModulePathElements")
                || args
                    .iter()
                    .any(|a| a == "-Dexpression=project.compileModulepathElements")
                || args
                    .iter()
                    .any(|a| a == "-Dexpression=project.testCompileModulePathElements")
        }));
    }

    #[test]
    fn java_compile_config_skips_module_path_elements_when_jpms_flags_present() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        std::fs::write(
            project_root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();

        std::fs::create_dir_all(project_root.join("src/main/java")).unwrap();

        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let mut outputs = HashMap::new();
        outputs.insert(
            "project.compileClasspathElements".to_string(),
            format!(
                "[{},{},{}]\n",
                named.display(),
                automatic.display(),
                dep.display()
            ),
        );
        outputs.insert(
            "project.testClasspathElements".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.compileSourceRoots".to_string(), "[]\n".to_string());
        outputs.insert(
            "project.testCompileSourceRoots".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.testSourceRoots".to_string(), "[]\n".to_string());

        // JPMS signal via compiler args.
        outputs.insert(
            "maven.compiler.compilerArgs".to_string(),
            "[-p]\n".to_string(),
        );

        let runner = Arc::new(StaticMavenRunner::new(outputs));
        let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());
        let cache = BuildCache::new(tmp.path().join("cache"));

        let cfg = build
            .java_compile_config(&project_root, None, &cache)
            .unwrap();

        assert_eq!(cfg.module_path, vec![named, automatic]);

        // Since JPMS flags are present, we should not need to evaluate Maven's module path element
        // expressions.
        assert!(!runner.invocations().iter().any(|args| {
            args.iter()
                .any(|a| a == "-Dexpression=project.compileModulePathElements")
                || args
                    .iter()
                    .any(|a| a == "-Dexpression=project.compileModulepathElements")
                || args
                    .iter()
                    .any(|a| a == "-Dexpression=project.testCompileModulePathElements")
        }));
    }

    #[test]
    fn java_compile_config_detects_jpms_flags_in_compiler_argument_string() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        std::fs::write(
            project_root.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion></project>",
        )
        .unwrap();

        std::fs::create_dir_all(project_root.join("src/main/java")).unwrap();

        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        let mut outputs = HashMap::new();
        outputs.insert(
            "project.compileClasspathElements".to_string(),
            format!(
                "[{},{},{}]\n",
                named.display(),
                automatic.display(),
                dep.display()
            ),
        );
        outputs.insert(
            "project.testClasspathElements".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.compileSourceRoots".to_string(), "[]\n".to_string());
        outputs.insert(
            "project.testCompileSourceRoots".to_string(),
            "[]\n".to_string(),
        );
        outputs.insert("project.testSourceRoots".to_string(), "[]\n".to_string());

        // JPMS signal via a single string containing multiple args separated by whitespace.
        outputs.insert(
            "maven.compiler.compilerArgument".to_string(),
            "--module-path /tmp\n".to_string(),
        );

        let runner = Arc::new(StaticMavenRunner::new(outputs));
        let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());
        let cache = BuildCache::new(tmp.path().join("cache"));

        let cfg = build
            .java_compile_config(&project_root, None, &cache)
            .unwrap();

        assert_eq!(cfg.module_path, vec![named, automatic]);

        // Since JPMS flags are present (even in a whitespace-separated string), we should not need
        // to evaluate Maven's module path element expressions.
        assert!(!runner.invocations().iter().any(|args| {
            args.iter()
                .any(|a| a == "-Dexpression=project.compileModulePathElements")
                || args
                    .iter()
                    .any(|a| a == "-Dexpression=project.compileModulepathElements")
                || args
                    .iter()
                    .any(|a| a == "-Dexpression=project.testCompileModulePathElements")
        }));
    }
}
