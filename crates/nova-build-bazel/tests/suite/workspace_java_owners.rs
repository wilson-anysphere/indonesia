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
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

impl CommandRunner for QueryRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
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
fn path_to_label_root_package_subdir_file() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join("BUILD"), "# root package\n").unwrap();
    create_file(&dir.path().join("subdir/Hello.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("subdir/Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//:subdir/Hello.java"));
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
#[cfg(unix)]
fn path_to_label_accepts_canonical_paths_when_root_is_symlink() {
    use std::os::unix::fs::symlink;

    let real = tempdir().unwrap();
    std::fs::write(real.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&real.path().join("java/BUILD"), "# java package\n");
    create_file(&real.path().join("java/Hello.java"));

    let link_parent = tempdir().unwrap();
    let link_root = link_parent.path().join("ws");
    symlink(real.path(), &link_root).unwrap();

    let workspace = BazelWorkspace::new(link_root, NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(&real.path().join("java/Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:Hello.java"));
}

#[test]
#[cfg(unix)]
fn path_to_label_accepts_nonexistent_canonical_paths_when_root_is_symlink() {
    use std::os::unix::fs::symlink;

    let real = tempdir().unwrap();
    std::fs::write(real.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&real.path().join("java/BUILD"), "# java package\n");

    let link_parent = tempdir().unwrap();
    let link_root = link_parent.path().join("ws");
    symlink(real.path(), &link_root).unwrap();

    let workspace = BazelWorkspace::new(link_root, NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(&real.path().join("java/NewFile.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:NewFile.java"));
}

#[test]
#[cfg(unix)]
fn path_to_label_accepts_nonexistent_file_through_alternate_symlink_path() {
    use std::os::unix::fs::symlink;

    let real = tempdir().unwrap();
    std::fs::write(real.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&real.path().join("java/BUILD"), "# java package\n");

    let link_parent = tempdir().unwrap();
    let link_root_a = link_parent.path().join("ws_a");
    let link_root_b = link_parent.path().join("ws_b");
    symlink(real.path(), &link_root_a).unwrap();
    symlink(real.path(), &link_root_b).unwrap();

    let workspace = BazelWorkspace::new(link_root_a, NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(&link_root_b.join("java/NewFile.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:NewFile.java"));
}

#[test]
fn path_to_label_recognizes_build_bazel() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD.bazel"), "# java package\n");
    create_file(&dir.path().join("java/com/Hello.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/com/Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:com/Hello.java"));
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

#[test]
fn path_to_label_returns_none_when_no_build_file_found() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    create_file(&dir.path().join("java/Hello.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/Hello.java"))
        .unwrap();
    assert_eq!(label, None);
}

#[test]
fn workspace_file_label_is_cached_until_build_file_invalidation() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");
    create_file(&dir.path().join("java/Hello.java"));

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:Hello.java"));

    // Remove the BUILD file; without caching this would now resolve to `None`.
    std::fs::remove_file(dir.path().join("java/BUILD")).unwrap();

    let cached = workspace
        .workspace_file_label(Path::new("java/Hello.java"))
        .unwrap();
    assert_eq!(cached.as_deref(), Some("//java:Hello.java"));

    workspace
        .invalidate_changed_files(&[PathBuf::from("java/BUILD")])
        .unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/Hello.java"))
        .unwrap();
    assert_eq!(label, None);
}

#[test]
fn workspace_package_cache_is_reused_across_multiple_files() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");
    create_file(&dir.path().join("java/A.java"));
    create_file(&dir.path().join("java/B.java"));

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/A.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:A.java"));

    // Remove the BUILD file. Without a package-dir cache, a fresh lookup for a different file in
    // the same directory would now return `None`.
    std::fs::remove_file(dir.path().join("java/BUILD")).unwrap();

    let cached = workspace
        .workspace_file_label(Path::new("java/B.java"))
        .unwrap();
    assert_eq!(cached.as_deref(), Some("//java:B.java"));

    workspace
        .invalidate_changed_files(&[PathBuf::from("java/BUILD")])
        .unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/B.java"))
        .unwrap();
    assert_eq!(label, None);
}

#[test]
fn workspace_file_label_cache_is_not_cleared_by_bazelrc_changes() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join(".bazelrc"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");
    create_file(&dir.path().join("java/Hello.java"));

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("java/Hello.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//java:Hello.java"));

    // Remove the BUILD file; without a cache hit, label resolution would now return `None`.
    std::fs::remove_file(dir.path().join("java/BUILD")).unwrap();

    // `.bazelrc` can affect query/aquery evaluation, but it cannot change Bazel package boundaries.
    // Keep the file-label cache to avoid unnecessary filesystem scans.
    std::fs::write(dir.path().join(".bazelrc"), "# changed\n").unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from(".bazelrc")])
        .unwrap();

    let cached = workspace
        .workspace_file_label(Path::new("java/Hello.java"))
        .unwrap();
    assert_eq!(cached.as_deref(), Some("//java:Hello.java"));
}

