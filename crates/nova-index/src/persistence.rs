use crate::indexes::{
    AnnotationIndex, AnnotationLocation, ArchivedAnnotationLocation, ArchivedIndexedSymbol,
    ArchivedReferenceLocation, InheritanceIndex, ProjectIndexes, ReferenceIndex, ReferenceLocation,
    SymbolIndex, SymbolLocation,
};
use crate::segments::{build_file_to_newest_segment_map, build_segment_files, segment_file_name};
use nova_cache::{
    CacheDir, CacheLock, CacheMetadata, CacheMetadataArchive, Fingerprint, ProjectSnapshot,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Read;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

// Bump whenever the on-disk index format or interpretation changes.
//
// This intentionally invalidates existing persisted indexes so Nova can rebuild
// them, e.g. when we start storing real symbol (line, column) locations instead
// of placeholder `(0, 0)` values.
pub const INDEX_SCHEMA_VERSION: u32 = 4;

const INDEX_WRITE_LOCK_NAME: &str = ".lock";

const MAX_SEGMENTS_BEFORE_COMPACTION: usize = 32;
const MAX_SEGMENT_BYTES_BEFORE_COMPACTION: u64 = 256 * 1024 * 1024;
pub const DEFAULT_SHARD_COUNT: u32 = 64;

pub type ShardId = u32;

#[derive(Debug)]
enum MetadataSource {
    Archived(CacheMetadataArchive),
    Owned(CacheMetadata),
}

impl MetadataSource {
    fn is_compatible(&self) -> bool {
        match self {
            Self::Archived(meta) => meta.is_compatible(),
            Self::Owned(meta) => meta.is_compatible(),
        }
    }

    fn project_hash_matches(&self, snapshot: &ProjectSnapshot) -> bool {
        match self {
            Self::Archived(meta) => meta.project_hash() == snapshot.project_hash().as_str(),
            Self::Owned(meta) => &meta.project_hash == snapshot.project_hash(),
        }
    }

    fn last_updated_millis(&self) -> u64 {
        match self {
            Self::Archived(meta) => meta.last_updated_millis(),
            Self::Owned(meta) => meta.last_updated_millis,
        }
    }

    fn diff_files(&self, snapshot: &ProjectSnapshot) -> Vec<String> {
        match self {
            Self::Archived(meta) => meta.diff_files(snapshot),
            Self::Owned(meta) => meta.diff_files(snapshot),
        }
    }

    fn diff_files_fast(&self, snapshot: &ProjectSnapshot) -> Vec<String> {
        match self {
            Self::Archived(meta) => meta.diff_files_fast(snapshot),
            Self::Owned(meta) => meta.diff_files_fast(snapshot),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexPersistenceError {
    #[error(transparent)]
    Cache(#[from] nova_cache::CacheError),

    #[error(transparent)]
    Storage(#[from] nova_storage::StorageError),

    #[error("{message}")]
    Json { message: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("invalid shard count {shard_count}")]
    InvalidShardCount { shard_count: u32 },

    #[error("shard vector length mismatch: expected {expected}, got {found}")]
    ShardVectorLenMismatch { expected: usize, found: usize },
}

impl From<serde_json::Error> for IndexPersistenceError {
    fn from(err: serde_json::Error) -> Self {
        // `serde_json::Error` display strings can include user-provided scalar values (e.g.
        // `invalid type: string "..."`). Persisted index manifests may contain user-controlled
        // values; avoid echoing string values in errors.
        let message = sanitize_json_error_message(&err.to_string());
        Self::Json { message }
    }
}

fn sanitize_json_error_message(message: &str) -> String {
    // Conservatively redact all double-quoted substrings. This keeps the error actionable (it
    // retains the overall structure + line/column info) without echoing potentially-sensitive
    // content embedded in strings.
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'"' {
                continue;
            }

            // Treat quotes preceded by an odd number of backslashes as escaped.
            let mut backslashes = 0usize;
            let mut k = idx;
            while k > 0 && bytes[k - 1] == b'\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                end = Some(idx);
                break;
            }
        }

        let Some(end) = end else {
            // Unterminated quote: redact the remainder and stop.
            out.push_str("<redacted>");
            rest = "";
            break;
        };
        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        if let Some(end_rel) = out[start.saturating_add(1)..].find('`') {
            let end = start.saturating_add(1).saturating_add(end_rel);
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

#[derive(Clone, Debug)]
pub struct LoadedIndexes {
    pub indexes: ProjectIndexes,
    pub invalidated_files: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_persistence_error_json_does_not_echo_string_values() {
        let secret_suffix = "nova-index-secret-token";
        let secret = format!("prefix\"{secret_suffix}");
        let err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let index_err = IndexPersistenceError::from(err);
        let message = index_err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected IndexPersistenceError json message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected IndexPersistenceError json message to include redaction marker: {message}"
        );
    }
}

#[derive(Debug)]
pub struct IndexSegmentArchive {
    pub id: u64,
    pub files: Vec<String>,
    pub archive: nova_storage::PersistedArchive<ProjectIndexes>,
}

#[derive(Debug)]
pub struct LoadedIndexArchives {
    pub symbols: nova_storage::PersistedArchive<SymbolIndex>,
    pub references: nova_storage::PersistedArchive<ReferenceIndex>,
    pub inheritance: nova_storage::PersistedArchive<InheritanceIndex>,
    pub annotations: nova_storage::PersistedArchive<AnnotationIndex>,
    pub segments: Vec<IndexSegmentArchive>,
    /// Maps each covered file to the newest segment index (0-based into
    /// [`LoadedIndexArchives::segments`]).
    pub file_to_segment: BTreeMap<String, usize>,
    pub invalidated_files: Vec<String>,
}

impl LoadedIndexArchives {
    #[must_use]
    pub fn symbol_locations(&self, symbol: &str) -> Vec<SymbolLocation> {
        let mut out = merged_locations(
            symbol,
            &self.file_to_segment,
            &self.invalidated_files,
            &self.segments,
            |segment| segment.archive.archived().symbols.symbols.get(symbol),
            self.symbols.archived().symbols.get(symbol),
            |loc| loc.location.file.as_str(),
            |loc| SymbolLocation {
                file: loc.location.file.as_str().to_string(),
                line: loc.location.line,
                column: loc.location.column,
            },
        );
        out.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
        });
        out
    }

    #[must_use]
    pub fn reference_locations(&self, symbol: &str) -> Vec<ReferenceLocation> {
        let mut out = merged_locations(
            symbol,
            &self.file_to_segment,
            &self.invalidated_files,
            &self.segments,
            |segment| segment.archive.archived().references.references.get(symbol),
            self.references.archived().references.get(symbol),
            |loc| loc.file.as_str(),
            |loc| ReferenceLocation {
                file: loc.file.as_str().to_string(),
                line: loc.line,
                column: loc.column,
            },
        );
        out.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
        });
        out
    }

    #[must_use]
    pub fn annotation_locations(&self, annotation: &str) -> Vec<AnnotationLocation> {
        let mut out = merged_locations(
            annotation,
            &self.file_to_segment,
            &self.invalidated_files,
            &self.segments,
            |segment| {
                segment
                    .archive
                    .archived()
                    .annotations
                    .annotations
                    .get(annotation)
            },
            self.annotations.archived().annotations.get(annotation),
            |loc| loc.file.as_str(),
            |loc| AnnotationLocation {
                file: loc.file.as_str().to_string(),
                line: loc.line,
                column: loc.column,
            },
        );
        out.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
        });
        out
    }
}

/// A zero-copy, mmap-backed view over persisted project indexes.
///
/// This is intended for warm-start queries that can operate directly on the
/// archived representation without allocating a full `ProjectIndexes` in memory.
///
/// The view also tracks `invalidated_files` (based on the current
/// [`ProjectSnapshot`]) and filters out results coming from those files so
/// callers see an effectively "pruned" index without requiring
/// `PersistedArchive::to_owned()`.
#[derive(Debug)]
pub struct ProjectIndexesView {
    pub symbols: nova_storage::PersistedArchive<SymbolIndex>,
    pub references: nova_storage::PersistedArchive<ReferenceIndex>,
    pub inheritance: nova_storage::PersistedArchive<InheritanceIndex>,
    pub annotations: nova_storage::PersistedArchive<AnnotationIndex>,
    pub segments: Vec<IndexSegmentArchive>,
    /// Maps each covered file to the newest segment index (0-based into
    /// [`ProjectIndexesView::segments`]).
    pub file_to_segment: BTreeMap<String, usize>,

    /// Files whose contents differ from the snapshot used to persist the
    /// indexes (new/modified/deleted).
    pub invalidated_files: BTreeSet<String>,

    /// Optional in-memory overlay for newly indexed/updated files.
    ///
    /// Callers can keep this empty if they only need read-only access to the
    /// persisted archives.
    pub overlay: ProjectIndexes,
}

/// A lightweight, allocation-free view of a location stored in either a
/// persisted archive or the in-memory overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocationRef<'a> {
    pub file: &'a str,
    pub line: u32,
    pub column: u32,
}

fn merge_sorted_dedup<'a, L, R>(left: L, right: R) -> impl Iterator<Item = &'a str> + 'a
where
    L: Iterator<Item = &'a str> + 'a,
    R: Iterator<Item = &'a str> + 'a,
{
    let mut left = left.peekable();
    let mut right = right.peekable();
    std::iter::from_fn(move || match (left.peek(), right.peek()) {
        (Some(&a), Some(&b)) => match a.cmp(b) {
            std::cmp::Ordering::Less => left.next(),
            std::cmp::Ordering::Greater => right.next(),
            std::cmp::Ordering::Equal => {
                let item = left.next();
                right.next();
                item
            }
        },
        (Some(_), None) => left.next(),
        (None, Some(_)) => right.next(),
        (None, None) => None,
    })
}

impl ProjectIndexesView {
    /// Returns `true` if `file` should be treated as stale and filtered out of
    /// archived query results.
    #[inline]
    pub fn is_file_invalidated(&self, file: &str) -> bool {
        self.invalidated_files.contains(file)
    }

    /// Returns all symbol names that have at least one location in a
    /// non-invalidated file.
    pub fn symbol_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let base = self
            .symbols
            .archived()
            .symbols
            .iter()
            .filter(move |&(_name, locations)| {
                locations.iter().any(|loc| {
                    let file = loc.location.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            })
            .map(|(name, _locations)| name.as_str());

        let mut iter: Box<dyn Iterator<Item = &'a str> + 'a> = Box::new(base);
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            let names = segment
                .archive
                .archived()
                .symbols
                .symbols
                .iter()
                .filter(move |&(_name, locations)| {
                    locations.iter().any(|loc| {
                        let file = loc.location.file.as_str();
                        !invalidated_files.contains(file)
                            && file_to_segment.get(file).copied() == Some(segment_idx)
                    })
                })
                .map(|(name, _locations)| name.as_str());

            iter = Box::new(merge_sorted_dedup(iter, names));
        }

        iter
    }

    /// Returns symbol names starting with `prefix` that have at least one
    /// location in a non-invalidated file.
    pub fn symbol_names_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let base = self
            .symbols
            .archived()
            .symbols
            .iter()
            .skip_while(move |(name, _)| name.as_str() < prefix)
            .take_while(move |(name, _)| name.as_str().starts_with(prefix))
            .filter(move |&(_name, locations)| {
                locations.iter().any(|loc| {
                    let file = loc.location.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            })
            .map(|(name, _locations)| name.as_str());

        let mut iter: Box<dyn Iterator<Item = &'a str> + 'a> = Box::new(base);
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            let names = segment
                .archive
                .archived()
                .symbols
                .symbols
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.as_str().starts_with(prefix))
                .filter(move |&(_name, locations)| {
                    locations.iter().any(|loc| {
                        let file = loc.location.file.as_str();
                        !invalidated_files.contains(file)
                            && file_to_segment.get(file).copied() == Some(segment_idx)
                    })
                })
                .map(|(name, _locations)| name.as_str());

            iter = Box::new(merge_sorted_dedup(iter, names));
        }

