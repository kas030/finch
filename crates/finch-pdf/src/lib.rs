//! A bounded PDF actor. Native handles never leave its sole worker thread.
//!
//! Consumers must release CPU tile bytes after uploading them, and bound their
//! own texture cache. This crate reserves 39 MiB for tile reuse, 8 MiB for the
//! bounded result queue, and one MiB for the tile currently being rendered.

mod native;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use finch_core::{FileRevision, PageSize, Rect};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
};

pub const TILE_SIZE: u32 = 512;
pub const CPU_BUDGET_BYTES: usize = 48 * 1024 * 1024;
const EVENT_CAPACITY: usize = 8;
const CACHE_BYTES: usize =
    CPU_BUDGET_BYTES - (EVENT_CAPACITY + 1) * TILE_SIZE as usize * TILE_SIZE as usize * 4;
const REQUEST_CAPACITY: usize = 64;
static WORKER_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, thiserror::Error)]
pub enum PdfError {
    #[error("PDFium could not be loaded: {0}")]
    Library(String),
    #[error("A PDF worker is already running")]
    AlreadyRunning,
    #[error("PDF request queue is full")]
    QueueFull,
    #[error("PDF worker has stopped")]
    Stopped,
    #[error("PDF operation was cancelled")]
    Cancelled,
    #[error("This PDF requires a password, or the supplied password is incorrect")]
    PasswordRequired,
    #[error("No PDF is open")]
    NoDocument,
    #[error("The PDF changed on disk; reopen it to continue reading")]
    FileChanged,
    #[error("Page index {0} is outside the document")]
    InvalidPage(usize),
    #[error("Invalid render request: {0}")]
    InvalidRender(String),
    #[error("File error: {0}")]
    Io(String),
    #[error("PDF error: {0}")]
    Native(String),
    #[error("PDF content exceeded the safety bound: {0}")]
    Limit(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Visible,
    Interactive,
    Prefetch,
    Background,
}

#[derive(Debug, Clone)]
pub struct PdfRequest {
    pub id: u64,
    pub generation: u64,
    pub priority: Priority,
    pub command: PdfCommand,
}

#[derive(Debug, Clone)]
pub enum PdfCommand {
    Open {
        path: PathBuf,
        password: Option<String>,
    },
    /// Low-priority metadata import; does not replace the open document.
    Metadata {
        path: PathBuf,
    },
    Close,
    PageInfo {
        page: usize,
    },
    RenderTile {
        key: TileKey,
    },
    Text {
        page: usize,
    },
    Search {
        page: usize,
        query: String,
        case_sensitive: bool,
    },
    Bookmarks,
    Links {
        page: usize,
    },
}

#[derive(Debug, Clone)]
pub struct PdfEvent {
    pub id: u64,
    pub generation: u64,
    pub result: Result<PdfResponse, PdfError>,
}

#[derive(Debug, Clone)]
pub enum PdfResponse {
    Ready,
    Opened(DocumentInfo),
    Metadata {
        path: PathBuf,
        title: Option<String>,
        revision: FileRevision,
        page_count: usize,
    },
    Closed,
    PageInfo {
        page: usize,
        size: PageSize,
    },
    Tile(TileBitmap),
    Text(PageText),
    Search {
        page: usize,
        hits: Vec<SearchHit>,
    },
    Bookmarks(Vec<Bookmark>),
    Links {
        page: usize,
        links: Vec<Link>,
    },
}

#[derive(Debug, Clone)]
pub struct DocumentInfo {
    pub path: PathBuf,
    pub page_count: usize,
    pub first_page: PageSize,
    pub revision: FileRevision,
    pub title: Option<String>,
}

/// Offsets are device pixels measured from the rendered page's top-left.
/// Scale is device pixels per PDF point multiplied by 1000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub page: usize,
    pub scale_milli: u32,
    pub x: u32,
    pub y: u32,
}

