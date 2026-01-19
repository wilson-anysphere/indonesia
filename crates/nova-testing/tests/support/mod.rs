use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    original_path: Option<OsString>,
}

impl EnvGuard {
    #[track_caller]
    pub fn prepend_path(bin_dir: &Path) -> io::Result<Self> {
        let lock = match ENV_LOCK.lock() {
            Ok(lock) => lock,
            Err(err) => {
                let loc = Location::caller();
                tracing::error!(
                    target = "nova.testing",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    error = %err,
                    "env lock poisoned; continuing with recovered guard"
                );
                err.into_inner()
            }
        };

        let original_path = env::var_os("PATH");
        let mut paths = Vec::new();
        paths.push(bin_dir.to_path_buf());
        if let Some(ref existing) = original_path {
            paths.extend(env::split_paths(existing));
        }
        let joined =
            env::join_paths(paths).map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        env::set_var("PATH", &joined);

        Ok(Self {
            _lock: lock,
            original_path,
        })
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.original_path.take() {
            Some(path) => env::set_var("PATH", path),
            None => env::remove_var("PATH"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ProjectMarker {
    Maven,
    Gradle,
}

pub struct Workspace {
    _temp_dir: TempDir,
    pub project_root: PathBuf,
    pub bin_dir: PathBuf,
}

impl Workspace {
    pub fn new(marker: ProjectMarker) -> io::Result<Self> {
        let temp_dir = TempDir::new()?;
        let bin_dir = temp_dir.path().join("bin");
        let project_root = temp_dir.path().join("project");

        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&project_root)?;

        match marker {
            ProjectMarker::Maven => {
                fs::write(project_root.join("pom.xml"), "<project/>")?;
            }
            ProjectMarker::Gradle => {
                fs::write(project_root.join("build.gradle"), "// fake gradle project")?;
            }
        }

        Ok(Self {
            _temp_dir: temp_dir,
            project_root,
            bin_dir,
        })
    }
}

pub const JUNIT_XML_FULL: &str = r#"
<testsuite name="com.example.CalculatorTest" tests="6" failures="2" errors="0" skipped="1" time="0.012">
  <testcase classname="com.example.CalculatorTest" name="adds" time="0.001"/>
  <testcase classname="com.example.CalculatorTest" name="divides" time="0.002">
    <failure message="boom" type="java.lang.AssertionError">trace</failure>
  </testcase>
  <testcase classname="com.example.CalculatorTest" name="skipped" time="0.000">
    <skipped/>
  </testcase>
  <testcase classname="com.example.CalculatorTest" name="parameterizedAdds(int)[1]" time="0.001"/>
  <testcase classname="com.example.CalculatorTest" name="parameterizedAdds(int)[2]" time="0.002">
    <failure message="boom2" type="java.lang.AssertionError">trace2</failure>
  </testcase>
  <testcase classname="com.example.OtherTest" name="other" time="0.001"/>
</testsuite>
"#;

pub const JUNIT_XML_STALE: &str = r#"
<testsuite name="com.example.StaleTest" tests="1" failures="0" errors="0" skipped="0" time="0.001">
  <testcase classname="com.example.StaleTest" name="stale" time="0.001"/>
</testsuite>
"#;

pub fn write_fake_maven(
    bin_dir: &Path,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    junit_xml: &str,
) -> io::Result<PathBuf> {
    write_fake_tool(
        bin_dir,
        "mvn",
        exit_code,
        stdout,
        stderr,
        "target/surefire-reports",
        "TEST-com.example.CalculatorTest.xml",
        junit_xml,
    )
}

pub fn write_fake_gradle(
    bin_dir: &Path,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    junit_xml: &str,
) -> io::Result<PathBuf> {
    write_fake_tool(
        bin_dir,
        "gradle",
        exit_code,
        stdout,
        stderr,
        "build/test-results/test",
        "TEST-com.example.CalculatorTest.xml",
        junit_xml,
    )
}

fn write_fake_tool(
    bin_dir: &Path,
    tool_name: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    report_dir: &str,
    report_file: &str,
    junit_xml: &str,
) -> io::Result<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let exe_path = bin_dir.join(fake_executable_name(tool_name));

    let script = if cfg!(windows) {
        windows_cmd_script(
            exit_code,
            stdout,
            stderr,
            report_dir,
            report_file,
            junit_xml,
        )
    } else {
        sh_script(
            exit_code,
            stdout,
            stderr,
            report_dir,
            report_file,
            junit_xml,
        )
    };

    fs::write(&exe_path, script)?;
    make_executable(&exe_path)?;
    Ok(exe_path)
}

fn sh_script(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    report_dir: &str,
    report_file: &str,
    junit_xml: &str,
) -> String {
    format!(
        r#"#!/bin/sh
set -eu
echo "{stdout}"
echo "{stderr}" 1>&2
echo "ARGS:$*"
mkdir -p "{report_dir}"
cat > "{report_dir}/{report_file}" <<'EOF'
{junit_xml}
EOF
exit {exit_code}
"#
    )
}

fn windows_cmd_script(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    report_dir: &str,
    report_file: &str,
    junit_xml: &str,
) -> String {
    let report_dir = report_dir.replace('/', "\\");
    let report_path = format!("{report_dir}\\{report_file}");
    let xml = escape_cmd_echo(&compact_xml(junit_xml));

    format!(
        r#"@echo off
echo {stdout}
echo {stderr} 1>&2
echo ARGS:%*
if not exist "{report_dir}" mkdir "{report_dir}"
echo {xml} > "{report_path}"
exit /b {exit_code}
"#
    )
}

fn compact_xml(xml: &str) -> String {
    xml.lines().map(str::trim).collect()
}

fn escape_cmd_echo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '^' => out.push_str("^^"),
            '<' => out.push_str("^<"),
            '>' => out.push_str("^>"),
            '&' => out.push_str("^&"),
            '|' => out.push_str("^|"),
            '%' => out.push_str("%%"),
            _ => out.push(ch),
        }
    }
    out
}

fn fake_executable_name(tool_name: &str) -> String {
    if cfg!(windows) {
        format!("{tool_name}.cmd")
    } else {
        tool_name.to_string()
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

#[cfg(windows)]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}
