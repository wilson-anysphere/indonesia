//! Path/URI utilities.
//!
//! `nova-core` keeps dependencies light, so file-URI handling is implemented
//! without pulling in a full URL parser. The helpers here only support `file:`
//! URIs which are what Nova uses to communicate with LSP clients.

use std::ffi::OsString;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};

/// An absolute filesystem path.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct AbsPathBuf(PathBuf);

impl AbsPathBuf {
    pub fn new(path: PathBuf) -> Result<Self, AbsPathError> {
        if path.is_absolute() {
            Ok(Self(path))
        } else {
            Err(AbsPathError::NotAbsolute(path))
        }
    }

    /// Canonicalize a path on disk.
    ///
    /// This resolves symlinks and normalizes platform-specific path quirks.
    pub fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = dunce::canonicalize(path)?;
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl Deref for AbsPathBuf {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

impl std::fmt::Debug for AbsPathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AbsPathBuf").field(&self.0).finish()
    }
}

impl TryFrom<PathBuf> for AbsPathBuf {
    type Error = AbsPathError;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug)]
pub enum AbsPathError {
    NotAbsolute(PathBuf),
}

impl std::fmt::Display for AbsPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbsPathError::NotAbsolute(path) => {
                write!(f, "path is not absolute: {}", path.display())
            }
        }
    }
}

impl std::error::Error for AbsPathError {}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PathToUriError {
    NonUtf8Path,
    InvalidUri,
    UnsupportedWindowsPath,
}

impl std::fmt::Display for PathToUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathToUriError::NonUtf8Path => f.write_str("path is not valid UTF-8"),
            PathToUriError::InvalidUri => f.write_str("path could not be converted into a valid URI"),
            PathToUriError::UnsupportedWindowsPath => f.write_str("unsupported Windows path"),
        }
    }
}

impl std::error::Error for PathToUriError {}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum UriToPathError {
    NotAFileUri,
    InvalidUri,
    InvalidPercentEncoding,
    InvalidUtf8,
    NotAbsolutePath,
    UnsupportedWindowsUri,
}

impl std::fmt::Display for UriToPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UriToPathError::NotAFileUri => f.write_str("URI is not a file URI"),
            UriToPathError::InvalidUri => f.write_str("invalid file URI"),
            UriToPathError::InvalidPercentEncoding => {
                f.write_str("invalid percent-encoding in URI")
            }
            UriToPathError::InvalidUtf8 => f.write_str("decoded URI is not valid UTF-8"),
            UriToPathError::NotAbsolutePath => f.write_str("URI does not map to an absolute path"),
            UriToPathError::UnsupportedWindowsUri => f.write_str("unsupported Windows file: URI"),
        }
    }
}

impl std::error::Error for UriToPathError {}

/// Convert an absolute filesystem path into a `file://` URI string.
pub fn path_to_file_uri(path: &AbsPathBuf) -> Result<String, PathToUriError> {
    #[cfg(windows)]
    {
        path_to_file_uri_windows(path)
    }

    #[cfg(not(windows))]
    {
        let path = path.as_path().to_str().ok_or(PathToUriError::NonUtf8Path)?;
        Ok(format!("file://{}", encode_uri_path(path)))
    }
}

/// Convert a `file://` URI string into an absolute filesystem path.
pub fn file_uri_to_path(uri: &str) -> Result<AbsPathBuf, UriToPathError> {
    #[cfg(windows)]
    {
        file_uri_to_path_windows(uri)
    }

    #[cfg(not(windows))]
    {
        let (authority, path) = split_file_uri(uri)?;
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return Err(UriToPathError::InvalidUri);
        }

        let decoded = percent_decode(path)?;
        if !decoded.starts_with('/') {
            return Err(UriToPathError::NotAbsolutePath);
        }
        let normalized = normalize_logical_path(Path::new(&decoded));
        AbsPathBuf::new(normalized).map_err(|_| UriToPathError::NotAbsolutePath)
    }
}

