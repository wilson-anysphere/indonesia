use std::borrow::Cow;
use std::path::{Path, PathBuf};

use nova_project::{load_workspace_model_with_options, LoadOptions, WorkspaceModuleBuildId};

use super::config::load_workspace_config_with_path;

pub(super) fn resolve_gradle_module_root(
    workspace_root: &Path,
    project_path: &str,
) -> Option<PathBuf> {
    let project_path = normalize_gradle_project_path(project_path)?;

    let (nova_config, nova_config_path) = load_workspace_config_with_path(workspace_root);
    let mut options = LoadOptions::default();
    options.nova_config = nova_config;
    options.nova_config_path = nova_config_path;

    let model = load_workspace_model_with_options(workspace_root, &options).ok()?;
    model
        .modules
        .iter()
        .find_map(|module| match &module.build_id {
            WorkspaceModuleBuildId::Gradle { project_path: id } if id == project_path.as_ref() => {
                Some(module.root.clone())
            }
            _ => None,
        })
}

fn normalize_gradle_project_path(project_path: &str) -> Option<Cow<'_, str>> {
    let project_path = project_path.trim();
    if project_path.is_empty() || project_path == ":" {
        return None;
    }

    if project_path.starts_with(':') {
        Some(Cow::Borrowed(project_path))
    } else {
        Some(Cow::Owned(format!(":{project_path}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_project_dir_override_via_workspace_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.gradle"),
            "include ':app'\nproject(':app').projectDir = file('modules/application')\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("modules/application")).unwrap();

        let resolved = resolve_gradle_module_root(dir.path(), ":app").expect("module root");
        let expected = dir.path().join("modules/application").canonicalize().unwrap();

        assert_eq!(resolved, expected);
        assert_eq!(resolved.file_name().and_then(|s| s.to_str()), Some("application"));
    }
}

