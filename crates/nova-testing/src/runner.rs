use crate::report::{merge_case_results, parse_junit_report};
use crate::schema::{
    BuildTool, TestCaseResult, TestRunRequest, TestRunResponse, TestRunSummary, TestStatus,
};
use crate::test_id::{parse_qualified_test_id, qualify_test_id};
use crate::util::{collect_module_roots, module_for_path, ModuleRoot};
use crate::{NovaTestingError, Result, SCHEMA_VERSION};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub fn run_tests(req: &TestRunRequest) -> Result<TestRunResponse> {
    if req.project_root.trim().is_empty() {
        return Err(NovaTestingError::InvalidRequest(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let project_root = PathBuf::from(&req.project_root);
    let project_root = match project_root.canonicalize() {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => project_root,
        Err(err) => {
            tracing::debug!(
                target = "nova.testing",
                path = %project_root.display(),
                error = %err,
                "failed to canonicalize project root for test run"
            );
            project_root
        }
    };

    let tool = match req.build_tool {
        BuildTool::Auto => detect_build_tool(&project_root)?,
        other => other,
    };

    let started_at = SystemTime::now();
    let runs = build_runs(&project_root, tool, &req.tests)?;
    let multi_run = runs.len() > 1;

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = 0;
    let mut success = true;

    for run in runs {
        let output = command_output(run.command)?;
        let run_exit_code = output.status.code().unwrap_or(-1);
        if run_exit_code != 0 && exit_code == 0 {
            exit_code = run_exit_code;
        }
        success &= run_exit_code == 0;

        let label = run.module_rel_path.as_deref().unwrap_or("<workspace>");
        append_scoped_output(&mut stdout, label, output.stdout, multi_run);
        append_scoped_output(&mut stderr, label, output.stderr, multi_run);
    }

    let cutoff = started_at.checked_sub(Duration::from_secs(2));
    let allow_cached_reports = success || !req.tests.is_empty();
    let mut tests = collect_and_parse_reports(
        &project_root,
        tool,
        cutoff,
        allow_cached_reports,
        &req.tests,
    )?;
    tests = filter_results_by_request(tests, &req.tests);
    tests.sort_by(|a, b| a.id.cmp(&b.id));

    let summary = summarize(&tests);

    Ok(TestRunResponse {
        schema_version: SCHEMA_VERSION,
        tool,
        success,
        exit_code,
        stdout,
        stderr,
        tests,
        summary,
    })
}

pub(crate) fn detect_build_tool(project_root: &Path) -> Result<BuildTool> {
    let pom = project_root.join("pom.xml");
    if pom.exists() {
        return Ok(BuildTool::Maven);
    }

    let gradle_markers = [
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
    ];
    if gradle_markers
        .iter()
        .any(|marker| project_root.join(marker).exists())
    {
        return Ok(BuildTool::Gradle);
    }

    Err(NovaTestingError::UnsupportedBuildTool(
        project_root.display().to_string(),
    ))
}

struct ModuleRun {
    module_rel_path: Option<String>,
    command: Command,
}

fn build_runs(project_root: &Path, tool: BuildTool, tests: &[String]) -> Result<Vec<ModuleRun>> {
    if tests.is_empty() {
        return Ok(vec![ModuleRun {
            module_rel_path: None,
            command: command_for_tests(project_root, tool, None, tests),
        }]);
    }

    let mut seen = BTreeSet::new();
    let mut groups: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    for id in tests {
        let parsed = parse_qualified_test_id(id);
        // Preserve request order, but avoid duplicated patterns within a module group.
        if !seen.insert((parsed.module.clone(), parsed.test.clone())) {
            continue;
        }
        groups.entry(parsed.module).or_default().push(parsed.test);
    }

    Ok(groups
        .into_iter()
        .map(|(module_rel_path, ids)| ModuleRun {
            command: command_for_tests(project_root, tool, module_rel_path.as_deref(), &ids),
            module_rel_path,
        })
        .collect())
}

pub(crate) fn maven_executable(project_root: &Path) -> &'static str {
    if cfg!(windows) {
        if project_root.join("mvnw.cmd").exists() {
            "mvnw.cmd"
        } else if project_root.join("mvnw.bat").exists() {
            "mvnw.bat"
        } else {
            "mvn"
        }
    } else if project_root.join("mvnw").exists() {
        "./mvnw"
    } else {
        "mvn"
    }
}

pub(crate) fn gradle_executable(project_root: &Path) -> &'static str {
    if cfg!(windows) {
        if project_root.join("gradlew.bat").exists() {
            "gradlew.bat"
        } else if project_root.join("gradlew.cmd").exists() {
            "gradlew.cmd"
        } else {
            "gradle"
        }
    } else if project_root.join("gradlew").exists() {
        "./gradlew"
    } else {
        "gradle"
    }
}

fn command_for_tests(
    project_root: &Path,
    tool: BuildTool,
    module_rel_path: Option<&str>,
    tests: &[String],
) -> Command {
    match tool {
        BuildTool::Maven => {
            let executable = maven_executable(project_root);

            let mut cmd = Command::new(executable);
            cmd.current_dir(project_root);

            if let Some(module_rel_path) = module_rel_path {
                // `"."` is the canonical encoding for the workspace root module. Avoid passing it
                // to `-pl` because Maven does not guarantee that `-pl .` is valid syntax.
                if module_rel_path != "." {
                    cmd.arg("-pl").arg(module_rel_path);
                    cmd.arg("-am");
                }
            }

            if !tests.is_empty() {
                let pattern = tests.join(",");
                cmd.arg(format!("-Dtest={pattern}"));
            }
            cmd.arg("test");
            cmd
        }
        BuildTool::Gradle => {
            let executable = gradle_executable(project_root);

            let mut cmd = Command::new(executable);
            cmd.current_dir(project_root);
            let task = match module_rel_path {
                Some(".") => ":test".to_string(),
                Some(path) => format!(":{}:test", path.replace('/', ":")),
                None => "test".to_string(),
            };
            cmd.arg(task);
            for test in tests {
                let pattern = test.replace('#', ".");
                cmd.arg("--tests").arg(pattern);
            }
            cmd
        }
        BuildTool::Auto => unreachable!("auto must be resolved before command construction"),
    }
}

fn command_output(mut command: Command) -> Result<std::process::Output> {
    let desc = format!("{:?}", command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = {
        let mut attempt = 0u8;
        let mut backoff = Duration::from_millis(10);
        loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(err) if should_retry_spawn(&err) && attempt < 4 => {
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_millis(200));
                    attempt += 1;
                }
                Err(err) => {
                    return Err(NovaTestingError::CommandFailed(format!("{desc}: {err}")));
                }
            }
        }
    };

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NovaTestingError::CommandFailed(format!(
                "{desc}: failed to capture stdout"
            )));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NovaTestingError::CommandFailed(format!(
                "{desc}: failed to capture stderr"
            )));
        }
    };

    let stdout_handle = match std::thread::Builder::new()
        .name("nova-testing-stdout".to_string())
        .spawn(move || capture_stream(stdout))
    {
        Ok(handle) => handle,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NovaTestingError::CommandFailed(format!(
                "{desc}: failed to spawn stdout reader: {err}"
            )));
        }
    };

    let stderr_handle = match std::thread::Builder::new()
        .name("nova-testing-stderr".to_string())
        .spawn(move || capture_stream(stderr))
    {
        Ok(handle) => handle,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            static STDOUT_JOIN_PANIC_LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            if stdout_handle.join().is_err() {
                if STDOUT_JOIN_PANIC_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.testing",
                        "stdout reader thread panicked while handling stderr spawn failure (best effort)"
                    );
                }
            }
            return Err(NovaTestingError::CommandFailed(format!(
                "{desc}: failed to spawn stderr reader: {err}"
            )));
        }
    };

    let status = child.wait()?;

    let stdout = stdout_handle.join().map_err(|_| {
        NovaTestingError::CommandFailed(format!("{desc}: stdout reader panicked"))
    })??;
    let stderr = stderr_handle.join().map_err(|_| {
        NovaTestingError::CommandFailed(format!("{desc}: stderr reader panicked"))
    })??;

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn should_retry_spawn(err: &std::io::Error) -> bool {
    // Under high system load `spawn()` can fail with EAGAIN ("Resource temporarily unavailable").
    // Retrying keeps the runner resilient in constrained environments (including CI).
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    ) || err.raw_os_error() == Some(11)
}

