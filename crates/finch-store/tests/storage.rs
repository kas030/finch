use finch_core::{ReadingAnchor, ReadingState, Zoom};
use finch_store::{
    AppPaths, LibraryQuery, LibrarySort, SaveOutcome, SessionSnapshot, SnapshotStore, Store,
    StoreCommand, StoreRequest, StoreResponse, StoreWorker,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

fn pdf(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, b"%PDF-1.7\nfixture\n%%EOF\n").unwrap();
    path
}

fn state(item: &finch_store::LibraryItem) -> ReadingState {
    ReadingState {
        document_id: item.id.clone(),
        path: item.path.clone(),
        revision: item.revision,
        page: 17,
        zoom: Zoom::Scale(1.25),
        anchor: ReadingAnchor {
            page_x: 0.2,
            page_y: 0.7,
            viewport_x: 0.4,
            viewport_y: 0.1,
        },
    }
}

#[test]
fn exact_snapshot_retry_repairs_sql_after_atomic_file_commit_and_removed_ids_stay_removed() {
    let temp = TempDir::new().unwrap();
    let paths = AppPaths::in_directory(temp.path().join("data"));
    let mut store = Store::open_with_paths(&paths.database, &paths.session).unwrap();
    let item = store
        .import(&[pdf(temp.path(), "retry.pdf")])
        .unwrap()
        .imported
        .remove(0);
    let snapshot = SessionSnapshot {
        version: 1,
        state: state(&item),
        clean_shutdown: false,
    };
    let database = rusqlite::Connection::open(&paths.database).unwrap();
    database.execute_batch("CREATE TRIGGER fail_state BEFORE INSERT ON reading_states BEGIN SELECT RAISE(ABORT, 'simulated storage failure'); END;").unwrap();
    assert!(store.save_snapshot(&snapshot).is_err());
    assert_eq!(store.load_snapshot().snapshot.as_ref(), Some(&snapshot));
    assert!(store.reading_state(&item.id).unwrap().is_none());
    database.execute_batch("DROP TRIGGER fail_state;").unwrap();
    assert_eq!(store.save_snapshot(&snapshot).unwrap(), SaveOutcome::Stale);
    assert_eq!(
        store.reading_state(&item.id).unwrap().as_ref(),
        Some(&snapshot.state)
    );
    store.remove(&item.id).unwrap();
    assert!(
        store
            .save_snapshot(&SessionSnapshot {
                version: 2,
                ..snapshot
            })
            .is_err()
    );
    assert!(store.load_snapshot().snapshot.is_none());
}

#[test]
fn imports_references_deduplicates_paths_and_hardlinks_and_reports_invalid_files() {
    let temp = TempDir::new().unwrap();
    let path = pdf(temp.path(), "中文论文.pdf");
    let alias = temp.path().join("hardlink.pdf");
    fs::hard_link(&path, &alias).unwrap();
    let invalid = temp.path().join("invalid.pdf");
    fs::write(&invalid, "not a PDF").unwrap();
    let mut store = Store::open_in(temp.path().join("data")).unwrap();
    let report = store
        .import(&[path.clone(), path.clone(), alias, invalid])
        .unwrap();
    assert_eq!(report.imported.len(), 1);
    assert_eq!(report.existing.len(), 2);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.imported[0].title, "中文论文");
    let id = report.imported[0].id.clone();
    store.set_tags(&id, &["temporary".into()]).unwrap();
    store
        .save_snapshot(&SessionSnapshot {
            version: 1,
            state: state(&report.imported[0]),
            clean_shutdown: true,
        })
        .unwrap();
    store.remove(&id).unwrap();
    assert!(
        path.exists(),
        "removing metadata must not remove the source PDF"
    );
    assert!(store.query(&LibraryQuery::default()).unwrap().is_empty());
    assert!(store.reading_state(&id).unwrap().is_none());
    assert!(store.load_snapshot().snapshot.is_none());
}

#[test]
fn title_tags_favorites_literal_search_and_recent_are_persisted() {
    let temp = TempDir::new().unwrap();
    let mut store = Store::open_in(temp.path().join("data")).unwrap();
    let report = store
        .import(&[
            pdf(temp.path(), "first.pdf"),
            pdf(temp.path(), "second.pdf"),
        ])
        .unwrap();
    let first = &report.imported[0];
    let second = &report.imported[1];
    store
        .update_title(&first.id, "ÄLGORITHM 100%_proof")
        .unwrap();
    store
        .set_tags(
            &first.id,
            &[" Rust ".into(), "rust".into(), "论文".into(), "".into()],
        )
        .unwrap();
    store.set_favorite(&first.id, true).unwrap();
    let mut query = LibraryQuery {
        text: "älgorithm".into(),
        favorites_only: true,
        ..LibraryQuery::default()
    };
    assert_eq!(store.query(&query).unwrap()[0].id, first.id);
    query.text = "%_".into();
    assert_eq!(store.query(&query).unwrap().len(), 1);
    query.text = "论文".into();
    query.tag = Some("RUST".into());
    let result = store.query(&query).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].tags, vec!["Rust", "论文"]);
    store.save_reading_state(&state(second)).unwrap();
    let recent = store
        .query(&LibraryQuery {
            sort: LibrarySort::RecentlyRead,
            ..LibraryQuery::default()
        })
        .unwrap();
    assert_eq!(recent[0].id, second.id);
    drop(store);
    let store = Store::open_in(temp.path().join("data")).unwrap();
    assert_eq!(
        store.reading_state(&second.id).unwrap(),
        Some(state(second))
    );
}