#[test]
fn owning_targets_returns_empty_when_no_build_file_found() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    create_file(&dir.path().join("java/Hello.java"));

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let owners = workspace
        .java_owning_targets_for_file(Path::new("java/Hello.java"))
        .unwrap();
    assert!(owners.is_empty());
}

#[test]
fn owning_targets_returns_empty_when_file_does_not_exist() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");

    // Don't create the file on disk.
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let owners = workspace
        .java_owning_targets_for_file(Path::new("java/Hello.java"))
        .unwrap();
    assert!(owners.is_empty());
}

#[test]
fn path_to_label_errors_for_file_outside_workspace() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");

    let outside = tempdir().unwrap();
    create_file(&outside.path().join("Hello.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let err = workspace
        .workspace_file_label(&outside.path().join("Hello.java"))
        .unwrap_err();
    assert!(
        err.to_string().contains("outside the Bazel workspace root"),
        "unexpected error: {err}"
    );
}

#[test]
fn path_to_label_errors_when_relative_path_escapes_workspace_root() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let err = workspace
        .workspace_file_label(Path::new("..").join("outside").join("Hello.java").as_path())
        .unwrap_err();
    assert!(
        err.to_string().contains("path escapes workspace root"),
        "unexpected error: {err}"
    );
}

#[test]
fn owning_targets_errors_when_relative_path_escapes_workspace_root() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# java package\n");

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let err = workspace
        .java_owning_targets_for_file(Path::new("..").join("outside").join("Hello.java").as_path())
        .unwrap_err();
    assert!(
        err.to_string().contains("path escapes workspace root"),
        "unexpected error: {err}"
    );
}

fn minimal_java_package(workspace_root: &Path) -> PathBuf {
    std::fs::write(workspace_root.join("WORKSPACE"), "# test\n").unwrap();
    write_file(&workspace_root.join("java/BUILD"), "# java package\n");
    let file = workspace_root.join("java/Hello.java");
    create_file(&file);
    file
}

