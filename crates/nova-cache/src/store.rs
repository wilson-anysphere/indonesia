use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

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

        std::fs::copy(&path, dest)?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HttpStore;

impl CacheStore for HttpStore {
    fn fetch(&self, url: &str, dest: &Path) -> Result<()> {
        let response = ureq::get(url).call().map_err(|err| CacheError::Http {
            message: err.to_string(),
        })?;

        let mut reader = response.into_reader();
        let mut file = File::create(dest)?;
        io::copy(&mut reader, &mut file)?;
        Ok(())
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

        let runtime = tokio::runtime::Runtime::new().map_err(|err| CacheError::S3 {
            message: err.to_string(),
        })?;

        runtime.block_on(async move {
            let config = aws_config::load_from_env().await;
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

            let data = object
                .body
                .collect()
                .await
                .map_err(|err| CacheError::S3 {
                    message: err.to_string(),
                })?
                .into_bytes();

            std::fs::write(dest, data)?;
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
