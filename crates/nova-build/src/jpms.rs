use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

pub(crate) fn main_source_roots_have_module_info(main_source_roots: &[PathBuf]) -> bool {
    main_source_roots
        .iter()
        .any(|root| root.join("module-info.java").is_file())
}

pub(crate) fn infer_module_path_entries(classpath: &[PathBuf]) -> Vec<PathBuf> {
    let mut module_path = Vec::new();
    for entry in classpath {
        if stable_module_path_entry(entry) {
            module_path.push(entry.clone());
        }
    }
    dedupe_paths(&mut module_path);
    module_path
}

/// Returns `true` if the path is a stable JPMS module:
/// - a jar/directory containing `module-info.class`, or
/// - a jar/directory whose manifest contains `Automatic-Module-Name`.
pub(crate) fn stable_module_path_entry(path: &Path) -> bool {
    if path.is_dir() {
        return directory_contains_module_info(path) || directory_has_automatic_module_name(path);
    }
    if !path.is_file() {
        return false;
    }

    archive_is_stable_module(path)
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
}

fn directory_contains_module_info(dir: &Path) -> bool {
    dir.join("module-info.class").is_file()
        || dir.join("META-INF/versions/9/module-info.class").is_file()
        || dir.join("classes/module-info.class").is_file()
        || dir
            .join("classes/META-INF/versions/9/module-info.class")
            .is_file()
}

fn directory_has_automatic_module_name(dir: &Path) -> bool {
    for manifest_path in ["META-INF/MANIFEST.MF", "classes/META-INF/MANIFEST.MF"] {
        let manifest_path = dir.join(manifest_path);
        let Ok(bytes) = std::fs::read(&manifest_path) else {
            continue;
        };
        let manifest = String::from_utf8_lossy(&bytes);
        if manifest_main_attribute(&manifest, "Automatic-Module-Name")
            .is_some_and(|name| !name.is_empty())
        {
            return true;
        }
    }

    false
}

fn archive_is_stable_module(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(mut archive) = ZipArchive::new(file) else {
        return false;
    };

    for candidate in [
        "module-info.class",
        "META-INF/versions/9/module-info.class",
        "classes/module-info.class",
        "classes/META-INF/versions/9/module-info.class",
    ] {
        if archive.by_name(candidate).is_ok() {
            return true;
        }
    }

    zip_manifest_main_attribute(&mut archive, "Automatic-Module-Name")
        .is_some_and(|name| !name.is_empty())
}

fn zip_manifest_main_attribute<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    key: &str,
) -> Option<String> {
    for manifest_path in ["META-INF/MANIFEST.MF", "classes/META-INF/MANIFEST.MF"] {
        let mut file = match archive.by_name(manifest_path) {
            Ok(file) => file,
            Err(zip::result::ZipError::FileNotFound) => continue,
            Err(_) => continue,
        };

        let mut bytes = Vec::with_capacity(file.size() as usize);
        if file.read_to_end(&mut bytes).is_err() {
            continue;
        }
        let manifest = String::from_utf8_lossy(&bytes);
        if let Some(value) = manifest_main_attribute(&manifest, key) {
            return Some(value);
        }
    }

    None
}

fn manifest_main_attribute(manifest: &str, key: &str) -> Option<String> {
    let mut current_key: Option<&str> = None;
    let mut current_value = String::new();

    for line in manifest.lines() {
        let line = line.trim_end_matches('\r');

        // The first empty line terminates the main attributes section.
        if line.is_empty() {
            break;
        }

        if let Some(rest) = line.strip_prefix(' ') {
            if current_key.is_some() {
                current_value.push_str(rest);
            }
            continue;
        }

        if let Some(k) = current_key.take() {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(current_value.trim().to_string());
            }
        }
        current_value.clear();

        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        current_key = Some(k);
        current_value.push_str(v.trim_start());
    }

    if let Some(k) = current_key {
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(current_value.trim().to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn stable_module_path_entry_detects_jars() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        assert!(stable_module_path_entry(&named));
        assert!(stable_module_path_entry(&automatic));
        assert!(!stable_module_path_entry(&dep));

        let inferred =
            infer_module_path_entries(&[named.clone(), dep, automatic.clone(), named.clone()]);
        assert_eq!(inferred, vec![named, automatic]);
    }

    #[test]
    fn stable_module_path_entry_detects_jmods() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jmod");
        let dep = testdata_dir.join("dep.jar");

        assert!(stable_module_path_entry(&named));
        let inferred = infer_module_path_entries(&[dep, named.clone()]);
        assert_eq!(inferred, vec![named]);
    }

    #[test]
    fn stable_module_path_entry_detects_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let explicit = root.join("explicit");
        std::fs::create_dir_all(&explicit).unwrap();
        std::fs::write(explicit.join("module-info.class"), b"cafebabe").unwrap();
        assert!(stable_module_path_entry(&explicit));

        let automatic = root.join("automatic");
        std::fs::create_dir_all(automatic.join("META-INF")).unwrap();
        std::fs::write(
            automatic.join("META-INF").join("MANIFEST.MF"),
            "Manifest-Version: 1.0\nAutomatic-Module-Name: com.example.foo\n\n",
        )
        .unwrap();
        assert!(stable_module_path_entry(&automatic));

        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(!stable_module_path_entry(&plain));
    }

    #[test]
    fn main_source_roots_have_module_info_checks_for_source_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let src = root.join("src/main/java");
        std::fs::create_dir_all(&src).unwrap();
        assert!(!main_source_roots_have_module_info(&[src.clone()]));
        std::fs::write(src.join("module-info.java"), "module demo {}".as_bytes()).unwrap();
        assert!(main_source_roots_have_module_info(&[src]));
    }
}
