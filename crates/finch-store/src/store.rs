use crate::snapshot::validate_state;
use crate::{
    AppPaths, Result, SaveOutcome, SessionSnapshot, SnapshotLoad, SnapshotStore, StoreError, now_ms,
};
use finch_core::{DocumentId, FileRevision, ReadingState};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LibraryItem {
    pub id: DocumentId,
    pub path: PathBuf,
    pub title: String,
    pub file_name: String,
    pub tags: Vec<String>,
    pub favorite: bool,
    pub added_at_ms: u64,
    pub last_opened_at_ms: Option<u64>,
    pub revision: FileRevision,
    pub missing: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum LibrarySort {
    #[default]
    RecentlyRead,
    RecentlyAdded,
    Title,
}

#[derive(Clone, Debug)]
pub struct LibraryQuery {
    pub text: String,
    pub tag: Option<String>,
    pub favorites_only: bool,
    pub sort: LibrarySort,
    pub limit: usize,
    pub offset: usize,
}

impl Default for LibraryQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            tag: None,
            favorites_only: false,
            sort: LibrarySort::RecentlyRead,
            limit: 200,
            offset: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<LibraryItem>,
    pub existing: Vec<LibraryItem>,
    pub failed: Vec<ImportFailure>,
}

#[derive(Clone, Debug)]
pub struct ImportFailure {
    pub path: PathBuf,
    pub message: String,
}

/// Own this connection on a storage thread. Source PDF files are never modified.
pub struct Store {
    connection: Connection,
    snapshots: SnapshotStore,
}

impl Store {
    pub fn open(database: impl AsRef<Path>) -> Result<Self> {
        let database = database.as_ref();
        let parent = database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        Self::open_with_paths(database, parent.join("session.json"))
    }

    pub fn open_in(data_dir: impl AsRef<Path>) -> Result<Self> {
        let paths = AppPaths::in_directory(data_dir);
        Self::open_with_paths(paths.database, paths.session)
    }

    pub fn open_with_paths(database: impl AsRef<Path>, session: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = database
            .as_ref()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(database)?;
        connection.busy_timeout(Duration::from_secs(3))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-2048; PRAGMA temp_store=MEMORY;")?;
        migrate(&mut connection)?;
        Ok(Self {
            connection,
            snapshots: SnapshotStore::new(session),
        })
    }