        iter
    }

    /// Returns all symbol names from both persisted archives (filtering out
    /// invalidated files) and the in-memory overlay.
    pub fn symbol_names_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        merge_sorted_dedup(
            self.symbol_names(),
            self.overlay
                .symbols
                .symbols
                .keys()
                .map(|name| name.as_str()),
        )
    }

    /// Returns symbol names starting with `prefix` from both persisted archives
    /// (filtering out invalidated files) and the in-memory overlay.
    pub fn symbol_names_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        merge_sorted_dedup(
            self.symbol_names_with_prefix(prefix),
            self.overlay
                .symbols
                .symbols
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.starts_with(prefix))
                .map(|(name, _)| name.as_str()),
        )
    }

    /// Returns symbol definition locations for `name`, filtering out any
    /// locations that come from invalidated files.
    pub fn symbol_locations<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = &'a ArchivedIndexedSymbol> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let mut segment_lists = Vec::new();
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            if let Some(locations) = segment.archive.archived().symbols.symbols.get(name) {
                segment_lists.push((segment_idx, locations));
            }
        }
        let base_locations = self.symbols.archived().symbols.get(name);

        segment_lists
            .into_iter()
            .flat_map(move |(segment_idx, locations)| {
                locations.iter().filter(move |loc| {
                    let file = loc.location.file.as_str();
                    !invalidated_files.contains(file)
                        && file_to_segment.get(file).copied() == Some(segment_idx)
                })
            })
            .chain(base_locations.into_iter().flat_map(move |locations| {
                locations.iter().filter(move |loc| {
                    let file = loc.location.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            }))
    }

    /// Returns symbol definition locations for `name`, merging persisted results
    /// (with invalidated files filtered out) and the in-memory overlay.
    ///
    /// This is useful for incremental indexing flows where callers want to
    /// query updated files (stored in `overlay`) without materializing the full
    /// persisted index into memory.
    pub fn symbol_locations_merged<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.symbol_locations(name).map(|loc| LocationRef {
            file: loc.location.file.as_str(),
            line: loc.location.line,
            column: loc.location.column,
        });

        let overlay = self
            .overlay
            .symbols
            .symbols
            .get(name)
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.location.file.as_str(),
                line: loc.location.line,
                column: loc.location.column,
            });

        archived.chain(overlay)
    }

    /// Returns annotation locations for `name`, filtering out any locations
    /// that come from invalidated files.
    pub fn annotation_locations<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = &'a ArchivedAnnotationLocation> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let mut segment_lists = Vec::new();
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            if let Some(locations) = segment.archive.archived().annotations.annotations.get(name) {
                segment_lists.push((segment_idx, locations));
            }
        }
        let base_locations = self.annotations.archived().annotations.get(name);

        segment_lists
            .into_iter()
            .flat_map(move |(segment_idx, locations)| {
                locations.iter().filter(move |loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file)
                        && file_to_segment.get(file).copied() == Some(segment_idx)
                })
            })
            .chain(base_locations.into_iter().flat_map(move |locations| {
                locations.iter().filter(move |loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            }))
    }

    /// Returns annotation locations for `name`, merging persisted results (with
    /// invalidated files filtered out) and the in-memory overlay.
    pub fn annotation_locations_merged<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.annotation_locations(name).map(|loc| LocationRef {
            file: loc.file.as_str(),
            line: loc.line,
            column: loc.column,
        });

        let overlay = self
            .overlay
            .annotations
            .annotations
            .get(name)
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
    }

    /// Returns all annotation names that have at least one location in a
    /// non-invalidated file.
    pub fn annotation_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let base = self
            .annotations
            .archived()
            .annotations
            .iter()
            .filter(move |&(_name, locations)| {
                locations.iter().any(|loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            })
            .map(|(name, _locations)| name.as_str());

        let mut iter: Box<dyn Iterator<Item = &'a str> + 'a> = Box::new(base);
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            let names = segment
                .archive
                .archived()
                .annotations
                .annotations
                .iter()
                .filter(move |&(_name, locations)| {
                    locations.iter().any(|loc| {
                        let file = loc.file.as_str();
                        !invalidated_files.contains(file)
                            && file_to_segment.get(file).copied() == Some(segment_idx)
                    })
                })
                .map(|(name, _locations)| name.as_str());

            iter = Box::new(merge_sorted_dedup(iter, names));
        }

        iter
    }

    /// Returns annotation names starting with `prefix` that have at least one
    /// location in a non-invalidated file.
    pub fn annotation_names_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let base = self
            .annotations
            .archived()
            .annotations
            .iter()
            .skip_while(move |(name, _)| name.as_str() < prefix)
            .take_while(move |(name, _)| name.as_str().starts_with(prefix))
            .filter(move |&(_name, locations)| {
                locations.iter().any(|loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            })
            .map(|(name, _locations)| name.as_str());

        let mut iter: Box<dyn Iterator<Item = &'a str> + 'a> = Box::new(base);
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            let names = segment
                .archive
                .archived()
                .annotations
                .annotations
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.as_str().starts_with(prefix))
                .filter(move |&(_name, locations)| {
                    locations.iter().any(|loc| {
                        let file = loc.file.as_str();
                        !invalidated_files.contains(file)
                            && file_to_segment.get(file).copied() == Some(segment_idx)
                    })
                })
                .map(|(name, _locations)| name.as_str());

            iter = Box::new(merge_sorted_dedup(iter, names));
        }

        iter
    }

    /// Returns all annotation names from both persisted archives (filtering out
    /// invalidated files) and the in-memory overlay.
    pub fn annotation_names_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        merge_sorted_dedup(
            self.annotation_names(),
            self.overlay
                .annotations
                .annotations
                .keys()
                .map(|name| name.as_str()),
        )
    }

    /// Returns annotation names starting with `prefix` from both persisted
    /// archives (filtering out invalidated files) and the in-memory overlay.
    pub fn annotation_names_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        merge_sorted_dedup(
            self.annotation_names_with_prefix(prefix),
            self.overlay
                .annotations
                .annotations
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.starts_with(prefix))
                .map(|(name, _)| name.as_str()),
        )
    }

    /// Returns reference locations for `symbol`, filtering out any locations
    /// that come from invalidated files.
    pub fn reference_locations<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = &'a ArchivedReferenceLocation> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let mut segment_lists = Vec::new();
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            if let Some(locations) = segment.archive.archived().references.references.get(symbol) {
                segment_lists.push((segment_idx, locations));
            }
        }
        let base_locations = self.references.archived().references.get(symbol);

        segment_lists
            .into_iter()
            .flat_map(move |(segment_idx, locations)| {
                locations.iter().filter(move |loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file)
                        && file_to_segment.get(file).copied() == Some(segment_idx)
                })
            })
            .chain(base_locations.into_iter().flat_map(move |locations| {
                locations.iter().filter(move |loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            }))
    }

    /// Returns reference locations for `symbol`, merging persisted results
    /// (with invalidated files filtered out) and the in-memory overlay.
    pub fn reference_locations_merged<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.reference_locations(symbol).map(|loc| LocationRef {
            file: loc.file.as_str(),
            line: loc.line,
            column: loc.column,
        });

        let overlay = self
            .overlay
            .references
            .references
            .get(symbol)
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
    }

    /// Returns all symbols that have at least one reference location in a
    /// non-invalidated file.
    pub fn referenced_symbols<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let base = self
            .references
            .archived()
            .references
            .iter()
            .filter(move |&(_symbol, locations)| {
                locations.iter().any(|loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            })
            .map(|(symbol, _locations)| symbol.as_str());

        let mut iter: Box<dyn Iterator<Item = &'a str> + 'a> = Box::new(base);
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            let names = segment
                .archive
                .archived()
                .references
                .references
                .iter()
                .filter(move |&(_symbol, locations)| {
                    locations.iter().any(|loc| {
                        let file = loc.file.as_str();
                        !invalidated_files.contains(file)
                            && file_to_segment.get(file).copied() == Some(segment_idx)
                    })
                })
                .map(|(symbol, _locations)| symbol.as_str());

            iter = Box::new(merge_sorted_dedup(iter, names));
        }

        iter
    }

    /// Returns referenced symbol names starting with `prefix` that have at
    /// least one location in a non-invalidated file.
    pub fn referenced_symbols_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let file_to_segment = &self.file_to_segment;

        let base = self
            .references
            .archived()
            .references
            .iter()
            .skip_while(move |(name, _)| name.as_str() < prefix)
            .take_while(move |(name, _)| name.as_str().starts_with(prefix))
            .filter(move |&(_symbol, locations)| {
                locations.iter().any(|loc| {
                    let file = loc.file.as_str();
                    !invalidated_files.contains(file) && !file_to_segment.contains_key(file)
                })
            })
            .map(|(symbol, _locations)| symbol.as_str());

        let mut iter: Box<dyn Iterator<Item = &'a str> + 'a> = Box::new(base);
        for (segment_idx, segment) in self.segments.iter().enumerate() {
            let names = segment
                .archive
                .archived()
                .references
                .references
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.as_str().starts_with(prefix))
                .filter(move |&(_symbol, locations)| {
                    locations.iter().any(|loc| {
                        let file = loc.file.as_str();
                        !invalidated_files.contains(file)
                            && file_to_segment.get(file).copied() == Some(segment_idx)
                    })
                })
                .map(|(symbol, _locations)| symbol.as_str());

            iter = Box::new(merge_sorted_dedup(iter, names));
        }

        iter
    }

    /// Returns all referenced symbols from both persisted archives (filtering
    /// out invalidated files) and the in-memory overlay.
    pub fn referenced_symbols_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        merge_sorted_dedup(
            self.referenced_symbols(),
            self.overlay
                .references
                .references
                .keys()
                .map(|name| name.as_str()),
        )
    }

    /// Returns referenced symbol names starting with `prefix` from both
    /// persisted archives (filtering out invalidated files) and the in-memory
    /// overlay.
    pub fn referenced_symbols_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        merge_sorted_dedup(
            self.referenced_symbols_with_prefix(prefix),
            self.overlay
                .references
                .references
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.starts_with(prefix))
                .map(|(name, _)| name.as_str()),
        )
    }
}

pub fn save_indexes(
    cache_dir: &CacheDir,
    snapshot: &ProjectSnapshot,
    indexes: &mut ProjectIndexes,
) -> Result<(), IndexPersistenceError> {
    let indexes_dir = cache_dir.indexes_dir();
    std::fs::create_dir_all(&indexes_dir)?;

    let _lock = acquire_index_write_lock(&indexes_dir)?;

    let metadata_path = cache_dir.metadata_path();
    let (mut metadata, previous_generation) = match CacheMetadata::load(&metadata_path) {
        Ok(existing)
            if existing.is_compatible() && &existing.project_hash == snapshot.project_hash() =>
        {
            let previous = existing.last_updated_millis;
            (existing, previous)
        }
        _ => (CacheMetadata::new(snapshot), 0),
    };

    let generation = next_generation(previous_generation);
    indexes.set_generation(generation);

    write_index_file(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
        &indexes.symbols,
    )?;
    write_index_file(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
        &indexes.references,
    )?;
    write_index_file(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
        &indexes.inheritance,
    )?;
    write_index_file(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
        &indexes.annotations,
    )?;

    metadata.update_from_snapshot(snapshot);
    metadata.last_updated_millis = generation;
    metadata.save(metadata_path)?;
    crate::segments::clear_segments(cache_dir)?;
    Ok(())
}

