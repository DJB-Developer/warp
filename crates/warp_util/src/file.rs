use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use warp_errors::{ErrorExt, register_error};

#[derive(thiserror::Error, Debug)]
pub enum FileSaveError {
    #[error("No file path associated with file when saving file {0:?}")]
    NoFilePath(FileId),
    #[error("IO error when saving file.")]
    IOError {
        #[source]
        error: io::Error,
        path: PathBuf,
    },
    #[error("Remote file operation failed: {0}")]
    RemoteError(String),
    /// A non-IO failure with a self-describing message (e.g. content could
    /// not be derived for the write).
    #[error("{0}")]
    Other(String),
}

impl ErrorExt for FileSaveError {
    fn is_actionable(&self) -> bool {
        match self {
            FileSaveError::NoFilePath(_) | FileSaveError::Other(_) => true,
            FileSaveError::IOError { .. } | FileSaveError::RemoteError(_) => false,
        }
    }
}
register_error!(FileSaveError);

/// Maximum size, in bytes, of a file that can be fully loaded into memory as a
/// `String` (e.g. to populate an editor buffer). Reading larger files whole
/// risks multi-gigabyte allocations for pathologically large files (logs,
/// binaries opened by mistake, etc.); callers should check
/// [`FileLoadError::TooLarge`] and surface a friendly error instead of
/// attempting the read.
pub const MAX_LOADABLE_FILE_SIZE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(thiserror::Error, Debug)]
pub enum FileLoadError {
    #[error("File does not exist")]
    DoesNotExist,
    #[error("IO error when loading file.")]
    IOError(#[from] io::Error),
    #[error("File is too large to open ({size_bytes} bytes, limit is {limit_bytes} bytes)")]
    TooLarge { size_bytes: u64, limit_bytes: u64 },
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileId(usize);

impl FileId {
    /// Constructs a new globally-unique file ID.
    #[allow(clippy::new_without_default)]
    pub fn new() -> FileId {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let raw = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        FileId(raw)
    }
}
