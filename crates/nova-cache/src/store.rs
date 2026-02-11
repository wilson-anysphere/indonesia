use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

#[cfg(feature = "s3")]
use std::ffi::{OsStr, OsString};

#[cfg(feature = "s3")]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{CacheError, Result};

pub trait CacheStore {
    fn fetch(&self, url: &str, dest: &Path) -> Result<()>;
}

const URL_REDACTION: &str = "<redacted>";

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalStore;

impl CacheStore for LocalStore {
    fn fetch(&self, url: &str, dest: &Path) -> Result<()> {
        let path = if let Some(stripped) = url.strip_prefix("file://") {
            PathBuf::from(stripped)
        } else {
            PathBuf::from(url)
        };

        if path.is_dir() {
            return Err(CacheError::UnsupportedFetchUrl {
                url: sanitize_fetch_url(url),
            });
        }

        crate::util::atomic_write_with(dest, |out| {
            let mut reader = File::open(&path)?;
            io::copy(&mut reader, out)?;
            Ok(())
        })
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HttpStore;

impl CacheStore for HttpStore {
    fn fetch(&self, url: &str, dest: &Path) -> Result<()> {
        let safe_url = sanitize_fetch_url(url);
        let response = ureq::get(url).call().map_err(|err| {
            let message = match err {
                ureq::Error::Status(code, _response) => {
                    format!("server returned status {code} for {safe_url}")
                }
                ureq::Error::Transport(transport) => {
                    format!("transport error for {safe_url}: {transport}")
                }
            };
            CacheError::Http { message }
        })?;

        crate::util::atomic_write_with(dest, |out| {
            let mut reader = response.into_reader();
            io::copy(&mut reader, out)?;
            Ok(())
        })
    }
}

#[cfg(feature = "s3")]
#[derive(Debug, Default, Clone, Copy)]
pub struct S3Store;

#[cfg(feature = "s3")]
impl CacheStore for S3Store {
    fn fetch(&self, url: &str, dest: &Path) -> Result<()> {
        let (bucket, key) = parse_s3_url(url).ok_or_else(|| CacheError::UnsupportedFetchUrl {
            url: sanitize_fetch_url(url),
        })?;

        let max_download_bytes = s3_max_download_bytes_from_env()?;
        let dest = dest.to_path_buf();

        // We use a single-thread runtime here: `fetch` is a sync API, and spinning up a full
        // multi-thread runtime (defaulting to `num_cpus` worker threads) is unnecessarily heavy
        // for a single download.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| CacheError::S3 {
                message: err.to_string(),
            })?;

        runtime.block_on(async move {
            let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let client = aws_sdk_s3::Client::new(&config);
            let object = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| CacheError::S3 {
                    message: err.to_string(),
                })?;

            stream_async_read_to_path(object.body.into_async_read(), &dest, max_download_bytes)
                .await?;
            Ok(())
        })
    }
}

#[cfg(feature = "s3")]
fn parse_s3_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("s3://")?;
    let (bucket, key) = rest.split_once('/')?;
    if bucket.is_empty() || key.is_empty() {
        return None;
    }
    Some((bucket.to_string(), key.to_string()))
}

#[cfg(feature = "s3")]
fn s3_max_download_bytes_from_env() -> Result<Option<u64>> {
    // Safety valve: large cache packages can be multi-GB, and disk is shared across many agents.
    // When set (in bytes), downloads larger than this will fail before being published to `dest`.
    let raw = match std::env::var("NOVA_CACHE_MAX_DOWNLOAD_BYTES") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => {
            return Err(CacheError::S3 {
                message: err.to_string(),
            })
        }
    };

    let raw = raw.trim();
    if raw.is_empty() || raw == "0" {
        return Ok(None);
    }

    let parsed = raw.parse::<u64>().map_err(|err| CacheError::S3 {
        message: format!("invalid NOVA_CACHE_MAX_DOWNLOAD_BYTES={raw:?}: {err}"),
    })?;
    Ok(Some(parsed))
}

#[cfg(feature = "s3")]
static TMP_DOWNLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "s3")]
async fn open_unique_tmp_file(dest: &Path) -> io::Result<(PathBuf, tokio::fs::File)> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    let file_name = dest.file_name().unwrap_or_else(|| OsStr::new("download"));
    let pid = std::process::id();

    loop {
        let counter = TMP_DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = OsString::from(".");
        tmp_name.push(file_name);
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(&tmp_name);

        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(feature = "s3")]
