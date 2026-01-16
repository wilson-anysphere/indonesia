use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use nova_core::{file_uri_to_path, path_to_file_uri, AbsPathBuf};

use crate::archive::{ArchiveKind, ArchivePath};

/// A path that can be resolved by the VFS.
///
/// Today this supports local file system paths and archive paths. In the future
/// additional schemes (e.g. remote URIs) can be added.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VfsPath {
    /// A file on the local OS file system.
    Local(PathBuf),
    /// A file inside an archive such as a `.jar` or `.jmod`.
    Archive(ArchivePath),
    /// A virtual document produced by Nova, identified by the content hash of the binary input
    /// and the binary name it was decompiled as.
    ///
    /// Canonical URI form: `nova:///decompiled/<hash>/<binary-name>.java`.
    Decompiled {
        content_hash: String,
        binary_name: String,
    },
    /// Legacy decompiled virtual document URI (pre ADR0006).
    ///
    /// Canonical URI form: `nova-decompile:///com/example/Foo.class`.
    ///
    /// Prefer [`VfsPath::Decompiled`]; this exists for backwards compatibility
    /// while the rest of the system migrates.
    LegacyDecompiled { internal_name: String },
    /// A generic URI string that an external implementation can resolve.
    Uri(String),
}

impl VfsPath {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self::Local(normalize_local_path(&path))
    }

    pub fn jar(archive: impl Into<PathBuf>, entry: impl Into<String>) -> Self {
        let archive = normalize_local_path(&archive.into());
        let entry = entry.into();
        match normalize_archive_entry(&entry) {
            Some(entry) => Self::Archive(ArchivePath::new(ArchiveKind::Jar, archive, entry)),
            None => Self::Uri(format_archive_uri_fallback(
                ArchiveKind::Jar,
                &archive,
                &entry,
            )),
        }
    }

    pub fn jmod(archive: impl Into<PathBuf>, entry: impl Into<String>) -> Self {
        let archive = normalize_local_path(&archive.into());
        let entry = entry.into();
        match normalize_archive_entry(&entry) {
            Some(entry) => Self::Archive(ArchivePath::new(ArchiveKind::Jmod, archive, entry)),
            None => Self::Uri(format_archive_uri_fallback(
                ArchiveKind::Jmod,
                &archive,
                &entry,
            )),
        }
    }

    pub fn decompiled(content_hash: impl Into<String>, binary_name: impl Into<String>) -> Self {
        let content_hash = content_hash.into();
        let binary_name = binary_name.into();
        let content_hash = normalize_decompiled_hash(content_hash);
        let binary_name = normalize_decompiled_binary_name(binary_name);
        if binary_name.is_empty() || binary_name.contains('?') || binary_name.contains('#') {
            // Don't allow construction of the structured decompiled variant with a non-canonical
            // binary name; callers that need to preserve arbitrary URIs can use `VfsPath::uri`.
            return Self::Uri(format!(
                "nova:///decompiled/{content_hash}/{binary_name}.java"
            ));
        }
        if !is_decompiled_hash(&content_hash) {
            // Don't allow construction of the structured decompiled variant with a non-canonical
            // hash; callers that need to preserve arbitrary URIs can use `VfsPath::uri`.
            return Self::Uri(format!(
                "nova:///decompiled/{content_hash}/{binary_name}.java"
            ));
        }
        Self::Decompiled {
            content_hash,
            binary_name,
        }
    }

    pub fn legacy_decompiled(internal_name: impl Into<String>) -> Self {
        Self::LegacyDecompiled {
            internal_name: normalize_legacy_internal_name(internal_name.into()),
        }
    }

    pub fn uri(uri: impl Into<String>) -> Self {
        let uri = uri.into();
        if let Some(archive) = parse_archive_uri(&uri) {
            return Self::Archive(archive);
        }
        // Treat `file:` URIs as local paths so that LSP buffers and disk paths
        // map to the same `VfsPath`/`FileId`.
        if uri.starts_with("file:") {
            if let Ok(path) = file_uri_to_path(&uri) {
                return Self::Local(normalize_local_path(path.as_path()));
            }
        }
        if let Some(decompiled) = parse_decompiled_uri(&uri) {
            return decompiled;
        }
        if let Some(legacy) = parse_legacy_decompiled_uri(&uri) {
            return legacy;
        }
        Self::Uri(uri)
    }

    pub fn as_local_path(&self) -> Option<&Path> {
        match self {
            VfsPath::Local(path) => Some(path.as_path()),
            _ => None,
        }
    }

    /// Convert this path into a `file://` URI, if it represents an absolute local path.
    pub fn to_file_uri(&self) -> Option<String> {
        match self {
            VfsPath::Local(path) => {
                let normalized = normalize_local_path(path.as_path());
                let abs = AbsPathBuf::new(normalized).ok()?;
                path_to_file_uri(&abs).ok()
            }
            _ => None,
        }
    }

    /// Convert this path into a URI string suitable for editor/LSP-facing APIs.
    ///
    /// - Local absolute paths use the `file:` scheme.
    /// - Archive paths use `jar:` / `jmod:` and embed the archive's `file:` URI.
    /// - Decompiled virtual documents use `nova:`.
    /// - Legacy decompiled virtual documents use `nova-decompile:`.
    /// - `Uri` paths are returned as-is.
    pub fn to_uri(&self) -> Option<String> {
        match self {
            VfsPath::Local(_) => self.to_file_uri(),
            VfsPath::Archive(path) => {
                let normalized = normalize_local_path(path.archive.as_path());
                let abs = AbsPathBuf::new(normalized).ok()?;
                let archive_uri = path_to_file_uri(&abs).ok()?;
                let scheme = match path.kind {
                    ArchiveKind::Jar => "jar",
                    ArchiveKind::Jmod => "jmod",
                };
                let entry = path.entry.strip_prefix('/').unwrap_or(&path.entry);
                let entry = percent_encode_archive_entry(entry);
                Some(format!("{scheme}:{archive_uri}!/{entry}"))
            }
            VfsPath::Decompiled {
                content_hash,
                binary_name,
            } => Some(format!(
                "nova:///decompiled/{content_hash}/{binary_name}.java"
            )),
            VfsPath::LegacyDecompiled { internal_name } => {
                Some(format!("nova-decompile:///{internal_name}.class"))
            }
            VfsPath::Uri(uri) => Some(uri.clone()),
        }
    }

    pub fn as_decompiled(&self) -> Option<(&str, &str)> {
        match self {
            VfsPath::Decompiled {
                content_hash,
                binary_name,
            } => Some((content_hash, binary_name)),
            _ => None,
        }
    }

    pub fn as_legacy_decompiled(&self) -> Option<&str> {
        match self {
            VfsPath::LegacyDecompiled { internal_name } => Some(internal_name),
            _ => None,
        }
    }

    #[cfg(feature = "lsp")]
    pub fn to_lsp_uri(&self) -> Option<lsp_types::Uri> {
        self.to_uri()?.parse().ok()
    }
}

