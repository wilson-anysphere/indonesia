use crate::{NovaLspError, Result};
use nova_testing::schema::{TestDebugRequest, TestDiscoverRequest, TestRunRequest};

use super::build::BuildStatusGuard;

pub fn handle_discover(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: TestDiscoverRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    let resp = nova_testing::discover_tests(&req).map_err(map_testing_error)?;
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_run(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: TestRunRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    if req.project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }
    let root = std::path::PathBuf::from(&req.project_root);
    let mut status_guard = BuildStatusGuard::new(&root);

    let resp_result = nova_testing::run_tests(&req);
    match &resp_result {
        Ok(resp) if resp.success => status_guard.mark_success(),
        Ok(resp) => status_guard.mark_failure(Some(format!(
            "test run failed with exit code {}",
            resp.exit_code
        ))),
        Err(err) => status_guard.mark_failure(Some(err.to_string())),
    }

    let resp = resp_result.map_err(map_testing_error)?;
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_debug_configuration(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: TestDebugRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    let resp =
        nova_testing::debug::debug_configuration_for_request(&req).map_err(map_testing_error)?;
    serde_json::to_value(resp).map_err(|err| NovaLspError::Internal(err.to_string()))
}

fn map_testing_error(err: nova_testing::NovaTestingError) -> NovaLspError {
    match err {
        nova_testing::NovaTestingError::InvalidRequest(msg) => NovaLspError::InvalidParams(msg),
        other => NovaLspError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    #[test]
    fn test_run_marks_build_status_building_then_failed() {
        use std::time::{Duration, Instant};

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("maven-project");
        std::fs::create_dir_all(&root).unwrap();

        std::fs::write(
            root.join("pom.xml"),
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
        )
        .unwrap();

        // `nova-testing` prefers wrapper scripts over requiring a system `mvn`.
        let mvnw = root.join("mvnw");
        std::fs::write(
            &mvnw,
            r#"#!/usr/bin/env sh
set -e
echo started > .nova_test_started
while [ ! -f .nova_test_release ]; do
  sleep 0.05
done
mkdir -p target/surefire-reports
cat > target/surefire-reports/TEST-com.example.CalculatorTest.xml <<'EOF'
<testsuite tests="1" failures="1" errors="0" skipped="0" time="0.001">
  <testcase classname="com.example.CalculatorTest" name="adds" time="0.001">
    <failure message="boom">boom</failure>
  </testcase>
</testsuite>
EOF
exit 1
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&mvnw).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&mvnw, perms).unwrap();

        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            handle_run(serde_json::json!({
                "projectRoot": root_for_thread.to_string_lossy(),
                "buildTool": "maven",
                "tests": ["com.example.CalculatorTest#adds"],
            }))
        });

        let started_path = root.join(".nova_test_started");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started_path.is_file() {
            if Instant::now() >= deadline {
                panic!("timed out waiting for test runner to start");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let status = super::super::build::handle_build_status(serde_json::json!({
            "projectRoot": root.to_string_lossy(),
        }))
        .unwrap();
        assert_eq!(
            status.get("status").and_then(|v| v.as_str()),
            Some("building"),
            "expected build status to be building while tests are running: {status:?}"
        );

        std::fs::write(root.join(".nova_test_release"), "").unwrap();
        let run_result = handle.join().unwrap().unwrap();
        assert_eq!(
            run_result.get("success").and_then(|v| v.as_bool()),
            Some(false),
            "expected test run to report failure: {run_result:?}"
        );

        let status = super::super::build::handle_build_status(serde_json::json!({
            "projectRoot": root.to_string_lossy(),
        }))
        .unwrap();
        assert_eq!(
            status.get("status").and_then(|v| v.as_str()),
            Some("failed"),
            "expected build status to be failed after test run failure: {status:?}"
        );
        assert!(
            status
                .get("lastError")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("exit code 1"),
            "expected lastError to include exit code: {status:?}"
        );
    }
}