impl TileKey {
    pub fn scale(self) -> f32 {
        self.scale_milli as f32 / 1000.0
    }
    pub fn dimensions(self, page: PageSize) -> Result<(u32, u32, u32, u32), PdfError> {
        if !(10..=64_000).contains(&self.scale_milli)
            || !page.width.is_finite()
            || !page.height.is_finite()
            || page.width <= 0.0
            || page.height <= 0.0
        {
            return Err(PdfError::InvalidRender(
                "non-finite, empty, or excessive scale/dimensions".into(),
            ));
        }
        let full_width = (page.width * self.scale()).ceil();
        let full_height = (page.height * self.scale()).ceil();
        if full_width > i32::MAX as f32 / 2.0 || full_height > i32::MAX as f32 / 2.0 {
            return Err(PdfError::InvalidRender(
                "page dimensions overflow native coordinates".into(),
            ));
        }
        let (full_width, full_height) = (full_width as u32, full_height as u32);
        if self.x >= full_width || self.y >= full_height {
            return Err(PdfError::InvalidRender("tile is outside the page".into()));
        }
        Ok((
            (full_width - self.x).min(TILE_SIZE),
            (full_height - self.y).min(TILE_SIZE),
            full_width,
            full_height,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct TileBitmap {
    pub key: TileKey,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct TextChar {
    pub index: usize,
    pub character: char,
    pub bounds: Rect,
}

#[derive(Debug, Clone)]
pub struct PageText {
    pub page: usize,
    pub text: String,
    pub chars: Vec<TextChar>,
}

impl PageText {
    pub fn selected_text(&self, first: usize, last: usize) -> String {
        let (start, end) = (first.min(last), first.max(last));
        self.chars
            .iter()
            .filter(|ch| ch.index >= start && ch.index <= end)
            .map(|ch| ch.character)
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub char_index: usize,
    pub char_count: usize,
    pub bounds: Vec<Rect>,
}

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub title: String,
    pub page: Option<usize>,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub bounds: Rect,
    pub target: LinkTarget,
}

#[derive(Debug, Clone)]
pub enum LinkTarget {
    Page(usize),
    Uri(String),
}

/// Small, on-demand diagnostics; reading counters does not schedule any work.
#[derive(Debug, Clone, Copy, Default)]
pub struct PdfWorkerStats {
    pub started_renders: u64,
    pub progressive_pauses: u64,
    pub cancelled_renders: u64,
}

#[derive(Default)]
struct RenderCounters {
    started: AtomicU64,
    pauses: AtomicU64,
    cancelled: AtomicU64,
}

pub struct PdfWorker {
    requests: Sender<PdfRequest>,
    events: Receiver<PdfEvent>,
    generation: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    stop: Sender<()>,
    counters: Arc<RenderCounters>,
    thread: Option<JoinHandle<()>>,
}

impl PdfWorker {
    /// The first event (id 0) is Ready, or a library loading error. Binding and
    /// initialization happen on the PDF thread and never block window creation.
    pub fn spawn(library_path: Option<PathBuf>) -> Result<Self, PdfError> {
        Self::spawn_with_cache_budget(library_path, CACHE_BYTES)
    }

    /// Set this to zero when the UI owns raw image bytes in its own cache. The
    /// caller must reserve another 9 MiB for queued and in-flight native tiles.
    pub fn spawn_with_cache_budget(
        library_path: Option<PathBuf>,
        cache_bytes: usize,
    ) -> Result<Self, PdfError> {
        if WORKER_RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PdfError::AlreadyRunning);
        }
        let (requests, request_rx) = bounded(REQUEST_CAPACITY);
        let (event_tx, events) = bounded(EVENT_CAPACITY);
        let (stop, stop_rx) = bounded(1);
        let generation = Arc::new(AtomicU64::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(RenderCounters::default());
        let worker_counters = counters.clone();
        let (worker_generation, worker_stopping) = (generation.clone(), stopping.clone());
        let thread = std::thread::Builder::new()
            .name("finch-pdf".into())
            .spawn(move || {
                struct RunningGuard;
                impl Drop for RunningGuard {
                    fn drop(&mut self) {
                        WORKER_RUNNING.store(false, Ordering::Release);
                    }
                }
                let _running = RunningGuard;
                native::run(
                    library_path,
                    native::Channels {
                        requests: request_rx,
                        events: event_tx,
                        stop: stop_rx,
                    },
                    worker_generation,
                    worker_stopping,
                    cache_bytes.min(CACHE_BYTES),
                    worker_counters,
                );
            })
            .map_err(|error| {
                WORKER_RUNNING.store(false, Ordering::Release);
                PdfError::Io(error.to_string())
            })?;
        Ok(Self {
            requests,
            events,
            generation,
            stopping,
            stop,
            counters,
            thread: Some(thread),
        })
    }

    pub fn events(&self) -> Receiver<PdfEvent> {
        self.events.clone()
    }
    pub fn stats(&self) -> PdfWorkerStats {
        PdfWorkerStats {
            started_renders: self.counters.started.load(Ordering::Relaxed),
            progressive_pauses: self.counters.pauses.load(Ordering::Relaxed),
            cancelled_renders: self.counters.cancelled.load(Ordering::Relaxed),
        }
    }
    pub fn set_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }
    pub fn try_send(&self, request: PdfRequest) -> Result<(), PdfError> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => PdfError::QueueFull,
                TrySendError::Disconnected(_) => PdfError::Stopped,
            })
    }
}