impl From<PathBuf> for VfsPath {
    fn from(value: PathBuf) -> Self {
        VfsPath::local(value)
    }
}

impl From<&Path> for VfsPath {
    fn from(value: &Path) -> Self {
        VfsPath::local(value.to_path_buf())
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsPath::Local(path) => write!(f, "{}", path.display()),
            VfsPath::Archive(archive) => write!(f, "{archive}"),
            VfsPath::Decompiled {
                content_hash,
                binary_name,
            } => write!(f, "nova:///decompiled/{content_hash}/{binary_name}.java"),
            VfsPath::LegacyDecompiled { internal_name } => {
                write!(f, "nova-decompile:///{internal_name}.class")
            }
            VfsPath::Uri(uri) => write!(f, "{uri}"),
        }
    }
}

#[cfg(feature = "lsp")]
impl From<&lsp_types::Uri> for VfsPath {
    fn from(value: &lsp_types::Uri) -> Self {
        VfsPath::uri(value.to_string())
    }
}

#[cfg(feature = "lsp")]
impl From<lsp_types::Uri> for VfsPath {
    fn from(value: lsp_types::Uri) -> Self {
        VfsPath::uri(value.to_string())
    }
}

fn normalize_archive_entry(entry: &str) -> Option<String> {
    let entry = entry.trim_start_matches(['/', '\\']);
    let entry = if entry.contains('\\') {
        entry.replace('\\', "/")
    } else {
        entry.to_string()
    };
    let bytes = entry.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return None;
    }
    if entry.contains("//") {
        return None;
    }
    if entry.split('/').any(|segment| segment == "..") {
        return None;
    }
    Some(entry)
}

fn percent_decode_utf8(s: &str) -> Option<String> {
    if !s.as_bytes().contains(&b'%') {
        return Some(s.to_string());
    }

    fn from_hex(b: u8) -> Option<u8> {
        Some(match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => 10 + (b - b'a'),
            b'A'..=b'F' => 10 + (b - b'A'),
            _ => return None,
        })
    }

    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = *bytes.get(i + 1)?;
                let lo = *bytes.get(i + 2)?;
                out.push((from_hex(hi)? << 4) | from_hex(lo)?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8(out).ok()
}