fn sync_parent_dir_best_effort(dest: &Path) {
    // Best-effort durability: after publishing a new file via rename, fsync the directory entry
    // so the rename survives a crash/power loss.
    #[cfg(unix)]
    if let Some(parent) = dest.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
    }

    #[cfg(not(unix))]
    let _ = dest;
}

#[cfg(feature = "s3")]
async fn stream_async_read_to_path(
    reader: impl tokio::io::AsyncRead,
    dest: &Path,
    max_bytes: Option<u64>,
) -> Result<u64> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let (tmp_path, file) = open_unique_tmp_file(dest).await?;

    let result = async {
        let mut file = file;

        let mut reader = Box::pin(reader);
        let copied = match max_bytes {
            Some(max_bytes) => {
                let mut limited = reader.take(max_bytes.saturating_add(1));
                tokio::io::copy(&mut limited, &mut file).await?
            }
            None => tokio::io::copy(&mut reader, &mut file).await?,
        };

        file.flush().await?;
        // Best-effort: ensure bytes are on disk before we publish the final path.
        let _ = file.sync_all().await;
        drop(file);

        if let Some(max_bytes) = max_bytes {
            if copied > max_bytes {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(CacheError::S3 {
                    message: format!(
                        "download exceeded NOVA_CACHE_MAX_DOWNLOAD_BYTES={max_bytes} (downloaded {copied} bytes)"
                    ),
                });
            }
        }

        match tokio::fs::rename(&tmp_path, dest).await {
            Ok(()) => {
                sync_parent_dir_best_effort(dest);
                Ok(copied)
            }
            Err(err) => {
                // On Windows, rename doesn't overwrite. Try remove + rename.
                if cfg!(windows)
                    && (err.kind() == io::ErrorKind::AlreadyExists
                        || tokio::fs::metadata(dest).await.is_ok())
                {
                    let _ = tokio::fs::remove_file(dest).await;
                    if tokio::fs::rename(&tmp_path, dest).await.is_ok() {
                        sync_parent_dir_best_effort(dest);
                        return Ok(copied);
                    }
                }

                // If we can't rename, fall back to copying the temp file into place.
                //
                // Note: this is less atomic than `rename()`, but keeps the behavior working on
                // platforms/filesystems where rename cannot replace the destination path.
                let mut tmp_reader = tokio::fs::File::open(&tmp_path).await?;
                let copy_result = async {
                    let mut dest_file = tokio::fs::File::create(dest).await?;
                    tokio::io::copy(&mut tmp_reader, &mut dest_file).await?;
                    dest_file.flush().await?;
                    let _ = dest_file.sync_all().await;
                    Ok::<(), std::io::Error>(())
                }
                .await;
                if let Err(err) = copy_result {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(err.into());
                }
                let _ = tokio::fs::remove_file(&tmp_path).await;
                sync_parent_dir_best_effort(dest);
                Ok(copied)
            }
        }
    }
    .await;

    if result.is_err() {
        // Best-effort cleanup: if we failed before publishing the final path, don't leave behind
        // a potentially huge partial download.
        let _ = tokio::fs::remove_file(&tmp_path).await;
    }

    result
}

pub fn store_for_url(url: &str) -> Result<Box<dyn CacheStore>> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(Box::new(HttpStore));
    }

    if url.starts_with("s3://") {
        #[cfg(feature = "s3")]
        {
            return Ok(Box::new(S3Store));
        }
        #[cfg(not(feature = "s3"))]
        {
            return Err(CacheError::UnsupportedFetchUrl {
                url: sanitize_fetch_url(url),
            });
        }
    }

    Ok(Box::new(LocalStore))
}

fn sanitize_fetch_url(url: &str) -> String {
    // Treat any `scheme://...` substring as a URL. Cache package URLs can be pre-signed (S3, etc.)
    // and often contain credentials in query parameters; never echo those values in errors.
    let Some(scheme_idx) = url.find("://") else {
        return url.to_owned();
    };

    let (scheme, rest) = url.split_at(scheme_idx + 3);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);

    let authority = if let Some(at_pos) = authority.rfind('@') {
        let host = &authority[at_pos + 1..];
        format!("{URL_REDACTION}@{host}")
    } else {
        authority.to_owned()
    };

    let tail = sanitize_url_tail(tail);
    format!("{scheme}{authority}{tail}")
}