#[test]
fn relink_preserves_identity_tags_and_reading_position() {
    let temp = TempDir::new().unwrap();
    let source = pdf(temp.path(), "source.pdf");
    let destination = temp.path().join("moved.pdf");
    let mut store = Store::open_in(temp.path().join("data")).unwrap();
    let item = store
        .import(std::slice::from_ref(&source))
        .unwrap()
        .imported
        .remove(0);
    store.set_tags(&item.id, &["read".into()]).unwrap();
    store.save_reading_state(&state(&item)).unwrap();
    fs::rename(&source, &destination).unwrap();
    assert!(store.get(&item.id).unwrap().missing);
    store.relink(&item.id, &destination).unwrap();
    let restored = store.get(&item.id).unwrap();
    assert!(!restored.missing);
    assert_eq!(restored.tags, ["read"]);
    assert_eq!(restored.title, "source");
    let reading = store.reading_state(&item.id).unwrap().unwrap();
    assert_eq!(reading.page, 17);
    assert_eq!(reading.path, fs::canonicalize(destination).unwrap());
}

#[test]
fn snapshot_is_atomic_monotonic_and_recovers_a_corrupt_primary() {
    let temp = TempDir::new().unwrap();
    let mut store = Store::open_in(temp.path().join("data")).unwrap();
    let item = store
        .import(&[pdf(temp.path(), "paper.pdf")])
        .unwrap()
        .imported
        .remove(0);
    let original = SessionSnapshot {
        version: 100,
        state: state(&item),
        clean_shutdown: false,
    };
    assert_eq!(store.save_snapshot(&original).unwrap(), SaveOutcome::Saved);
    let mut newer = original.clone();
    newer.version += 1;
    newer.state.page = 99;
    newer.clean_shutdown = true;
    assert_eq!(store.save_snapshot(&newer).unwrap(), SaveOutcome::Saved);
    assert_eq!(store.save_snapshot(&original).unwrap(), SaveOutcome::Stale);
    assert_eq!(store.reading_state(&item.id).unwrap().unwrap().page, 99);
    drop(store);
    let mut store = Store::open_in(temp.path().join("data")).unwrap();
    assert_eq!(store.save_snapshot(&original).unwrap(), SaveOutcome::Stale);
    assert_eq!(store.load_snapshot().snapshot, Some(newer));
    fs::write(temp.path().join("data/session.json"), b"{broken").unwrap();
    let loaded = store.load_snapshot();
    assert_eq!(loaded.snapshot, Some(original));
    assert!(loaded.warning.is_some());
}

#[test]
fn invalid_snapshot_state_cannot_replace_a_valid_one() {
    let temp = TempDir::new().unwrap();
    let mut store = Store::open_in(temp.path().join("data")).unwrap();
    let item = store
        .import(&[pdf(temp.path(), "paper.pdf")])
        .unwrap()
        .imported
        .remove(0);
    let mut snapshot = SessionSnapshot {
        version: 1,
        state: state(&item),
        clean_shutdown: true,
    };
    store.save_snapshot(&snapshot).unwrap();
    snapshot.version = 2;
    snapshot.state.anchor.page_y = f32::NAN;
    assert!(store.save_snapshot(&snapshot).is_err());
    assert_eq!(store.load_snapshot().snapshot.unwrap().version, 1);
    assert!(
        SnapshotStore::load(temp.path().join("absent.json"))
            .warning
            .is_none()
    );
}