fn minimal_root_package(workspace_root: &Path) -> PathBuf {
    std::fs::write(workspace_root.join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(workspace_root.join("BUILD"), "# root package\n").unwrap();
    let file = workspace_root.join("subdir/Foo.java");
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
fn alias_chain_file_to_alias_to_java_library() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([
        (
            format!("same_pkg_direct_rdeps({file_label})"),
            MockResponse::Ok("alias rule //java:hello_alias\n".to_string()),
        ),
        (
            "same_pkg_direct_rdeps(//java:hello_alias)".to_string(),
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
                "same_pkg_direct_rdeps(//java:hello_alias)".to_string(),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn does_not_expand_java_rules_to_find_higher_level_binaries() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([
        (
            format!("same_pkg_direct_rdeps({file_label})"),
            MockResponse::Ok("filegroup rule //java:srcs\n".to_string()),
        ),
        (
            "same_pkg_direct_rdeps(//java:srcs)".to_string(),
            MockResponse::Ok("java_library rule //java:lib\n".to_string()),
        ),
        // Intentionally omit a response for `same_pkg_direct_rdeps(//java:lib)` to assert the
        // implementation stops traversal at java rules.
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners, vec!["//java:lib".to_string()]);
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
                "same_pkg_direct_rdeps(//java:srcs)".to_string(),
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
        vec![
            "//java:direct_lib".to_string(),
            "//java:other_lib".to_string()
        ]
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

#[test]
fn does_not_retry_same_pkg_direct_rdeps_after_failure() {
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
            MockResponse::Ok("filegroup rule //java:hello_files\n".to_string()),
        ),
        (
            "rdeps(//java:*, //java:hello_files, 1)".to_string(),
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
            ],
            vec![
                "query".to_string(),
                "rdeps(//java:*, //java:hello_files, 1)".to_string(),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn rdeps_fallback_batches_frontier_union_per_layer() {
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
            MockResponse::Ok("filegroup rule //java:fg1\nfilegroup rule //java:fg2\n".to_string()),
        ),
        (
            "rdeps(//java:*, (//java:fg1 + //java:fg2), 1)".to_string(),
            MockResponse::Ok(
                "java_library rule //java:lib1\njava_library rule //java:lib2\n".to_string(),
            ),
        ),
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(
        owners,
        vec!["//java:lib1".to_string(), "//java:lib2".to_string()]
    );

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
            ],
            vec![
                "query".to_string(),
                "rdeps(//java:*, (//java:fg1 + //java:fg2), 1)".to_string(),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn run_target_closure_batches_frontier_union_per_step() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";
    let run_target = "//app:run";

    let runner = QueryRunner::new([
        (
            format!("rdeps(deps({run_target}), ({file_label}), 1)"),
            MockResponse::Ok("filegroup rule //java:fg1\nalias rule //java:alias1\n".to_string()),
        ),
        (
            format!("rdeps(deps({run_target}), (//java:alias1 + //java:fg1), 1)"),
            MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
        ),
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace
        .java_owning_targets_for_file_in_run_target_closure(&file, run_target)
        .unwrap();
    assert_eq!(owners, vec!["//java:hello_lib".to_string()]);

    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "query".to_string(),
                format!("rdeps(deps({run_target}), ({file_label}), 1)"),
                "--output=label_kind".to_string()
            ],
            vec![
                "query".to_string(),
                format!("rdeps(deps({run_target}), (//java:alias1 + //java:fg1), 1)"),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn owning_targets_are_cached_per_file() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

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
fn owning_targets_cache_hits_across_absolute_and_relative_paths() {
    let dir = tempdir().unwrap();
    let file_abs = minimal_java_package(dir.path());
    let file_rel = PathBuf::from("java/Hello.java");
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file_abs).unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file_rel).unwrap();
    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);
    assert_eq!(runner.calls().len(), 1);
}

#[test]
fn owning_targets_cache_is_cleared_by_invalidate_changed_files() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from("java/BUILD")])
        .unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();

    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

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
                format!("same_pkg_direct_rdeps({file_label})"),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn owning_targets_cache_is_not_cleared_by_invalidate_changed_files_in_bazelignored_dir() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    std::fs::write(dir.path().join(".bazelignore"), "ignored\n").unwrap();
    write_file(&dir.path().join("ignored/BUILD"), "# ignored package\n");

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from("ignored/BUILD")])
        .unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();

    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

    // Cache should still hit since BUILD changes under `.bazelignore` do not affect Bazel queries.
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
fn owning_targets_cache_is_cleared_by_invalidate_changed_files_on_bazelignore() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);

    // Add `.bazelignore` to exclude the `java/` package and notify the workspace; cached owning
    // targets should be invalidated.
    std::fs::write(dir.path().join(".bazelignore"), "java\n").unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from(".bazelignore")])
        .unwrap();

    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();
    assert!(owners2.is_empty());

    // The second lookup should return early (ignored path) without invoking Bazel.
    assert_eq!(runner.calls().len(), 1);
}

#[test]
fn owning_targets_cache_is_not_cleared_by_invalidate_changed_files_on_source_edits() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from("java/Hello.java")])
        .unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();

    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

    // The second lookup should still hit the in-memory cache.
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
fn owning_targets_are_cached_per_file_in_run_target_closure() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";
    let run_target = "//app:run";

    let runner = QueryRunner::new([(
        format!("rdeps(deps({run_target}), ({file_label}), 1)"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace
        .java_owning_targets_for_file_in_run_target_closure(&file, run_target)
        .unwrap();
    let owners2 = workspace
        .java_owning_targets_for_file_in_run_target_closure(&file, run_target)
        .unwrap();
    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

    assert_eq!(
        runner.calls(),
        vec![vec![
            "query".to_string(),
            format!("rdeps(deps({run_target}), ({file_label}), 1)"),
            "--output=label_kind".to_string()
        ]]
    );
}

#[test]
fn owning_targets_cache_is_cleared_by_invalidate_changed_files_on_bazelrc() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from(".bazelrc")])
        .unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();

    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

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
                format!("same_pkg_direct_rdeps({file_label})"),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn owning_targets_cache_is_cleared_by_invalidate_changed_files_on_bazelrc_import() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    // `.bazelrc` can be split across multiple files via `import` / `try-import`; those imported
    // files can affect query results and should clear the owning-target cache.
    write_file(&dir.path().join(".bazelrc"), "try-import tools/bazel.rc\n");
    write_file(
        &dir.path().join("tools/bazel.rc"),
        "query --output=label_kind\n",
    );

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from("tools/bazel.rc")])
        .unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();

    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

    // Cache should have been cleared, resulting in a second query.
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
                format!("same_pkg_direct_rdeps({file_label})"),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