fn sanitize_url_tail(tail: &str) -> String {
    let (before_fragment, has_fragment) = match tail.find('#') {
        Some(pos) => (&tail[..pos], true),
        None => (tail, false),
    };

    let sanitized = match before_fragment.find('?') {
        Some(q_pos) => {
            let (before_q, after_q) = before_fragment.split_at(q_pos + 1);
            let query = &after_q;
            let sanitized_query = sanitize_query(query);
            format!("{before_q}{sanitized_query}")
        }
        None => before_fragment.to_owned(),
    };

    if has_fragment {
        format!("{sanitized}#{URL_REDACTION}")
    } else {
        sanitized
    }
}

fn sanitize_query(query: &str) -> String {
    let mut out = String::new();
    for (idx, part) in query.split('&').enumerate() {
        if idx > 0 {
            out.push('&');
        }
        if part.is_empty() {
            continue;
        }

        match part.split_once('=') {
            Some((key, _value)) => {
                out.push_str(key);
                out.push('=');
                // Be conservative: query parameters often contain secrets under arbitrary keys.
                out.push_str(URL_REDACTION);
            }
            None => {
                out.push_str(part);
                out.push('=');
                out.push_str(URL_REDACTION);
            }
        }
    }
    out
}

#[cfg(all(test, not(feature = "s3")))]
mod redaction_tests {
    use super::*;

    #[test]
    fn unsupported_fetch_url_errors_redact_query_values_and_userinfo() {
        let secret = "super-secret-token";
        let url = format!("s3://user:pass@bucket/key?X-Amz-Signature={secret}&foo=bar#fragment");

        let err = match store_for_url(&url) {
            Ok(_) => panic!("expected s3 to be unsupported in tests"),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            !message.contains(secret),
            "CacheError should not echo URL query secrets: {message}"
        );
        assert!(
            message.contains(URL_REDACTION),
            "CacheError should include redaction marker: {message}"
        );
    }
}

#[cfg(all(test, feature = "s3"))]
mod tests {
    use super::*;

    #[test]
    fn parse_s3_url_valid() {
        assert_eq!(
            parse_s3_url("s3://bucket/key"),
            Some(("bucket".to_string(), "key".to_string()))
        );
        assert_eq!(
            parse_s3_url("s3://bucket/dir/file.tar.zst"),
            Some(("bucket".to_string(), "dir/file.tar.zst".to_string()))
        );
    }

    #[test]
    fn parse_s3_url_invalid() {
        assert_eq!(parse_s3_url("https://bucket/key"), None);
        assert_eq!(parse_s3_url("s3://"), None);
        assert_eq!(parse_s3_url("s3://bucket"), None);
        assert_eq!(parse_s3_url("s3:///key"), None);
        assert_eq!(parse_s3_url("s3://bucket/"), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_async_read_to_path_writes_large_payload() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let dest = tmp.path().join("nested").join("payload.bin");

        let bytes = vec![0xAB_u8; 16 * 1024 * 1024];
        let len = bytes.len();
        let cursor = std::io::Cursor::new(bytes);

        let copied = stream_async_read_to_path(cursor, &dest, None).await?;
        assert_eq!(copied as usize, len);

        let on_disk = tokio::fs::read(&dest).await?;
        assert_eq!(on_disk.len(), len);
        assert!(on_disk.iter().all(|&b| b == 0xAB));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_async_read_to_path_enforces_max_bytes() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let dest = tmp.path().join("payload.bin");

        let bytes = vec![0xCD_u8; 1024 * 1024];
        let cursor = std::io::Cursor::new(bytes);

        let err = stream_async_read_to_path(cursor, &dest, Some(64 * 1024))
            .await
            .unwrap_err();
        match err {
            CacheError::S3 { message } => {
                assert!(message.contains("NOVA_CACHE_MAX_DOWNLOAD_BYTES"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        assert!(!dest.exists());
        for entry in std::fs::read_dir(tmp.path())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".tmp."),
                "left behind temp download file {name:?} in {}",
                tmp.path().display()
            );
        }
        Ok(())
    }
}