#[test]
fn schema_migration_is_idempotent_and_refuses_future_versions() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("library.sqlite3");
    drop(Store::open(&path).unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let schema: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(schema, 2);
    drop(connection);
    drop(Store::open(&path).unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    drop(connection);
    assert!(matches!(
        Store::open(&path),
        Err(finch_store::StoreError::NewerSchema { found: 999, .. })
    ));
}

#[test]
fn migration_from_version_one_rebuilds_unicode_search_and_preserves_tags() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("library.sqlite3");
    let mut store = Store::open(&path).unwrap();
    let item = store
        .import(&[pdf(temp.path(), "paper.pdf")])
        .unwrap()
        .imported
        .remove(0);
    store.update_title(&item.id, "Überprüfung").unwrap();
    store.set_tags(&item.id, &["测试".into()]).unwrap();
    drop(store);
    // Recreate the exact v1 representation by removing only the v2 additions.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("DROP INDEX documents_file_identity; ALTER TABLE documents DROP COLUMN search_text; PRAGMA user_version=1;").unwrap();
    drop(connection);
    let store = Store::open(&path).unwrap();
    let query = LibraryQuery {
        text: "über".into(),
        tag: Some("测试".into()),
        ..LibraryQuery::default()
    };
    let restored = store.query(&query).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, item.id);
}

#[test]
fn source_replacement_refreshes_revision_without_losing_library_metadata() {
    let temp = TempDir::new().unwrap();
    let source = pdf(temp.path(), "source.pdf");
    let mut store = Store::open_in(temp.path().join("data")).unwrap();
    let item = store
        .import(std::slice::from_ref(&source))
        .unwrap()
        .imported
        .remove(0);
    store.set_favorite(&item.id, true).unwrap();
    fs::write(
        &source,
        b"%PDF-1.7\nreplacement content with a different length\n%%EOF\n",
    )
    .unwrap();
    let report = store.import(&[source]).unwrap();
    assert!(report.imported.is_empty());
    assert_eq!(report.existing[0].id, item.id);
    assert!(report.existing[0].favorite);
    assert_ne!(report.existing[0].revision.len, item.revision.len);
}

#[test]
fn actor_preserves_request_order_and_flushes_pending_writes_on_shutdown() {
    let temp = TempDir::new().unwrap();
    let paths = AppPaths::in_directory(temp.path().join("data"));
    let worker = StoreWorker::spawn(paths.clone()).unwrap();
    let events = worker.events();
    let sender = worker.sender();
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(5)).unwrap().result,
        Ok(StoreResponse::Ready(_))
    ));
    sender
        .send(StoreRequest {
            id: 1,
            command: StoreCommand::Import(vec![pdf(temp.path(), "paper.pdf")]),
        })
        .unwrap();
    let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(event.id, 1);
    let StoreResponse::Imported(report) = event.result.unwrap() else {
        panic!("wrong response")
    };
    let item = &report.imported[0];
    sender
        .send(StoreRequest {
            id: 2,
            command: StoreCommand::SaveSnapshot(SessionSnapshot {
                version: 1,
                state: state(item),
                clean_shutdown: true,
            }),
        })
        .unwrap();
    // Intentionally do not consume the save event; shutdown still drains writes.
    drop(sender);
    worker.shutdown().unwrap();
    let store = Store::open_in(paths.data_dir).unwrap();
    assert_eq!(store.load_snapshot().snapshot.unwrap().state.page, 17);
}

#[test]
fn saturated_actor_drains_all_pending_writes_before_a_final_direct_save() {
    let temp = TempDir::new().unwrap();
    let paths = AppPaths::in_directory(temp.path().join("data"));
    let mut initial = Store::open_in(&paths.data_dir).unwrap();
    let item = initial
        .import(&[pdf(temp.path(), "paper.pdf")])
        .unwrap()
        .imported
        .remove(0);
    drop(initial);
    let worker = StoreWorker::spawn(paths.clone()).unwrap();
    let sender = worker.sender();
    // Keep every event unread: after 64 replies the worker must be blocked on
    // publication while later requests, including the final saves, are queued.
    for id in 1..=118 {
        sender
            .send(StoreRequest {
                id,
                command: StoreCommand::SetFavorite {
                    document_id: item.id.clone(),
                    favorite: id % 2 == 0,
                },
            })
            .unwrap();
    }
    let mut snapshot = SessionSnapshot {
        version: 10,
        state: state(&item),
        clean_shutdown: false,
    };
    snapshot.state.page = 77;
    sender
        .send(StoreRequest {
            id: 119,
            command: StoreCommand::SaveSnapshot(snapshot.clone()),
        })
        .unwrap();
    let mut stale = snapshot.clone();
    stale.version = 9;
    stale.state.page = 1;
    sender
        .send(StoreRequest {
            id: 120,
            command: StoreCommand::SaveSnapshot(stale),
        })
        .unwrap();
    drop(sender);
    worker.shutdown().unwrap();
    let mut final_store = Store::open_in(&paths.data_dir).unwrap();
    assert_eq!(final_store.load_snapshot().snapshot.unwrap().state.page, 77);
    snapshot.version = 11;
    snapshot.state.page = 99;
    snapshot.clean_shutdown = true;
    assert_eq!(
        final_store.save_snapshot(&snapshot).unwrap(),
        SaveOutcome::Saved
    );
    drop(final_store);
    assert_eq!(SnapshotStore::load(&paths.session).snapshot, Some(snapshot));
}