fn append_scoped_output(out: &mut String, label: &str, bytes: Vec<u8>, multi_run: bool) {
    let chunk = bytes_to_string(bytes);
    if chunk.is_empty() && !multi_run {
        return;
    }
    if multi_run {
        out.push_str(&format!("===== {label} =====\n"));
    }
    out.push_str(&chunk);
    if multi_run && !out.ends_with('\n') {
        out.push('\n');
    }
    if multi_run {
        out.push_str(&format!("===== end {label} =====\n"));
    }
}

fn collect_and_parse_reports(
    project_root: &Path,
    tool: BuildTool,
    modified_since: Option<SystemTime>,
    allow_cached_reports: bool,
    requested: &[String],
) -> Result<Vec<TestCaseResult>> {
    let modules = project_modules(project_root);
    let qualify_ids = modules.len() > 1;
    let report_dirs = discover_report_dirs(&modules, tool)?;
    collect_and_parse_reports_in_dirs(
        &report_dirs,
        modified_since,
        allow_cached_reports,
        requested,
        &modules,
        qualify_ids,
    )
}

fn collect_and_parse_reports_in_dirs(
    report_dirs: &[PathBuf],
    modified_since: Option<SystemTime>,
    allow_cached_reports: bool,
    requested: &[String],
    modules: &[ModuleRoot],
    qualify_ids: bool,
) -> Result<Vec<TestCaseResult>> {
    let mut xml_files = collect_report_files(report_dirs, modified_since)?;
    xml_files.sort();

    let mut by_id: BTreeMap<String, TestCaseResult> = BTreeMap::new();
    merge_report_files(&mut by_id, xml_files, None, modules, qualify_ids)?;

    if by_id.is_empty() && allow_cached_reports {
        let xml_files = collect_recent_report_files(report_dirs, 50)?;
        let requested_exact = requested_exact_ids_for_early_stop(requested);
        merge_report_files(
            &mut by_id,
            xml_files,
            requested_exact.as_ref(),
            modules,
            qualify_ids,
        )?;
    }

    Ok(by_id.into_values().collect())
}