/// Append a delta segment (LSM-style) containing only the supplied `delta_indexes`.
///
/// `covered_files` must include every file whose base contribution should be
/// superseded by this segment, including deleted files (tombstones). For deleted
/// files, `delta_indexes` should contain no entries and `snapshot` will not have
/// a fingerprint; those files are still recorded in the manifest with a `None`
/// fingerprint so queries can reliably ignore the base entries.
pub fn append_index_segment(
    cache_dir: &CacheDir,
    snapshot: &ProjectSnapshot,
    covered_files: &[String],
    delta_indexes: &mut ProjectIndexes,
) -> Result<(), IndexPersistenceError> {
    let indexes_dir = cache_dir.indexes_dir();
    std::fs::create_dir_all(&indexes_dir)?;

    let should_compact = {
        let _lock = acquire_index_write_lock(&indexes_dir)?;

        let metadata_path = cache_dir.metadata_path();
        let (mut metadata, previous_generation) = match CacheMetadata::load(&metadata_path) {
            Ok(existing)
                if existing.is_compatible()
                    && &existing.project_hash == snapshot.project_hash() =>
            {
                let previous = existing.last_updated_millis;
                (existing, previous)
            }
            _ => (CacheMetadata::new(snapshot), 0),
        };

        let generation = next_generation(previous_generation);
        delta_indexes.set_generation(generation);

        crate::segments::ensure_segments_dir(&indexes_dir)?;
        let mut manifest = match crate::segments::load_manifest(&indexes_dir) {
            Ok(Some(manifest)) if manifest.is_compatible() => manifest,
            Ok(Some(_)) | Err(_) => {
                crate::segments::clear_segments(cache_dir)?;
                crate::segments::SegmentManifest::new()
            }
            Ok(None) => crate::segments::SegmentManifest::new(),
        };

        let id = manifest.next_segment_id();
        let file_name = segment_file_name(id);
        let segment_path = crate::segments::segment_path(&indexes_dir, &file_name);

        nova_storage::write_archive_atomic(
            &segment_path,
            nova_storage::ArtifactKind::ProjectIndexSegment,
            INDEX_SCHEMA_VERSION,
            delta_indexes,
            nova_storage::Compression::None,
        )?;

        let bytes = std::fs::metadata(&segment_path).ok().map(|m| m.len());
        let entry = crate::segments::SegmentEntry {
            id,
            created_at_millis: generation,
            file_name,
            files: build_segment_files(snapshot, covered_files),
            bytes,
        };

        manifest.last_updated_millis = generation;
        manifest.segments.push(entry);
        crate::segments::save_manifest(&indexes_dir, &manifest)?;

        metadata.update_from_snapshot(snapshot);
        metadata.last_updated_millis = generation;
        metadata.save(metadata_path)?;

        if manifest.segments.len() > MAX_SEGMENTS_BEFORE_COMPACTION {
            true
        } else {
            let total_bytes: u64 = manifest
                .segments
                .iter()
                .map(|segment| segment.bytes.unwrap_or(0))
                .sum();
            total_bytes > MAX_SEGMENT_BYTES_BEFORE_COMPACTION
        }
    };

    if should_compact {
        // Best-effort: segment compaction can be expensive and isn't required
        // for correctness.
        let _ = compact_index_segments(cache_dir);
    }

    Ok(())
}

/// Merge all segments into a new compacted base and clear the segment directory.
pub fn compact_index_segments(cache_dir: &CacheDir) -> Result<(), IndexPersistenceError> {
    let indexes_dir = cache_dir.indexes_dir();
    let _lock = acquire_index_write_lock(&indexes_dir)?;

    let manifest = match crate::segments::load_manifest(&indexes_dir) {
        Ok(Some(manifest)) if manifest.is_compatible() => manifest,
        Ok(Some(_)) | Err(_) => {
            crate::segments::clear_segments(cache_dir)?;
            return Ok(());
        }
        Ok(None) => return Ok(()),
    };

    if manifest.segments.is_empty() {
        crate::segments::clear_segments(cache_dir)?;
        return Ok(());
    }

    let metadata_path = cache_dir.metadata_path();
    let metadata = match CacheMetadata::load(&metadata_path) {
        Ok(metadata) if metadata.is_compatible() => metadata,
        _ => return Ok(()),
    };

    let snapshot = ProjectSnapshot::from_fingerprints(
        cache_dir.project_root(),
        metadata.project_hash.clone(),
        metadata.file_fingerprints.clone(),
    )?;

    let Some(mut loaded) = load_indexes(cache_dir, &snapshot)? else {
        return Ok(());
    };

    save_indexes(cache_dir, &snapshot, &mut loaded.indexes)?;
    crate::segments::clear_segments(cache_dir)?;
    Ok(())
}

pub fn save_indexes_with_fingerprints(
    cache_dir: &CacheDir,
    file_fingerprints: &BTreeMap<String, Fingerprint>,
    indexes: &mut ProjectIndexes,
) -> Result<(), IndexPersistenceError> {
    let indexes_dir = cache_dir.indexes_dir();
    std::fs::create_dir_all(&indexes_dir)?;

    let _lock = acquire_index_write_lock(&indexes_dir)?;

    let metadata_path = cache_dir.metadata_path();
    let (mut metadata, previous_generation) = match CacheMetadata::load(&metadata_path) {
        Ok(existing)
            if existing.is_compatible() && &existing.project_hash == cache_dir.project_hash() =>
        {
            let previous = existing.last_updated_millis;
            (existing, previous)
        }
        _ => {
            let now = nova_cache::now_millis();
            (
                CacheMetadata {
                    schema_version: nova_cache::CACHE_METADATA_SCHEMA_VERSION,
                    nova_version: nova_core::NOVA_VERSION.to_string(),
                    created_at_millis: now,
                    last_updated_millis: now,
                    project_hash: cache_dir.project_hash().clone(),
                    file_fingerprints: file_fingerprints.clone(),
                    file_metadata_fingerprints: compute_metadata_fingerprints(
                        cache_dir,
                        file_fingerprints,
                    ),
                },
                0,
            )
        }
    };

    let generation = next_generation(previous_generation);
    indexes.set_generation(generation);

    write_index_file(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
        &indexes.symbols,
    )?;
    write_index_file(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
        &indexes.references,
    )?;
    write_index_file(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
        &indexes.inheritance,
    )?;
    write_index_file(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
        &indexes.annotations,
    )?;

    metadata.last_updated_millis = generation;
    metadata.project_hash = cache_dir.project_hash().clone();
    metadata.file_fingerprints = file_fingerprints.clone();
    metadata.file_metadata_fingerprints =
        compute_metadata_fingerprints(cache_dir, file_fingerprints);
    metadata.save(metadata_path)?;
    Ok(())
}

