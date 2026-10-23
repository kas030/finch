use crate::{Result, StoreError};
use finch_core::{ReadingState, Zoom};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionSnapshot {
    /// Monotonically increasing across launches. Begin with the loaded version + 1.
    pub version: u64,
    pub state: ReadingState,
    /// Set false before restoring; true only after a controlled application exit.
    pub clean_shutdown: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotLoad {
    pub snapshot: Option<SessionSnapshot>,
    /// The UI should show this message once. Corrupt data is never silently ignored.
    pub warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Stale,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format_version: u32,
    session: SessionSnapshot,
}

/// A tiny snapshot independent of the library's initialization/migration path.
pub struct SnapshotStore {
    path: PathBuf,
    last_version: Option<u64>,
}

impl SnapshotStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let last_version = Self::load(&path).snapshot.map(|s| s.version);
        Self { path, last_version }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(path: impl AsRef<Path>) -> SnapshotLoad {
        let path = path.as_ref();
        match read_snapshot(path) {
            Ok(snapshot) => SnapshotLoad {
                snapshot: Some(snapshot),
                warning: None,
            },
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                SnapshotLoad::default()
            }
            Err(error) => {
                let backup = read_snapshot(&backup_path(path)).ok();
                SnapshotLoad {
                    warning: Some(format!(
                        "Could not read saved reading session: {error}. {}",
                        if backup.is_some() {
                            "Recovered the previous session."
                        } else {
                            "Open a document to start a new session."
                        }
                    )),
                    snapshot: backup,
                }
            }
        }
    }

    pub fn save(&mut self, snapshot: &SessionSnapshot) -> Result<SaveOutcome> {
        validate_state(&snapshot.state)?;
        if self
            .last_version
            .is_some_and(|version| snapshot.version <= version)
        {
            return Ok(SaveOutcome::Stale);
        }
        let data = serde_json::to_vec(&Envelope {
            format_version: FORMAT_VERSION,
            session: snapshot.clone(),
        })?;
        if data.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(StoreError::InvalidState(
                "session snapshot exceeds 1 MiB".into(),
            ));
        }
        // Never overwrite a good backup with a damaged primary snapshot.
        if read_snapshot(&self.path).is_ok() {
            atomic_write(&backup_path(&self.path), &fs::read(&self.path)?)?;
        }
        atomic_write(&self.path, &data)?;
        self.last_version = Some(snapshot.version);
        Ok(SaveOutcome::Saved)
    }

    pub fn clear(&mut self) -> Result<()> {
        for path in [&self.path, &backup_path(&self.path)] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.last_version = None;
        Ok(())
    }
}

pub(crate) fn validate_state(state: &ReadingState) -> Result<()> {
    if state.document_id.is_empty() || state.path.as_os_str().is_empty() {
        return Err(StoreError::InvalidState(
            "document ID and path must be nonempty".into(),
        ));
    }
    if let Zoom::Scale(scale) = state.zoom
        && (!scale.is_finite() || scale <= 0.0)
    {
        return Err(StoreError::InvalidState(
            "zoom must be finite and positive".into(),
        ));
    }
    for value in [
        state.anchor.page_x,
        state.anchor.page_y,
        state.anchor.viewport_x,
        state.anchor.viewport_y,
    ] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(StoreError::InvalidState(
                "reading anchor must be within 0..1".into(),
            ));
        }
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

fn read_snapshot(path: &Path) -> Result<SessionSnapshot> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        return Err(StoreError::InvalidState(
            "session snapshot exceeds 1 MiB".into(),
        ));
    }
    let envelope: Envelope = serde_json::from_slice(&bytes)?;
    if envelope.format_version != FORMAT_VERSION {
        return Err(StoreError::InvalidState(format!(
            "unsupported session format {}",
            envelope.format_version
        )));
    }
    validate_state(&envelope.session.state)?;
    Ok(envelope.session)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    // fsync the directory entry on Unix. tempfile handles Windows replacement.
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
