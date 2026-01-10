use std::path::Path;

use nova_hir::module_info::lower_module_info_source;

use crate::{JpmsModuleRoot, Module};

pub(crate) fn discover_jpms_modules(modules: &[Module]) -> Vec<JpmsModuleRoot> {
    let mut out = Vec::new();
    for module in modules {
        if let Some(root) = discover_jpms_module_root(&module.root) {
            out.push(root);
        }
    }

    out.sort_by(|a, b| {
        a.root
            .cmp(&b.root)
            .then(a.name.as_str().cmp(b.name.as_str()))
    });
    out.dedup_by(|a, b| {
        a.root == b.root && a.name == b.name && a.module_info == b.module_info && a.info == b.info
    });
    out
}

fn discover_jpms_module_root(module_root: &Path) -> Option<JpmsModuleRoot> {
    let candidates = [
        module_root.join("src/main/java/module-info.java"),
        module_root.join("src/module-info.java"),
        module_root.join("module-info.java"),
    ];

    let module_info_path = candidates.into_iter().find(|p| p.is_file())?;
    let src = std::fs::read_to_string(&module_info_path).ok()?;
    let info = lower_module_info_source(&src).ok()?;

    Some(JpmsModuleRoot {
        name: info.name.clone(),
        root: module_root.to_path_buf(),
        module_info: module_info_path,
        info,
    })
}
