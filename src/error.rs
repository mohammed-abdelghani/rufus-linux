//! Shared error types for the parsing/formatting modules.
//! The write engine (engine.rs) stays on anyhow and converts these via `?`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("not an ISO9660 image (no Primary Volume Descriptor found)")]
    NotAnIso,

    #[error("ISO9660 directory record truncated")]
    TruncatedRecord,

    #[error("too many ISO entries (corrupt image?)")]
    TooManyEntries,

    #[error("partition too small for FAT32 (need >= ~33 MiB)")]
    PartitionTooSmall,

    #[error("partition too large for FAT32 (max ~2 TiB with 512-byte sectors)")]
    PartitionTooLarge,

    #[error("could not compute FAT size (bad geometry?)")]
    FatSizeLoop,

    #[error("device too small for the requested partition layout")]
    DeviceTooSmall,

    #[error("not a GPT header")]
    NotGpt,

    #[error("bad GPT entry size {0}")]
    BadGptEntrySize(usize),

    #[error("bad GPT partition range")]
    BadGptRange,

    #[error("invalid FAT32 volume: {0}")]
    BadFat32(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
