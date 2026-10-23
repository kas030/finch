use crate::{
    AppPaths, ImportReport, LibraryItem, LibraryQuery, Result, SaveOutcome, SessionSnapshot,
    SnapshotLoad, Store, StoreError,
};
use crossbeam_channel::{Receiver, Sender, bounded, select_biased};
use finch_core::{DocumentId, ReadingState};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};

const QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct StoreRequest {
    /// Caller-owned correlation ID; 0 is reserved for the startup event.
    pub id: u64,
    pub command: StoreCommand,
}

#[derive(Clone, Debug)]
pub enum StoreCommand {
    Import(Vec<PathBuf>),
    Query(LibraryQuery),
    GetReadingState(DocumentId),
    SetFavorite {
        document_id: DocumentId,
        favorite: bool,
    },
    SetTags {
        document_id: DocumentId,
        tags: Vec<String>,
    },
    UpdateTitle {
        document_id: DocumentId,
        title: String,
    },
    Remove(DocumentId),
    Relink {
        document_id: DocumentId,
        path: PathBuf,
    },
    SaveReadingState(ReadingState),
    SaveSnapshot(SessionSnapshot),
    Flush,
}

#[derive(Clone, Debug)]
pub struct StoreEvent {
    pub id: u64,
    pub result: std::result::Result<StoreResponse, String>,
}

#[derive(Clone, Debug)]
pub enum StoreResponse {
    Ready(SnapshotLoad),
    Imported(ImportReport),
    Library(Vec<LibraryItem>),
    ReadingState(Option<ReadingState>),
    Relinked(LibraryItem),
    Saved(SaveOutcome),
    Done,
}

/// One storage thread with bounded queues. It sleeps on channel input when idle.
/// Use `try_send` on the UI thread; retain and retry pending state when full.
pub struct StoreWorker {
    sender: Option<Sender<StoreRequest>>,
    events: Option<Receiver<StoreEvent>>,
    stop: Sender<()>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl StoreWorker {
    pub fn spawn(paths: AppPaths) -> Result<Self> {
        let (sender, requests) = bounded(QUEUE_CAPACITY);
        let (event_sender, events) = bounded(QUEUE_CAPACITY);
        let (stop, stop_rx) = bounded(1);
        let thread = thread::Builder::new()
            .name("finch-storage".into())
            .spawn(move || {
                let mut store = match Store::open_with_paths(paths.database, paths.session) {
                    Ok(store) => store,
                    Err(error) => {
                        let message = error.to_string();
                        let _ = event_sender.try_send(StoreEvent {
                            id: 0,
                            result: Err(message),
                        });
                        return Err(error);
                    }
                };
                let ready = StoreEvent {
                    id: 0,
                    result: Ok(StoreResponse::Ready(store.load_snapshot())),
                };
                if !publish(&event_sender, &stop_rx, ready) {
                    return drain_and_flush(&mut store, &requests);
                }
                loop {
                    select_biased! {
                        recv(stop_rx) -> _ => break,
                        recv(requests) -> request => {
                            let Ok(request) = request else { break; };
                            let event = run_request(&mut store, request);
                            if !publish(&event_sender, &stop_rx, event) { break; }
                        }
                    }
                }
                drain_and_flush(&mut store, &requests)
            })?;
        Ok(Self {
            sender: Some(sender),
            events: Some(events),
            stop,
            thread: Some(thread),
        })
    }

    pub fn sender(&self) -> Sender<StoreRequest> {
        self.sender
            .as_ref()
            .expect("worker sender exists until shutdown")
            .clone()
    }

    /// Clone this receiver once for the application's subscription. Clones compete
    /// for events, so multiple active consumers must not be created.
    pub fn events(&self) -> Receiver<StoreEvent> {
        self.events
            .as_ref()
            .expect("worker receiver exists until shutdown")
            .clone()
    }

    /// Stop producers first. Already queued requests are processed in FIFO order,
    /// even when the event channel is full, then the database is flushed and the
    /// thread joined. Call during exit, outside the UI update callback.
    pub fn shutdown(mut self) -> Result<()> {
        let _ = self.stop.try_send(());
        self.sender.take();
        self.events.take();
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| StoreError::Worker("thread panicked".into()))?,
            None => Ok(()),
        }
    }
}

impl Drop for StoreWorker {
    fn drop(&mut self) {
        // Nonblocking fallback. Explicit shutdown is the durable exit path.
        let _ = self.stop.try_send(());
    }
}

fn publish(events: &Sender<StoreEvent>, stop: &Receiver<()>, event: StoreEvent) -> bool {
    select_biased! {
        recv(stop) -> _ => false,
        send(events, event) -> result => result.is_ok(),
    }
}

fn drain_and_flush(store: &mut Store, requests: &Receiver<StoreRequest>) -> Result<()> {
    let mut first_error = None;
    for request in requests.try_iter() {
        if let Err(error) = execute(store, request.command) {
            first_error.get_or_insert(error);
        }
    }
    store.flush()?;
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn run_request(store: &mut Store, request: StoreRequest) -> StoreEvent {
    StoreEvent {
        id: request.id,
        result: execute(store, request.command).map_err(|error| error.to_string()),
    }
}

fn execute(store: &mut Store, command: StoreCommand) -> Result<StoreResponse> {
    match command {
        StoreCommand::Import(paths) => Ok(StoreResponse::Imported(store.import(&paths)?)),
        StoreCommand::Query(query) => Ok(StoreResponse::Library(store.query(&query)?)),
        StoreCommand::GetReadingState(id) => {
            Ok(StoreResponse::ReadingState(store.reading_state(&id)?))
        }
        StoreCommand::SetFavorite {
            document_id,
            favorite,
        } => {
            store.set_favorite(&document_id, favorite)?;
            Ok(StoreResponse::Done)
        }
        StoreCommand::SetTags { document_id, tags } => {
            store.set_tags(&document_id, &tags)?;
            Ok(StoreResponse::Done)
        }
        StoreCommand::UpdateTitle { document_id, title } => {
            store.update_title(&document_id, &title)?;
            Ok(StoreResponse::Done)
        }
        StoreCommand::Remove(id) => {
            store.remove(&id)?;
            Ok(StoreResponse::Done)
        }
        StoreCommand::Relink { document_id, path } => {
            store.relink(&document_id, &path)?;
            Ok(StoreResponse::Relinked(store.get(&document_id)?))
        }
        StoreCommand::SaveReadingState(state) => {
            store.save_reading_state(&state)?;
            Ok(StoreResponse::Done)
        }
        StoreCommand::SaveSnapshot(snapshot) => {
            Ok(StoreResponse::Saved(store.save_snapshot(&snapshot)?))
        }
        StoreCommand::Flush => {
            store.flush()?;
            Ok(StoreResponse::Done)
        }
    }
}
