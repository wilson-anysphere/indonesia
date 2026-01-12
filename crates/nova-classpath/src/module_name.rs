use std::io::{Read, Seek};
use std::path::Path;

use nova_modules::ModuleName;

const MANIFEST_CANDIDATES: [&str; 2] = ["META-INF/MANIFEST.MF", "classes/META-INF/MANIFEST.MF"];

/// Return the value of `key` from the main section of a JAR manifest.
///
/// Manifest files are line-oriented and can fold long values onto continuation
/// lines that start with a single space character.
pub(crate) fn manifest_main_attribute(manifest: &str, key: &str) -> Option<String> {
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

pub(crate) fn zip_manifest_main_attribute<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    key: &str,
) -> Option<String> {
    for candidate in MANIFEST_CANDIDATES {
        let mut file = match archive.by_name(candidate) {
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

pub(crate) fn automatic_module_name_from_jar_manifest<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Option<ModuleName> {
    let name = zip_manifest_main_attribute(archive, "Automatic-Module-Name")?;
    let name = name.trim();
    (!name.is_empty()).then(|| ModuleName::new(name.to_string()))
}

/// Derive the automatic module name for a JAR based on its file name.
///
/// This follows the same derivation used by the Java module system when placing
/// non-modular JARs on the module path (see `java.lang.module.ModuleFinder`):
///
/// 1. Use the JAR file name without the `.jar` extension.
/// 2. Strip a trailing "version" suffix that starts at the first `-` followed
///    by digits and then either `.` or the end of the string (equivalent to
///    the regex `-(\\d+(\\.|$))`). This avoids stripping artifact IDs like
///    `foo-2bar` while handling Maven-style versions like `foo-1.2.3`.
/// 3. Replace all non-alphanumeric characters with `.`.
/// 4. Collapse consecutive `.` into a single `.` and trim leading/trailing
///    `.` characters.
/// 5. For each `.`-separated segment, if it starts with a digit, prefix it
///    with `_` to produce a valid Java identifier.
pub(crate) fn derive_automatic_module_name_from_jar_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    derive_automatic_module_name_from_jar_stem(&stem)
}

pub(crate) fn derive_automatic_module_name_from_jar_stem(stem: &str) -> Option<String> {
    if stem.is_empty() {
        return None;
    }

    let stem = strip_jar_version(stem);

    // Replace non-alphanumeric characters with '.' and collapse sequences of
    // separators down to a single dot.
    let mut normalized = String::with_capacity(stem.len());
    let mut last_was_dot = true; // trim leading dots by default
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dot = false;
        } else if !last_was_dot {
            normalized.push('.');
            last_was_dot = true;
        }
    }
    if normalized.ends_with('.') {
        normalized.pop();
    }

    if normalized.is_empty() {
        return Some("_".to_string());
    }

    let mut parts_out = Vec::new();
    for part in normalized.split('.') {
        if part.is_empty() {
            continue;
        }
        if part.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            parts_out.push(format!("_{part}"));
        } else {
            parts_out.push(part.to_string());
        }
    }

    if parts_out.is_empty() {
        return Some("_".to_string());
    }

    Some(parts_out.join("."))
}

fn strip_jar_version(stem: &str) -> &str {
    let bytes = stem.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j == bytes.len() || bytes[j] == b'.' {
                return &stem[..i];
            }
        }
        i += 1;
    }
    stem
}