fn owning_targets_cache_is_cleared_by_invalidate_changed_files_on_bazelrc_import_without_rc_ext() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";

    // Repositories may import bazelrc fragments with arbitrary filenames/extensions.
    write_file(
        &dir.path().join(".bazelrc"),
        "try-import tools/bazelconfig\n",
    );
    write_file(
        &dir.path().join("tools/bazelconfig"),
        "query --output=label_kind\n",
    );

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from("tools/bazelconfig")])
        .unwrap();
    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();

    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(owners2, owners1);

    // Cache should have been cleared, resulting in a second query.
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
                format!("same_pkg_direct_rdeps({file_label})"),
                "--output=label_kind".to_string()
            ],
        ]
    );
}

#[test]
#[cfg(unix)]
fn invalidate_changed_files_matches_bazelrc_import_paths_when_root_is_symlink() {
    use std::os::unix::fs::symlink;

    let real = tempdir().unwrap();
    minimal_java_package(real.path());

    // `.bazelrc` imports a file without a `.rc` extension.
    write_file(
        &real.path().join(".bazelrc"),
        "try-import tools/bazelconfig\n",
    );
    write_file(
        &real.path().join("tools/bazelconfig"),
        "query --output=label_kind\n",
    );

    let link_parent = tempdir().unwrap();
    let link_root = link_parent.path().join("ws");
    symlink(real.path(), &link_root).unwrap();

    let file_label = "//java:Hello.java";
    let file = real.path().join("java/Hello.java");

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(link_root, runner.clone()).unwrap();

    // Cache owning targets using a canonical (real) file path, while the workspace root is a
    // symlink.
    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(runner.calls().len(), 1);

    // Invalidate using the canonical path to the imported file; this should still clear caches
    // even though the workspace root is a symlink.
    workspace
        .invalidate_changed_files(&[real.path().join("tools/bazelconfig")])
        .unwrap();

    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners2, owners1);
    assert_eq!(runner.calls().len(), 2);
}

#[test]
#[cfg(unix)]
fn invalidate_changed_files_matches_bazelrc_import_paths_for_deleted_files_when_changed_path_is_symlink(
) {
    use std::os::unix::fs::symlink;

    let real = tempdir().unwrap();
    minimal_java_package(real.path());

    // `.bazelrc` imports a file without a `.rc` extension.
    write_file(
        &real.path().join(".bazelrc"),
        "try-import tools/bazelconfig\n",
    );
    write_file(
        &real.path().join("tools/bazelconfig"),
        "query --output=label_kind\n",
    );

    let link_parent = tempdir().unwrap();
    let link_root = link_parent.path().join("ws");
    symlink(real.path(), &link_root).unwrap();

    let file_label = "//java:Hello.java";
    let file = real.path().join("java/Hello.java");

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //java:hello_lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(real.path().to_path_buf(), runner.clone()).unwrap();

    // Prime the owning-target cache.
    let owners1 = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners1, vec!["//java:hello_lib".to_string()]);
    assert_eq!(runner.calls().len(), 1);

    // Delete the imported file, then invalidate using the symlink path to the (now missing) file.
    std::fs::remove_file(real.path().join("tools/bazelconfig")).unwrap();
    workspace
        .invalidate_changed_files(&[link_root.join("tools/bazelconfig")])
        .unwrap();

    let owners2 = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners2, owners1);
    assert_eq!(
        runner.calls().len(),
        2,
        "expected invalidate_changed_files to clear the owning-target cache"
    );
}