fn normalize_logical_path(path: &Path) -> PathBuf {
    let mut prefix: Option<OsString> = None;
    let mut has_root = false;
    let mut stack: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix_component) => {
                prefix = Some(prefix_component.as_os_str().to_owned());
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

#[cfg(feature = "lsp")]
pub fn path_to_lsp_uri(path: &AbsPathBuf) -> Result<lsp_types::Uri, PathToUriError> {
    let uri = path_to_file_uri(path)?;
    uri.parse().map_err(|_| PathToUriError::InvalidUri)
}

#[cfg(feature = "lsp")]
pub fn lsp_uri_to_path(uri: &lsp_types::Uri) -> Result<AbsPathBuf, UriToPathError> {
    file_uri_to_path(uri.as_str())
}

fn split_file_uri(uri: &str) -> Result<(&str, &str), UriToPathError> {
    if uri.contains(['#', '?']) {
        return Err(UriToPathError::InvalidUri);
    }

    let rest = uri
        .strip_prefix("file:")
        .ok_or(UriToPathError::NotAFileUri)?;
    let rest = if let Some(rest) = rest.strip_prefix("//") {
        rest
    } else if let Some(_rest) = rest.strip_prefix('/') {
        // `file:/path` form (single slash).
        return Ok(("", &uri["file:".len()..]));
    } else {
        return Err(UriToPathError::InvalidUri);
    };

    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    if path.is_empty() {
        return Err(UriToPathError::InvalidUri);
    }

    Ok((authority, path))
}

fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.as_bytes() {
        let b = *b;
        if is_uri_unreserved(b) || b == b'/' || b == b':' {
            out.push(b as char);
        } else {
            push_pct_encoded(&mut out, b);
        }
    }
    out
}

fn percent_decode(input: &str) -> Result<String, UriToPathError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());

    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(UriToPathError::InvalidPercentEncoding);
                }
                let hi = from_hex(bytes[i + 1]).ok_or(UriToPathError::InvalidPercentEncoding)?;
                let lo = from_hex(bytes[i + 2]).ok_or(UriToPathError::InvalidPercentEncoding)?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8(out).map_err(|_| UriToPathError::InvalidUtf8)
}

fn is_uri_unreserved(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~')
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn push_pct_encoded(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0x0F) as usize] as char);
}

#[cfg(windows)]
fn path_to_file_uri_windows(path: &AbsPathBuf) -> Result<String, PathToUriError> {
    let path = path.as_path().to_str().ok_or(PathToUriError::NonUtf8Path)?;

    // UNC paths: \\server\share\path
    if let Some(stripped) = path.strip_prefix(r"\\") {
        let mut parts = stripped.split('\\');
        let host = parts.next().ok_or(PathToUriError::UnsupportedWindowsPath)?;
        let share = parts.next().ok_or(PathToUriError::UnsupportedWindowsPath)?;
        // Canonicalize the host/share casing so semantically-equal UNC paths map to
        // a single URI/key.
        let host = host.to_ascii_lowercase();
        let share = share.to_ascii_lowercase();

        let mut uri = String::from("file://");
        uri.push_str(&encode_uri_path(&host));
        uri.push('/');
        uri.push_str(&encode_uri_path(&share));

        for part in parts {
            uri.push('/');
            uri.push_str(&encode_uri_path(part));
        }
        return Ok(uri);
    }

    // Drive letter paths: C:\path\to\file
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        // Canonicalize the drive letter to uppercase so `c:` and `C:` map to the same key.
        let drive = (bytes[0] as char).to_ascii_uppercase();
        let mut rest = &path[2..];
        if rest.starts_with('\\') || rest.starts_with('/') {
            rest = &rest[1..];
        }
        let rest = rest.replace('\\', "/");

        let mut uri = String::from("file:///");
        uri.push(drive);
        uri.push(':');
        uri.push('/');
        uri.push_str(&encode_uri_path(&rest));
        return Ok(uri);
    }

    Err(PathToUriError::UnsupportedWindowsPath)
}

