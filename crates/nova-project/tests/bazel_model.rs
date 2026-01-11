use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nova_build_bazel::{CommandOutput, CommandRunner};
use nova_project::{BazelLoadOptions, JavaLanguageLevel, JavaVersion, LoadOptions, SourceRootKind};

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

fn read_fixture(rel: &str) -> String {
    fs::read_to_string(testdata_path(rel)).expect("read fixture")
}

#[derive(Clone, Default)]
struct MockRunner {
    inner: Arc<MockRunnerInner>,
}

#[derive(Default)]
struct MockRunnerInner {
    responses: Mutex<BTreeMap<(String, Vec<String>), CommandOutput>>,
}

impl MockRunner {
    fn with_stdout(mut self, program: &str, args: &[&str], stdout: String) -> Self {
        let key = (
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        );
        let mut guard = self.inner.responses.lock().expect("responses lock");
        guard.insert(
            key,
            CommandOutput {
                stdout,
                stderr: String::new(),
            },
        );
        self
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        let key = (
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        );
        let guard = self.inner.responses.lock().expect("responses lock");
        guard
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unexpected command: {program} {:?}", args))
    }
}

fn options_with_bazel_enabled() -> LoadOptions {
    LoadOptions {
        bazel: BazelLoadOptions {
            enable_target_loading: true,
            target_limit: 200,
            targets: None,
        },
        ..LoadOptions::default()
    }
}

#[test]
fn loads_bazel_targets_as_module_configs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    let query = read_fixture("bazel/query.txt");
    let aquery_lib = read_fixture("bazel/aquery_lib.textproto");
    let aquery_test = read_fixture("bazel/aquery_lib_test.textproto");
    let aquery_alias = read_fixture("bazel/aquery_alias.textproto");

    let runner = MockRunner::default()
        .with_stdout("bazel", &["query", r#"kind("java_.* rule", //...)"#], query)
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:lib)"#,
            ],
            aquery_lib.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:lib))"#,
            ],
            aquery_lib,
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:lib_test)"#,
            ],
            aquery_test.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:lib_test))"#,
            ],
            aquery_test,
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:alias)"#,
            ],
            aquery_alias.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:alias))"#,
            ],
            aquery_alias,
        );

    let options = options_with_bazel_enabled();
    let model = nova_project::load_bazel_workspace_model_with_runner(tmp.path(), &options, runner)
        .expect("load bazel workspace model");

    assert_eq!(
        model
            .modules
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["//java/com/example:lib", "//java/com/example:lib_test"]
    );

    let lib = model
        .modules
        .iter()
        .find(|m| m.id == "//java/com/example:lib")
        .unwrap();
    assert_eq!(lib.source_roots.len(), 1);
    assert_eq!(lib.source_roots[0].kind, SourceRootKind::Main);
    assert_eq!(
        lib.source_roots[0].path.strip_prefix(tmp.path()).unwrap(),
        Path::new("java/com/example")
    );
    assert_eq!(lib.classpath.len(), 2);
    assert!(lib.classpath.iter().all(|cp| cp.path.is_absolute()));
    assert_eq!(
        lib.language_level,
        JavaLanguageLevel {
            release: Some(JavaVersion(21)),
            source: None,
            target: None,
            preview: true,
        }
    );
    assert_eq!(
        lib.output_dir
            .as_ref()
            .unwrap()
            .strip_prefix(tmp.path())
            .unwrap(),
        Path::new("bazel-out/k8-fastbuild/bin/java/com/example/lib_classes")
    );

    let test = model
        .modules
        .iter()
        .find(|m| m.id == "//java/com/example:lib_test")
        .unwrap();
    assert_eq!(test.source_roots.len(), 1);
    assert_eq!(test.source_roots[0].kind, SourceRootKind::Test);
    assert_eq!(
        test.source_roots[0].path.strip_prefix(tmp.path()).unwrap(),
        Path::new("javatests/com/example")
    );
    assert_eq!(
        test.language_level,
        JavaLanguageLevel {
            release: None,
            source: Some(JavaVersion(17)),
            target: Some(JavaVersion(17)),
            preview: false,
        }
    );
}

#[test]
fn applies_bazel_target_limit_after_skipping_aliases() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    let query = read_fixture("bazel/query.txt");
    let aquery_lib = read_fixture("bazel/aquery_lib.textproto");
    let aquery_alias = read_fixture("bazel/aquery_alias.textproto");

    let runner = MockRunner::default()
        .with_stdout("bazel", &["query", r#"kind("java_.* rule", //...)"#], query)
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:lib)"#,
            ],
            aquery_lib.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:lib))"#,
            ],
            aquery_lib,
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:alias)"#,
            ],
            aquery_alias.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:alias))"#,
            ],
            aquery_alias,
        );

    let mut options = options_with_bazel_enabled();
    options.bazel.target_limit = 1;

    let model = nova_project::load_bazel_workspace_model_with_runner(tmp.path(), &options, runner)
        .expect("load bazel workspace model");
    assert_eq!(model.modules.len(), 1);
    assert_eq!(model.modules[0].id, "//java/com/example:lib");
}

#[test]
fn reuses_bazel_query_cache_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    let cache_path = tmp.path().join(".nova/queries/bazel.json");

    let query = read_fixture("bazel/query.txt");
    let aquery_lib = read_fixture("bazel/aquery_lib.textproto");
    let aquery_test = read_fixture("bazel/aquery_lib_test.textproto");
    let aquery_alias = read_fixture("bazel/aquery_alias.textproto");

    let runner = MockRunner::default()
        .with_stdout(
            "bazel",
            &["query", r#"kind("java_.* rule", //...)"#],
            query.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:lib)"#,
            ],
            aquery_lib.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:lib))"#,
            ],
            aquery_lib,
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:lib_test)"#,
            ],
            aquery_test.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:lib_test))"#,
            ],
            aquery_test,
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:alias)"#,
            ],
            aquery_alias.clone(),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:alias))"#,
            ],
            aquery_alias,
        );

    let options = options_with_bazel_enabled();
    let model = nova_project::load_bazel_workspace_model_with_runner(tmp.path(), &options, runner)
        .expect("load bazel workspace model");
    assert_eq!(model.modules.len(), 2);
    assert!(
        cache_path.is_file(),
        "expected {cache_path:?} to be written"
    );

    // Second load: provide `bazel query` output and only the *skipped* target's `aquery`.
    //
    // The module-producing targets should be served from the on-disk cache; if Nova tries to
    // `aquery` them again, the mock runner will fail the test.
    let runner = MockRunner::default()
        .with_stdout("bazel", &["query", r#"kind("java_.* rule", //...)"#], query)
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", //java/com/example:alias)"#,
            ],
            read_fixture("bazel/aquery_alias.textproto"),
        )
        .with_stdout(
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                r#"mnemonic("Javac", deps(//java/com/example:alias))"#,
            ],
            read_fixture("bazel/aquery_alias.textproto"),
        );

    let model = nova_project::load_bazel_workspace_model_with_runner(tmp.path(), &options, runner)
        .expect("load bazel workspace model from cache");
    assert_eq!(model.modules.len(), 2);
}
