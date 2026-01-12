use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Clone, Debug)]
enum MockResponse {
    Ok(String),
    Err(String),
}

#[derive(Clone, Default)]
struct QueryRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    responses: Arc<HashMap<String, MockResponse>>,
}

impl QueryRunner {
    fn new(responses: impl IntoIterator<Item = (String, MockResponse)>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(responses.into_iter().collect()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for QueryRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        let ["query", expr, "--output=label_kind"] = args else {
            anyhow::bail!("unexpected bazel invocation: {args:?}");
        };

        match self
            .responses
            .get(*expr)
            .unwrap_or_else(|| panic!("no mock response for expression {expr:?}"))
        {
            MockResponse::Ok(stdout) => Ok(CommandOutput {
                stdout: stdout.clone(),
                stderr: String::new(),
            }),
            MockResponse::Err(message) => anyhow::bail!("{message}"),
        }
    }
}

#[derive(Clone, Default)]
struct NoopRunner;

impl CommandRunner for NoopRunner {
    fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> anyhow::Result<CommandOutput> {
        anyhow::bail!("unexpected command execution")
    }
}

fn create_file(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, "// test\n").unwrap();
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[test]
fn path_to_label_root_package() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join("BUILD"), "# root package\n").unwrap();
    create_file(&dir.path().join("Hello.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//:Hello.java"));
}

#[test]
fn path_to_label_nested_package() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");
    create_file(&dir.path().join("java/Hello.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:Hello.java"));
}

#[test]
fn path_to_label_subpackage_prefers_nearest_build() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");
    write_file(&dir.path().join("java/com/BUILD"), "# java/com package\n");
    create_file(&dir.path().join("java/com/Hello.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/com/Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java/com:Hello.java"));
}

#[test]
fn path_to_label_normalizes_dotdots_within_workspace() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");
    create_file(&dir.path().join("java/com/Hello.java"));

    let messy_path = dir
        .path()
        .join("java")
        .join("..")
        .join("java")
        .join("com")
        .join("Hello.java");

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace.workspace_file_label(&messy_path).unwrap();
    assert_eq!(label.as_deref(), Some("//java:com/Hello.java"));
}

#[test]
fn path_to_label_normalizes_dotdots_that_escape_and_reenter_workspace() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");
    create_file(&dir.path().join("java/com/Hello.java"));

    let root_name = dir.path().file_name().unwrap();
    let messy_path = dir
        .path()
        .join("..")
        .join(root_name)
        .join("java")
        .join("com")
        .join("Hello.java");

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace.workspace_file_label(&messy_path).unwrap();
    assert_eq!(label.as_deref(), Some("//java:com/Hello.java"));
}

fn minimal_java_package(workspace_root: &Path) -> PathBuf {
    std::fs::write(workspace_root.join("WORKSPACE"), "# test\n").unwrap();
    write_file(&workspace_root.join("java/BUILD"), "# java package\n");
    let file = workspace_root.join("java/Hello.java");
    create_file(&file);
    file
}

#[test]
fn direct_owner_file_is_in_java_library_srcs() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners, vec!["//java:hello_lib".to_string()]);

    assert_eq!(
        runner.calls(),
        vec![vec![
            "query".to_string(),
            format!("same_pkg_direct_rdeps({file_label})"),
            "--output=label_kind".to_string()
        ]]
    );
}

#[test]
fn filegroup_chain_file_to_filegroup_to_java_library() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([
        (
            format!("same_pkg_direct_rdeps({file_label})"),
            MockResponse::Ok("filegroup rule //java:hello_files\n".to_string()),
        ),
        (
            "same_pkg_direct_rdeps(//java:hello_files)".to_string(),
            MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
        ),
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners, vec!["//java:hello_lib".to_string()]);
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "query".to_string(),
                format!("same_pkg_direct_rdeps({file_label})"),
                "--output=label_kind".to_string()
            ],
            vec![
                "query".to_string(),
                "same_pkg_direct_rdeps(//java:hello_files)".to_string(),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn nested_filegroups_are_traversed() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([
        (
            format!("same_pkg_direct_rdeps({file_label})"),
            MockResponse::Ok("filegroup rule //java:fg1\n".to_string()),
        ),
        (
            "same_pkg_direct_rdeps(//java:fg1)".to_string(),
            MockResponse::Ok("filegroup rule //java:fg2\n".to_string()),
        ),
        (
            "same_pkg_direct_rdeps(//java:fg2)".to_string(),
            MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
        ),
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners, vec!["//java:hello_lib".to_string()]);
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "query".to_string(),
                format!("same_pkg_direct_rdeps({file_label})"),
                "--output=label_kind".to_string()
            ],
            vec![
                "query".to_string(),
                "same_pkg_direct_rdeps(//java:fg1)".to_string(),
                "--output=label_kind".to_string()
            ],
            vec![
                "query".to_string(),
                "same_pkg_direct_rdeps(//java:fg2)".to_string(),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn multiple_owners_direct_and_via_filegroup_chain() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([
        (
            format!("same_pkg_direct_rdeps({file_label})"),
            MockResponse::Ok(
                "java_library rule //java:direct_lib\nfilegroup rule //java:hello_files\n"
                    .to_string(),
            ),
        ),
        (
            "same_pkg_direct_rdeps(//java:hello_files)".to_string(),
            MockResponse::Ok("java_library rule //java:other_lib\n".to_string()),
        ),
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(
        owners,
        vec!["//java:direct_lib".to_string(), "//java:other_lib".to_string()]
    );
}

#[test]
fn falls_back_to_rdeps_when_same_pkg_direct_rdeps_is_unsupported() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([
        (
            format!("same_pkg_direct_rdeps({file_label})"),
            MockResponse::Err("same_pkg_direct_rdeps unsupported".to_string()),
        ),
        (
            format!("rdeps(//java:*, {file_label}, 1)"),
            MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
        ),
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners, vec!["//java:hello_lib".to_string()]);

    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "query".to_string(),
                format!("same_pkg_direct_rdeps({file_label})"),
                "--output=label_kind".to_string()
            ],
            vec![
                "query".to_string(),
                format!("rdeps(//java:*, {file_label}, 1)"),
                "--output=label_kind".to_string()
            ]
        ]
    );
}