impl Drop for PdfWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct TileCache {
    tiles: HashMap<TileKey, TileBitmap>,
    order: VecDeque<TileKey>,
    bytes: usize,
    budget: usize,
}

impl TileCache {
    fn new(budget: usize) -> Self {
        Self {
            tiles: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }
    fn clear(&mut self) {
        self.tiles.clear();
        self.order.clear();
        self.bytes = 0;
    }
    fn get(&mut self, key: TileKey) -> Option<TileBitmap> {
        let value = self.tiles.get(&key)?.clone();
        self.order.retain(|old| *old != key);
        self.order.push_back(key);
        Some(value)
    }
    fn insert(&mut self, bitmap: TileBitmap) {
        let bytes = bitmap.rgba.len();
        if bytes > self.budget {
            return;
        }
        if let Some(old) = self.tiles.remove(&bitmap.key) {
            self.bytes -= old.rgba.len();
            self.order.retain(|key| *key != bitmap.key);
        }
        while self.bytes + bytes > self.budget {
            if let Some(key) = self.order.pop_front() {
                if let Some(old) = self.tiles.remove(&key) {
                    self.bytes -= old.rgba.len();
                }
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.order.push_back(bitmap.key);
        self.tiles.insert(bitmap.key, bitmap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tile(x: u32) -> TileBitmap {
        TileBitmap {
            key: TileKey {
                page: 0,
                scale_milli: 1000,
                x,
                y: 0,
            },
            width: 2,
            height: 2,
            rgba: Arc::new(vec![255; 16]),
        }
    }
    #[test]
    fn cache_evicts_least_recently_used_with_exact_byte_budget() {
        let mut cache = TileCache::new(32);
        cache.insert(tile(0));
        cache.insert(tile(1));
        assert!(cache.get(tile(0).key).is_some());
        cache.insert(tile(2));
        assert!(cache.get(tile(1).key).is_none());
        assert_eq!(cache.bytes, 32);
        cache.clear();
        assert_eq!(cache.bytes, 0);
    }
    #[test]
    fn tile_edges_never_allocate_a_full_page() {
        let key = TileKey {
            page: 0,
            scale_milli: 2000,
            x: 1024,
            y: 1536,
        };
        assert_eq!(
            key.dimensions(PageSize {
                width: 600.0,
                height: 800.0
            })
            .unwrap(),
            (176, 64, 1200, 1600)
        );
        assert!(
            TileKey {
                scale_milli: u32::MAX,
                ..key
            }
            .dimensions(PageSize::default())
            .is_err()
        );
        assert!(
            TileKey { x: u32::MAX, ..key }
                .dimensions(PageSize::default())
                .is_err()
        );
    }
    #[test]
    fn reverse_selection_preserves_unicode_text_order() {
        let chars = "研究A"
            .chars()
            .enumerate()
            .map(|(index, character)| TextChar {
                index,
                character,
                bounds: Rect::default(),
            })
            .collect();
        let page = PageText {
            page: 0,
            text: "研究A".into(),
            chars,
        };
        assert_eq!(page.selected_text(2, 0), "研究A");
    }
}
