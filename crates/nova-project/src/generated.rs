use std::collections::HashSet;
use std::path::{Path, PathBuf};

use nova_config::NovaConfig;

use crate::{SourceRoot, SourceRootKind, SourceRootOrigin};

pub(crate) fn append_generated_source_roots(
    roots: &mut Vec<SourceRoot>,
    module_root: &Path,
    config: &NovaConfig,
) {
    let mut candidates: Vec<(SourceRootKind, PathBuf)> = Vec::new();

    if let Some(override_roots) = &config.generated_sources.override_roots {
        for root in override_roots {
            let path = if root.is_absolute() {
                root.clone()
            } else {
                module_root.join(root)
            };
            candidates.push((SourceRootKind::Main, path));
        }
    } else {
        // Maven defaults.
        candidates.push((
            SourceRootKind::Main,
            module_root.join("target/generated-sources/annotations"),
        ));
        candidates.push((
            SourceRootKind::Test,
            module_root.join("target/generated-test-sources/test-annotations"),
        ));

        // Gradle defaults.
        candidates.push((
            SourceRootKind::Main,
            module_root.join("build/generated/sources/annotationProcessor/java/main"),
        ));
        candidates.push((
            SourceRootKind::Test,
            module_root.join("build/generated/sources/annotationProcessor/java/test"),
        ));

        for root in &config.generated_sources.additional_roots {
            let path = if root.is_absolute() {
                root.clone()
            } else {
                module_root.join(root)
            };
            candidates.push((SourceRootKind::Main, path));
        }
    }

    let mut seen = HashSet::new();
    for (kind, path) in candidates {
        if !path.is_dir() {
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }

        roots.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Generated,
            path,
        });
    }
}
