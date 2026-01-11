use nova_build::{infer_module_graph, ModuleBuildInfo, ModuleId};
use std::path::PathBuf;

#[test]
fn infers_maven_module_edges_from_output_dirs_on_classpath() {
    let module_a = ModuleBuildInfo::new(
        ModuleId::new("module-a"),
        PathBuf::from("/ws/module-a/target/classes"),
        vec![
            // Maven classpaths typically include the current module's output dir.
            PathBuf::from("/ws/module-a/target/classes"),
            // Path normalization should handle `./` components and trailing slashes.
            PathBuf::from("/ws/module-b/target/./classes/"),
            PathBuf::from("/m2/repo/com/example/foo.jar"),
        ],
    );

    let module_b = ModuleBuildInfo::new(
        ModuleId::new("module-b"),
        PathBuf::from("/ws/module-b/target/classes"),
        vec![PathBuf::from("/ws/module-b/target/classes")],
    );

    let graph = infer_module_graph(&[module_a, module_b]);

    assert_eq!(
        graph.edges,
        vec![(ModuleId::new("module-a"), ModuleId::new("module-b"))]
    );
}

#[test]
fn infers_gradle_module_edges_from_explicit_project_dependencies() {
    let mut app = ModuleBuildInfo::new(
        ModuleId::new(":app"),
        PathBuf::from("/ws/app/build/classes/java/main"),
        vec![PathBuf::from("/ws/app/build/classes/java/main")],
    );
    // Gradle integrations may additionally provide explicit `project()` deps.
    app.project_dependencies = vec![ModuleId::new(":lib"), ModuleId::new(":lib")];

    let lib = ModuleBuildInfo::new(
        ModuleId::new(":lib"),
        PathBuf::from("/ws/lib/build/classes/java/main"),
        vec![PathBuf::from("/ws/lib/build/classes/java/main")],
    );

    let graph = infer_module_graph(&[app, lib]);

    assert_eq!(
        graph.edges,
        vec![(ModuleId::new(":app"), ModuleId::new(":lib"))]
    );
}