fn merge_report_files(
    by_id: &mut BTreeMap<String, TestCaseResult>,
    xml_files: Vec<PathBuf>,
    requested_exact: Option<&HashSet<String>>,
    modules: &[ModuleRoot],
    qualify_ids: bool,
) -> Result<()> {
    // Deduplicate by id to make results stable when multiple report files contain overlapping suites.
    let mut remaining = requested_exact.cloned();
    for path in xml_files {
        let module_rel_path = module_for_path(modules, &path).rel_path.clone();
        for mut case in parse_junit_report(&path)? {
            if qualify_ids {
                case.id = qualify_test_id(&module_rel_path, &case.id);
            }

            let id = case.id.clone();
            match by_id.get_mut(&id) {
                Some(existing) => merge_case_results(existing, case),
                None => {
                    by_id.insert(id.clone(), case);
                }
            };
            if let Some(remaining) = remaining.as_mut() {
                remaining.remove(&id);
                if remaining.is_empty() {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn discover_report_dirs(modules: &[ModuleRoot], tool: BuildTool) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();

    match tool {
        BuildTool::Maven => {
            for module_root in modules {
                dirs.push(module_root.root.join("target/surefire-reports"));
                dirs.push(module_root.root.join("target/failsafe-reports"));
            }
        }
        BuildTool::Gradle => {
            for module_root in modules {
                dirs.push(module_root.root.join("build/test-results/test"));

                // Best-effort: include any custom test tasks under `build/test-results/*`.
                let test_results_dir = module_root.root.join("build/test-results");
                match std::fs::read_dir(&test_results_dir) {
                    Ok(entries) => {
                        let mut logged_entry_error = false;
                        for entry in entries {
                            let entry = match entry {
                                Ok(entry) => entry,
                                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                                Err(err) => {
                                    if !logged_entry_error {
                                        tracing::debug!(
                                            target = "nova.testing",
                                            path = %test_results_dir.display(),
                                            error = %err,
                                            "failed to read directory entry while scanning test reports"
                                        );
                                        logged_entry_error = true;
                                    }
                                    continue;
                                }
                            };
                            let path = entry.path();
                            if path.is_dir() {
                                dirs.push(path);
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.testing",
                            path = %test_results_dir.display(),
                            error = %err,
                            "failed to scan test reports directory"
                        );
                    }
                }
            }
        }
        BuildTool::Auto => {}
    };

    // For simple projects (no build system), there are no reports to scan.
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

fn project_modules(project_root: &Path) -> Vec<ModuleRoot> {
    static PROJECT_LOAD_ERROR_LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

    // Best-effort: if project loading fails (malformed build files, etc), fall back to scanning just
    // the workspace root as a single module. This keeps `nova/test/run` resilient while still
    // avoiding full workspace walks for report discovery.
    nova_project::load_project(project_root)
        .map(|project| collect_module_roots(&project.workspace_root, &project.modules))
        .unwrap_or_else(|err| {
            if PROJECT_LOAD_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.testing",
                    project_root = %project_root.display(),
                    error = %err,
                    "failed to load project; falling back to single-module workspace"
                );
            }
            vec![ModuleRoot {
                root: project_root.to_path_buf(),
                rel_path: ".".to_string(),
            }]
        })
}

fn is_modified_since(path: &Path, cutoff: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    modified >= cutoff
}

fn file_modified_time(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(UNIX_EPOCH)
}

fn collect_report_files(
    report_dirs: &[PathBuf],
    modified_since: Option<SystemTime>,
) -> Result<Vec<PathBuf>> {
    let mut xml_files = Vec::new();
    for dir in report_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in WalkDir::new(dir).follow_links(false).into_iter() {
            let entry = entry.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|e| e.to_str()) != Some("xml") {
                continue;
            }
            if let Some(cutoff) = modified_since {
                if !is_modified_since(entry.path(), cutoff) {
                    continue;
                }
            }
            xml_files.push(entry.path().to_path_buf());
        }
    }
    Ok(xml_files)
}

fn collect_recent_report_files(
    report_dirs: &[PathBuf],
    per_dir_limit: usize,
) -> Result<Vec<PathBuf>> {
    let mut selected: Vec<(SystemTime, PathBuf)> = Vec::new();

    for dir in report_dirs {
        if !dir.exists() {
            continue;
        }
        let mut files = Vec::new();
        for entry in WalkDir::new(dir).follow_links(false).into_iter() {
            let entry = entry.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|e| e.to_str()) != Some("xml") {
                continue;
            }

            let modified = file_modified_time(entry.path());
            files.push((modified, entry.path().to_path_buf()));
        }

        // Pick the newest reports first; tie-break on path for deterministic ordering.
        files.sort_by(|(a_time, a_path), (b_time, b_path)| {
            b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
        });

        selected.extend(files.into_iter().take(per_dir_limit));
    }

    // Parse newest-to-oldest globally as well, to satisfy early-stop requests quickly.
    selected.sort_by(|(a_time, a_path), (b_time, b_path)| {
        b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
    });
    selected.dedup_by(|(_, a_path), (_, b_path)| a_path == b_path);

    Ok(selected.into_iter().map(|(_, path)| path).collect())
}

fn requested_exact_ids_for_early_stop(requested: &[String]) -> Option<HashSet<String>> {
    if requested.is_empty() {
        return None;
    }

    // Only attempt early stopping when the request is a set of specific test-case IDs. If any
    // request refers to a class prefix, we need to parse all selected reports to avoid returning a
    // partial class run in cached-report mode.
    if requested.iter().any(|req| !req.contains('#')) {
        return None;
    }

    Some(requested.iter().cloned().collect())
}

fn filter_results_by_request(
    cases: Vec<TestCaseResult>,
    requested: &[String],
) -> Vec<TestCaseResult> {
    if requested.is_empty() {
        return cases;
    }

    #[derive(Clone, Debug)]
    struct Matcher {
        module: Option<String>,
        test: String,
        is_exact: bool,
    }

    let matchers: Vec<Matcher> = requested
        .iter()
        .map(|req| {
            let parsed = parse_qualified_test_id(req);
            Matcher {
                module: parsed.module,
                is_exact: parsed.test.contains('#'),
                test: parsed.test,
            }
        })
        .collect();

    cases
        .into_iter()
        .filter(|case| {
            let parsed_case = parse_qualified_test_id(&case.id);
            matchers.iter().any(|matcher| {
                if let Some(module) = matcher.module.as_deref() {
                    if parsed_case.module.as_deref() != Some(module) {
                        return false;
                    }
                }

                if matcher.is_exact {
                    parsed_case.test == matcher.test
                } else {
                    parsed_case.test.starts_with(&format!("{}#", matcher.test))
                }
            })
        })
        .collect()
}

fn summarize(cases: &[TestCaseResult]) -> TestRunSummary {
    let mut summary = TestRunSummary::default();
    summary.total = cases.len() as u32;
    for case in cases {
        match case.status {
            TestStatus::Passed => summary.passed += 1,
            TestStatus::Failed => summary.failed += 1,
            TestStatus::Skipped => summary.skipped += 1,
        }
    }
    summary
}

fn bytes_to_string(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(&err.into_bytes()).into_owned(),
    }
}

