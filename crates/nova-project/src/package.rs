// Kept for backwards compatibility: this module historically lived in `nova-project`, but the
// implementation has moved to `nova-build-model` so that refactoring and other higher-level crates
// can reuse the helpers without depending on project discovery/build-system integrations.

pub use nova_build_model::package::{
    class_to_file_name, infer_source_root, is_valid_package_name, package_to_path, path_ends_with,
    validate_package_name, PackageNameError,
};
