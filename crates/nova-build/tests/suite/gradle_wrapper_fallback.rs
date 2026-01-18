use nova_build::{BuildCache, CommandOutput, CommandRunner, GradleBuild, GradleConfig};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct Invocation {
    program: PathBuf,
    args: Vec<String>,
}

#[derive(Debug)]
struct RecordingRunner {
    invocations: Mutex<Vec<Invocation>>,
    output: CommandOutput,
}

impl RecordingRunner {
    fn new(output: CommandOutput) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            output,
        }
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, _cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .push(Invocation {
                program: program.to_path_buf(),
                args: args.to_vec(),
            });
        Ok(self.output.clone())
    }
}

fn exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(code << 8)
}

fn output(code: i32, stdout: &str) -> CommandOutput {
    CommandOutput {
        status: exit_status(code),
        stdout: stdout.to_string(),
        stderr: String::new(),
        truncated: false,
    }
}

#[test]
fn gradle_wrapper_without_exec_bit_falls_back_to_sh() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let wrapper = project_root.join("gradlew");
    std::fs::write(&wrapper, "#!/bin/sh\necho wrapper\n").unwrap();
    let mut perms = std::fs::metadata(&wrapper).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&wrapper, perms).unwrap();

    let projects_json = format!(
        "NOVA_PROJECTS_BEGIN\n{{\"projects\":[{{\"path\":\":\",\"projectDir\":\"{}\"}}]}}\nNOVA_PROJECTS_END\n",
        project_root.to_string_lossy()
    );

    let runner = Arc::new(RecordingRunner::new(output(0, &projects_json)));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());

    let cache = BuildCache::new(tmp.path().join("cache"));
    let _ = gradle.projects(&project_root, &cache).unwrap();

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].program, PathBuf::from("sh"));
    assert_eq!(
        invocations[0].args.first().unwrap(),
        &wrapper.to_string_lossy().to_string()
    );
}

#[test]
fn executable_gradle_wrapper_runs_directly() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let wrapper = project_root.join("gradlew");
    std::fs::write(&wrapper, "#!/bin/sh\necho wrapper\n").unwrap();
    let mut perms = std::fs::metadata(&wrapper).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper, perms).unwrap();

    let projects_json = format!(
        "NOVA_PROJECTS_BEGIN\n{{\"projects\":[{{\"path\":\":\",\"projectDir\":\"{}\"}}]}}\nNOVA_PROJECTS_END\n",
        project_root.to_string_lossy()
    );

    let runner = Arc::new(RecordingRunner::new(output(0, &projects_json)));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());

    let cache = BuildCache::new(tmp.path().join("cache"));
    let _ = gradle.projects(&project_root, &cache).unwrap();

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].program, wrapper);
    assert_eq!(invocations[0].args.first().unwrap(), "--no-daemon");
}