    /// Import references in one transaction. Individual invalid files are reported
    /// without rejecting valid files in the same batch; database failures abort it.
    pub fn import(&mut self, paths: &[PathBuf]) -> Result<ImportReport> {
        let tx = self.connection.transaction()?;
        let mut report = ImportReport::default();
        for path in paths {
            let prepared = match prepare_file(path) {
                Ok(prepared) => prepared,
                Err(error) => {
                    report.failed.push(ImportFailure {
                        path: path.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            if let Some(id) = existing_id(&tx, &prepared)? {
                tx.execute(
                    "UPDATE documents SET file_identity=?2,file_len=?3,modified_ms=?4 WHERE id=?1",
                    params![
                        id,
                        prepared.identity,
                        prepared.revision.len,
                        prepared.revision.modified_ms
                    ],
                )?;
                report.existing.push(get_item(&tx, &id)?);
                continue;
            }
            let id: String =
                tx.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
            let title = prepared
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| prepared.file_name.clone());
            tx.execute(
                "INSERT INTO documents(id,path,file_identity,title,file_name,favorite,added_at_ms,last_opened_at_ms,file_len,modified_ms,search_text) VALUES(?1,?2,?3,?4,?5,0,?6,NULL,?7,?8,?9)",
                params![id, encode_path(&prepared.path), prepared.identity, title, prepared.file_name, now_ms(), prepared.revision.len, prepared.revision.modified_ms, search_text(&title, &prepared.file_name, &[])],
            )?;
            report.imported.push(get_item(&tx, &id)?);
        }
        tx.commit()?;
        Ok(report)
    }

    pub fn get(&self, document_id: &str) -> Result<LibraryItem> {
        get_item(&self.connection, document_id)
    }

    /// Search is literal substring search (including `%` and `_`) and Unicode
    /// case insensitive. Limits are capped to bound work and returned allocations.
    pub fn query(&self, query: &LibraryQuery) -> Result<Vec<LibraryItem>> {
        let ordering = match query.sort {
            LibrarySort::RecentlyRead => {
                "COALESCE(last_opened_at_ms,0) DESC, added_at_ms DESC, id ASC"
            }
            LibrarySort::RecentlyAdded => "added_at_ms DESC, id ASC",
            LibrarySort::Title => "title COLLATE NOCASE ASC, id ASC",
        };
        let sql = format!(
            "SELECT id,path,title,file_name,favorite,added_at_ms,last_opened_at_ms,file_len,modified_ms FROM documents WHERE (?1='' OR instr(search_text,?1)>0) AND (?2=0 OR favorite=1) AND (?3 IS NULL OR EXISTS(SELECT 1 FROM document_tags WHERE document_id=documents.id AND tag_key=?3)) ORDER BY {ordering} LIMIT ?4 OFFSET ?5"
        );
        let text = query.text.trim().to_lowercase();
        let tag = query.tag.as_ref().map(|value| value.trim().to_lowercase());
        let mut statement = self.connection.prepare(&sql)?;
        let mut items = statement
            .query_map(
                params![
                    text,
                    query.favorites_only,
                    tag,
                    query.limit.min(10_000) as i64,
                    query.offset.min(i64::MAX as usize) as i64
                ],
                item_from_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for item in &mut items {
            item.tags = get_tags(&self.connection, &item.id)?;
            item.missing = !item.path.is_file();
        }
        Ok(items)
    }

    pub fn set_favorite(&mut self, document_id: &str, favorite: bool) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE documents SET favorite=?2 WHERE id=?1",
            params![document_id, favorite],
        )?;
        require_changed(changed, document_id)
    }

    pub fn set_tags(&mut self, document_id: &str, tags: &[String]) -> Result<()> {
        let tx = self.connection.transaction()?;
        let item = get_item(&tx, document_id)?;
        tx.execute(
            "DELETE FROM document_tags WHERE document_id=?1",
            [document_id],
        )?;
        for tag in tags
            .iter()
            .map(|tag| tag.trim())
            .filter(|tag| !tag.is_empty())
        {
            // Keep the first spelling while deduplicating by Unicode lower case.
            tx.execute(
                "INSERT OR IGNORE INTO document_tags(document_id,tag,tag_key) VALUES(?1,?2,?3)",
                params![document_id, tag, tag.to_lowercase()],
            )?;
        }
        let tags = get_tags(&tx, document_id)?;
        tx.execute(
            "UPDATE documents SET search_text=?2 WHERE id=?1",
            params![
                document_id,
                search_text(&item.title, &item.file_name, &tags)
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_title(&mut self, document_id: &str, title: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(());
        }
        let tx = self.connection.transaction()?;
        let item = get_item(&tx, document_id)?;
        tx.execute(
            "UPDATE documents SET title=?2,search_text=?3 WHERE id=?1",
            params![
                document_id,
                title,
                search_text(title, &item.file_name, &item.tags)
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes only database metadata. The source PDF is never removed or changed.
    pub fn remove(&mut self, document_id: &str) -> Result<()> {
        let active = self
            .load_snapshot()
            .snapshot
            .is_some_and(|s| s.state.document_id == document_id);
        let tx = self.connection.transaction()?;
        let changed = tx.execute("DELETE FROM documents WHERE id=?1", [document_id])?;
        require_changed(changed, document_id)?;
        if active {
            self.snapshots.clear()?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Keep the document identity, tags, title, and reading position when moving a
    /// source file. The caller should resave its active session after a relink.
    pub fn relink(&mut self, document_id: &str, path: &Path) -> Result<()> {
        let prepared = prepare_file(path)?;
        let tx = self.connection.transaction()?;
        let item = get_item(&tx, document_id)?;
        if let Some(other) = existing_id(&tx, &prepared)?
            && other != document_id
        {
            return Err(StoreError::AlreadyImported(other));
        }
        tx.execute("UPDATE documents SET path=?2,file_identity=?3,file_name=?4,file_len=?5,modified_ms=?6,search_text=?7 WHERE id=?1", params![document_id, encode_path(&prepared.path), prepared.identity, prepared.file_name, prepared.revision.len, prepared.revision.modified_ms, search_text(&item.title, &prepared.file_name, &item.tags)])?;
        if let Some(mut state) = read_state(&tx, document_id)? {
            state.path = prepared.path;
            state.revision = prepared.revision;
            tx.execute(
                "UPDATE reading_states SET state_json=?2 WHERE document_id=?1",
                params![document_id, serde_json::to_string(&state)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn reading_state(&self, document_id: &str) -> Result<Option<ReadingState>> {
        read_state(&self.connection, document_id)
    }

    pub fn save_reading_state(&mut self, state: &ReadingState) -> Result<()> {
        validate_state(state)?;
        let tx = self.connection.transaction()?;
        let changed = tx.execute(
            "UPDATE documents SET last_opened_at_ms=?2,file_len=?3,modified_ms=?4 WHERE id=?1",
            params![
                state.document_id,
                now_ms(),
                state.revision.len,
                state.revision.modified_ms
            ],
        )?;
        require_changed(changed, &state.document_id)?;
        tx.execute("INSERT INTO reading_states(document_id,state_json) VALUES(?1,?2) ON CONFLICT(document_id) DO UPDATE SET state_json=excluded.state_json", params![state.document_id, serde_json::to_string(state)?])?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_snapshot(&self) -> SnapshotLoad {
        SnapshotStore::load(self.snapshots.path())
    }

    /// Reject out-of-order writes before updating either the snapshot or the
    /// per-document state. A snapshot remains usable if SQLite is unavailable.
    pub fn save_snapshot(&mut self, snapshot: &SessionSnapshot) -> Result<SaveOutcome> {
        // A removed record must never be resurrected as the startup session.
        // Check before the atomic file replacement, not only in the SQL write.
        self.get(&snapshot.state.document_id)?;
        let outcome = self.snapshots.save(snapshot)?;
        // The atomic snapshot can succeed while SQLite fails (e.g. disk full).
        // An exact retry must repair the per-document state despite its version
        // now being equal. Truly older or conflicting snapshots remain ignored.
        if outcome == SaveOutcome::Saved || self.load_snapshot().snapshot.as_ref() == Some(snapshot)
        {
            self.save_reading_state(&snapshot.state)?;
        }
        Ok(outcome)
    }

    pub fn flush(&self) -> Result<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        Ok(())
    }
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    let tx = connection.transaction()?;
    if version < 1 {
        tx.execute_batch(
            "CREATE TABLE documents(
            id TEXT PRIMARY KEY NOT NULL,
            path BLOB NOT NULL UNIQUE,
            file_identity TEXT,
            title TEXT NOT NULL,
            file_name TEXT NOT NULL,
            favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
            added_at_ms INTEGER NOT NULL,
            last_opened_at_ms INTEGER,
            file_len INTEGER NOT NULL,
            modified_ms INTEGER NOT NULL
        );
        CREATE TABLE document_tags(
            document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
            tag TEXT NOT NULL,
            tag_key TEXT NOT NULL,
            PRIMARY KEY(document_id,tag_key)
        );
        CREATE TABLE reading_states(
            document_id TEXT PRIMARY KEY NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
            state_json TEXT NOT NULL
        );
        CREATE INDEX documents_recent ON documents(last_opened_at_ms DESC,added_at_ms DESC);
        CREATE INDEX document_tags_key ON document_tags(tag_key);",
        )?;
    }
    if version < 2 {
        tx.execute_batch("ALTER TABLE documents ADD COLUMN search_text TEXT NOT NULL DEFAULT ''; CREATE INDEX documents_file_identity ON documents(file_identity) WHERE file_identity IS NOT NULL;")?;
        let mut statement = tx.prepare("SELECT id,title,file_name FROM documents")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        for (id, title, file_name) in rows {
            let tags = get_tags(&tx, &id)?;
            tx.execute(
                "UPDATE documents SET search_text=?2 WHERE id=?1",
                params![id, search_text(&title, &file_name, &tags)],
            )?;
        }
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

struct PreparedFile {
    path: PathBuf,
    identity: Option<String>,
    file_name: String,
    revision: FileRevision,
}

fn prepare_file(path: &Path) -> Result<PreparedFile> {
    let path = fs::canonicalize(path)?;
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() {
        return Err(StoreError::NotPdf(path));
    }
    let mut header = [0_u8; 1024];
    let count = File::open(&path)?.read(&mut header)?;
    if !header[..count].windows(5).any(|window| window == b"%PDF-") {
        return Err(StoreError::NotPdf(path));
    }
    let identity = file_id::get_file_id(&path).ok().map(|id| match id {
        file_id::FileId::Inode {
            device_id,
            inode_number,
        } => format!("inode:{device_id:x}:{inode_number:x}"),
        file_id::FileId::LowRes {
            volume_serial_number,
            file_index,
        } => format!("win64:{volume_serial_number:x}:{file_index:x}"),
        file_id::FileId::HighRes {
            volume_serial_number,
            file_id,
        } => format!("win128:{volume_serial_number:x}:{file_id:x}"),
    });
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let revision = FileRevision {
        len: metadata.len(),
        modified_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as u64,
    };
    Ok(PreparedFile {
        path,
        identity,
        file_name,
        revision,
    })
}

fn existing_id(connection: &Connection, file: &PreparedFile) -> Result<Option<String>> {
    // File IDs are only trusted while the old source still has that identity;
    // filesystems may reuse IDs after deletion. Canonical paths remain stable.
    if let Some(id) = connection
        .query_row(
            "SELECT id FROM documents WHERE path=?1",
            [encode_path(&file.path)],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(Some(id));
    }
    if let Some(identity) = &file.identity {
        let mut statement =
            connection.prepare("SELECT id,path FROM documents WHERE file_identity=?1")?;
        let matches = statement.query_map([identity], |row| {
            Ok((row.get::<_, String>(0)?, decode_path(row.get(1)?)?))
        })?;
        for candidate in matches {
            let (id, old_path) = candidate?;
            if old_path.is_file()
                && matches!((file_id::get_file_id(&old_path), file_id::get_file_id(&file.path)), (Ok(left), Ok(right)) if left == right)
            {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

fn get_item(connection: &Connection, document_id: &str) -> Result<LibraryItem> {
    let mut item = connection.query_row("SELECT id,path,title,file_name,favorite,added_at_ms,last_opened_at_ms,file_len,modified_ms FROM documents WHERE id=?1", [document_id], item_from_row).optional()?.ok_or_else(|| StoreError::NotFound(document_id.to_owned()))?;
    item.tags = get_tags(connection, document_id)?;
    item.missing = !item.path.is_file();
    Ok(item)
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<LibraryItem> {
    Ok(LibraryItem {
        id: row.get(0)?,
        path: decode_path(row.get(1)?)?,
        title: row.get(2)?,
        file_name: row.get(3)?,
        favorite: row.get(4)?,
        added_at_ms: row.get(5)?,
        last_opened_at_ms: row.get(6)?,
        revision: FileRevision {
            len: row.get(7)?,
            modified_ms: row.get(8)?,
        },
        tags: Vec::new(),
        missing: false,
    })
}

fn get_tags(connection: &Connection, document_id: &str) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare("SELECT tag FROM document_tags WHERE document_id=?1 ORDER BY tag_key")?;
    Ok(statement
        .query_map([document_id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

fn read_state(connection: &Connection, document_id: &str) -> Result<Option<ReadingState>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT state_json FROM reading_states WHERE document_id=?1",
            [document_id],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| {
        let state: ReadingState = serde_json::from_str(&json)?;
        validate_state(&state)?;
        if state.document_id != document_id {
            return Err(StoreError::InvalidState(
                "saved state belongs to a different document".into(),
            ));
        }
        Ok(state)
    })
    .transpose()
}

fn require_changed(changed: usize, document_id: &str) -> Result<()> {
    if changed == 0 {
        Err(StoreError::NotFound(document_id.to_owned()))
    } else {
        Ok(())
    }
}

fn search_text(title: &str, file_name: &str, tags: &[String]) -> String {
    format!("{}\n{}\n{}", title, file_name, tags.join("\n")).to_lowercase()
}

#[cfg(windows)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
fn decode_path(bytes: Vec<u8>) -> rusqlite::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Blob,
            "invalid UTF-16 path data".into(),
        ));
    }
    let words: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(std::ffi::OsString::from_wide(&words).into())
}

#[cfg(unix)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn decode_path(bytes: Vec<u8>) -> rusqlite::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(std::ffi::OsString::from_vec(bytes).into())
}
