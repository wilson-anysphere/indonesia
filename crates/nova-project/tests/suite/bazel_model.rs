use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nova_build_bazel::{CommandOutput, CommandRunner};
use nova_project::{
    BazelLoadOptions, JavaConfig, JavaLanguageLevel, JavaVersion, LanguageLevelProvenance,
    LoadOptions, SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId,
};

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
    fn with_stdout(self, program: &str, args: &[&str], stdout: String) -> Self {
        let key = (
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        );
        {
            let mut guard = self.inner.responses.lock().expect("responses lock");
            guard.insert(
                key,
                CommandOutput {
                    stdout,
                    stderr: String::new(),
                },
            );
        }
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
            target_universe: None,
            target_limit: 200,
            targets: None,
        },
        ..LoadOptions::default()
    }
}

#[test]
fn bazel_workspace_project_model_classifies_overrides_onto_module_path_for_jpms() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    // Enable JPMS via a module-info in the lib target's root (//java/com/example:lib ->
    // java/com/example).
    let module_root = tmp.path().join("java/com/example");
    fs::create_dir_all(&module_root).expect("create module root");
    fs::write(
        module_root.join("module-info.java"),
        "module mod.a { requires mod.b; }",
    )
    .expect("write module-info.java");

    // Dependency override directory providing a stable module name.
    let dep_dir = tmp.path().join("deps/mod-b");
    fs::create_dir_all(dep_dir.join("META-INF")).expect("mkdir dep META-INF");
    fs::write(
        dep_dir.join("META-INF/MANIFEST.MF"),
        b"Manifest-Version: 1.0\r\nAutomatic-Module-Name: mod.b\r\n\r\n",
    )
    .expect("write manifest");

    let query = read_fixture("bazel/query.txt");
    let aquery_lib = read_fixture("bazel/aquery_lib.textproto");
    let aquery_test = read_fixture("bazel/aquery_lib_test.textproto");
    let aquery_alias = read_fixture("bazel/aquery_alias.textproto");

    let runner = MockRunner::default()
        .with_stdout(
            "bazel",
            &["info", "execution_root"],
            format!("{}\n", tmp.path().display()),
        )
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

    let mut options = options_with_bazel_enabled();
    options.classpath_overrides.push(dep_dir.clone());

    let model =
        nova_project::load_bazel_workspace_project_model_with_runner(tmp.path(), &options, runner)
            .expect("load bazel workspace project model");

    let lib = model
        .module_by_id("//java/com/example:lib")
        .expect("expected lib module");
    assert!(
        lib.module_path.iter().any(|e| e.path == dep_dir),
        "override dir should be classified onto module-path when JPMS is enabled"
    );
    assert!(
        !lib.classpath.iter().any(|e| e.path == dep_dir),
        "override dir should not remain on classpath when JPMS is enabled"
    );
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
        .with_stdout(
            "bazel",
            &["info", "execution_root"],
            format!("{}\n", tmp.path().display()),
        )
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
    assert_eq!(lib.source_roots.len(), 2);
    let lib_source_root = lib
        .source_roots
        .iter()
        .find(|root| {
            root.origin == SourceRootOrigin::Source
                && root.path.strip_prefix(tmp.path()).unwrap() == Path::new("java/com/example")
        })
        .expect("lib source root");
    assert_eq!(lib_source_root.kind, SourceRootKind::Main);
    let lib_generated_root = lib
        .source_roots
        .iter()
        .find(|root| {
            root.origin == SourceRootOrigin::Generated
                && root.path.strip_prefix(tmp.path()).unwrap()
                    == Path::new(
                        "bazel-out/k8-fastbuild/bin/java/com/example/lib_generated_sources",
                    )
        })
        .expect("lib generated source root");
    assert_eq!(lib_generated_root.kind, SourceRootKind::Main);
    assert_eq!(lib.classpath.len(), 2);
    assert!(lib.classpath.iter().all(|cp| cp.path.is_absolute()));
    assert_eq!(
        lib.language_level,
        JavaLanguageLevel {
            release: Some(JavaVersion(21)),
            source: Some(JavaVersion(21)),
            target: Some(JavaVersion(21)),
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
fn loads_bazel_targets_as_module_configs_with_target_universe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    let query = read_fixture("bazel/query.txt");
    let aquery_lib = read_fixture("bazel/aquery_lib.textproto");
    let aquery_test = read_fixture("bazel/aquery_lib_test.textproto");
    let aquery_alias = read_fixture("bazel/aquery_alias.textproto");

    let universe = "deps(//java/com/example:lib_test)";
    let java_query = format!(r#"kind("java_.* rule", {universe})"#);

    let runner = MockRunner::default()
        .with_stdout(
            "bazel",
            &["info", "execution_root"],
            format!("{}\n", tmp.path().display()),
        )
        // Intentionally only provide the scoped-universe query; if Nova issues a `//...` query the
        // mock runner will fail the test.
        .with_stdout("bazel", &["query", &java_query], query)
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

    let mut options = options_with_bazel_enabled();
    options.bazel.target_universe = Some(universe.to_string());

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
}

#[test]
fn resolves_aquery_paths_relative_to_execution_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    let execroot = tempfile::tempdir().expect("execroot");

    let query = read_fixture("bazel/query.txt");
    let aquery_lib = read_fixture("bazel/aquery_lib.textproto");

    let runner = MockRunner::default()
        .with_stdout(
            "bazel",
            &["info", "execution_root"],
            format!("{}\n", execroot.path().display()),
        )
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
        );

    let mut options = options_with_bazel_enabled();
    options.bazel.targets = Some(vec!["//java/com/example:lib".to_string()]);

    let model = nova_project::load_bazel_workspace_model_with_runner(tmp.path(), &options, runner)
        .expect("load bazel workspace model");

    assert_eq!(model.modules.len(), 1);
    let lib = model
        .modules
        .iter()
        .find(|m| m.id == "//java/com/example:lib")
        .unwrap();

    assert!(
        lib.source_roots
            .iter()
            .all(|root| root.path.starts_with(tmp.path())),
        "expected source roots to be resolved relative to workspace root"
    );

    assert!(
        lib.classpath
            .iter()
            .all(|cp| cp.path.starts_with(execroot.path())),
        "expected classpath entries to be resolved relative to execroot"
    );
    assert!(
        lib.module_path
            .iter()
            .all(|cp| cp.path.starts_with(execroot.path())),
        "expected module-path entries to be resolved relative to execroot"
    );

    assert_eq!(
        lib.output_dir
            .as_ref()
            .unwrap()
            .strip_prefix(execroot.path())
            .unwrap(),
        Path::new("bazel-out/k8-fastbuild/bin/java/com/example/lib_classes"),
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
        .with_stdout(
            "bazel",
            &["info", "execution_root"],
            format!("{}\n", tmp.path().display()),
        )
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
            &["info", "execution_root"],
            format!("{}\n", tmp.path().display()),
        )
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
        .with_stdout(
            "bazel",
            &["info", "execution_root"],
            format!("{}\n", tmp.path().display()),
        )
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

#[test]
fn loads_bazel_targets_as_workspace_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    let query = read_fixture("bazel/query.txt");
    let aquery_lib = read_fixture("bazel/aquery_lib.textproto");
    let aquery_test = read_fixture("bazel/aquery_lib_test.textproto");
    let aquery_alias = read_fixture("bazel/aquery_alias.textproto");

    let runner = MockRunner::default()
        .with_stdout(
            "bazel",
            &["info", "execution_root"],
            format!("{}\n", tmp.path().display()),
        )
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
    let model =
        nova_project::load_bazel_workspace_project_model_with_runner(tmp.path(), &options, runner)
            .expect("load bazel workspace project model");

    assert_eq!(
        model
            .modules
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["//java/com/example:lib", "//java/com/example:lib_test"]
    );

    assert_eq!(
        model.java,
        JavaConfig {
            source: JavaVersion(21),
            target: JavaVersion(21),
            enable_preview: true,
        }
    );

    let lib = model.module_by_id("//java/com/example:lib").unwrap();
    assert!(matches!(
        &lib.build_id,
        WorkspaceModuleBuildId::Bazel { label }
            if label == "//java/com/example:lib"
    ));
    assert!(matches!(
        lib.language_level.provenance,
        LanguageLevelProvenance::Default
    ));
    assert_eq!(
        lib.language_level.level,
        JavaLanguageLevel {
            release: Some(JavaVersion(21)),
            source: Some(JavaVersion(21)),
            target: Some(JavaVersion(21)),
            preview: true,
        }
    );
    assert!(
        lib.source_roots.iter().any(|root| {
            root.origin == SourceRootOrigin::Generated
                && root.path.strip_prefix(tmp.path()).unwrap()
                    == Path::new(
                        "bazel-out/k8-fastbuild/bin/java/com/example/lib_generated_sources",
                    )
        }),
        "expected lib generated sources root to be present; got: {:?}",
        lib.source_roots
    );

    let test = model.module_by_id("//java/com/example:lib_test").unwrap();
    assert!(matches!(
        &test.build_id,
        WorkspaceModuleBuildId::Bazel { label }
            if label == "//java/com/example:lib_test"
    ));
    assert_eq!(test.source_roots.len(), 1);
    assert_eq!(test.source_roots[0].kind, SourceRootKind::Test);
    assert_eq!(
        test.language_level.level,
        JavaLanguageLevel {
            release: None,
            source: Some(JavaVersion(17)),
            target: Some(JavaVersion(17)),
            preview: false,
        }
    );

    let lib_file = tmp.path().join("java/com/example/Foo.java");
    assert_eq!(
        model.module_for_path(&lib_file).unwrap().module.id,
        "//java/com/example:lib"
    );

    let generated_file = tmp.path().join(
        "bazel-out/k8-fastbuild/bin/java/com/example/lib_generated_sources/com/example/Gen.java",
    );
    let generated_owner = model
        .module_for_path(&generated_file)
        .expect("generated owner");
    assert_eq!(generated_owner.module.id, "//java/com/example:lib");
    assert_eq!(
        generated_owner.source_root.origin,
        SourceRootOrigin::Generated
    );

    let test_file = tmp.path().join("javatests/com/example/FooTest.java");
    assert_eq!(
        model.module_for_path(&test_file).unwrap().module.id,
        "//java/com/example:lib_test"
    );
}
