use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Cursor, Read, Write};

use nova_core::{Endian, NOVA_VERSION};

pub const HEADER_LEN: usize = 64;

const MAGIC: [u8; 8] = *b"NOVAIDX\x01";
const HEADER_VERSION: u16 = 1;
const VERSION_STR_LEN: usize = 16;
const PAYLOAD_OFFSET: u32 = HEADER_LEN as u32;

/// Artifact kind identifier embedded in persisted headers.
///
/// These values are part of the on-disk format; do not reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ArtifactKind {
    SymbolIndex = 1,
    ReferenceIndex = 2,
    InheritanceIndex = 3,
    AnnotationIndex = 4,
    /// A `ProjectIndexes` delta segment stored under `<cache>/indexes/segments`.
    ProjectIndexSegment = 5,
    /// Reserved range for cache artifacts.
    AstArtifacts = 100,
    /// Global dependency (JAR/JMOD) classpath stubs.
    DepsIndexBundle = 101,
    /// Per-project cache metadata (`metadata.bin`).
    ProjectMetadata = 102,
}

impl ArtifactKind {
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::SymbolIndex),
            2 => Some(Self::ReferenceIndex),
            3 => Some(Self::InheritanceIndex),
            4 => Some(Self::AnnotationIndex),
            5 => Some(Self::ProjectIndexSegment),
            100 => Some(Self::AstArtifacts),
            101 => Some(Self::DepsIndexBundle),
            102 => Some(Self::ProjectMetadata),
            _ => None,
        }
    }
}

/// Payload compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Compression {
    #[default]
    None = 0,
    Zstd = 1,
}

impl Compression {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Zstd),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageHeader {
    pub kind: ArtifactKind,
    pub schema_version: u32,
    pub nova_version: String,
    pub endian: Endian,
    pub pointer_width: u8,
    pub compression: Compression,
    pub payload_offset: u32,
    pub payload_len: u64,
    pub uncompressed_len: u64,
    /// Truncated blake3 hash of the uncompressed payload (first 8 bytes, LE).
    pub content_hash: u64,
}

impl StorageHeader {
    pub fn new(
        kind: ArtifactKind,
        schema_version: u32,
        compression: Compression,
        payload_len: u64,
        uncompressed_len: u64,
        content_hash: u64,
    ) -> Self {
        Self {
            kind,
            schema_version,
            nova_version: NOVA_VERSION.to_owned(),
            endian: nova_core::target_endian(),
            pointer_width: nova_core::target_pointer_width(),
            compression,
            payload_offset: PAYLOAD_OFFSET,
            payload_len,
            uncompressed_len,
            content_hash,
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        let mut w = Cursor::new(buf.as_mut_slice());

        w.write_all(&MAGIC).expect("in-memory write");
        w.write_u16::<LittleEndian>(HEADER_VERSION)
            .expect("in-memory write");
        w.write_u16::<LittleEndian>(self.kind as u16)
            .expect("in-memory write");
        w.write_u32::<LittleEndian>(self.schema_version)
            .expect("in-memory write");

        let mut version_bytes = [0u8; VERSION_STR_LEN];
        let version_src = self.nova_version.as_bytes();
        let copy_len = version_src.len().min(VERSION_STR_LEN);
        version_bytes[..copy_len].copy_from_slice(&version_src[..copy_len]);
        w.write_all(&version_bytes).expect("in-memory write");

        w.write_u8(self.endian as u8).expect("in-memory write");
        w.write_u8(self.pointer_width).expect("in-memory write");
        w.write_u8(self.compression as u8).expect("in-memory write");
        w.write_u8(0).expect("in-memory write"); // flags (reserved)

        w.write_u32::<LittleEndian>(self.payload_offset)
            .expect("in-memory write");
        w.write_u64::<LittleEndian>(self.payload_len)
            .expect("in-memory write");
        w.write_u64::<LittleEndian>(self.uncompressed_len)
            .expect("in-memory write");
        w.write_u64::<LittleEndian>(self.content_hash)
            .expect("in-memory write");

        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, crate::StorageError> {
        if bytes.len() < HEADER_LEN {
            return Err(crate::StorageError::Truncated {
                expected: HEADER_LEN,
                found: bytes.len(),
            });
        }

        let mut r = Cursor::new(&bytes[..HEADER_LEN]);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if magic != MAGIC {
            return Err(crate::StorageError::InvalidHeader("bad magic"));
        }

        let header_version = r.read_u16::<LittleEndian>()?;
        if header_version != HEADER_VERSION {
            return Err(crate::StorageError::InvalidHeader(
                "unsupported header version",
            ));
        }

        let kind_raw = r.read_u16::<LittleEndian>()?;
        let kind = ArtifactKind::from_u16(kind_raw)
            .ok_or(crate::StorageError::InvalidHeader("unknown artifact kind"))?;

        let schema_version = r.read_u32::<LittleEndian>()?;

        let mut version_bytes = [0u8; VERSION_STR_LEN];
        r.read_exact(&mut version_bytes)?;
        let version_end = version_bytes
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(VERSION_STR_LEN);
        let nova_version = String::from_utf8_lossy(&version_bytes[..version_end]).to_string();

        let endian = match r.read_u8()? {
            0 => Endian::Little,
            1 => Endian::Big,
            _ => return Err(crate::StorageError::InvalidHeader("unknown endian tag")),
        };

        let pointer_width = r.read_u8()?;
        let compression = Compression::from_u8(r.read_u8()?)
            .ok_or(crate::StorageError::InvalidHeader("unknown compression"))?;
        let _flags = r.read_u8()?;

        let payload_offset = r.read_u32::<LittleEndian>()?;
        let payload_len = r.read_u64::<LittleEndian>()?;
        let uncompressed_len = r.read_u64::<LittleEndian>()?;
        let content_hash = r.read_u64::<LittleEndian>()?;

        Ok(Self {
            kind,
            schema_version,
            nova_version,
            endian,
            pointer_width,
            compression,
            payload_offset,
            payload_len,
            uncompressed_len,
            content_hash,
        })
    }
}
