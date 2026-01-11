use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct ModuleRoot {
    pub(crate) root: PathBuf,
    pub(crate) rel_path: String,
}

pub(crate) fn rel_path_string(project_root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(project_root).unwrap_or(path);
    pathbuf_to_slash_string(rel)
}

pub(crate) fn module_rel_path_string(workspace_root: &Path, module_root: &Path) -> String {
    let rel = module_root
        .strip_prefix(workspace_root)
        .unwrap_or(module_root);
    let rel_str = pathbuf_to_slash_string(rel);
    if rel_str.is_empty() {
        ".".to_string()
    } else {
        rel_str
    }
}

pub(crate) fn collect_module_roots(
    workspace_root: &Path,
    modules: &[nova_project::Module],
) -> Vec<ModuleRoot> {
    let mut out = Vec::new();
    out.push(ModuleRoot {
        root: workspace_root.to_path_buf(),
        rel_path: ".".to_string(),
    });

    for module in modules {
        out.push(ModuleRoot {
            root: module.root.clone(),
            rel_path: module_rel_path_string(workspace_root, &module.root),
        });
    }

    // Sort so longest roots match first.
    out.sort_by(|a, b| {
        b.root
            .components()
            .count()
            .cmp(&a.root.components().count())
            .then(a.root.cmp(&b.root))
    });
    out.dedup_by(|a, b| a.root == b.root);
    out
}

pub(crate) fn module_for_path<'a>(modules: &'a [ModuleRoot], path: &Path) -> &'a ModuleRoot {
    modules
        .iter()
        .find(|module| path.starts_with(&module.root))
        .unwrap_or_else(|| {
            modules
                .last()
                .expect("module list always contains workspace root")
        })
}

fn pathbuf_to_slash_string(path: &Path) -> String {
    let mut out = String::new();
    for (idx, component) in path.components().enumerate() {
        if idx > 0 {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
}