fn percent_encode_archive_entry(entry: &str) -> String {
    fn is_unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
    }

    fn is_sub_delim(b: u8) -> bool {
        matches!(
            b,
            b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
        )
    }

    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let bytes = entry.as_bytes();
    let needs_encoding = bytes
        .iter()
        .any(|&b| !(is_unreserved(b) || is_sub_delim(b) || matches!(b, b'/' | b':' | b'@')));
    if !needs_encoding {
        return entry.to_string();
    }

    let encoded_bytes = bytes
        .iter()
        .filter(|&&b| !(is_unreserved(b) || is_sub_delim(b) || matches!(b, b'/' | b':' | b'@')))
        .count();
    let mut out = String::with_capacity(entry.len() + 2 * encoded_bytes);
    for &b in bytes {
        if is_unreserved(b) || is_sub_delim(b) || b == b'/' || b == b':' || b == b'@' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

fn format_archive_uri_fallback(kind: ArchiveKind, archive: &Path, entry: &str) -> String {
    let scheme = match kind {
        ArchiveKind::Jar => "jar",
        ArchiveKind::Jmod => "jmod",
    };
    if let Ok(abs) = AbsPathBuf::new(archive.to_path_buf()) {
        if let Ok(archive_uri) = path_to_file_uri(&abs) {
            return format!("{scheme}:{archive_uri}!/{entry}");
        }
    }
    format!("{scheme}:{}!/{entry}", archive.display())
}

fn normalize_local_path(path: &Path) -> PathBuf {
    let mut prefix: Option<OsString> = None;
    let mut has_root = false;
    let mut stack: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix_component) => {
                prefix = Some(normalize_prefix(prefix_component));
            }
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = stack.last() {
                    if last != ".." {
                        stack.pop();
                        continue;
                    }
                }

                if !has_root {
                    stack.push(OsString::from(".."));
                }
            }
            Component::Normal(segment) => stack.push(segment.to_owned()),
        }
    }

    let mut out = PathBuf::new();
    match (prefix, has_root) {
        (Some(mut prefix), true) => {
            prefix.push(std::path::MAIN_SEPARATOR.to_string());
            out.push(prefix);
        }
        (Some(prefix), false) => out.push(prefix),
        (None, true) => out.push(std::path::MAIN_SEPARATOR.to_string()),
        (None, false) => {}
    }
    out.extend(stack);
    out
}

fn normalize_prefix(prefix_component: std::path::PrefixComponent<'_>) -> OsString {
    #[cfg(windows)]
    {
        let prefix = prefix_component.as_os_str().to_string_lossy().into_owned();
        if let Some(colon) = prefix.rfind(':') {
            if colon > 0 {
                let mut bytes = prefix.into_bytes();
                let drive = bytes[colon - 1];
                if drive.is_ascii_alphabetic() {
                    bytes[colon - 1] = drive.to_ascii_uppercase();
                }
                return OsString::from(String::from_utf8(bytes).unwrap_or_default());
            }
        }
        OsString::from(prefix)
    }

    #[cfg(not(windows))]
    {
        prefix_component.as_os_str().to_owned()
    }
}

fn normalize_decompiled_segment(segment: String) -> String {
    segment.trim_matches(|c| c == '/' || c == '\\').to_string()
}

fn normalize_decompiled_hash(segment: String) -> String {
    normalize_decompiled_segment(segment).to_ascii_lowercase()
}

fn normalize_decompiled_binary_name(binary_name: String) -> String {
    let binary_name = binary_name
        .strip_suffix(".java")
        .unwrap_or(&binary_name)
        .replace(['\\', '/'], ".")
        .trim_matches('.')
        .to_string();

    let mut out = String::with_capacity(binary_name.len());
    let mut last_dot = false;
    for ch in binary_name.chars() {
        if ch == '.' {
            if last_dot {
                continue;
            }
            last_dot = true;
            out.push('.');
        } else {
            last_dot = false;
            out.push(ch);
        }
    }

    out
}

fn normalize_legacy_internal_name(internal_name: String) -> String {
    let internal_name = internal_name
        .strip_suffix(".class")
        .unwrap_or(&internal_name)
        .to_string();
    let raw = internal_name.trim_matches(|c| c == '/' || c == '\\');
    let raw = if raw.contains('\\') {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    };
    let segments: Vec<&str> = raw.split('/').filter(|s| !s.is_empty()).collect();
    segments.join("/")
}

