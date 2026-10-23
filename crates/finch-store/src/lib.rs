//! Local, transactional library storage and a separately readable startup snapshot.
//!
//! [`Store`] is synchronous and intended to be owned by one worker thread. Use
//! [`StoreWorker`] to keep disk and SQLite work off the application's UI thread.

mod actor;
mod paths;
mod snapshot;
mod store;

pub use actor::{StoreCommand, StoreEvent, StoreRequest, StoreResponse, StoreWorker};
pub use paths::AppPaths;
pub use snapshot::{SaveOutcome, SessionSnapshot, SnapshotLoad, SnapshotStore};
pub use store::{ImportFailure, ImportReport, LibraryItem, LibraryQuery, LibrarySort, Store};

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("library database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("invalid saved data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no local application data directory is available")]
    NoDataDirectory,
    #[error("library schema {found} is newer than this application supports ({supported})")]
    NewerSchema { found: u32, supported: u32 },
    #[error("document does not exist in the library: {0}")]
    NotFound(String),
    #[error("not a PDF file: {0}")]
    NotPdf(PathBuf),
    #[error("the replacement file already belongs to document {0}")]
    AlreadyImported(String),
    #[error("invalid reading state: {0}")]
    InvalidState(String),
    #[error("storage worker stopped: {0}")]
    Worker(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}