/// Loads indexes as validated `rkyv` archives backed by an mmap when possible.
///
/// Callers that require an owned, mutable `ProjectIndexes` should use
/// [`load_indexes`]. This function is intended for warm-start queries where the
/// archived representation can be queried without allocating an owned copy.
pub fn load_index_archives(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
) -> Result<Option<LoadedIndexArchives>, IndexPersistenceError> {
    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadataArchive::open(&metadata_path)? {
        Some(metadata) => MetadataSource::Archived(metadata),
        None => match CacheMetadata::load(&metadata_path) {
            Ok(metadata) => MetadataSource::Owned(metadata),
            Err(_) => return Ok(None),
        },
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }
    if !metadata.project_hash_matches(current_snapshot) {
        return Ok(None);
    }

    let indexes_dir = cache_dir.indexes_dir();

    let symbols = match open_index_file::<SymbolIndex>(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let references = match open_index_file::<ReferenceIndex>(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let inheritance = match open_index_file::<InheritanceIndex>(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let annotations = match open_index_file::<AnnotationIndex>(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };

    let base_generation = symbols.generation;
    if references.generation != base_generation
        || inheritance.generation != base_generation
        || annotations.generation != base_generation
    {
        return Ok(None);
    }

    let (segments, file_to_segment, expected_generation) =
        match crate::segments::load_manifest(&indexes_dir) {
            Ok(Some(manifest)) => {
                if !manifest.is_compatible() {
                    return Ok(None);
                }

                if manifest.segments.is_empty() {
                    (Vec::new(), BTreeMap::new(), base_generation)
                } else {
                    let expected_generation = manifest.last_updated_millis;
                    if expected_generation < base_generation {
                        return Ok(None);
                    }

                    let file_to_segment = build_file_to_newest_segment_map(&manifest);
                    let mut segments = Vec::with_capacity(manifest.segments.len());
                    let mut last_segment_generation = None;

                    for segment in manifest.segments {
                        let archive = match open_index_file::<ProjectIndexes>(
                            crate::segments::segment_path(&indexes_dir, &segment.file_name),
                            nova_storage::ArtifactKind::ProjectIndexSegment,
                        ) {
                            Some(value) => value,
                            None => return Ok(None),
                        };

                        let seg = archive.archived();
                        let seg_generation = seg.symbols.generation;
                        if seg.references.generation != seg_generation
                            || seg.inheritance.generation != seg_generation
                            || seg.annotations.generation != seg_generation
                        {
                            return Ok(None);
                        }

                        // Ensure the manifest and segment payload agree about the generation.
                        if seg_generation != segment.created_at_millis {
                            return Ok(None);
                        }

                        last_segment_generation = Some(seg_generation);
                        let files = segment.files.into_iter().map(|file| file.path).collect();
                        segments.push(IndexSegmentArchive {
                            id: segment.id,
                            files,
                            archive,
                        });
                    }

                    if last_segment_generation != Some(expected_generation) {
                        return Ok(None);
                    }

                    (segments, file_to_segment, expected_generation)
                }
            }
            Ok(None) => (Vec::new(), BTreeMap::new(), base_generation),
            Err(_) => return Ok(None),
        };

    if metadata.last_updated_millis() != expected_generation {
        return Ok(None);
    }

    let invalidated = metadata.diff_files(current_snapshot);

    Ok(Some(LoadedIndexArchives {
        symbols,
        references,
        inheritance,
        annotations,
        segments,
        file_to_segment,
        invalidated_files: invalidated,
    }))
}

/// Loads indexes as validated `rkyv` archives backed by an mmap when possible,
/// using a fast per-file fingerprint based on file metadata (size + mtime).
///
/// This avoids hashing full file contents before deciding whether persisted
/// indexes are reusable. It is best-effort: modifications that preserve both
/// file size and mtime may be missed.
pub fn load_index_archives_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
) -> Result<Option<LoadedIndexArchives>, IndexPersistenceError> {
    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadataArchive::open(&metadata_path)? {
        Some(metadata) => MetadataSource::Archived(metadata),
        None => match CacheMetadata::load(&metadata_path) {
            Ok(metadata) => MetadataSource::Owned(metadata),
            Err(_) => return Ok(None),
        },
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }

    let current_snapshot = match ProjectSnapshot::new_fast(project_root, files) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    if !metadata.project_hash_matches(&current_snapshot) {
        return Ok(None);
    }

    let indexes_dir = cache_dir.indexes_dir();

    let symbols = match open_index_file::<SymbolIndex>(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let references = match open_index_file::<ReferenceIndex>(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let inheritance = match open_index_file::<InheritanceIndex>(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let annotations = match open_index_file::<AnnotationIndex>(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };

    let base_generation = symbols.generation;
    if references.generation != base_generation
        || inheritance.generation != base_generation
        || annotations.generation != base_generation
    {
        return Ok(None);
    }

    let (segments, file_to_segment, expected_generation) =
        match crate::segments::load_manifest(&indexes_dir) {
            Ok(Some(manifest)) => {
                if !manifest.is_compatible() {
                    return Ok(None);
                }

                if manifest.segments.is_empty() {
                    (Vec::new(), BTreeMap::new(), base_generation)
                } else {
                    let expected_generation = manifest.last_updated_millis;
                    if expected_generation < base_generation {
                        return Ok(None);
                    }

                    let file_to_segment = build_file_to_newest_segment_map(&manifest);
                    let mut segments = Vec::with_capacity(manifest.segments.len());
                    let mut last_segment_generation = None;

                    for segment in manifest.segments {
                        let archive = match open_index_file::<ProjectIndexes>(
                            crate::segments::segment_path(&indexes_dir, &segment.file_name),
                            nova_storage::ArtifactKind::ProjectIndexSegment,
                        ) {
                            Some(value) => value,
                            None => return Ok(None),
                        };

                        let seg = archive.archived();
                        let seg_generation = seg.symbols.generation;
                        if seg.references.generation != seg_generation
                            || seg.inheritance.generation != seg_generation
                            || seg.annotations.generation != seg_generation
                        {
                            return Ok(None);
                        }

                        if seg_generation != segment.created_at_millis {
                            return Ok(None);
                        }

                        last_segment_generation = Some(seg_generation);
                        let files = segment.files.into_iter().map(|file| file.path).collect();
                        segments.push(IndexSegmentArchive {
                            id: segment.id,
                            files,
                            archive,
                        });
                    }

                    if last_segment_generation != Some(expected_generation) {
                        return Ok(None);
                    }

                    (segments, file_to_segment, expected_generation)
                }
            }
            Ok(None) => (Vec::new(), BTreeMap::new(), base_generation),
            Err(_) => return Ok(None),
        };

    if metadata.last_updated_millis() != expected_generation {
        return Ok(None);
    }

    let invalidated = metadata.diff_files_fast(&current_snapshot);

    Ok(Some(LoadedIndexArchives {
        symbols,
        references,
        inheritance,
        annotations,
        segments,
        file_to_segment,
        invalidated_files: invalidated,
    }))
}

pub fn load_indexes(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
) -> Result<Option<LoadedIndexes>, IndexPersistenceError> {
    let Some(archives) = load_index_archives(cache_dir, current_snapshot)? else {
        return Ok(None);
    };

    let symbols = match archives.symbols.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let references = match archives.references.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let inheritance = match archives.inheritance.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let annotations = match archives.annotations.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let mut indexes = ProjectIndexes {
        symbols,
        references,
        inheritance,
        annotations,
    };

    for segment in &archives.segments {
        for file in &segment.files {
            indexes.invalidate_file(file);
        }
        let delta = match segment.archive.to_owned() {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        indexes.merge_from(delta);
    }

    for file in &archives.invalidated_files {
        indexes.invalidate_file(file);
    }

    Ok(Some(LoadedIndexes {
        indexes,
        invalidated_files: archives.invalidated_files,
    }))
}

pub fn load_indexes_with_fingerprints(
    cache_dir: &CacheDir,
    current_file_fingerprints: &BTreeMap<String, Fingerprint>,
) -> Result<Option<LoadedIndexes>, IndexPersistenceError> {
    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadata::load(metadata_path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }
    if &metadata.project_hash != cache_dir.project_hash() {
        return Ok(None);
    }

    let indexes_dir = cache_dir.indexes_dir();

    let symbols = match open_index_file::<SymbolIndex>(
        indexes_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let references = match open_index_file::<ReferenceIndex>(
        indexes_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let inheritance = match open_index_file::<InheritanceIndex>(
        indexes_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };
    let annotations = match open_index_file::<AnnotationIndex>(
        indexes_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
    ) {
        Some(value) => value,
        None => return Ok(None),
    };

    let generation = symbols.generation;
    if references.generation != generation
        || inheritance.generation != generation
        || annotations.generation != generation
    {
        return Ok(None);
    }
    if metadata.last_updated_millis != generation {
        return Ok(None);
    }

    let invalidated_files = metadata.diff_file_fingerprints(current_file_fingerprints);

    let symbols = match symbols.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let references = match references.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let inheritance = match inheritance.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let annotations = match annotations.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let mut indexes = ProjectIndexes {
        symbols,
        references,
        inheritance,
        annotations,
    };

    for file in &invalidated_files {
        indexes.invalidate_file(file);
    }

    Ok(Some(LoadedIndexes {
        indexes,
        invalidated_files,
    }))
}

/// Loads indexes as a zero-copy view backed by validated `rkyv` archives.
///
/// This is similar to [`load_indexes`], but avoids deserializing the full
/// `ProjectIndexes` into memory. Instead, callers can query the persisted
/// archives directly via helper methods on [`ProjectIndexesView`].
pub fn load_index_view(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
) -> Result<Option<ProjectIndexesView>, IndexPersistenceError> {
    let Some(archives) = load_index_archives(cache_dir, current_snapshot)? else {
        return Ok(None);
    };

    let invalidated_files = archives
        .invalidated_files
        .into_iter()
        .collect::<BTreeSet<_>>();

    Ok(Some(ProjectIndexesView {
        symbols: archives.symbols,
        references: archives.references,
        inheritance: archives.inheritance,
        annotations: archives.annotations,
        segments: archives.segments,
        file_to_segment: archives.file_to_segment,
        invalidated_files,
        overlay: ProjectIndexes::default(),
    }))
}

/// Loads indexes as a zero-copy view backed by validated `rkyv` archives, using
/// a fast per-file fingerprint based on file metadata (size + mtime).
///
/// This avoids hashing full file contents before deciding whether persisted
/// indexes are reusable. It is best-effort: modifications that preserve both
/// file size and mtime may be missed.
pub fn load_index_view_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
) -> Result<Option<ProjectIndexesView>, IndexPersistenceError> {
    let Some(archives) = load_index_archives_fast(cache_dir, project_root, files)? else {
        return Ok(None);
    };

    let invalidated_files = archives
        .invalidated_files
        .into_iter()
        .collect::<BTreeSet<_>>();

    Ok(Some(ProjectIndexesView {
        symbols: archives.symbols,
        references: archives.references,
        inheritance: archives.inheritance,
        annotations: archives.annotations,
        segments: archives.segments,
        file_to_segment: archives.file_to_segment,
        invalidated_files,
        overlay: ProjectIndexes::default(),
    }))
}

pub fn load_indexes_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
) -> Result<Option<LoadedIndexes>, IndexPersistenceError> {
    let Some(archives) = load_index_archives_fast(cache_dir, project_root, files)? else {
        return Ok(None);
    };

    let symbols = match archives.symbols.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let references = match archives.references.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let inheritance = match archives.inheritance.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let annotations = match archives.annotations.to_owned() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let mut indexes = ProjectIndexes {
        symbols,
        references,
        inheritance,
        annotations,
    };

    for segment in &archives.segments {
        for file in &segment.files {
            indexes.invalidate_file(file);
        }
        let delta = match segment.archive.to_owned() {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        indexes.merge_from(delta);
    }

    for file in &archives.invalidated_files {
        indexes.invalidate_file(file);
    }

    Ok(Some(LoadedIndexes {
        indexes,
        invalidated_files: archives.invalidated_files,
    }))
}

fn write_index_file<T>(
    path: PathBuf,
    kind: nova_storage::ArtifactKind,
    payload: &T,
) -> Result<(), IndexPersistenceError>
where
    T: nova_storage::WritableArchive,
{
    nova_storage::write_archive_atomic(
        &path,
        kind,
        INDEX_SCHEMA_VERSION,
        payload,
        nova_storage::Compression::None,
    )?;
    Ok(())
}

fn open_index_file<T>(
    path: PathBuf,
    kind: nova_storage::ArtifactKind,
) -> Option<nova_storage::PersistedArchive<T>>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: nova_storage::CheckableArchived,
{
    nova_storage::PersistedArchive::<T>::open_optional(&path, kind, INDEX_SCHEMA_VERSION)
        .unwrap_or_default()
}

struct IndexWriteLock {
    path: PathBuf,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for IndexWriteLock {
    fn drop(&mut self) {
        INDEX_WRITE_LOCKS.with(|locks_cell| {
            let mut locks = locks_cell.borrow_mut();
            let Some(entry) = locks.get_mut(&self.path) else {
                return;
            };

            entry.count = entry.count.saturating_sub(1);
            if entry.count == 0 {
                locks.remove(&self.path);
            }
        });
    }
}

struct ReentrantWriteLockEntry {
    _lock: CacheLock,
    count: usize,
}

thread_local! {
    static INDEX_WRITE_LOCKS: RefCell<std::collections::HashMap<PathBuf, ReentrantWriteLockEntry>> =
        RefCell::new(std::collections::HashMap::new());
}

fn acquire_index_write_lock(indexes_dir: &Path) -> Result<IndexWriteLock, IndexPersistenceError> {
    let lock_path = indexes_dir.join(INDEX_WRITE_LOCK_NAME);

    let already_held = INDEX_WRITE_LOCKS.with(|locks_cell| {
        let mut locks = locks_cell.borrow_mut();
        if let Some(existing) = locks.get_mut(&lock_path) {
            existing.count = existing.count.saturating_add(1);
            true
        } else {
            false
        }
    });

    if already_held {
        return Ok(IndexWriteLock {
            path: lock_path,
            _not_send: PhantomData,
        });
    }

    let lock = CacheLock::lock_exclusive(&lock_path)?;

    INDEX_WRITE_LOCKS.with(|locks_cell| {
        locks_cell.borrow_mut().insert(
            lock_path.clone(),
            ReentrantWriteLockEntry {
                _lock: lock,
                count: 1,
            },
        );
    });

    Ok(IndexWriteLock {
        path: lock_path,
        _not_send: PhantomData,
    })
}

fn next_generation(previous_generation: u64) -> u64 {
    let now = nova_cache::now_millis();
    std::cmp::max(now, previous_generation.saturating_add(1))
}

#[allow(clippy::too_many_arguments)]
fn merged_locations<ArchivedLoc, Out, SegmentGet, FileOf, Convert>(
    _key: &str,
    file_to_segment: &BTreeMap<String, usize>,
    invalidated_files: &[String],
    segments: &[IndexSegmentArchive],
    segment_get: SegmentGet,
    base_locations: Option<&rkyv::vec::ArchivedVec<ArchivedLoc>>,
    file_of: FileOf,
    convert: Convert,
) -> Vec<Out>
where
    SegmentGet:
        for<'a> Fn(&'a IndexSegmentArchive) -> Option<&'a rkyv::vec::ArchivedVec<ArchivedLoc>>,
    FileOf: Fn(&ArchivedLoc) -> &str,
    Convert: Fn(&ArchivedLoc) -> Out,
{
    let invalidated: HashSet<&str> = invalidated_files.iter().map(|s| s.as_str()).collect();
    let mut out = Vec::new();

    for (segment_idx, segment) in segments.iter().enumerate() {
        let Some(locations) = segment_get(segment) else {
            continue;
        };

        for loc in locations.iter() {
            let file = file_of(loc);
            if invalidated.contains(file) {
                continue;
            }
            if file_to_segment.get(file).copied() == Some(segment_idx) {
                out.push(convert(loc));
            }
        }
    }

    if let Some(locations) = base_locations {
        for loc in locations.iter() {
            let file = file_of(loc);
            if invalidated.contains(file) {
                continue;
            }
            if file_to_segment.contains_key(file) {
                continue;
            }
            out.push(convert(loc));
        }
    }

    out
}

fn compute_metadata_fingerprints(
    cache_dir: &CacheDir,
    file_fingerprints: &BTreeMap<String, Fingerprint>,
) -> BTreeMap<String, Fingerprint> {
    let mut fingerprints = BTreeMap::new();
    for path in file_fingerprints.keys() {
        let full_path = cache_dir.project_root().join(path);
        if let Ok(fp) = Fingerprint::from_file_metadata(full_path) {
            fingerprints.insert(path.clone(), fp);
        }
    }
    fingerprints
}

// ---------------------------------------------------------------------
// Sharded persistence (incremental, per-shard archives)
// ---------------------------------------------------------------------

const SHARDS_DIR_NAME: &str = "shards";
const SHARD_MANIFEST_FILE: &str = "manifest.txt";

#[derive(Debug)]
pub struct LoadedShardIndexArchives {
    pub symbols: nova_storage::PersistedArchive<SymbolIndex>,
    pub references: nova_storage::PersistedArchive<ReferenceIndex>,
    pub inheritance: nova_storage::PersistedArchive<InheritanceIndex>,
    pub annotations: nova_storage::PersistedArchive<AnnotationIndex>,
}

#[derive(Debug)]
pub struct LoadedShardedIndexArchives {
    /// Per-shard persisted archives. `None` indicates the shard is missing or corrupt.
    pub shards: Vec<Option<LoadedShardIndexArchives>>,
    /// Files that should be re-indexed in the current snapshot.
    pub invalidated_files: Vec<String>,
    /// Shards that are missing or corrupt and therefore must be rebuilt.
    pub missing_shards: BTreeSet<ShardId>,
}

#[derive(Debug)]
pub struct LoadedShardedIndexView {
    pub view: ShardedIndexView,
    pub invalidated_files: Vec<String>,
    pub missing_shards: BTreeSet<ShardId>,
}

/// In-memory overlay for sharded index updates.
///
/// This stores incremental index deltas for a subset of shards without requiring
/// callers to materialize all shards in memory. Callers typically populate the
/// overlay with newly indexed results for files in `invalidated_files`, then use
/// `*_merged` query helpers on [`ShardedIndexView`] to see a combined view of
/// persisted data (with invalidated files filtered out) and freshly indexed
/// overlay data.
#[derive(Clone, Debug)]
pub struct ShardedIndexOverlay {
    shard_count: u32,
    shards: BTreeMap<ShardId, ProjectIndexes>,
    /// Files for which this overlay contains authoritative (fresh) results.
    ///
    /// This includes both re-indexed files (`apply_file_delta`) and deleted
    /// files (`mark_file_deleted`).
    pub covered_files: BTreeSet<String>,
}

impl ShardedIndexOverlay {
    pub fn new(shard_count: u32) -> Result<Self, IndexPersistenceError> {
        if shard_count == 0 {
            return Err(IndexPersistenceError::InvalidShardCount { shard_count });
        }

        Ok(Self {
            shard_count,
            shards: BTreeMap::new(),
            covered_files: BTreeSet::new(),
        })
    }

    #[must_use]
    pub fn shard_count(&self) -> u32 {
        self.shard_count
    }

    pub fn apply_file_delta(&mut self, file_rel_path: &str, delta: ProjectIndexes) {
        let shard_id = shard_id_for_path(file_rel_path, self.shard_count);
        let shard = self.shards.entry(shard_id).or_default();

        // Repeated updates should replace previous overlay state for this file.
        shard.invalidate_file(file_rel_path);
        shard.merge_from(delta);

        self.covered_files.insert(file_rel_path.to_string());
    }

    pub fn mark_file_deleted(&mut self, file_rel_path: &str) {
        let shard_id = shard_id_for_path(file_rel_path, self.shard_count);
        if let Some(shard) = self.shards.get_mut(&shard_id) {
            shard.invalidate_file(file_rel_path);
        }

        self.covered_files.insert(file_rel_path.to_string());
    }

    #[must_use]
    pub fn shard(&self, shard_id: ShardId) -> Option<&ProjectIndexes> {
        self.shards.get(&shard_id)
    }

    pub fn iter_shards(&self) -> impl Iterator<Item = (ShardId, &ProjectIndexes)> {
        self.shards.iter().map(|(&id, shard)| (id, shard))
    }
}

/// Query interface over sharded, persisted indexes with an optional in-memory overlay.
///
/// Archived results are filtered by `invalidated_files` so callers get an
/// effectively "pruned" view of the on-disk cache without requiring
/// `PersistedArchive::to_owned()`. The `*_merged` helpers combine these filtered
/// persisted results with unfiltered results from [`ShardedIndexOverlay`].
#[derive(Debug)]
pub struct ShardedIndexView {
    shards: Vec<Option<LoadedShardIndexArchives>>,
    invalidated_files: BTreeSet<String>,
    /// Optional in-memory overlay for newly indexed/updated files.
    pub overlay: ShardedIndexOverlay,
}

impl ShardedIndexView {
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    #[must_use]
    pub fn shard(&self, shard_id: ShardId) -> Option<&LoadedShardIndexArchives> {
        self.shards.get(shard_id as usize)?.as_ref()
    }

    /// Returns `true` if `file` should be treated as stale and filtered out of
    /// archived query results.
    #[inline]
    pub fn is_file_invalidated(&self, file: &str) -> bool {
        self.invalidated_files.contains(file)
    }

    /// Return all symbol definition locations for `symbol` across all available shards.
    ///
    /// This is a convenience helper for consumers that want a global view without
    /// deserializing the entire index set.
    pub fn symbol_locations<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let invalidated_files = &self.invalidated_files;

        let mut lists = Vec::new();
        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            if let Some(locations) = shard.symbols.archived().symbols.get(symbol) {
                lists.push(locations);
            }
        }

        lists
            .into_iter()
            .flat_map(move |locations| locations.iter())
            .filter(move |loc| !invalidated_files.contains(loc.location.file.as_str()))
            .map(|loc| LocationRef {
                file: loc.location.file.as_str(),
                line: loc.location.line,
                column: loc.location.column,
            })
    }

    /// Return all symbol definition locations for `symbol`, merging persisted
    /// results (with invalidated files filtered out) and the in-memory overlay.
    pub fn symbol_locations_merged<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.symbol_locations(symbol);

        let mut overlay_lists = Vec::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            if let Some(locations) = shard.symbols.symbols.get(symbol) {
                overlay_lists.push(locations.as_slice());
            }
        }

        let overlay = overlay_lists
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.location.file.as_str(),
                line: loc.location.line,
                column: loc.location.column,
            });

        archived.chain(overlay)
    }

    /// Returns all symbol names that have at least one location in a
    /// non-invalidated file.
    pub fn symbol_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            for (name, locations) in shard.symbols.archived().symbols.iter() {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.location.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns symbol names starting with `prefix` that have at least one
    /// location in a non-invalidated file.
    pub fn symbol_names_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            for (name, locations) in shard
                .symbols
                .archived()
                .symbols
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.as_str().starts_with(prefix))
            {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.location.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns all symbol names from both persisted archives (filtering out
    /// invalidated files) and the in-memory overlay.
    pub fn symbol_names_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(shard.symbols.symbols.keys().map(|name| name.as_str()));
        }

        merge_sorted_dedup(self.symbol_names(), overlay_names.into_iter())
    }

    /// Returns symbol names starting with `prefix` from both persisted archives
    /// (filtering out invalidated files) and the in-memory overlay.
    pub fn symbol_names_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(
                shard
                    .symbols
                    .symbols
                    .keys()
                    .skip_while(move |name| name.as_str() < prefix)
                    .take_while(move |name| name.starts_with(prefix))
                    .map(|name| name.as_str()),
            );
        }

        merge_sorted_dedup(
            self.symbol_names_with_prefix(prefix),
            overlay_names.into_iter(),
        )
    }

    pub fn reference_locations<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let invalidated_files = &self.invalidated_files;

        let mut lists = Vec::new();
        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            if let Some(locations) = shard.references.archived().references.get(symbol) {
                lists.push(locations);
            }
        }

        lists
            .into_iter()
            .flat_map(move |locations| locations.iter())
            .filter(move |loc| !invalidated_files.contains(loc.file.as_str()))
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            })
    }

    /// Returns reference locations for `symbol`, merging persisted results (with
    /// invalidated files filtered out) and the in-memory overlay.
    pub fn reference_locations_merged<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.reference_locations(symbol);

        let mut overlay_lists = Vec::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            if let Some(locations) = shard.references.references.get(symbol) {
                overlay_lists.push(locations.as_slice());
            }
        }

        let overlay = overlay_lists
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
    }

    /// Returns all symbols that have at least one reference location in a
    /// non-invalidated file.
    pub fn referenced_symbols<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut symbols: BTreeSet<&'a str> = BTreeSet::new();

        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            for (symbol, locations) in shard.references.archived().references.iter() {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    symbols.insert(symbol.as_str());
                }
            }
        }

        symbols.into_iter()
    }

    /// Returns referenced symbols starting with `prefix` that have at least one
    /// location in a non-invalidated file.
    pub fn referenced_symbols_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut symbols: BTreeSet<&'a str> = BTreeSet::new();

        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            for (symbol, locations) in shard
                .references
                .archived()
                .references
                .iter()
                .skip_while(move |(symbol, _)| symbol.as_str() < prefix)
                .take_while(move |(symbol, _)| symbol.as_str().starts_with(prefix))
            {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    symbols.insert(symbol.as_str());
                }
            }
        }

        symbols.into_iter()
    }

    /// Returns referenced symbols from both persisted archives (filtering out
    /// invalidated files) and the in-memory overlay.
    pub fn referenced_symbols_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_symbols: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_symbols.extend(
                shard
                    .references
                    .references
                    .keys()
                    .map(|symbol| symbol.as_str()),
            );
        }

        merge_sorted_dedup(self.referenced_symbols(), overlay_symbols.into_iter())
    }

    /// Returns referenced symbols starting with `prefix` from both persisted
    /// archives (filtering out invalidated files) and the in-memory overlay.
    pub fn referenced_symbols_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_symbols: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_symbols.extend(
                shard
                    .references
                    .references
                    .keys()
                    .skip_while(move |symbol| symbol.as_str() < prefix)
                    .take_while(move |symbol| symbol.starts_with(prefix))
                    .map(|symbol| symbol.as_str()),
            );
        }

        merge_sorted_dedup(
            self.referenced_symbols_with_prefix(prefix),
            overlay_symbols.into_iter(),
        )
    }

    pub fn annotation_locations<'a>(
        &'a self,
        annotation: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let invalidated_files = &self.invalidated_files;

        let mut lists = Vec::new();
        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            if let Some(locations) = shard.annotations.archived().annotations.get(annotation) {
                lists.push(locations);
            }
        }

        lists
            .into_iter()
            .flat_map(move |locations| locations.iter())
            .filter(move |loc| !invalidated_files.contains(loc.file.as_str()))
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            })
    }

    /// Returns annotation locations for `annotation`, merging persisted results
    /// (with invalidated files filtered out) and the in-memory overlay.
    pub fn annotation_locations_merged<'a>(
        &'a self,
        annotation: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.annotation_locations(annotation);

        let mut overlay_lists = Vec::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            if let Some(locations) = shard.annotations.annotations.get(annotation) {
                overlay_lists.push(locations.as_slice());
            }
        }

        let overlay = overlay_lists
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
    }

    /// Returns all annotation names that have at least one location in a
    /// non-invalidated file.
    pub fn annotation_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            for (name, locations) in shard.annotations.archived().annotations.iter() {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns annotation names starting with `prefix` that have at least one
    /// location in a non-invalidated file.
    pub fn annotation_names_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard in self.shards.iter().filter_map(|s| s.as_ref()) {
            for (name, locations) in shard
                .annotations
                .archived()
                .annotations
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.as_str().starts_with(prefix))
            {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns annotation names from both persisted archives (filtering out
    /// invalidated files) and the in-memory overlay.
    pub fn annotation_names_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(
                shard
                    .annotations
                    .annotations
                    .keys()
                    .map(|name| name.as_str()),
            );
        }

        merge_sorted_dedup(self.annotation_names(), overlay_names.into_iter())
    }

    /// Returns annotation names starting with `prefix` from both persisted
    /// archives (filtering out invalidated files) and the in-memory overlay.
    pub fn annotation_names_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(
                shard
                    .annotations
                    .annotations
                    .keys()
                    .skip_while(move |name| name.as_str() < prefix)
                    .take_while(move |name| name.starts_with(prefix))
                    .map(|name| name.as_str()),
            );
        }

        merge_sorted_dedup(
            self.annotation_names_with_prefix(prefix),
            overlay_names.into_iter(),
        )
    }
}

