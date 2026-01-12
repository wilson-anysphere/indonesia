use std::path::Path;

use nova_build::JavaCompileConfig;

/// Select the Java compilation configuration used for the injected stream-debug helper.
///
/// If a build-system config can be resolved we honor it (release/source/target/preview).
/// Otherwise, we default to `--release 8` since Java streams require Java 8+ and
/// this maximizes compatibility with older debuggee JVMs when attaching.
pub(crate) fn select_stream_debug_java_compile_config(
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
        let config = select_stream_debug_java_compile_config(None, |_root| {
            panic!("build config resolver should not be called when project_root is None")
        });

        assert_eq!(config.release.as_deref(), Some("8"));
    }

    #[test]
    fn honors_resolved_build_config_without_overriding_release() {
        let project_root = PathBuf::from("/fake/project");
        let config = select_stream_debug_java_compile_config(Some(&project_root), |_root| {
            Some(JavaCompileConfig {
                release: Some("21".to_string()),
                ..JavaCompileConfig::default()
            })
        });

        assert_eq!(config.release.as_deref(), Some("21"));
    }
}