fn parse_archive_uri(uri: &str) -> Option<ArchivePath> {
    let (kind, rest) = if let Some(rest) = uri.strip_prefix("jar:") {
        (ArchiveKind::Jar, rest)
    } else if let Some(rest) = uri.strip_prefix("jmod:") {
        (ArchiveKind::Jmod, rest)
    } else {
        return None;
    };

    // Reject query/fragment on the archive URI. If callers intend `?`/`#` to be part of the entry,
    // they must percent-encode them (e.g. `%3F`, `%23`).
    if rest.contains('?') || rest.contains('#') {
        return None;
    }

    let (archive_uri, entry) = rest.split_once('!')?;
    let archive = file_uri_to_path(archive_uri).ok()?;
    let archive = normalize_local_path(archive.as_path());
    let entry = percent_decode_utf8(entry)?;
    let entry = normalize_archive_entry(&entry)?;
    Some(ArchivePath::new(kind, archive, entry))
}

fn parse_decompiled_uri(uri: &str) -> Option<VfsPath> {
    // The canonical ADR0006 decompiled URI is:
    // `nova:///decompiled/<content-hash>/<binary-name>.java`.
    //
    // Keep this parser strict so we don't accidentally treat unrelated `nova:`
    // URIs as decompiled virtual documents.

    // Canonical form does not include query/fragment.
    if uri.contains('?') || uri.contains('#') {
        return None;
    }

    // Only match the canonical scheme + empty authority form (`nova:///...`).
    // This rejects `nova://host/...` (non-empty authority) and `nova:/...`
    // (non-canonical path form) to align with `nova_decompile::parse_decompiled_uri`.
    let rest = uri.strip_prefix("nova:///decompiled/")?;

    // Require exactly two additional path segments: `<hash>/<filename>.java`.
    // (`split_once` is sufficient because we reject additional `/` below.)
    let (content_hash, filename) = rest.split_once('/')?;

    // Reject path traversal attempts and empty segments.
    if content_hash.is_empty() || content_hash == ".." || filename.is_empty() || filename == ".." {
        return None;
    }

    if !is_decompiled_hash(content_hash) {
        return None;
    }

    // Require `<binary-name>.java` as the last segment.
    let binary_name = filename.strip_suffix(".java")?;
    if binary_name.is_empty() || binary_name.contains('/') {
        return None;
    }

    // Align with `nova_decompile::parse_decompiled_uri`: reject names that normalize to empty.
    if normalize_decompiled_binary_name(binary_name.to_string()).is_empty() {
        return None;
    }

    // Reject any remaining traversal segments (e.g. `hash/../X.java`).
    if rest.split('/').any(|segment| segment == "..") {
        return None;
    }

    Some(VfsPath::decompiled(
        content_hash.to_string(),
        binary_name.to_string(),
    ))
}