#[derive(Debug)]
pub struct LoadedLazyShardedIndexView {
    pub view: LazyShardedIndexView,
    pub invalidated_files: Vec<String>,
}

/// Query interface over sharded, persisted indexes that loads individual shard archives on demand,
/// with an optional in-memory overlay.
///
/// Unlike [`ShardedIndexView`], this struct does not eagerly open or validate every shard during
/// construction. Instead, each shard is opened the first time it is accessed via [`Self::shard`]
/// and the result (including failures) is cached for subsequent calls.
///
/// Missing or corrupt shards are therefore discovered lazily: callers should treat `None` results
/// from [`Self::shard`] as an indication that the shard needs to be rebuilt.
#[derive(Debug)]
pub struct LazyShardedIndexView {
    shard_count: u32,
    shards_root: PathBuf,
    invalidated_files: BTreeSet<String>,
    shards: Vec<OnceLock<Option<LoadedShardIndexArchives>>>,
    /// Optional in-memory overlay for newly indexed/updated files.
    pub overlay: ShardedIndexOverlay,
}

impl LazyShardedIndexView {
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shard_count as usize
    }

    #[must_use]
    pub fn shard(&self, shard_id: ShardId) -> Option<&LoadedShardIndexArchives> {
        let cell = self.shards.get(shard_id as usize)?;
        let archives = cell.get_or_init(|| {
            let shard_dir = shard_dir(&self.shards_root, shard_id);
            load_shard_archives(&shard_dir)
        });
        archives.as_ref()
    }

    /// Returns `true` if `file` should be treated as stale and filtered out of archived query
    /// results.
    #[inline]
    pub fn is_file_invalidated(&self, file: &str) -> bool {
        self.invalidated_files.contains(file)
    }

    /// Returns the set of shards that have been accessed and were found to be missing or corrupt.
    ///
    /// Note that this does not report shards that have not been accessed yet.
    #[must_use]
    pub fn missing_shards(&self) -> BTreeSet<ShardId> {
        self.shards
            .iter()
            .enumerate()
            .filter_map(|(idx, cell)| match cell.get()? {
                None => Some(idx as ShardId),
                Some(_) => None,
            })
            .collect()
    }

    /// Return all symbol definition locations for `symbol` across all available shards.
    ///
    /// This convenience helper will load every shard (lazily) to answer the query. Callers that
    /// only need to inspect a subset of shards should instead call [`Self::shard`] directly.
    pub fn symbol_locations<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let invalidated_files = &self.invalidated_files;

        let mut lists = Vec::new();
        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };
            if let Some(locations) = shard.symbols.archived().symbols.get(symbol) {
                lists.push(locations);
            }
        }

        lists
            .into_iter()
            .flat_map(move |locations| locations.iter())
            .filter(move |loc| !invalidated_files.contains(loc.location.file.as_str()))
            .map(|loc| LocationRef {
                file: loc.location.file.as_str(),
                line: loc.location.line,
                column: loc.location.column,
            })
    }

    /// Return all symbol definition locations for `symbol`, merging persisted results (with
    /// invalidated files filtered out) and the in-memory overlay.
    pub fn symbol_locations_merged<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.symbol_locations(symbol);

        let mut overlay_lists = Vec::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            if let Some(locations) = shard.symbols.symbols.get(symbol) {
                overlay_lists.push(locations.as_slice());
            }
        }

        let overlay = overlay_lists
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.location.file.as_str(),
                line: loc.location.line,
                column: loc.location.column,
            });

        archived.chain(overlay)
    }

    /// Returns all symbol names that have at least one location in a non-invalidated file.
    ///
    /// This convenience helper will load every shard (lazily) to answer the query.
    pub fn symbol_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };

            for (name, locations) in shard.symbols.archived().symbols.iter() {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.location.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns symbol names starting with `prefix` that have at least one location in a
    /// non-invalidated file.
    ///
    /// This convenience helper will load every shard (lazily) to answer the query.
    pub fn symbol_names_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };

            for (name, locations) in shard
                .symbols
                .archived()
                .symbols
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.as_str().starts_with(prefix))
            {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.location.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns all symbol names from both persisted archives (filtering out invalidated files) and
    /// the in-memory overlay.
    pub fn symbol_names_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(shard.symbols.symbols.keys().map(|name| name.as_str()));
        }

        merge_sorted_dedup(self.symbol_names(), overlay_names.into_iter())
    }

    /// Returns symbol names starting with `prefix` from both persisted archives (filtering out
    /// invalidated files) and the in-memory overlay.
    pub fn symbol_names_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(
                shard
                    .symbols
                    .symbols
                    .keys()
                    .skip_while(move |name| name.as_str() < prefix)
                    .take_while(move |name| name.starts_with(prefix))
                    .map(|name| name.as_str()),
            );
        }

        merge_sorted_dedup(
            self.symbol_names_with_prefix(prefix),
            overlay_names.into_iter(),
        )
    }

    pub fn reference_locations<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let invalidated_files = &self.invalidated_files;

        let mut lists = Vec::new();
        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };
            if let Some(locations) = shard.references.archived().references.get(symbol) {
                lists.push(locations);
            }
        }

        lists
            .into_iter()
            .flat_map(move |locations| locations.iter())
            .filter(move |loc| !invalidated_files.contains(loc.file.as_str()))
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            })
    }

    /// Returns reference locations for `symbol`, merging persisted results (with invalidated files
    /// filtered out) and the in-memory overlay.
    pub fn reference_locations_merged<'a>(
        &'a self,
        symbol: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.reference_locations(symbol);

        let mut overlay_lists = Vec::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            if let Some(locations) = shard.references.references.get(symbol) {
                overlay_lists.push(locations.as_slice());
            }
        }

        let overlay = overlay_lists
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
    }

    /// Returns all symbols that have at least one reference location in a non-invalidated file.
    ///
    /// This convenience helper will load every shard (lazily) to answer the query.
    pub fn referenced_symbols<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut symbols: BTreeSet<&'a str> = BTreeSet::new();

        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };

            for (symbol, locations) in shard.references.archived().references.iter() {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    symbols.insert(symbol.as_str());
                }
            }
        }

        symbols.into_iter()
    }

    /// Returns referenced symbols starting with `prefix` that have at least one location in a
    /// non-invalidated file.
    ///
    /// This convenience helper will load every shard (lazily) to answer the query.
    pub fn referenced_symbols_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut symbols: BTreeSet<&'a str> = BTreeSet::new();

        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };

            for (symbol, locations) in shard
                .references
                .archived()
                .references
                .iter()
                .skip_while(move |(symbol, _)| symbol.as_str() < prefix)
                .take_while(move |(symbol, _)| symbol.as_str().starts_with(prefix))
            {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    symbols.insert(symbol.as_str());
                }
            }
        }

        symbols.into_iter()
    }

    /// Returns referenced symbols from both persisted archives (filtering out invalidated files)
    /// and the in-memory overlay.
    pub fn referenced_symbols_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_symbols: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_symbols.extend(
                shard
                    .references
                    .references
                    .keys()
                    .map(|symbol| symbol.as_str()),
            );
        }

        merge_sorted_dedup(self.referenced_symbols(), overlay_symbols.into_iter())
    }

    /// Returns referenced symbols starting with `prefix` from both persisted archives (filtering
    /// out invalidated files) and the in-memory overlay.
    pub fn referenced_symbols_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_symbols: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_symbols.extend(
                shard
                    .references
                    .references
                    .keys()
                    .skip_while(move |symbol| symbol.as_str() < prefix)
                    .take_while(move |symbol| symbol.starts_with(prefix))
                    .map(|symbol| symbol.as_str()),
            );
        }

        merge_sorted_dedup(
            self.referenced_symbols_with_prefix(prefix),
            overlay_symbols.into_iter(),
        )
    }

    pub fn annotation_locations<'a>(
        &'a self,
        annotation: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let invalidated_files = &self.invalidated_files;

        let mut lists = Vec::new();
        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };
            if let Some(locations) = shard.annotations.archived().annotations.get(annotation) {
                lists.push(locations);
            }
        }

        lists
            .into_iter()
            .flat_map(move |locations| locations.iter())
            .filter(move |loc| !invalidated_files.contains(loc.file.as_str()))
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            })
    }

    /// Returns annotation locations for `annotation`, merging persisted results (with invalidated
    /// files filtered out) and the in-memory overlay.
    pub fn annotation_locations_merged<'a>(
        &'a self,
        annotation: &str,
    ) -> impl Iterator<Item = LocationRef<'a>> + 'a {
        let archived = self.annotation_locations(annotation);

        let mut overlay_lists = Vec::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            if let Some(locations) = shard.annotations.annotations.get(annotation) {
                overlay_lists.push(locations.as_slice());
            }
        }

        let overlay = overlay_lists
            .into_iter()
            .flat_map(|locations| locations.iter())
            .map(|loc| LocationRef {
                file: loc.file.as_str(),
                line: loc.line,
                column: loc.column,
            });

        archived.chain(overlay)
    }

    /// Returns all annotation names that have at least one location in a non-invalidated file.
    ///
    /// This convenience helper will load every shard (lazily) to answer the query.
    pub fn annotation_names<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };

            for (name, locations) in shard.annotations.archived().annotations.iter() {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns annotation names starting with `prefix` that have at least one location in a
    /// non-invalidated file.
    ///
    /// This convenience helper will load every shard (lazily) to answer the query.
    pub fn annotation_names_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let invalidated_files = &self.invalidated_files;
        let mut names: BTreeSet<&'a str> = BTreeSet::new();

        for shard_id in 0..self.shard_count {
            let Some(shard) = self.shard(shard_id) else {
                continue;
            };

            for (name, locations) in shard
                .annotations
                .archived()
                .annotations
                .iter()
                .skip_while(move |(name, _)| name.as_str() < prefix)
                .take_while(move |(name, _)| name.as_str().starts_with(prefix))
            {
                if locations
                    .iter()
                    .any(|loc| !invalidated_files.contains(loc.file.as_str()))
                {
                    names.insert(name.as_str());
                }
            }
        }

        names.into_iter()
    }

    /// Returns annotation names from both persisted archives (filtering out invalidated files) and
    /// the in-memory overlay.
    pub fn annotation_names_merged<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(
                shard
                    .annotations
                    .annotations
                    .keys()
                    .map(|name| name.as_str()),
            );
        }

        merge_sorted_dedup(self.annotation_names(), overlay_names.into_iter())
    }

    /// Returns annotation names starting with `prefix` from both persisted archives (filtering out
    /// invalidated files) and the in-memory overlay.
    pub fn annotation_names_with_prefix_merged<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let mut overlay_names: BTreeSet<&'a str> = BTreeSet::new();
        for (_shard_id, shard) in self.overlay.iter_shards() {
            overlay_names.extend(
                shard
                    .annotations
                    .annotations
                    .keys()
                    .skip_while(move |name| name.as_str() < prefix)
                    .take_while(move |name| name.starts_with(prefix))
                    .map(|name| name.as_str()),
            );
        }

        merge_sorted_dedup(
            self.annotation_names_with_prefix(prefix),
            overlay_names.into_iter(),
        )
    }
}