#[cfg(windows)]
fn file_uri_to_path_windows(uri: &str) -> Result<AbsPathBuf, UriToPathError> {
    let (authority, path) = split_file_uri(uri)?;
    let authority = percent_decode(authority)?.to_ascii_lowercase();
    let path = percent_decode(path)?;

    if !authority.is_empty() && authority != "localhost" {
        // UNC
        let path = path.strip_prefix('/').ok_or(UriToPathError::InvalidUri)?;
        let mut parts = path.split('/');
        let share = parts
            .next()
            .ok_or(UriToPathError::UnsupportedWindowsUri)?
            .to_ascii_lowercase();

        let mut buf = String::from(r"\\");
        buf.push_str(&authority);
        buf.push('\\');
        buf.push_str(&share);
        for part in parts {
            if part.is_empty() {
                continue;
            }
            buf.push('\\');
            buf.push_str(part);
        }

        let normalized = normalize_logical_path(Path::new(&buf));
        return AbsPathBuf::new(normalized).map_err(|_| UriToPathError::NotAbsolutePath);
    }

    // Local file path.
    let path = path.strip_prefix('/').ok_or(UriToPathError::InvalidUri)?;
    if path.len() < 2 || !path.as_bytes()[0].is_ascii_alphabetic() || path.as_bytes()[1] != b':' {
        return Err(UriToPathError::UnsupportedWindowsUri);
    }

    let mut buf = String::new();
    // Canonicalize drive letter to uppercase so semantically-equal paths map to a single key.
    buf.push((path.as_bytes()[0] as char).to_ascii_uppercase());
    buf.push(':');
    buf.push('\\');
    buf.push_str(&path[2..].trim_start_matches('/').replace('/', "\\"));

    let normalized = normalize_logical_path(Path::new(&buf));
    AbsPathBuf::new(normalized).map_err(|_| UriToPathError::NotAbsolutePath)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(windows))]
    fn unix_file_uri_roundtrip() {
        let path = AbsPathBuf::new(PathBuf::from("/tmp/a b.java")).unwrap();
        let uri = path_to_file_uri(&path).unwrap();
        assert_eq!(uri, "file:///tmp/a%20b.java");

        let back = file_uri_to_path(&uri).unwrap();
        assert_eq!(back, path);
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_file_uri_accepts_localhost_case_insensitive() {
        let path = file_uri_to_path("file://LOCALHOST/tmp/a.java").unwrap();
        assert_eq!(path, AbsPathBuf::new(PathBuf::from("/tmp/a.java")).unwrap());
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_file_uri_rejects_query_and_fragment() {
        assert!(matches!(
            file_uri_to_path("file:///tmp/a.java?query"),
            Err(UriToPathError::InvalidUri)
        ));
        assert!(matches!(
            file_uri_to_path("file:///tmp/a.java#frag"),
            Err(UriToPathError::InvalidUri)
        ));
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_file_uri_allows_percent_encoded_reserved_chars() {
        let path = file_uri_to_path("file:///tmp/a%3Fb.java").unwrap();
        assert_eq!(path, AbsPathBuf::new(PathBuf::from("/tmp/a?b.java")).unwrap());
    }

    #[test]
    #[cfg(windows)]
    fn windows_drive_file_uri_roundtrip() {
        let path = AbsPathBuf::new(PathBuf::from(r"C:\tmp\a b.java")).unwrap();
        let uri = path_to_file_uri(&path).unwrap();
        assert_eq!(uri, "file:///C:/tmp/a%20b.java");

        let back = file_uri_to_path(&uri).unwrap();
        assert_eq!(back, path);
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_file_uri_normalizes_dotdot() {
        let path = file_uri_to_path("file:///a/b/../c.java").unwrap();
        assert_eq!(path, AbsPathBuf::new(PathBuf::from("/a/c.java")).unwrap());
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_file_uri_removes_dot_segments() {
        let path = file_uri_to_path("file:///a/./b/./c.java").unwrap();
        assert_eq!(path, AbsPathBuf::new(PathBuf::from("/a/b/c.java")).unwrap());
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_file_uri_clamps_dotdot_at_root() {
        let path = file_uri_to_path("file:///a/../../b.java").unwrap();
        assert_eq!(path, AbsPathBuf::new(PathBuf::from("/b.java")).unwrap());
    }

    #[test]
    #[cfg(windows)]
    fn windows_drive_letter_is_canonicalized() {
        let uri = "file:///c:/tmp/a%20b.java";
        let path = file_uri_to_path(uri).unwrap();
        assert_eq!(
            path,
            AbsPathBuf::new(PathBuf::from(r"C:\tmp\a b.java")).unwrap()
        );

        let uri2 = path_to_file_uri(&path).unwrap();
        assert_eq!(uri2, "file:///C:/tmp/a%20b.java");
    }

    #[test]
    #[cfg(windows)]
    fn windows_drive_file_uri_normalizes_dotdot() {
        let path = file_uri_to_path("file:///C:/a/b/../c.java").unwrap();
        assert_eq!(
            path,
            AbsPathBuf::new(PathBuf::from(r"C:\a\c.java")).unwrap()
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_drive_file_uri_removes_dot_segments() {
        let path = file_uri_to_path("file:///C:/a/./b/./c.java").unwrap();
        assert_eq!(
            path,
            AbsPathBuf::new(PathBuf::from(r"C:\a\b\c.java")).unwrap()
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_drive_file_uri_clamps_dotdot_at_root() {
        let path = file_uri_to_path("file:///C:/a/../../b.java").unwrap();
        assert_eq!(path, AbsPathBuf::new(PathBuf::from(r"C:\b.java")).unwrap());
    }

    #[test]
    #[cfg(windows)]
    fn windows_unc_file_uri_is_logically_normalized() {
        let path = file_uri_to_path("file://server/share/a/b/../c.java").unwrap();
        assert_eq!(
            path,
            AbsPathBuf::new(PathBuf::from(r"\\server\share\a\c.java")).unwrap()
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_unc_host_is_canonicalized() {
        let uri = "file://SERVER/Share/tmp/a.java";
        let path = file_uri_to_path(uri).unwrap();
        assert_eq!(
            path,
            AbsPathBuf::new(PathBuf::from(r"\\server\share\tmp\a.java")).unwrap()
        );
        let uri2 = path_to_file_uri(&path).unwrap();
        assert_eq!(uri2, "file://server/share/tmp/a.java");
    }
}
