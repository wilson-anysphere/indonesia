use std::path::PathBuf;

use nova_modules::ModuleName;
use nova_project::{load_project_with_options, LoadOptions};
use tempfile::tempdir;

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

#[test]
fn parses_module_info_and_builds_workspace_graph() {
    let root = testdata_path("jpms-maven-transitive");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load jpms maven workspace");

    let app = ModuleName::new("com.example.app");
    let lib = ModuleName::new("com.example.lib");
    let util = ModuleName::new("com.example.util");
    let extra = ModuleName::new("com.example.extra");

    let app_root = config
        .jpms_modules
        .iter()
        .find(|m| m.name == app)
        .expect("expected com.example.app module-info");
    assert_eq!(app_root.info.name, app);

    let lib_root = config
        .jpms_modules
        .iter()
        .find(|m| m.name == lib)
        .expect("expected com.example.lib module-info");

    assert!(
        lib_root
            .info
            .requires
            .iter()
            .any(|r| r.module == util && r.is_transitive),
        "expected lib to require transitive util"
    );
    assert!(
        lib_root
            .info
            .requires
            .iter()
            .any(|r| r.module == extra && !r.is_transitive),
        "expected lib to require extra (non-transitive)"
    );

    let exports = lib_root
        .info
        .exports
        .iter()
        .find(|e| e.package == "com.example.lib.api")
        .expect("expected lib to export com.example.lib.api");
    assert_eq!(exports.to, vec![app.clone()]);

    let graph = config.jpms_module_graph();
    assert!(graph.get(&app).is_some(), "graph should contain app");
    assert!(graph.get(&lib).is_some(), "graph should contain lib");
    assert!(graph.get(&util).is_some(), "graph should contain util");
    assert!(graph.get(&extra).is_some(), "graph should contain extra");

    assert!(
        graph.can_read(&app, &util),
        "app should read util via lib's `requires transitive`"
    );
    assert!(
        !graph.can_read(&app, &extra),
        "app should not read extra because lib does not re-export readability"
    );

    let config2 =
        load_project_with_options(&root, &options).expect("reload jpms maven workspace");
    assert_eq!(config, config2);
}

#[test]
fn module_info_parse_errors_are_best_effort() {
    let root = testdata_path("jpms-invalid");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config =
        load_project_with_options(&root, &options).expect("load workspace with invalid module-info");

    let invalid = ModuleName::new("com.example.invalid");
    let module = config
        .jpms_modules
        .iter()
        .find(|m| m.name == invalid)
        .expect("expected module name to be recovered from invalid module-info");
    assert_eq!(module.info.name, invalid);
}