/// Deterministically map a relative file path to a shard id.
///
/// Sharding is stable across runs for the same `path`/`shard_count` combination.
#[must_use]
pub fn shard_id_for_path(path: &str, shard_count: u32) -> ShardId {
    if shard_count == 0 {
        return 0;
    }

    let hash = blake3::hash(path.as_bytes());
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&hash.as_bytes()[..8]);
    let value = u64::from_le_bytes(prefix);
    (value % shard_count as u64) as ShardId
}

#[must_use]
pub fn affected_shards(invalidated_files: &[String], shard_count: u32) -> BTreeSet<ShardId> {
    invalidated_files
        .iter()
        .map(|path| shard_id_for_path(path, shard_count))
        .collect()
}

pub fn save_sharded_indexes(
    cache_dir: &CacheDir,
    snapshot: &ProjectSnapshot,
    shard_count: u32,
    shards: &mut [ProjectIndexes],
) -> Result<(), IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }
    if shards.len() != shard_count as usize {
        return Err(IndexPersistenceError::ShardVectorLenMismatch {
            expected: shard_count as usize,
            found: shards.len(),
        });
    }

    let indexes_dir = cache_dir.indexes_dir();
    std::fs::create_dir_all(&indexes_dir)?;
    let _lock = acquire_index_write_lock(&indexes_dir)?;

    let shards_root = indexes_dir.join(SHARDS_DIR_NAME);
    std::fs::create_dir_all(&shards_root)?;

    let shard_count_changed =
        read_shard_manifest(&shards_root).is_some_and(|value| value != shard_count);

    // Write/update shard manifest so loads can treat shard-count changes as a cache miss.
    write_shard_manifest(&shards_root, shard_count)?;

    let metadata_path = cache_dir.metadata_path();
    let previous_metadata = CacheMetadata::load(&metadata_path)
        .ok()
        .filter(|m| m.is_compatible() && &m.project_hash == snapshot.project_hash());

    let generation = next_generation(
        previous_metadata
            .as_ref()
            .map(|m| m.last_updated_millis)
            .unwrap_or(0),
    );

    // Determine which shards need to be rewritten based on the previous metadata snapshot.
    let mut shards_to_write = if shard_count_changed {
        // Existing shards were written with a different modulo space, so they are not reusable.
        (0..shard_count).collect()
    } else {
        match &previous_metadata {
            Some(metadata) => affected_shards(&metadata.diff_files(snapshot), shard_count),
            None => (0..shard_count).collect(),
        }
    };

    if !shard_count_changed {
        // Also rewrite shards that are missing/corrupt on disk (best-effort recovery).
        for shard_id in 0..shard_count {
            if !shard_on_disk_is_healthy(&shards_root, shard_id) {
                shards_to_write.insert(shard_id);
            }
        }
    }

    for shard_id in shards_to_write {
        let shard_dir = shard_dir(&shards_root, shard_id);
        std::fs::create_dir_all(&shard_dir)?;
        let shard = &mut shards[shard_id as usize];
        shard.set_generation(generation);

        write_index_file(
            shard_dir.join("symbols.idx"),
            nova_storage::ArtifactKind::SymbolIndex,
            &shard.symbols,
        )?;
        write_index_file(
            shard_dir.join("references.idx"),
            nova_storage::ArtifactKind::ReferenceIndex,
            &shard.references,
        )?;
        write_index_file(
            shard_dir.join("inheritance.idx"),
            nova_storage::ArtifactKind::InheritanceIndex,
            &shard.inheritance,
        )?;
        write_index_file(
            shard_dir.join("annotations.idx"),
            nova_storage::ArtifactKind::AnnotationIndex,
            &shard.annotations,
        )?;
    }

    // Update metadata after persisting the shards.
    let mut metadata = match previous_metadata {
        Some(existing) => existing,
        None => CacheMetadata::new(snapshot),
    };
    metadata.update_from_snapshot(snapshot);
    metadata.last_updated_millis = generation;
    metadata.save(metadata_path)?;

    Ok(())
}