fn capture_stream(mut reader: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    const MARKER: &[u8] = b"\n... <truncated> ...\n";

    let head_cap = MAX_BYTES / 2;
    let tail_cap = MAX_BYTES - head_cap;

    let mut head = Vec::with_capacity(head_cap.min(8 * 1024));
    let mut tail = std::collections::VecDeque::with_capacity(tail_cap.min(8 * 1024));

    let mut total = 0usize;
    let mut buf = [0u8; 8 * 1024];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }

        total = total.saturating_add(n);

        let mut chunk = &buf[..n];
        if head.len() < head_cap {
            let remaining = head_cap - head.len();
            let take = remaining.min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }

        if !chunk.is_empty() && tail_cap > 0 {
            tail.extend(chunk.iter().copied());
            if tail.len() > tail_cap {
                let excess = tail.len() - tail_cap;
                tail.drain(..excess);
            }
        }
    }

    if total <= MAX_BYTES {
        let mut out = head;
        out.extend(tail);
        return Ok(out);
    }

    let mut out = head;
    out.extend_from_slice(MARKER);
    out.extend(tail);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn fixture_root(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn detects_build_tool_from_project_files() {
        let maven = fixture_root("maven-junit5");
        assert_eq!(detect_build_tool(&maven).unwrap(), BuildTool::Maven);

        let gradle = fixture_root("gradle-junit4");
        assert_eq!(detect_build_tool(&gradle).unwrap(), BuildTool::Gradle);
    }

    #[test]
    fn constructs_maven_test_command() {
        let root = fixture_root("maven-junit5");
        let cmd = command_for_tests(
            &root,
            BuildTool::Maven,
            None,
            &vec!["com.example.CalculatorTest#adds".to_string()],
        );

        assert_eq!(cmd.get_program().to_string_lossy(), "mvn");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-Dtest=com.example.CalculatorTest#adds", "test"]);
    }

    #[test]
    fn constructs_gradle_test_command() {
        let root = fixture_root("gradle-junit4");
        let cmd = command_for_tests(
            &root,
            BuildTool::Gradle,
            None,
            &vec!["com.example.LegacyCalculatorTest#legacyAdds".to_string()],
        );

        assert_eq!(cmd.get_program().to_string_lossy(), "gradle");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "test",
                "--tests",
                "com.example.LegacyCalculatorTest.legacyAdds"
            ]
        );
    }

    #[test]
    fn prefers_maven_wrapper_when_present() {
        let root = TempDir::new().unwrap();
        if cfg!(windows) {
            std::fs::write(root.path().join("mvnw.bat"), "").unwrap();
            std::fs::write(root.path().join("mvnw.cmd"), "").unwrap();
        } else {
            std::fs::write(root.path().join("mvnw"), "").unwrap();
        }

        let cmd = command_for_tests(root.path(), BuildTool::Maven, None, &[]);
        let expected = if cfg!(windows) { "mvnw.cmd" } else { "./mvnw" };
        assert_eq!(cmd.get_program().to_string_lossy(), expected);
    }

    #[test]
    fn prefers_gradle_wrapper_when_present() {
        let root = TempDir::new().unwrap();
        if cfg!(windows) {
            std::fs::write(root.path().join("gradlew.cmd"), "").unwrap();
            std::fs::write(root.path().join("gradlew.bat"), "").unwrap();
        } else {
            std::fs::write(root.path().join("gradlew"), "").unwrap();
        }

        let cmd = command_for_tests(root.path(), BuildTool::Gradle, None, &[]);
        let expected = if cfg!(windows) {
            "gradlew.bat"
        } else {
            "./gradlew"
        };
        assert_eq!(cmd.get_program().to_string_lossy(), expected);
    }

    #[test]
    fn constructs_module_scoped_maven_command_with_wrapper() {
        let root = TempDir::new().unwrap();
        if cfg!(windows) {
            std::fs::write(root.path().join("mvnw.bat"), "").unwrap();
            std::fs::write(root.path().join("mvnw.cmd"), "").unwrap();
        } else {
            std::fs::write(root.path().join("mvnw"), "").unwrap();
        }

        let cmd = command_for_tests(
            root.path(),
            BuildTool::Maven,
            Some("service-a"),
            &vec!["com.example.DuplicateTest#ok".to_string()],
        );

        let expected = if cfg!(windows) { "mvnw.cmd" } else { "./mvnw" };
        assert_eq!(cmd.get_program().to_string_lossy(), expected);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "-pl",
                "service-a",
                "-am",
                "-Dtest=com.example.DuplicateTest#ok",
                "test"
            ]
        );
    }

    #[test]
    fn constructs_module_scoped_gradle_command_with_wrapper() {
        let root = TempDir::new().unwrap();
        if cfg!(windows) {
            std::fs::write(root.path().join("gradlew.cmd"), "").unwrap();
            std::fs::write(root.path().join("gradlew.bat"), "").unwrap();
        } else {
            std::fs::write(root.path().join("gradlew"), "").unwrap();
        }

        let cmd = command_for_tests(
            root.path(),
            BuildTool::Gradle,
            Some("module-a"),
            &vec!["com.example.DuplicateTest#ok".to_string()],
        );

        let expected = if cfg!(windows) {
            "gradlew.bat"
        } else {
            "./gradlew"
        };
        assert_eq!(cmd.get_program().to_string_lossy(), expected);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![":module-a:test", "--tests", "com.example.DuplicateTest.ok"]
        );
    }

    #[test]
    fn discovers_maven_report_dirs_from_project_modules() {
        let root = fixture_root("maven-junit5").canonicalize().unwrap();
        let modules = project_modules(&root);
        let dirs = discover_report_dirs(&modules, BuildTool::Maven).unwrap();
        assert!(dirs.contains(&root.join("target/surefire-reports")));
        assert!(dirs.contains(&root.join("target/failsafe-reports")));
    }

    #[test]
    fn discovers_gradle_report_dirs_from_project_modules() {
        let root = fixture_root("gradle-junit4").canonicalize().unwrap();
        let modules = project_modules(&root);
        let dirs = discover_report_dirs(&modules, BuildTool::Gradle).unwrap();
        assert!(dirs.contains(&root.join("build/test-results/test")));
    }

    #[test]
    fn falls_back_to_cached_reports_when_reports_are_not_modified_since_cutoff() {
        let tmp: TempDir = tempfile::Builder::new()
            .prefix("nova-testing-cached-reports")
            .tempdir()
            .unwrap();
        let report_dir = tmp.path().join("target/surefire-reports");
        std::fs::create_dir_all(&report_dir).unwrap();

        let report_path = report_dir.join("TEST-com.example.CalculatorTest.xml");
        std::fs::write(
            &report_path,
            r#"<testsuite name="suite" tests="1" failures="0" errors="0" skipped="0">
  <testcase classname="com.example.CalculatorTest" name="adds" time="0.001" />
</testsuite>"#,
        )
        .unwrap();

        let cutoff = SystemTime::now()
            .checked_add(Duration::from_secs(60))
            .unwrap();
        let modules = vec![ModuleRoot {
            root: tmp.path().to_path_buf(),
            rel_path: ".".to_string(),
        }];
        let cases = collect_and_parse_reports_in_dirs(
            &[report_dir],
            Some(cutoff),
            true,
            &[],
            &modules,
            false,
        )
        .unwrap();

        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "com.example.CalculatorTest#adds");
        assert_eq!(cases[0].status, TestStatus::Passed);
    }

    #[test]
    fn constructs_module_scoped_maven_commands_for_qualified_ids() {
        let root = fixture_root("maven-multi-module");
        let runs = build_runs(
            &root,
            BuildTool::Maven,
            &vec![
                "service-a::com.example.DuplicateTest#ok".to_string(),
                "service-b::com.example.DuplicateTest#ok".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(runs.len(), 2);

        let args_for = |cmd: &Command| {
            cmd.get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(runs[0].module_rel_path.as_deref(), Some("service-a"));
        assert_eq!(runs[0].command.get_program().to_string_lossy(), "mvn");
        assert_eq!(
            args_for(&runs[0].command),
            vec![
                "-pl",
                "service-a",
                "-am",
                "-Dtest=com.example.DuplicateTest#ok",
                "test"
            ]
        );

        assert_eq!(runs[1].module_rel_path.as_deref(), Some("service-b"));
        assert_eq!(
            args_for(&runs[1].command),
            vec![
                "-pl",
                "service-b",
                "-am",
                "-Dtest=com.example.DuplicateTest#ok",
                "test"
            ]
        );
    }

    #[test]
    fn constructs_workspace_root_maven_command_for_qualified_ids() {
        let root = fixture_root("maven-multi-module");
        let runs = build_runs(
            &root,
            BuildTool::Maven,
            &vec![".::com.example.DuplicateTest#ok".to_string()],
        )
        .unwrap();

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].module_rel_path.as_deref(), Some("."));

        let args = runs[0]
            .command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(args, vec!["-Dtest=com.example.DuplicateTest#ok", "test"]);
    }

    #[test]
    fn constructs_module_scoped_gradle_commands_for_qualified_ids() {
        let root = fixture_root("gradle-multi-module");
        let runs = build_runs(
            &root,
            BuildTool::Gradle,
            &vec![
                "module-a::com.example.DuplicateTest#ok".to_string(),
                "module-b::com.example.DuplicateTest#ok".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(runs.len(), 2);

        let args_for = |cmd: &Command| {
            cmd.get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(runs[0].module_rel_path.as_deref(), Some("module-a"));
        assert_eq!(
            args_for(&runs[0].command),
            vec![":module-a:test", "--tests", "com.example.DuplicateTest.ok"]
        );

        assert_eq!(runs[1].module_rel_path.as_deref(), Some("module-b"));
        assert_eq!(
            args_for(&runs[1].command),
            vec![":module-b:test", "--tests", "com.example.DuplicateTest.ok"]
        );
    }

    #[test]
    fn prefixes_junit_report_results_with_module_paths() {
        let tmp: TempDir = tempfile::Builder::new()
            .prefix("nova-testing-maven-multi-module")
            .tempdir()
            .unwrap();

        let service_a_root = tmp.path().join("service-a");
        let service_b_root = tmp.path().join("service-b");
        let service_a_reports = service_a_root.join("target/surefire-reports");
        let service_b_reports = service_b_root.join("target/surefire-reports");

        std::fs::create_dir_all(&service_a_reports).unwrap();
        std::fs::create_dir_all(&service_b_reports).unwrap();

        // The actual Maven output location (`target/surefire-reports`) is ignored by git, so the
        // module-prefixing tests construct minimal reports at runtime.
        let xml = r#"<testsuite name="suite" tests="1" failures="0" errors="0" skipped="0">
  <testcase classname="com.example.DuplicateTest" name="ok" time="0.001" />
</testsuite>"#;
        std::fs::write(
            service_a_reports.join("TEST-com.example.DuplicateTest.xml"),
            xml,
        )
        .unwrap();
        std::fs::write(
            service_b_reports.join("TEST-com.example.DuplicateTest.xml"),
            xml,
        )
        .unwrap();

        let mut modules = vec![
            ModuleRoot {
                root: tmp.path().to_path_buf(),
                rel_path: ".".to_string(),
            },
            ModuleRoot {
                root: service_a_root,
                rel_path: "service-a".to_string(),
            },
            ModuleRoot {
                root: service_b_root,
                rel_path: "service-b".to_string(),
            },
        ];
        modules.sort_by(|a, b| {
            b.root
                .components()
                .count()
                .cmp(&a.root.components().count())
                .then(a.root.cmp(&b.root))
        });
        modules.dedup_by(|a, b| a.root == b.root);

        let mut cases = collect_and_parse_reports_in_dirs(
            &[service_a_reports, service_b_reports],
            None,
            false,
            &[],
            &modules,
            true,
        )
        .unwrap();
        cases.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].id, "service-a::com.example.DuplicateTest#ok");
        assert_eq!(cases[1].id, "service-b::com.example.DuplicateTest#ok");
    }

    #[test]
    fn prefixes_gradle_junit_report_results_with_module_paths() {
        let root = fixture_root("gradle-multi-module");

        let mut cases =
            collect_and_parse_reports(&root, BuildTool::Gradle, None, false, &[]).unwrap();
        cases.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].id, "module-a::com.example.DuplicateTest#ok");
        assert_eq!(cases[1].id, "module-b::com.example.DuplicateTest#ok");
    }

    #[test]
    fn capture_stream_returns_full_output_when_within_limit() {
        let max_bytes = 2 * 1024 * 1024;
        let data: Vec<u8> = (0..max_bytes).map(|i| (i % 251) as u8).collect();

        let out = capture_stream(Cursor::new(data.as_slice())).unwrap();
        assert_eq!(out.len(), data.len());
        assert!(out == data);
    }

    #[test]
    fn capture_stream_truncates_and_preserves_head_and_tail() {
        let max_bytes = 2 * 1024 * 1024;
        let marker: &[u8] = b"\n... <truncated> ...\n";

        let head_cap = max_bytes / 2;
        let tail_cap = max_bytes - head_cap;

        let data_len = max_bytes + 123;
        let data: Vec<u8> = (0..data_len).map(|i| (i % 251) as u8).collect();

        let out = capture_stream(Cursor::new(data.as_slice())).unwrap();

        assert_eq!(out.len(), head_cap + marker.len() + tail_cap);
        assert_eq!(&out[..head_cap], &data[..head_cap]);
        assert_eq!(&out[head_cap..head_cap + marker.len()], marker);
        assert_eq!(
            &out[head_cap + marker.len()..],
            &data[data.len() - tail_cap..]
        );
    }

    #[test]
    fn append_scoped_output_is_noop_for_single_run_empty_chunk() {
        let mut out = String::new();
        append_scoped_output(&mut out, "module-a", Vec::new(), false);
        assert_eq!(out, "");
    }

    #[test]
    fn append_scoped_output_wraps_multi_run_output_and_normalizes_trailing_newline() {
        let mut out = String::new();
        append_scoped_output(&mut out, "module-a", b"hello".to_vec(), true);
        assert_eq!(
            out,
            "===== module-a =====\nhello\n===== end module-a =====\n"
        );

        let mut out = String::new();
        append_scoped_output(&mut out, "module-a", b"hello\n".to_vec(), true);
        assert_eq!(
            out,
            "===== module-a =====\nhello\n===== end module-a =====\n"
        );
    }
}
