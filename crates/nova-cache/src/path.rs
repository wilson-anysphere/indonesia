use crate::fingerprint::Fingerprint;
use std::collections::BTreeMap;

/// Normalize a cache key that represents a path relative to the project root.
///
/// This is intentionally OS-agnostic: it treats both `/` and `\\` as separators
/// so that caches created on Windows can be reused on Unix (and vice versa).
///
/// Normalization rules:
/// - convert `\\` to `/`
/// - remove empty segments and `.` segments (including leading `./`)
/// - replace any `..` segment with `_` (best-effort sanitization; callers should
///   not be using `..` in cache-relative paths)
/// - ensure the returned path never starts with `/`
pub fn normalize_rel_path(raw: &str) -> String {
    // Fast path for the already-canonical format we expect (`src/Foo.java`).
    // We still need to reject/sanitize `..` segments, so we only take this path
    // when it's obviously safe.
    if !raw.contains('\\') && !raw.contains("/.") && !raw.starts_with('.') && !raw.contains("..") {
        return raw.trim_start_matches('/').to_string();
    }

    let replaced = raw.replace('\\', "/");
    let mut out: Vec<&str> = Vec::new();
    for seg in replaced.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            out.push("_");
        } else {
            out.push(seg);
        }
    }

    // Ensure we never produce an empty key (can happen for `""`, `"."`, `"./"`).
    if out.is_empty() {
        return "_".to_string();
    }

    out.join("/")
}

/// Normalize a fingerprint map whose keys are cache-relative paths.
///
/// This is a convenience for [`DerivedArtifactCache`] callers that key their
/// `input_fingerprints` by file path.
pub fn normalize_inputs_map(
    input_fingerprints: &BTreeMap<String, Fingerprint>,
) -> BTreeMap<String, Fingerprint> {
    let mut out = BTreeMap::new();
    for (key, fp) in input_fingerprints {
        let key = normalize_rel_path(key);
        match out.get(&key) {
            None => {
                out.insert(key, fp.clone());
            }
            Some(existing) if existing == fp => {
                // Duplicate path spelled differently (`src\\A.java` vs `src/A.java`).
                // Keep the existing value.
            }
            Some(existing) => {
                // Two different fingerprints for the same normalized key. This is
                // unexpected, but we want deterministic behavior and we don't want
                // caches to depend on which spelling happened to win.
                //
                // Combine both fingerprints into a new one so that the cache key
                // changes (forcing a miss) rather than silently picking one.
                let (a, b) = if existing.as_str() <= fp.as_str() {
                    (existing.as_str(), fp.as_str())
                } else {
                    (fp.as_str(), existing.as_str())
                };
                let mut bytes = Vec::with_capacity(a.len() + b.len() + 1);
                bytes.extend_from_slice(a.as_bytes());
                bytes.push(0);
                bytes.extend_from_slice(b.as_bytes());
                out.insert(key, Fingerprint::from_bytes(bytes));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rel_path_unifies_separators_and_dot_segments() {
        assert_eq!(normalize_rel_path("src\\\\A.java"), "src/A.java");
        assert_eq!(normalize_rel_path("./src/A.java"), "src/A.java");
        assert_eq!(normalize_rel_path("src/./A.java"), "src/A.java");
        assert_eq!(normalize_rel_path("/src/A.java"), "src/A.java");
    }

    #[test]
    fn normalize_rel_path_sanitizes_dotdot_segments() {
        let normalized = normalize_rel_path("../A.java");
        assert_eq!(normalized, "_/A.java");
        assert!(!normalized.contains(".."));
        assert!(!normalized.starts_with('/'));
    }
}
