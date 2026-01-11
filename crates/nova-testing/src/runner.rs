use crate::report::{merge_case_results, parse_junit_report};
use crate::schema::{
    BuildTool, TestCaseResult, TestRunRequest, TestRunResponse, TestRunSummary, TestStatus,
};
use crate::{NovaTestingError, Result, SCHEMA_VERSION};
use std::collections::BTreeMap;
use std::collections::HashSet;
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
    let project_root = project_root.canonicalize().unwrap_or(project_root);

    let tool = match req.build_tool {
        BuildTool::Auto => detect_build_tool(&project_root)?,
        other => other,
    };

    let command = command_for_tests(&project_root, tool, &req.tests);
    let started_at = SystemTime::now();
    let output = command_output(command)?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = bytes_to_string(output.stdout);
    let stderr = bytes_to_string(output.stderr);

    let cutoff = started_at.checked_sub(Duration::from_secs(2));
    let allow_cached_reports = output.status.success() || !req.tests.is_empty();
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
        success: exit_code == 0,
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

fn command_for_tests(project_root: &Path, tool: BuildTool, tests: &[String]) -> Command {
    match tool {
        BuildTool::Maven => {
            let mvnw = project_root.join("mvnw");
            let executable = if mvnw.exists() { "./mvnw" } else { "mvn" };

            let mut cmd = Command::new(executable);
            cmd.current_dir(project_root);

            if !tests.is_empty() {
                let pattern = tests.join(",");
                cmd.arg(format!("-Dtest={pattern}"));
            }
            cmd.arg("test");
            cmd
        }
        BuildTool::Gradle => {
            let gradlew = project_root.join("gradlew");
            let executable = if gradlew.exists() {
                "./gradlew"
            } else {
                "gradle"
            };

            let mut cmd = Command::new(executable);
            cmd.current_dir(project_root);
            cmd.arg("test");
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
            let _ = stdout_handle.join();
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
    matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted)
        || err.raw_os_error() == Some(11)
}

fn collect_and_parse_reports(
    project_root: &Path,
    tool: BuildTool,
    modified_since: Option<SystemTime>,
    allow_cached_reports: bool,
    requested: &[String],
) -> Result<Vec<TestCaseResult>> {
    let report_dirs = discover_report_dirs(project_root, tool)?;
    collect_and_parse_reports_in_dirs(
        &report_dirs,
        modified_since,
        allow_cached_reports,
        requested,
    )
}

fn collect_and_parse_reports_in_dirs(
    report_dirs: &[PathBuf],
    modified_since: Option<SystemTime>,
    allow_cached_reports: bool,
    requested: &[String],
) -> Result<Vec<TestCaseResult>> {
    let mut xml_files = collect_report_files(report_dirs, modified_since)?;
    xml_files.sort();

    let mut by_id: BTreeMap<String, TestCaseResult> = BTreeMap::new();
    merge_report_files(&mut by_id, xml_files, None)?;

    if by_id.is_empty() && allow_cached_reports {
        let xml_files = collect_recent_report_files(report_dirs, 50)?;
        let requested_exact = requested_exact_ids_for_early_stop(requested);
        merge_report_files(&mut by_id, xml_files, requested_exact.as_ref())?;
    }

    Ok(by_id.into_values().collect())
}

fn merge_report_files(
    by_id: &mut BTreeMap<String, TestCaseResult>,
    xml_files: Vec<PathBuf>,
    requested_exact: Option<&HashSet<String>>,
) -> Result<()> {
    // Deduplicate by id to make results stable when multiple report files contain overlapping suites.
    let mut remaining = requested_exact.cloned();
    for path in xml_files {
        for case in parse_junit_report(&path)? {
            let id = case.id.clone();
            match by_id.get_mut(&id) {
                Some(existing) => merge_case_results(existing, case),
                None => {
                    by_id.insert(id.clone(), case);
                }
            }
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

fn discover_report_dirs(project_root: &Path, tool: BuildTool) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    let module_roots = project_modules(project_root);

    match tool {
        BuildTool::Maven => {
            for module_root in module_roots {
                dirs.push(module_root.join("target/surefire-reports"));
                dirs.push(module_root.join("target/failsafe-reports"));
            }
        }
        BuildTool::Gradle => {
            for module_root in module_roots {
                dirs.push(module_root.join("build/test-results/test"));

                // Best-effort: include any custom test tasks under `build/test-results/*`.
                let test_results_dir = module_root.join("build/test-results");
                if let Ok(entries) = std::fs::read_dir(&test_results_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            dirs.push(path);
                        }
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

fn project_modules(project_root: &Path) -> Vec<PathBuf> {
    // Best-effort: if project loading fails (malformed build files, etc), fall back to scanning just
    // the workspace root as a single module. This keeps `nova/test/run` resilient while still
    // avoiding full workspace walks for report discovery.
    nova_project::load_project(project_root)
        .map(|project| project.modules.into_iter().map(|m| m.root).collect())
        .unwrap_or_else(|_| vec![project_root.to_path_buf()])
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

    let mut exact = HashSet::<String>::new();
    let mut prefixes = Vec::<String>::new();

    for req in requested {
        if req.contains('#') {
            exact.insert(req.clone());
        } else {
            prefixes.push(format!("{req}#"));
        }
    }

    cases
        .into_iter()
        .filter(|case| {
            exact.contains(&case.id) || prefixes.iter().any(|prefix| case.id.starts_with(prefix))
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
    fn discovers_maven_report_dirs_from_project_modules() {
        let root = fixture_root("maven-junit5").canonicalize().unwrap();
        let dirs = discover_report_dirs(&root, BuildTool::Maven).unwrap();
        assert!(dirs.contains(&root.join("target/surefire-reports")));
        assert!(dirs.contains(&root.join("target/failsafe-reports")));
    }

    #[test]
    fn discovers_gradle_report_dirs_from_project_modules() {
        let root = fixture_root("gradle-junit4").canonicalize().unwrap();
        let dirs = discover_report_dirs(&root, BuildTool::Gradle).unwrap();
        assert!(dirs.contains(&root.join("build/test-results/test")));
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let mut path = std::env::temp_dir();
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!("nova-testing-{prefix}-{unique}"));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn falls_back_to_cached_reports_when_reports_are_not_modified_since_cutoff() {
        let tmp = TempDir::new("cached-reports");
        let report_dir = tmp.path.join("target/surefire-reports");
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
        let cases =
            collect_and_parse_reports_in_dirs(&[report_dir], Some(cutoff), true, &[]).unwrap();

        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "com.example.CalculatorTest#adds");
        assert_eq!(cases[0].status, TestStatus::Passed);
    }
}