fn is_decompiled_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_legacy_decompiled_uri(uri: &str) -> Option<VfsPath> {
    let rest = uri.strip_prefix("nova-decompile:")?;
    if rest.contains('?') || rest.contains('#') {
        return None;
    }

    // Extract the path component, rejecting URIs with a non-empty authority.
    let path = if let Some(after_slashes) = rest.strip_prefix("//") {
        if !after_slashes.starts_with('/') {
            return None;
        }
        after_slashes
    } else if rest.starts_with('/') {
        rest
    } else {
        return None;
    };

    let path = if path.contains('\\') {
        std::borrow::Cow::Owned(path.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(path)
    };

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return None;
    }
    if segments.contains(&"..") {
        return None;
    }

    let last = segments.last()?;
    let stem = last.strip_suffix(".class")?;
    if stem.is_empty() {
        return None;
    }

    let mut internal = String::new();
    for (idx, seg) in segments.iter().enumerate() {
        if idx > 0 {
            internal.push('/');
        }
        if idx + 1 == segments.len() {
            internal.push_str(stem);
        } else {
            internal.push_str(seg);
        }
    }

    Some(VfsPath::legacy_decompiled(internal))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn jar_uri_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let jar = VfsPath::jar(archive_path, "/com/example/Foo.class");

        let uri = jar.to_uri().expect("jar uri");
        let round = VfsPath::uri(uri);
        assert_eq!(round, jar);
    }

    #[test]
    fn jar_uri_parsing_normalizes_archive_path() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = dir.path().join("lib.jar");
        let with_dotdot = dir.path().join("x").join("..").join("lib.jar");

        let abs = AbsPathBuf::new(with_dotdot).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();
        let uri = format!("jar:{archive_uri}!/com/example/Foo.class");

        let parsed = VfsPath::uri(uri);
        assert_eq!(
            parsed,
            VfsPath::jar(normalized, "/com/example/Foo.class"),
            "jar: URIs should normalize dot segments in the embedded archive file URI"
        );
    }

    #[test]
    fn jar_uri_entry_percent_decoding_and_encoding_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path.clone()).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        // Parsing should decode percent-escapes in the entry path.
        let parsed = VfsPath::uri(format!("jar:{archive_uri}!/com/example/A%20B.class"));
        assert_eq!(
            parsed,
            VfsPath::jar(archive_path.clone(), "/com/example/A B.class")
        );

        // Formatting should encode the entry path so the URI stays unambiguous and valid.
        let jar = VfsPath::jar(archive_path, "/com/example/A B.class");
        let uri = jar.to_uri().expect("jar uri");
        assert!(
            uri.contains("A%20B.class"),
            "expected percent-encoded space in uri: {uri:?}"
        );
        assert_eq!(VfsPath::uri(uri), jar);
    }

    #[test]
    fn jar_uri_entry_allows_percent_encoded_query_and_fragment_chars() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path.clone()).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let parsed = VfsPath::uri(format!("jar:{archive_uri}!/com/example/A%3FB%23C.class"));
        assert_eq!(
            parsed,
            VfsPath::jar(archive_path.clone(), "/com/example/A?B#C.class")
        );

        let jar = VfsPath::jar(archive_path, "/com/example/A?B#C.class");
        let uri = jar.to_uri().expect("jar uri");
        assert!(
            uri.contains("A%3FB%23C.class"),
            "expected percent-encoded reserved chars in uri: {uri:?}"
        );
        assert_eq!(VfsPath::uri(uri), jar);
    }

    #[test]
    fn jar_uri_entry_unicode_roundtrips_via_percent_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");

        let jar = VfsPath::jar(archive_path.clone(), "com/example/Δ.java");
        let uri = jar.to_uri().expect("jar uri");
        assert!(
            uri.contains("%CE%94"),
            "expected percent-encoded UTF-8 bytes: {uri:?}"
        );
        assert_eq!(VfsPath::uri(uri), jar);
    }

    #[test]
    fn jar_uri_entry_invalid_percent_escape_is_not_parsed_as_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let uri = format!("jar:{archive_uri}!/com/example/A%2G.java");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
    }

    #[test]
    fn jmod_uri_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("java.base.jmod");
        let jmod = VfsPath::jmod(archive_path, "classes/java/lang/String.class");

        let uri = jmod.to_uri().expect("jmod uri");
        let round = VfsPath::uri(uri);
        assert_eq!(round, jmod);
    }

    #[test]
    fn jmod_uri_parsing_normalizes_archive_path() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = dir.path().join("java.base.jmod");
        let with_dotdot = dir.path().join("x").join("..").join("java.base.jmod");

        let abs = AbsPathBuf::new(with_dotdot).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();
        let uri = format!("jmod:{archive_uri}!/classes/java/lang/String.class");

        let parsed = VfsPath::uri(uri);
        assert_eq!(
            parsed,
            VfsPath::jmod(normalized, "classes/java/lang/String.class"),
            "jmod: URIs should normalize dot segments in the embedded archive file URI"
        );
    }

    #[test]
    fn archive_entries_normalize_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let a = VfsPath::jar(archive_path.clone(), "com\\example\\A.java");
        let b = VfsPath::jar(archive_path, "com/example/A.java");
        assert_eq!(a, b);
    }

    #[test]
    fn file_uri_paths_are_logically_normalized() {
        #[cfg(not(windows))]
        let (uri, expected) = ("file:///a/b/../c.java", PathBuf::from("/a/c.java"));

        #[cfg(windows)]
        let (uri, expected) = ("file:///C:/a/b/../c.java", PathBuf::from(r"C:\a\c.java"));

        assert_eq!(VfsPath::uri(uri), VfsPath::Local(expected));
    }

    #[test]
    #[cfg(not(windows))]
    fn file_uri_localhost_is_case_insensitive() {
        let uri = "file://LOCALHOST/tmp/A.java";
        assert_eq!(
            VfsPath::uri(uri),
            VfsPath::Local(PathBuf::from("/tmp/A.java"))
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn file_uri_with_query_or_fragment_is_not_treated_as_local_path() {
        for uri in ["file:///tmp/A.java?query", "file:///tmp/A.java#frag"] {
            assert_eq!(VfsPath::uri(uri), VfsPath::Uri(uri.to_string()));
        }
    }

    #[test]
    fn file_uri_normalization_clamps_dotdot_at_root() {
        #[cfg(not(windows))]
        let (uri, expected) = ("file:///a/../../b.java", PathBuf::from("/b.java"));

        #[cfg(windows)]
        let (uri, expected) = ("file:///C:/a/../../b.java", PathBuf::from(r"C:\b.java"));

        assert_eq!(VfsPath::uri(uri), VfsPath::Local(expected));
    }

    #[test]
    fn file_uri_normalization_removes_dot_segments() {
        #[cfg(not(windows))]
        let (uri, expected) = ("file:///a/./b/./c.java", PathBuf::from("/a/b/c.java"));

        #[cfg(windows)]
        let (uri, expected) = ("file:///C:/a/./b/./c.java", PathBuf::from(r"C:\a\b\c.java"));

        assert_eq!(VfsPath::uri(uri), VfsPath::Local(expected));
    }

    #[test]
    fn to_file_uri_normalizes_local_paths() {
        #[cfg(not(windows))]
        {
            let path = VfsPath::Local(PathBuf::from("/a/b/../c.java"));
            assert_eq!(path.to_uri().as_deref(), Some("file:///a/c.java"));
        }

        #[cfg(windows)]
        {
            let path = VfsPath::Local(PathBuf::from(r"C:\a\b\..\c.java"));
            assert_eq!(path.to_uri().as_deref(), Some("file:///C:/a/c.java"));
        }
    }

    #[test]
    fn to_uri_normalizes_archive_paths() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = dir.path().join("lib.jar");
        let with_dotdot = dir.path().join("x").join("..").join("lib.jar");

        let abs = AbsPathBuf::new(normalized).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();
        let entry = "com/example/Foo.class".to_string();
        let expected = format!("jar:{archive_uri}!/{entry}");

        let jar = VfsPath::Archive(ArchivePath::new(ArchiveKind::Jar, with_dotdot, entry));
        assert_eq!(jar.to_uri().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn local_paths_are_logically_normalized() {
        #[cfg(not(windows))]
        {
            let local = VfsPath::local("/a/b/../c.java");
            let uri = VfsPath::uri("file:///a/c.java");
            assert_eq!(local, uri);
        }

        #[cfg(windows)]
        {
            let local = VfsPath::local(r"C:\a\b\..\c.java");
            let uri = VfsPath::uri("file:///C:/a/c.java");
            assert_eq!(local, uri);
        }
    }

    #[test]
    fn local_paths_normalize_redundant_separators() {
        #[cfg(not(windows))]
        {
            let local = VfsPath::local("/a//b///../c.java");
            let uri = VfsPath::uri("file:///a/c.java");
            assert_eq!(local, uri);
        }

        #[cfg(windows)]
        {
            let local = VfsPath::local(r"C:\a\\b\..\c.java");
            let uri = VfsPath::uri("file:///C:/a/c.java");
            assert_eq!(local, uri);
        }
    }

    #[test]
    fn local_paths_clamp_dotdot_at_root() {
        #[cfg(not(windows))]
        {
            let local = VfsPath::local("/a/../../b.java");
            let uri = VfsPath::uri("file:///b.java");
            assert_eq!(local, uri);
        }

        #[cfg(windows)]
        {
            let local = VfsPath::local(r"C:\a\..\..\b.java");
            let uri = VfsPath::uri("file:///C:/b.java");
            assert_eq!(local, uri);
        }
    }

    #[test]
    fn local_paths_remove_dot_segments() {
        #[cfg(not(windows))]
        {
            let local = VfsPath::local("/a/./b/./c.java");
            let uri = VfsPath::uri("file:///a/b/c.java");
            assert_eq!(local, uri);
        }

        #[cfg(windows)]
        {
            let local = VfsPath::local(r"C:\a\.\b\.\c.java");
            let uri = VfsPath::uri("file:///C:/a/b/c.java");
            assert_eq!(local, uri);
        }
    }

    #[cfg(windows)]
    #[test]
    fn local_paths_normalize_drive_letter_case() {
        let upper = VfsPath::local(r"C:\a\b.java");
        let lower = VfsPath::local(r"c:\a\b.java");
        assert_eq!(upper, lower);

        let uri_upper = VfsPath::uri("file:///C:/a/b.java");
        let uri_lower = VfsPath::uri("file:///c:/a/b.java");
        assert_eq!(upper, uri_upper);
        assert_eq!(upper, uri_lower);
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_unc_paths_are_logically_normalized() {
        let uri = "file://server/share/a/b/../c.java";
        let expected = PathBuf::from(r"\\server\share\a\c.java");
        assert_eq!(VfsPath::uri(uri), VfsPath::Local(expected));
    }

    #[cfg(windows)]
    #[test]
    fn local_unc_paths_match_file_uris() {
        let local = VfsPath::local(r"\\server\share\a\b\..\c.java");
        let uri = VfsPath::uri("file://server/share/a/c.java");
        assert_eq!(local, uri);
    }

    #[test]
    fn jar_uris_reject_entry_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let uri = format!("jar:{archive_uri}!/../evil.class");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));

        let uri = format!("jar:{archive_uri}!/a/../evil.class");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
    }

    #[test]
    fn jar_uris_reject_drive_letter_entries() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let uri = format!("jar:{archive_uri}!/C:/evil.class");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
    }

    #[test]
    fn jar_uris_reject_empty_segments() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let uri = format!("jar:{archive_uri}!/a//b.class");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
    }

    #[test]
    fn jar_uris_reject_query_and_fragment_in_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let uri = format!("jar:{archive_uri}!/com/example/Foo.class?query");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));

        let uri = format!("jar:{archive_uri}!/com/example/Foo.class#frag");
        assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
    }

    #[test]
    fn jar_uris_accept_percent_encoded_query_and_fragment_chars_in_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");
        let abs = AbsPathBuf::new(archive_path.clone()).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();

        let parsed = VfsPath::uri(format!("jar:{archive_uri}!/com/example/Foo%3F.class"));
        assert_eq!(
            parsed,
            VfsPath::jar(archive_path.clone(), "com/example/Foo?.class")
        );

        let parsed = VfsPath::uri(format!("jar:{archive_uri}!/com/example/Foo%23.class"));
        assert_eq!(parsed, VfsPath::jar(archive_path, "com/example/Foo#.class"));
    }

    #[test]
    fn jar_constructor_rejects_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");

        assert!(matches!(
            VfsPath::jar(archive_path.clone(), "../evil.class"),
            VfsPath::Uri(_)
        ));
        assert!(matches!(
            VfsPath::jar(archive_path.clone(), "a/../evil.class"),
            VfsPath::Uri(_)
        ));
        assert!(matches!(
            VfsPath::jar(archive_path.clone(), "a/b/../evil.class"),
            VfsPath::Uri(_)
        ));
        assert!(matches!(
            VfsPath::jar(archive_path.clone(), "C:/evil.class"),
            VfsPath::Uri(_)
        ));
    }

    #[test]
    fn jar_constructor_allows_query_and_fragment_chars_in_entry_when_encoded_in_uri() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("lib.jar");

        let jar_q = VfsPath::jar(archive_path.clone(), "com/example/Foo?.class");
        let uri_q = jar_q.to_uri().expect("jar uri");
        assert!(
            uri_q.contains("Foo%3F.class"),
            "expected percent-encoded '?': {uri_q:?}"
        );
        assert_eq!(VfsPath::uri(uri_q), jar_q);

        let jar_f = VfsPath::jar(archive_path, "com/example/Foo#.class");
        let uri_f = jar_f.to_uri().expect("jar uri");
        assert!(
            uri_f.contains("Foo%23.class"),
            "expected percent-encoded '#': {uri_f:?}"
        );
        assert_eq!(VfsPath::uri(uri_f), jar_f);
    }

    #[test]
    fn jar_constructor_normalizes_archive_path() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = dir.path().join("lib.jar");
        let with_dotdot = dir.path().join("x").join("..").join("lib.jar");

        let a = VfsPath::jar(with_dotdot, "/com/example/Foo.class");
        let b = VfsPath::jar(normalized, "/com/example/Foo.class");
        assert_eq!(a, b);
    }

    #[test]
    fn jmod_constructor_normalizes_archive_path() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = dir.path().join("java.base.jmod");
        let with_dotdot = dir.path().join("x").join("..").join("java.base.jmod");

        let a = VfsPath::jmod(with_dotdot, "classes/java/lang/String.class");
        let b = VfsPath::jmod(normalized, "classes/java/lang/String.class");
        assert_eq!(a, b);
    }

    #[test]
    fn jar_constructor_fallback_uri_normalizes_archive_path() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = dir.path().join("lib.jar");
        let with_dotdot = dir.path().join("x").join("..").join("lib.jar");

        let abs = AbsPathBuf::new(normalized).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();
        let expected = format!("jar:{archive_uri}!/a/b/../c.class");

        assert_eq!(
            VfsPath::jar(with_dotdot, "a/b/../c.class"),
            VfsPath::Uri(expected)
        );
    }

    #[test]
    fn jmod_constructor_fallback_uri_normalizes_archive_path() {
        let dir = tempfile::tempdir().unwrap();
        let normalized = dir.path().join("java.base.jmod");
        let with_dotdot = dir.path().join("x").join("..").join("java.base.jmod");

        let abs = AbsPathBuf::new(normalized).unwrap();
        let archive_uri = path_to_file_uri(&abs).unwrap();
        let expected = format!("jmod:{archive_uri}!/a/b/../c.class");

        assert_eq!(
            VfsPath::jmod(with_dotdot, "a/b/../c.class"),
            VfsPath::Uri(expected)
        );
    }

    #[test]
    fn decompiled_uri_roundtrips() {
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");
        let uri = path.to_uri().expect("decompiled uri");
        let round = VfsPath::uri(uri);
        assert_eq!(round, path);
    }

    #[test]
    fn decompiled_constructor_rejects_empty_binary_name() {
        assert!(matches!(VfsPath::decompiled(HASH_64, ""), VfsPath::Uri(_)));
    }

    #[test]
    fn decompiled_constructor_rejects_query_fragment_characters() {
        assert!(matches!(
            VfsPath::decompiled(HASH_64, "Foo?bar"),
            VfsPath::Uri(_)
        ));
        assert!(matches!(
            VfsPath::decompiled(HASH_64, "Foo#bar"),
            VfsPath::Uri(_)
        ));
    }

    #[test]
    fn decompiled_uri_rejects_invalid_hashes() {
        let wrong_len = "a".repeat(63);
        let non_hex = "g".repeat(64);

        for uri in [
            format!("nova:///decompiled/{wrong_len}/com.example.Foo.java"),
            format!("nova:///decompiled/{non_hex}/com.example.Foo.java"),
        ] {
            assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
        }
    }

    #[test]
    fn decompiled_uri_rejects_non_canonical_forms() {
        for uri in [
            // Non-empty authority.
            format!("nova://host/decompiled/{HASH_64}/com.example.Foo.java"),
            // Non-canonical path form (missing `//` authority marker).
            format!("nova:/decompiled/{HASH_64}/com.example.Foo.java"),
            // Non-canonical extra slashes / empty segments.
            format!("nova:////decompiled//{HASH_64}//com.example.Foo.java"),
            // Query/fragment are not permitted for canonical virtual documents.
            format!("nova:///decompiled/{HASH_64}/com.example.Foo.java?query"),
            format!("nova:///decompiled/{HASH_64}/com.example.Foo.java#fragment"),
        ] {
            assert!(matches!(VfsPath::uri(uri), VfsPath::Uri(_)));
        }
    }

    #[test]
    fn decompiled_uri_rejects_dotdot_segments() {
        let uri = format!("nova:///decompiled/{HASH_64}/../X.java");
        assert_eq!(VfsPath::uri(uri.as_str()), VfsPath::Uri(uri));
    }

    #[test]
    fn decompiled_uri_rejects_binary_name_that_normalizes_to_empty() {
        let uri = format!("nova:///decompiled/{HASH_64}/..java");
        assert_eq!(VfsPath::uri(uri.clone()), VfsPath::Uri(uri));
    }

    #[test]
    fn decompiled_uri_normalizes_binary_name_dots() {
        let uri = format!("nova:///decompiled/{HASH_64}/com..example..Foo.java");
        let parsed = VfsPath::uri(uri);
        let expected = format!("nova:///decompiled/{HASH_64}/com.example.Foo.java");
        assert_eq!(parsed, VfsPath::decompiled(HASH_64, "com.example.Foo"));
        assert_eq!(parsed.to_uri().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn decompiled_uri_normalizes_binary_name_backslashes() {
        let uri = format!("nova:///decompiled/{HASH_64}/com\\example\\Foo.java");
        let parsed = VfsPath::uri(uri);
        let expected = format!("nova:///decompiled/{HASH_64}/com.example.Foo.java");
        assert_eq!(parsed, VfsPath::decompiled(HASH_64, "com.example.Foo"));
        assert_eq!(parsed.to_uri().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn unknown_nova_uri_stays_uri() {
        let uri = "nova:///something/else";
        assert_eq!(VfsPath::uri(uri), VfsPath::Uri(uri.to_string()));
    }

    #[test]
    fn legacy_decompiled_uri_roundtrips() {
        let path = VfsPath::legacy_decompiled("com/example/Foo");
        let uri = path.to_uri().expect("uri");
        assert_eq!(uri, "nova-decompile:///com/example/Foo.class");
        let round = VfsPath::uri(uri);
        assert_eq!(round, path);
    }

    #[test]
    fn legacy_decompiled_uri_normalizes_extra_slashes_when_printing() {
        let parsed = VfsPath::uri("nova-decompile:////com//example///Foo.class");
        assert_eq!(
            parsed.to_uri().as_deref(),
            Some("nova-decompile:///com/example/Foo.class")
        );
    }

    #[test]
    fn legacy_decompiled_uri_rejects_dotdot_segments() {
        let uri = "nova-decompile:///com/example/../Foo.class";
        assert_eq!(VfsPath::uri(uri), VfsPath::Uri(uri.to_string()));
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_uri_roundtrips_for_local_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Main.java");
        let vfs_path = VfsPath::local(path.clone());
        let uri = vfs_path.to_lsp_uri().expect("lsp uri");
        let round = VfsPath::from(&uri);
        assert_eq!(round, vfs_path);
    }
}
