//! Abstractions for reading dependency archives (JARs) and exploded directories.
//!
//! In the full Nova system, this would support classpath caching and efficient
//! archive access. For configuration metadata indexing we only need best-effort
//! file reads.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;
use zip::ZipArchive;

#[derive(Clone, Debug)]
pub struct Archive {
    path: PathBuf,
}

impl Archive {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read a file from the archive.
    ///
    /// Returns `Ok(None)` when the file isn't present.
    pub fn read(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        if self.path.is_dir() {
            let candidate = self.path.join(name);
            if !candidate.exists() {
                return Ok(None);
            }
            let mut buf = Vec::new();
            File::open(&candidate)
                .with_context(|| format!("failed to open {}", candidate.display()))?
                .read_to_end(&mut buf)
                .with_context(|| format!("failed to read {}", candidate.display()))?;
            return Ok(Some(buf));
        }

        let file = File::open(&self.path)
            .with_context(|| format!("failed to open archive {}", self.path.display()))?;
        let mut zip = ZipArchive::new(file)
            .with_context(|| format!("failed to read zip {}", self.path.display()))?;
        let result = match zip.by_name(name) {
            Ok(mut entry) => {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).with_context(|| {
                    format!("failed to read {} from {}", name, self.path.display())
                })?;
                Ok(Some(buf))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(err) => Err(err).with_context(|| {
                format!("failed to read {} from zip {}", name, self.path.display())
            }),
        };
        result
    }
}
