use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

#[cfg(feature = "s3")]
use std::ffi::{OsStr, OsString};

#[cfg(feature = "s3")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "s3")]
use std::sync::OnceLock;

use crate::error::{CacheError, Result};

pub trait CacheStore {
    fn fetch(&self, url: &str, dest: &Path) -> Result<()>;
}

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
                url: url.to_string(),
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
        let response = ureq::get(url).call().map_err(|err| CacheError::Http {
            message: err.to_string(),
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
            url: url.to_string(),
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
    static SYNC_PARENT_DIR_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    // Best-effort durability: after publishing a new file via rename, fsync the directory entry
    // so the rename survives a crash/power loss.
    #[cfg(unix)]
    if let Some(parent) = dest.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        match std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                if SYNC_PARENT_DIR_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.cache",
                        dir = %parent.display(),
                        error = %err,
                        "failed to sync cache parent directory (best effort)"
                    );
                }
            }
        }
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
        static TMP_FILE_SYNC_ERROR_LOGGED: OnceLock<()> = OnceLock::new();
        if let Err(err) = file.sync_all().await {
            if TMP_FILE_SYNC_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.cache",
                    error = %err,
                    "failed to sync cache download temp file (best effort)"
                );
            }
        }
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
                    static DEST_FILE_SYNC_ERROR_LOGGED: OnceLock<()> = OnceLock::new();
                    if let Err(err) = dest_file.sync_all().await {
                        if DEST_FILE_SYNC_ERROR_LOGGED.set(()).is_ok() {
                            tracing::debug!(
                                target = "nova.cache",
                                path = %dest.display(),
                                error = %err,
                                "failed to sync cache download destination file (best effort)"
                            );
                        }
                    }
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
                url: url.to_string(),
            });
        }
    }

    Ok(Box::new(LocalStore))
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
