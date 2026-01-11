use crate::cache::{BuildCache, BuildFileFingerprint};
use crate::command::format_command;
use crate::{
    BuildError, BuildResult, BuildSystemKind, Classpath, CommandOutput, CommandRunner,
    DefaultCommandRunner, JavaCompileConfig, MavenBuildGoal, Result,
};
use nova_project::{AnnotationProcessing, AnnotationProcessingConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

        let main_source_roots = absolutize_paths(
            &module_dir,
            self.evaluate_path_list(project_root, module_relative, "project.compileSourceRoots")?,
        );

        let test_source_roots = {
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
            return Ok(BuildResult { diagnostics });
        }

        Err(BuildError::CommandFailed {
            tool: "maven",
            command: format_command(&program, &args),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
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
        let (program, args, output) = self.run(project_root, module_relative, &effective_pom_args)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "maven",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }

        let pom_xml = std::fs::read_to_string(&effective_pom)?;
        let _ = std::fs::remove_file(&effective_pom);

        let mut config = parse_maven_effective_pom_annotation_processing(&pom_xml, &module_root)?;

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
        let eval_args = self.help_evaluate_args(expression);
        let (program, args, output) = self.run(project_root, module_relative, &eval_args)?;
        if !output.status.success() {
            return Err(BuildError::CommandFailed {
                tool: "maven",
                command: format_command(&program, &args),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
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
        generated_sources_dir: Some(module_root.join("target/generated-test-sources/test-annotations")),
        ..Default::default()
    };

    // Apply plugin-level config.
    if let Some(config) = child_element(&plugin, "configuration") {
        apply_maven_compiler_config(&config, module_root, &mut main, &mut test);
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
                apply_maven_compiler_config(&config, module_root, &mut main, &mut test);
            }
            if goals.iter().any(|g| g == "testCompile") {
                // Execution config uses the same keys, but we treat it as test-specific.
                // Use a dummy "main" config to avoid double-borrowing `test`.
                let mut dummy_main = AnnotationProcessingConfig::default();
                apply_maven_compiler_config(&config, module_root, &mut dummy_main, &mut test);
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
                extracted.extend(text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
        }
        main.processors.extend(extracted.iter().cloned());
        test.processors.extend(extracted);
    }

    if let Some(paths) = child_element(config, "annotationProcessorPaths") {
        let repo = default_maven_repo();
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

            if let Some(jar) = maven_jar_path(&repo, &group_id, &artifact_id, &version, classifier.as_deref()) {
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
    config.processors.retain(|p| seen_processors.insert(p.clone()));

    let mut seen_paths = std::collections::HashSet::new();
    config.processor_path.retain(|p| seen_paths.insert(p.clone()));
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
        if let Some(plugin) = plugins
            .children()
            .find(|n| {
                n.is_element()
                    && n.has_tag_name("plugin")
                    && child_text(&n, "artifactId").as_deref() == Some("maven-compiler-plugin")
            })
        {
            return Some(plugin);
        }
    }

    if let Some(pm) = child_element(&build, "pluginManagement") {
        if let Some(plugins) = child_element(&pm, "plugins") {
            if let Some(plugin) = plugins
                .children()
                .find(|n| {
                    n.is_element()
                        && n.has_tag_name("plugin")
                        && child_text(&n, "artifactId").as_deref() == Some("maven-compiler-plugin")
                })
            {
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

fn maven_jar_path(
    repo: &Path,
    group_id: &str,
    artifact_id: &str,
    version: &str,
    classifier: Option<&str>,
) -> Option<PathBuf> {
    let group_path = group_id.replace('.', "/");
    let base = repo.join(group_path).join(artifact_id).join(version);
    let file_name = if let Some(classifier) = classifier {
        format!("{artifact_id}-{version}-{classifier}.jar")
    } else {
        format!("{artifact_id}-{version}.jar")
    };
    Some(base.join(file_name))
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

        let mut cfg = MavenConfig::default();
        cfg.prefer_wrapper = true;
        cfg.mvn_path = PathBuf::from("/custom/mvn");

        let build = MavenBuild::new(cfg.clone());
        assert_eq!(build.mvn_executable(root), cfg.mvn_path);
    }
}