/// Incrementally persist sharded index updates without materializing untouched shards.
///
/// This is intended for incremental indexing flows:
/// - Load the existing cache via [`load_sharded_index_archives`]
/// - Re-index `base.invalidated_files`
/// - Store newly indexed results in `overlay` (typically using
///   [`ShardedIndexOverlay::apply_file_delta`])
/// - Call this function to rewrite only affected shards and update the cache metadata.
///
/// If the shard manifest is missing or does not match `shard_count`, this
/// function returns `Ok(())` without writing anything. Callers should fall back
/// to a full rebuild/save using [`save_sharded_indexes`].
pub fn save_sharded_indexes_incremental(
    cache_dir: &CacheDir,
    snapshot: &ProjectSnapshot,
    shard_count: u32,
    base: &LoadedShardedIndexArchives,
    overlay: &ShardedIndexOverlay,
) -> Result<(), IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }
    if base.shards.len() != shard_count as usize {
        return Err(IndexPersistenceError::ShardVectorLenMismatch {
            expected: shard_count as usize,
            found: base.shards.len(),
        });
    }
    if overlay.shard_count() != shard_count {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "shard count mismatch: expected {shard_count}, got {}",
                overlay.shard_count()
            ),
        )
        .into());
    }

    let indexes_dir = cache_dir.indexes_dir();
    std::fs::create_dir_all(&indexes_dir)?;
    let _lock = acquire_index_write_lock(&indexes_dir)?;

    let shards_root = indexes_dir.join(SHARDS_DIR_NAME);
    std::fs::create_dir_all(&shards_root)?;

    match read_shard_manifest(&shards_root) {
        Some(value) if value == shard_count => {}
        _ => return Ok(()),
    }

    let metadata_path = cache_dir.metadata_path();
    let previous_metadata = CacheMetadata::load(&metadata_path)
        .ok()
        .filter(|m| m.is_compatible() && &m.project_hash == snapshot.project_hash());

    let generation = next_generation(
        previous_metadata
            .as_ref()
            .map(|m| m.last_updated_millis)
            .unwrap_or(0),
    );

    let shards_to_write = affected_shards(&base.invalidated_files, shard_count);

    let mut invalidated_by_shard: BTreeMap<ShardId, Vec<&str>> = BTreeMap::new();
    for file in &base.invalidated_files {
        let shard_id = shard_id_for_path(file, shard_count);
        invalidated_by_shard
            .entry(shard_id)
            .or_default()
            .push(file.as_str());
    }

    for shard_id in shards_to_write {
        let mut indexes = match base.shards.get(shard_id as usize).and_then(|s| s.as_ref()) {
            Some(archives) => ProjectIndexes {
                symbols: archives.symbols.to_owned()?,
                references: archives.references.to_owned()?,
                inheritance: archives.inheritance.to_owned()?,
                annotations: archives.annotations.to_owned()?,
            },
            None => ProjectIndexes::default(),
        };

        if let Some(files) = invalidated_by_shard.get(&shard_id) {
            for file in files {
                indexes.invalidate_file(file);
            }
        }

        if let Some(delta) = overlay.shard(shard_id) {
            indexes.merge_from(delta.clone());
        }

        indexes.set_generation(generation);

        let shard_dir = shard_dir(&shards_root, shard_id);
        std::fs::create_dir_all(&shard_dir)?;

        write_index_file(
            shard_dir.join("symbols.idx"),
            nova_storage::ArtifactKind::SymbolIndex,
            &indexes.symbols,
        )?;
        write_index_file(
            shard_dir.join("references.idx"),
            nova_storage::ArtifactKind::ReferenceIndex,
            &indexes.references,
        )?;
        write_index_file(
            shard_dir.join("inheritance.idx"),
            nova_storage::ArtifactKind::InheritanceIndex,
            &indexes.inheritance,
        )?;
        write_index_file(
            shard_dir.join("annotations.idx"),
            nova_storage::ArtifactKind::AnnotationIndex,
            &indexes.annotations,
        )?;
    }

    // Update metadata after persisting the shards.
    let mut metadata = match previous_metadata {
        Some(existing) => existing,
        None => CacheMetadata::new(snapshot),
    };
    metadata.update_from_snapshot(snapshot);
    metadata.last_updated_millis = generation;
    metadata.save(metadata_path)?;

    Ok(())
}

/// Load sharded indexes as validated `rkyv` archives backed by an mmap when possible.
///
/// Backwards compatibility:
/// - If `indexes/shards/manifest.txt` is missing, this treats the cache as a miss and does
///   **not** attempt to read legacy monolithic `indexes/symbols.idx` files. Callers should
///   rebuild and persist using the sharded APIs.
pub fn load_sharded_index_archives(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
    shard_count: u32,
) -> Result<Option<LoadedShardedIndexArchives>, IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }

    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadata::load(metadata_path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }
    if &metadata.project_hash != current_snapshot.project_hash() {
        return Ok(None);
    }

    let shards_root = cache_dir.indexes_dir().join(SHARDS_DIR_NAME);
    match read_shard_manifest(&shards_root) {
        Some(value) if value == shard_count => {}
        _ => return Ok(None),
    }

    let mut shards = Vec::with_capacity(shard_count as usize);
    let mut missing_shards = BTreeSet::new();

    for shard_id in 0..shard_count {
        let shard_dir = shard_dir(&shards_root, shard_id);
        let Some(shard_archives) = load_shard_archives(&shard_dir) else {
            shards.push(None);
            missing_shards.insert(shard_id);
            continue;
        };
        shards.push(Some(shard_archives));
    }

    // Base invalidation from snapshot diffs.
    let mut invalidated: BTreeSet<String> =
        metadata.diff_files(current_snapshot).into_iter().collect();

    // If a shard is missing/corrupt, treat all files that map to that shard as invalidated so the
    // caller can rebuild just those shards.
    if !missing_shards.is_empty() {
        for path in current_snapshot.file_fingerprints().keys() {
            if missing_shards.contains(&shard_id_for_path(path, shard_count)) {
                invalidated.insert(path.clone());
            }
        }
    }

    Ok(Some(LoadedShardedIndexArchives {
        shards,
        invalidated_files: invalidated.into_iter().collect(),
        missing_shards,
    }))
}

/// Loads sharded index archives using a precomputed "fast" snapshot where each file fingerprint
/// is derived from file metadata (size + mtime).
///
/// This is useful for callers that already paid the cost to stat all files (e.g. to report
/// metrics) and want to avoid re-statting them inside [`load_sharded_index_archives_fast`].
pub fn load_sharded_index_archives_from_fast_snapshot(
    cache_dir: &CacheDir,
    fast_snapshot: &ProjectSnapshot,
    shard_count: u32,
) -> Result<Option<LoadedShardedIndexArchives>, IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }

    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadataArchive::open(&metadata_path)? {
        Some(metadata) => MetadataSource::Archived(metadata),
        None => match CacheMetadata::load(&metadata_path) {
            Ok(metadata) => MetadataSource::Owned(metadata),
            Err(_) => return Ok(None),
        },
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }
    if !metadata.project_hash_matches(fast_snapshot) {
        return Ok(None);
    }

    let shards_root = cache_dir.indexes_dir().join(SHARDS_DIR_NAME);
    match read_shard_manifest(&shards_root) {
        Some(value) if value == shard_count => {}
        _ => return Ok(None),
    }

    let mut shards = Vec::with_capacity(shard_count as usize);
    let mut missing_shards = BTreeSet::new();

    for shard_id in 0..shard_count {
        let shard_dir = shard_dir(&shards_root, shard_id);
        let Some(shard_archives) = load_shard_archives(&shard_dir) else {
            shards.push(None);
            missing_shards.insert(shard_id);
            continue;
        };
        shards.push(Some(shard_archives));
    }

    let mut invalidated: BTreeSet<String> = metadata
        .diff_files_fast(fast_snapshot)
        .into_iter()
        .collect();

    if !missing_shards.is_empty() {
        for path in fast_snapshot.file_fingerprints().keys() {
            if missing_shards.contains(&shard_id_for_path(path, shard_count)) {
                invalidated.insert(path.clone());
            }
        }
    }

    Ok(Some(LoadedShardedIndexArchives {
        shards,
        invalidated_files: invalidated.into_iter().collect(),
        missing_shards,
    }))
}

