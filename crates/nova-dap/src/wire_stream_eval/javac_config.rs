use std::path::Path;

use nova_build::JavaCompileConfig;

/// Select the Java compilation configuration used for the injected stream-eval helper.
///
/// When no build-system Java compile configuration can be resolved (e.g. attach sessions without
/// `projectRoot` or projects without recognized build-tool metadata), we default to `--release 8`.
///
/// Stream debugging relies on Java streams (introduced in Java 8), so targeting Java 8 maximizes
/// compatibility with older debuggee JVMs while remaining sufficient for the helper source.
pub fn select_stream_eval_java_compile_config(
    project_root: Option<&Path>,
    resolve_build_config: impl FnOnce(&Path) -> Option<JavaCompileConfig>,
) -> JavaCompileConfig {
    if let Some(project_root) = project_root {
        if let Some(build_cfg) = resolve_build_config(project_root) {
            return build_cfg;
        }
    }

    JavaCompileConfig {
        release: Some("8".to_string()),
        ..JavaCompileConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn defaults_to_release_8_when_project_root_is_none() {
        let config = select_stream_eval_java_compile_config(None, |_root| {
            panic!("build config resolver should not be called when project_root is None")
        });

        assert_eq!(config.release.as_deref(), Some("8"));
    }

    #[test]
    fn honors_resolved_build_config_without_overriding_release() {
        let project_root = PathBuf::from("/fake/project");
        let config = select_stream_eval_java_compile_config(Some(&project_root), |_root| {
            Some(JavaCompileConfig {
                release: Some("21".to_string()),
                ..JavaCompileConfig::default()
            })
        });

        assert_eq!(config.release.as_deref(), Some("21"));
    }
}