#[test]
fn owning_targets_run_target_closure_cache_key_includes_run_target() {
    let dir = tempdir().unwrap();
    let file = minimal_java_package(dir.path());
    let file_label = "//java:Hello.java";
    let run_target1 = "//app:run1";
    let run_target2 = "//app:run2";

    let runner = QueryRunner::new([
        (
            format!("rdeps(deps({run_target1}), ({file_label}), 1)"),
            MockResponse::Ok("java_library rule //java:lib1\n".to_string()),
        ),
        (
            format!("rdeps(deps({run_target2}), ({file_label}), 1)"),
            MockResponse::Ok("java_library rule //java:lib2\n".to_string()),
        ),
    ]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners1 = workspace
        .java_owning_targets_for_file_in_run_target_closure(&file, run_target1)
        .unwrap();
    let owners2 = workspace
        .java_owning_targets_for_file_in_run_target_closure(&file, run_target2)
        .unwrap();
    assert_eq!(owners1, vec!["//java:lib1".to_string()]);
    assert_eq!(owners2, vec!["//java:lib2".to_string()]);
    assert_eq!(runner.calls().len(), 2);
}

#[test]
fn root_package_owner_resolution_uses_root_file_label() {
    let dir = tempdir().unwrap();
    let file = minimal_root_package(dir.path());
    let file_label = "//:subdir/Foo.java";

    let runner = QueryRunner::new([(
        format!("same_pkg_direct_rdeps({file_label})"),
        MockResponse::Ok("java_library rule //:lib\n".to_string()),
    )]);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let owners = workspace.java_owning_targets_for_file(&file).unwrap();
    assert_eq!(owners, vec!["//:lib".to_string()]);
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
fn bazelignore_excludes_file_label_and_owning_targets() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join(".bazelignore"), "ignored\n").unwrap();

    write_file(&dir.path().join("ignored/BUILD"), "# ignored package\n");
    create_file(&dir.path().join("ignored/Foo.java"));

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("ignored/Foo.java"))
        .unwrap();
    assert_eq!(label, None);

    let owners = workspace
        .java_owning_targets_for_file(Path::new("ignored/Foo.java"))
        .unwrap();
    assert!(owners.is_empty());
}

#[test]
fn bazelignore_prefix_matching_is_component_based() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join(".bazelignore"), "ignored\n").unwrap();

    write_file(&dir.path().join("ignored2/BUILD"), "# not ignored\n");
    create_file(&dir.path().join("ignored2/Foo.java"));

    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("ignored2/Foo.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//ignored2:Foo.java"));
}

#[test]
fn bazelignore_ignores_invalid_entries_that_escape_workspace() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join(".bazelignore"), "../escape\nignored\n").unwrap();

    write_file(&dir.path().join("ignored/BUILD"), "# ignored package\n");
    create_file(&dir.path().join("ignored/Foo.java"));

    // Still applies valid entries and does not error due to invalid ones.
    let workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("ignored/Foo.java"))
        .unwrap();
    assert_eq!(label, None);
}

#[test]
fn bazelignore_is_reloaded_after_invalidate_changed_files() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join(".bazelignore"), "ignored\n").unwrap();

    write_file(&dir.path().join("ignored/BUILD"), "# ignored package\n");
    create_file(&dir.path().join("ignored/Foo.java"));

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new("ignored/Foo.java"))
        .unwrap();
    assert_eq!(label, None);

    // Remove the ignore entry and notify the workspace; it should drop the cached ignore prefixes.
    std::fs::write(dir.path().join(".bazelignore"), "").unwrap();
    workspace
        .invalidate_changed_files(&[PathBuf::from(".bazelignore")])
        .unwrap();

    let label = workspace
        .workspace_file_label(Path::new("ignored/Foo.java"))
        .unwrap();
    assert_eq!(label.as_deref(), Some("//ignored:Foo.java"));
}

#[test]
#[cfg(unix)]
fn bazelignore_applies_to_canonical_paths_when_root_is_symlink() {
    use std::os::unix::fs::symlink;

    let real = tempdir().unwrap();
    std::fs::write(real.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(real.path().join(".bazelignore"), "ignored\n").unwrap();
    write_file(&real.path().join("ignored/BUILD"), "# ignored package\n");
    create_file(&real.path().join("ignored/Foo.java"));

    let link_parent = tempdir().unwrap();
    let link_root = link_parent.path().join("ws");
    symlink(real.path(), &link_root).unwrap();

    let workspace = BazelWorkspace::new(link_root, NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(&real.path().join("ignored/Foo.java"))
        .unwrap();
    assert_eq!(label, None);
}

#[test]
fn git_dir_is_ignored_by_default() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();

    // Even if `.git` contains BUILD files, Bazel treats it as outside the package universe.
    write_file(&dir.path().join(".git/BUILD"), "# pretend\n");
    create_file(&dir.path().join(".git/Foo.java"));

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let label = workspace
        .workspace_file_label(Path::new(".git/Foo.java"))
        .unwrap();
    assert_eq!(label, None);

    let owners = workspace
        .java_owning_targets_for_file(Path::new(".git/Foo.java"))
        .unwrap();
    assert!(owners.is_empty());
}