/// Fast variant of [`load_sharded_index_archives`] that uses per-file metadata
/// fingerprints (size + mtime) instead of hashing file contents.
///
/// This is intended for warm-start validation when callers want to avoid reading
/// the full contents of every file.
pub fn load_sharded_index_archives_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
    shard_count: u32,
) -> Result<Option<LoadedShardedIndexArchives>, IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }

    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let current_snapshot = match ProjectSnapshot::new_fast(project_root, files) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    load_sharded_index_archives_from_fast_snapshot(cache_dir, &current_snapshot, shard_count)
}

/// Loads sharded indexes as a zero-copy view backed by validated `rkyv` archives, loading shard
/// archives on demand.
///
/// This behaves like [`load_sharded_index_view`] except it does **not** eagerly open/validate
/// every shard during the load step. Missing or corrupt shards are discovered when callers first
/// access them via [`LazyShardedIndexView::shard`] (or via helper methods that scan all shards).
///
/// Note: `invalidated_files` is computed solely from metadata vs the provided snapshot. If a shard
/// is missing/corrupt, that condition is only discovered on first access and is *not* reflected in
/// `invalidated_files` until the caller observes a `None` shard.
pub fn load_sharded_index_view_lazy(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
    shard_count: u32,
) -> Result<Option<LoadedLazyShardedIndexView>, IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }

    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadataArchive::open(&metadata_path)? {
        Some(metadata) => MetadataSource::Archived(metadata),
        None => match CacheMetadata::load(&metadata_path) {
            Ok(metadata) => MetadataSource::Owned(metadata),
            Err(_) => return Ok(None),
        },
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }
    if !metadata.project_hash_matches(current_snapshot) {
        return Ok(None);
    }

    let shards_root = cache_dir.indexes_dir().join(SHARDS_DIR_NAME);
    match read_shard_manifest(&shards_root) {
        Some(value) if value == shard_count => {}
        _ => return Ok(None),
    }
    if !probe_sharded_symbols_idx_schema(&shards_root) {
        return Ok(None);
    }

    let invalidated_files_set: BTreeSet<String> =
        metadata.diff_files(current_snapshot).into_iter().collect();
    let invalidated_files = invalidated_files_set.iter().cloned().collect();

    let shards = (0..shard_count).map(|_| OnceLock::new()).collect();

    Ok(Some(LoadedLazyShardedIndexView {
        view: LazyShardedIndexView {
            shard_count,
            shards_root,
            invalidated_files: invalidated_files_set,
            shards,
            overlay: ShardedIndexOverlay::new(shard_count)?,
        },
        invalidated_files,
    }))
}

/// Loads sharded indexes as a lazy, zero-copy view backed by validated `rkyv` archives, using a
/// precomputed "fast" snapshot where each file fingerprint is derived from file metadata
/// (size + mtime).
///
/// This mirrors [`load_sharded_index_view_lazy`] but avoids hashing file contents. It is
/// best-effort: modifications that preserve both file size and mtime may be missed.
pub fn load_sharded_index_view_lazy_from_fast_snapshot(
    cache_dir: &CacheDir,
    fast_snapshot: &ProjectSnapshot,
    shard_count: u32,
) -> Result<Option<LoadedLazyShardedIndexView>, IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }

    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let metadata = match CacheMetadataArchive::open(&metadata_path)? {
        Some(metadata) => MetadataSource::Archived(metadata),
        None => match CacheMetadata::load(&metadata_path) {
            Ok(metadata) => MetadataSource::Owned(metadata),
            Err(_) => return Ok(None),
        },
    };
    if !metadata.is_compatible() {
        return Ok(None);
    }
    if !metadata.project_hash_matches(fast_snapshot) {
        return Ok(None);
    }

    let shards_root = cache_dir.indexes_dir().join(SHARDS_DIR_NAME);
    match read_shard_manifest(&shards_root) {
        Some(value) if value == shard_count => {}
        _ => return Ok(None),
    }
    if !probe_sharded_symbols_idx_schema(&shards_root) {
        return Ok(None);
    }

    let invalidated_files_set: BTreeSet<String> = metadata
        .diff_files_fast(fast_snapshot)
        .into_iter()
        .collect();
    let invalidated_files = invalidated_files_set.iter().cloned().collect();

    let shards = (0..shard_count).map(|_| OnceLock::new()).collect();

    Ok(Some(LoadedLazyShardedIndexView {
        view: LazyShardedIndexView {
            shard_count,
            shards_root,
            invalidated_files: invalidated_files_set,
            shards,
            overlay: ShardedIndexOverlay::new(shard_count)?,
        },
        invalidated_files,
    }))
}

/// Fast variant of [`load_sharded_index_view_lazy`] that uses per-file metadata fingerprints
/// (size + mtime) instead of hashing file contents.
pub fn load_sharded_index_view_lazy_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
    shard_count: u32,
) -> Result<Option<LoadedLazyShardedIndexView>, IndexPersistenceError> {
    if shard_count == 0 {
        return Err(IndexPersistenceError::InvalidShardCount { shard_count });
    }

    let metadata_path = cache_dir.metadata_path();
    if !metadata_path.exists() && !cache_dir.metadata_bin_path().exists() {
        return Ok(None);
    }
    let fast_snapshot = match ProjectSnapshot::new_fast(project_root, files) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    load_sharded_index_view_lazy_from_fast_snapshot(cache_dir, &fast_snapshot, shard_count)
}

pub fn load_sharded_index_view(
    cache_dir: &CacheDir,
    current_snapshot: &ProjectSnapshot,
    shard_count: u32,
) -> Result<Option<LoadedShardedIndexView>, IndexPersistenceError> {
    let Some(archives) = load_sharded_index_archives(cache_dir, current_snapshot, shard_count)?
    else {
        return Ok(None);
    };

    let invalidated_files_set = archives.invalidated_files.iter().cloned().collect();

    Ok(Some(LoadedShardedIndexView {
        view: ShardedIndexView {
            shards: archives.shards,
            invalidated_files: invalidated_files_set,
            overlay: ShardedIndexOverlay::new(shard_count)?,
        },
        invalidated_files: archives.invalidated_files,
        missing_shards: archives.missing_shards,
    }))
}

pub fn load_sharded_index_view_fast(
    cache_dir: &CacheDir,
    project_root: impl AsRef<Path>,
    files: Vec<PathBuf>,
    shard_count: u32,
) -> Result<Option<LoadedShardedIndexView>, IndexPersistenceError> {
    let Some(archives) =
        load_sharded_index_archives_fast(cache_dir, project_root, files, shard_count)?
    else {
        return Ok(None);
    };

    let invalidated_files_set = archives.invalidated_files.iter().cloned().collect();

    Ok(Some(LoadedShardedIndexView {
        view: ShardedIndexView {
            shards: archives.shards,
            invalidated_files: invalidated_files_set,
            overlay: ShardedIndexOverlay::new(shard_count)?,
        },
        invalidated_files: archives.invalidated_files,
        missing_shards: archives.missing_shards,
    }))
}

fn shard_dir(shards_root: &Path, shard_id: ShardId) -> PathBuf {
    shards_root.join(shard_id.to_string())
}

fn shard_manifest_path(shards_root: &Path) -> PathBuf {
    shards_root.join(SHARD_MANIFEST_FILE)
}

fn write_shard_manifest(shards_root: &Path, shard_count: u32) -> Result<(), IndexPersistenceError> {
    let manifest_path = shard_manifest_path(shards_root);
    nova_cache::atomic_write(&manifest_path, format!("{shard_count}\n").as_bytes())?;
    Ok(())
}

fn read_shard_manifest(shards_root: &Path) -> Option<u32> {
    let manifest_path = shard_manifest_path(shards_root);
    // Safety: the shard-count manifest should be a tiny text file. Guard against corrupted
    // manifests that would otherwise allocate unbounded memory.
    let meta = std::fs::symlink_metadata(&manifest_path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    if meta.len() > 1024 {
        return None;
    }

    let text = std::fs::read_to_string(manifest_path).ok()?;
    let line = text.lines().next()?.trim();
    line.parse::<u32>().ok()
}

fn probe_sharded_symbols_idx_schema(shards_root: &Path) -> bool {
    // `load_sharded_index_view_lazy(_from_fast_snapshot)` is allowed to avoid opening and
    // validating every shard archive. However, callers like `nova index`/`cache warm` can early
    // return on a cache hit (no invalidated files) and never touch any shard payloads.
    //
    // When the index schema changes, we must treat the cache as a miss even if file metadata
    // fingerprints are unchanged, otherwise those commands can incorrectly no-op while leaving an
    // incompatible cache on disk.
    //
    // Probe a single known shard file (`symbols.idx` in shard 0) by reading its header only. This
    // is O(1) and avoids mmap/`rkyv` validation work on the cache-hit fast path.
    let probe_path = shard_dir(shards_root, 0).join("symbols.idx");
    let Ok(mut file) = std::fs::File::open(&probe_path) else {
        return false;
    };

    let mut header_bytes = [0u8; nova_storage::HEADER_LEN];
    if file.read_exact(&mut header_bytes).is_err() {
        return false;
    }
    let Ok(header) = nova_storage::StorageHeader::decode(&header_bytes) else {
        return false;
    };

    header.kind == nova_storage::ArtifactKind::SymbolIndex
        && header.schema_version == INDEX_SCHEMA_VERSION
        && header.nova_version == nova_core::NOVA_VERSION
        && header.endian == nova_core::target_endian()
        && header.pointer_width == nova_core::target_pointer_width()
}

fn load_shard_archives(shard_dir: &Path) -> Option<LoadedShardIndexArchives> {
    let symbols = open_index_file::<SymbolIndex>(
        shard_dir.join("symbols.idx"),
        nova_storage::ArtifactKind::SymbolIndex,
    )?;
    let references = open_index_file::<ReferenceIndex>(
        shard_dir.join("references.idx"),
        nova_storage::ArtifactKind::ReferenceIndex,
    )?;
    let inheritance = open_index_file::<InheritanceIndex>(
        shard_dir.join("inheritance.idx"),
        nova_storage::ArtifactKind::InheritanceIndex,
    )?;
    let annotations = open_index_file::<AnnotationIndex>(
        shard_dir.join("annotations.idx"),
        nova_storage::ArtifactKind::AnnotationIndex,
    )?;

    let generation = symbols.generation;
    if references.generation != generation
        || inheritance.generation != generation
        || annotations.generation != generation
    {
        return None;
    }

    Some(LoadedShardIndexArchives {
        symbols,
        references,
        inheritance,
        annotations,
    })
}

fn shard_on_disk_is_healthy(shards_root: &Path, shard_id: ShardId) -> bool {
    let shard_dir = shard_dir(shards_root, shard_id);
    if !shard_dir.exists() {
        return false;
    }

    // Open each file to ensure the payload validates; this keeps the "missing shard" detection
    // aligned with `load_sharded_index_archives` so incremental rebuild/save logic agrees about
    // which shards are recoverable.
    load_shard_archives(&shard_dir).is_some()
}
