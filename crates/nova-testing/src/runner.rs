use crate::report::{merge_case_results, parse_junit_report};
use crate::schema::{
    BuildTool, TestCaseResult, TestRunRequest, TestRunResponse, TestRunSummary, TestStatus,
};
use crate::{NovaTestingError, Result, SCHEMA_VERSION};
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
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
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;

    let cutoff = started_at.checked_sub(Duration::from_secs(2));
    let mut tests = collect_and_parse_reports(&project_root, tool, cutoff)?;
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
    command
        .output()
        .map_err(|err| NovaTestingError::CommandFailed(format!("{desc}: {err}")))
}

fn collect_and_parse_reports(
    project_root: &Path,
    tool: BuildTool,
    modified_since: Option<SystemTime>,
) -> Result<Vec<TestCaseResult>> {
    let report_dirs = discover_report_dirs(project_root, tool)?;

    let mut xml_files = Vec::new();
    for dir in report_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in WalkDir::new(&dir).follow_links(false).into_iter() {
            let entry = entry.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|e| e.to_str()) == Some("xml") {
                if let Some(cutoff) = modified_since {
                    if !is_modified_since(entry.path(), cutoff) {
                        continue;
                    }
                }
                xml_files.push(entry.path().to_path_buf());
            }
        }
    }

    xml_files.sort();

    // Deduplicate by id to make results stable when multiple report files contain overlapping suites.
    let mut by_id: BTreeMap<String, TestCaseResult> = BTreeMap::new();
    for path in xml_files {
        for case in parse_junit_report(&path)? {
            match by_id.get_mut(&case.id) {
                Some(existing) => merge_case_results(existing, case),
                None => {
                    by_id.insert(case.id.clone(), case);
                }
            }
        }
    }

    Ok(by_id.into_values().collect())
}

fn discover_report_dirs(project_root: &Path, tool: BuildTool) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    match tool {
        BuildTool::Maven => {
            dirs.extend(find_dirs(project_root, |path| {
                let name = path.file_name().and_then(|s| s.to_str());
                let is_reports = matches!(name, Some("surefire-reports" | "failsafe-reports"));
                if !is_reports {
                    return false;
                }
                path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(OsStr::to_str)
                    == Some("target")
            })?);
        }
        BuildTool::Gradle => {
            // Gradle places JUnit XML under `build/test-results/test` per module.
            dirs.extend(find_dirs(project_root, |path| {
                if path.file_name().and_then(|s| s.to_str()) != Some("test") {
                    return false;
                }
                let parent = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(OsStr::to_str);
                let grandparent = path
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.file_name())
                    .and_then(OsStr::to_str);
                parent == Some("test-results") && grandparent == Some("build")
            })?);
        }
        BuildTool::Auto => {}
    }

    // For simple projects (no build system), there are no reports to scan.
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

fn find_dirs(project_root: &Path, predicate: impl Fn(&Path) -> bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(project_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            // Don't skip `target`/`build` since those are where reports live.
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | ".gradle" | "node_modules" | ".idea" | "out"
            )
        })
    {
        let entry = entry.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
        if !entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        if predicate(path) {
            out.push(path.to_path_buf());
        }
    }
    Ok(out)
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
}
