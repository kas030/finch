use crate::{
    gpu_tiles,
    library_panel::{self, LibraryPanel},
    platform::{Config, Instance, InstanceMessage, RestoreGuard},
    plugin_panel::{self, PluginPanel},
    reader::{Input, Reader},
    services::{self, EventStream, PluginEvent, Plugins, Timer, Timers},
    viewport::Viewport,
};
use finch_core::{ReadingState, Zoom};
use finch_pdf::{
    Bookmark, LinkTarget, PdfCommand, PdfError, PdfEvent, PdfRequest, PdfResponse, PdfWorker,
    Priority, TileKey,
};
use finch_plugin::{DocumentContext, PluginContext};
use finch_store::{
    AppPaths, LibraryItem, SessionSnapshot, SnapshotStore, StoreCommand, StoreEvent, StoreRequest,
    StoreResponse, StoreWorker,
};
use iced::{
    Element, Event, Length, Subscription, Task, Theme,
    widget::{Space, button, column, container, row, scrollable, text, text_input},
    window,
};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub enum Message {
    Bridged {
        message: Box<Message>,
        received: async_channel::Sender<()>,
    },
    Frame(Instant),
    TileAllocated {
        key: TileKey,
        handle_id: iced::advanced::image::Id,
        result: Result<gpu_tiles::Allocation, gpu_tiles::Error>,
    },
    Event(Event, iced::event::Status, window::Id),
    Pdf(PdfEvent),
    Store(StoreEvent),
    Instance(InstanceMessage),
    Reader(Input),
    Library(library_panel::Action),
    Plugin(plugin_panel::Action),
    PluginEvent(PluginEvent),
    FilesPicked {
        paths: Vec<PathBuf>,
        relink: Option<String>,
    },
    Open,
    ToggleLibrary,
    ToggleNavigation,
    TogglePlugins,
    Theme,
    Zoom(Zoom),
    ZoomStep(f32),
    PageInput(String),
    GoPage,
    PreviousPage,
    NextPage,
    SearchInput(String),
    Search,
    SearchNext(bool),
    ClearSearch,
    PasswordInput(String),
    Unlock,
    NavMode(bool),
    NavScroll(f32),
    Jump(usize),
    DismissError,
    Timer(Timer),
    Dpi(f32),
    QuitFinished(Result<(), String>),
    Captured(window::Screenshot),
    Restore,
    RetryFile,
}

enum Purpose {
    Query { revision: u64, append: bool },
    Import { open: bool },
    OpenState { item: LibraryItem, token: u64 },
    Mutate,
    Save { version: u64 },
    Remove { document_id: String },
    Relink { document_id: String },
}

#[derive(Clone)]
struct PendingSave {
    snapshot: SessionSnapshot,
    edit_revision: u64,
}

pub struct App {
    config: Config,
    paths: AppPaths,
    _instance: Instance,
    pdf: PdfWorker,
    store: Option<StoreWorker>,
    store_failed: bool,
    output: async_channel::Sender<Message>,
    events: EventStream,
    _bridge: services::Bridge,
    uploads: gpu_tiles::UploadGate,
    dpi: f32,
    timers: Timers,
    plugins: Option<Plugins>,
    pub library: LibraryPanel,
    pub plugin_panel: PluginPanel,
    pub reader: Option<Reader>,
    generation: u64,
    serial: u64,
    pending_pdf: HashMap<u64, PdfCommand>,
    pending_store: HashMap<u64, Purpose>,
    pending_open: Option<(PathBuf, Option<ReadingState>, String)>,
    opening: bool,
    pdf_ready: bool,
    restore: Option<ReadingState>,
    restore_guard: RestoreGuard,
    bookmarks: Vec<Bookmark>,
    bookmarks_loaded: bool,
    pdf_failed: bool,
    thumbnails: bool,
    nav_scroll: f32,
    metadata: VecDeque<(String, PathBuf)>,
    pub show_library: bool,
    show_navigation: bool,
    show_plugins: bool,
    dark: Option<bool>,
    error: Option<String>,
    password: Option<String>,
    page_input: String,
    search_input: String,
    search_query: String,
    search_next: usize,
    search_index: usize,
    search_bytes: usize,
    search_limited: bool,
    version: u64,
    dirty: bool,
    edit_revision: u64,
    saves: VecDeque<PendingSave>,
    save_retry_at: Option<Instant>,
    save_failures: u32,
    last_saved: Instant,
    last_change: Instant,
    open_token: u64,
    query_revision: u64,
    window: Option<window::Id>,
    quitting: bool,
    started: Instant,
    measurements: HashMap<String, f64>,
    captured: bool,
    smoke_step: usize,
}

impl App {
    pub fn new(
        config: Config,
        paths: AppPaths,
        instance: Instance,
        started: Instant,
    ) -> Result<Self, String> {
        let snapshot = SnapshotStore::load(&paths.session);
        let restore_guard = RestoreGuard::new(&paths);
        let interrupted = restore_guard.was_interrupted();
        let pdf = PdfWorker::spawn_with_cache_budget(Some(config.pdfium_path()?), 0)
            .map_err(|e| e.to_string())?;
        let store = StoreWorker::spawn(paths.clone()).map_err(|e| e.to_string())?;
        let (output, events) = async_channel::bounded(1);
        let bridge = services::bridge(&pdf, &store, &instance, output.clone());
        let timers = Timers::new(output.clone());
        if let Some(ms) = config.quit_after_ms {
            timers.after(Timer::Quit, Duration::from_millis(ms));
        }
        let restore = snapshot.snapshot.as_ref().map(|s| s.state.clone());
        let version = snapshot.snapshot.map_or(0, |s| s.version);
        let explicit_file = config.file.clone();
        let mut app = Self {
            config,
            paths,
            _instance: instance,
            pdf,
            store: Some(store),
            store_failed: false,
            output,
            events: EventStream(events),
            _bridge: bridge,
            uploads: gpu_tiles::UploadGate::default(),
            dpi: 1.0,
            timers,
            plugins: None,
            library: LibraryPanel::default(),
            plugin_panel: PluginPanel::default(),
            reader: None,
            generation: 0,
            serial: 1,
            pending_pdf: HashMap::new(),
            pending_store: HashMap::new(),
            pending_open: None,
            opening: false,
            pdf_ready: false,
            restore: restore.clone(),
            restore_guard,
            bookmarks: Vec::new(),
            bookmarks_loaded: false,
            pdf_failed: false,
            thumbnails: false,
            nav_scroll: 0.0,
            metadata: VecDeque::new(),
            show_library: restore.is_none() || interrupted,
            show_navigation: false,
            show_plugins: false,
            dark: None,
            error: snapshot.warning,
            password: None,
            page_input: String::new(),
            search_input: String::new(),
            search_query: String::new(),
            search_next: 0,
            search_index: 0,
            search_bytes: 0,
            search_limited: false,
            version,
            dirty: false,
            edit_revision: 0,
            saves: VecDeque::new(),
            save_retry_at: None,
            save_failures: 0,
            last_saved: Instant::now(),
            last_change: Instant::now(),
            open_token: 0,
            query_revision: 0,
            window: None,
            quitting: false,
            started,
            measurements: HashMap::new(),
            captured: false,
            smoke_step: 0,
        };
        if let Some(file) = explicit_file {
            app.send_store(
                StoreCommand::Import(vec![file]),
                Purpose::Import { open: true },
            );
        } else if !interrupted {
            if let Some(state) = restore {
                app.open_pdf(state.path.clone(), Some(state.clone()), state.document_id);
            }
        } else {
            app.error = Some("上次恢复 PDF 时未完成。可以点击“恢复上次阅读”重新尝试。".into());
        }
        Ok(app)
    }
    fn id(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }
    pub fn title(&self) -> String {
        self.reader.as_ref().map_or_else(
            || "Finch · 论文阅读".into(),
            |r| {
                format!(
                    "{} — Finch",
                    r.info.title.clone().unwrap_or_else(|| r
                        .info
                        .path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned())
                )
            },
        )
    }
    pub fn theme(&self) -> Option<Theme> {
        self.dark
            .map(|dark| if dark { Theme::Dark } else { Theme::Light })
    }
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run_with(self.events.clone(), services::stream),
            iced::event::listen_with(|event, status, id| Some(Message::Event(event, status, id))),
            if !self.measurements.contains_key("first_redraw_ms") || self.needs_uploads() {
                window::frames().map(Message::Frame)
            } else {
                Subscription::none()
            },
        ])
    }
    fn needs_uploads(&self) -> bool {
        !self.pdf_failed
            && !self.quitting
            && self.reader.as_ref().is_some_and(|reader| {
                reader.tiles.blocked_on_pixels()
                    || (!reader.software
                        && reader
                            .tiles
                            .entries
                            .values()
                            .any(|tile| !tile.drawable(false)))
            })
    }
    fn allocate_tiles(&mut self) -> Task<Message> {
        if self.pdf_failed || self.quitting || !self.uploads.can_start() {
            return Task::none();
        }
        let Some(reader) = &mut self.reader else {
            return Task::none();
        };
        if reader.software {
            return Task::none();
        }
        let mut keys = reader.tile_keys(0.0);
        keys.extend(reader.tiles.entries.keys().copied());
        let mut tasks = Vec::new();
        for key in keys {
            if !self.uploads.can_start() {
                break;
            }
            if let Some(handle) = reader.tiles.request_handle(key) {
                let handle_id = handle.id();
                if self.uploads.start(&handle) {
                    tasks.push(gpu_tiles::allocate(handle).map(move |result| {
                        Message::TileAllocated {
                            key,
                            handle_id,
                            result,
                        }
                    }));
                }
            }
        }
        Task::batch(tasks)
    }
    fn record(&mut self, name: &str) {
        self.measurements
            .entry(name.into())
            .or_insert_with(|| self.started.elapsed().as_secs_f64() * 1000.0);
    }
    fn try_store(&mut self, command: StoreCommand, purpose: Purpose) -> bool {
        if self.store_failed {
            return false;
        }
        let id = self.id();
        if let Some(store) = &self.store {
            match store.sender().try_send(StoreRequest { id, command }) {
                Ok(()) => {
                    self.pending_store.insert(id, purpose);
                    return true;
                }
                Err(e) => {
                    self.error = Some(format!("存储队列暂时不可用：{e}"));
                }
            }
        }
        false
    }
    fn send_store(&mut self, command: StoreCommand, purpose: Purpose) {
        self.try_store(command, purpose);
    }
    fn query_library(&mut self, append: bool) {
        self.query_revision += 1;
        self.library.loading = true;
        let query = if append {
            self.library.next_query()
        } else {
            self.library.query()
        };
        if !self.try_store(
            StoreCommand::Query(query),
            Purpose::Query {
                revision: self.query_revision,
                append,
            },
        ) {
            self.library.loading = false;
        }
    }
    fn invalidate_view(&mut self) {
        self.generation += 1;
        self.pdf.set_generation(self.generation);
        self.pending_pdf.clear();
    }
    fn send_pdf(&mut self, command: PdfCommand, priority: Priority) -> bool {
        if self.pending_pdf.len() >= 12 {
            return false;
        }
        let id = self.id();
        let request = PdfRequest {
            id,
            generation: self.generation,
            priority,
            command: command.clone(),
        };
        match self.pdf.try_send(request) {
            Ok(()) => {
                self.pending_pdf.insert(id, command);
                true
            }
            Err(PdfError::QueueFull) => false,
            Err(error) => {
                self.error = Some(error.to_string());
                false
            }
        }
    }
    fn open_pdf(&mut self, path: PathBuf, state: Option<ReadingState>, id: String) {
        if self.quitting || self.removing(&id) {
            return;
        }
        self.save(false);
        // A queued save may not have reached SQLite yet when the user returns
        // to a document. Prefer the exact state retained in memory.
        let state = self
            .saves
            .iter()
            .rev()
            .find(|save| save.snapshot.state.document_id == id)
            .map(|save| save.snapshot.state.clone())
            .or(state);
        self.invalidate_view();
        self.reader = None;
        self.dirty = false;
        self.bookmarks.clear();
        self.search_query.clear();
        self.search_input.clear();
        self.search_next = 0;
        self.password = None;
        self.pending_open = Some((path, state, id));
        self.opening = true;
        self.show_library = false;
        self.show_navigation = false;
        self.error = None;
        self.pdf_failed = false;
        self.bookmarks_loaded = false;
        if let Err(e) = self.restore_guard.mark() {
            self.error = Some(format!("无法记录恢复保护状态：{e}"));
        }
        self.schedule();
    }
    fn schedule(&mut self) {
        if !self.pdf_ready || self.quitting || self.pdf_failed {
            return;
        }
        if self.opening {
            if self.password.is_none()
                && !self
                    .pending_pdf
                    .values()
                    .any(|cmd| matches!(cmd, PdfCommand::Open { .. }))
                && let Some((path, _, _)) = &self.pending_open
            {
                self.send_pdf(
                    PdfCommand::Open {
                        path: path.clone(),
                        password: None,
                    },
                    Priority::Visible,
                );
            }
            return;
        }
        let Some(reader) = &mut self.reader else {
            return;
        };
        reader.begin_content_view(self.generation);
        let visible: Vec<_> = reader.visible_pages(0.0).collect();
        let mut requests = Vec::<(PdfCommand, Priority)>::new();
        let mut prefetch_candidates = Vec::new();
        for &page in &visible {
            if !reader.known_sizes.contains(&page)
                && !self
                    .pending_pdf
                    .values()
                    .any(|cmd| matches!(cmd,PdfCommand::PageInfo{page:p} if *p==page))
            {
                requests.push((PdfCommand::PageInfo { page }, Priority::Visible));
            }
        }
        for key in reader.tile_keys(0.0) {
            if !reader.tiles.contains(&key)
                && !self
                    .pending_pdf
                    .values()
                    .any(|cmd| matches!(cmd,PdfCommand::RenderTile{key:k} if *k==key))
            {
                requests.push((PdfCommand::RenderTile { key }, Priority::Visible));
            }
        }
        let page = reader.current_page();
        for page in reader.content_pages() {
            if reader.needs_text(page)
                && !self
                    .pending_pdf
                    .values()
                    .any(|cmd| matches!(cmd,PdfCommand::Text{page:p} if *p==page))
            {
                requests.push((PdfCommand::Text { page }, Priority::Interactive));
            }
            if reader.needs_links(page)
                && !self
                    .pending_pdf
                    .values()
                    .any(|cmd| matches!(cmd,PdfCommand::Links{page:p} if *p==page))
            {
                requests.push((PdfCommand::Links { page }, Priority::Interactive));
            }
        }
        if self.show_navigation
            && !self.bookmarks_loaded
            && !self
                .pending_pdf
                .values()
                .any(|cmd| matches!(cmd, PdfCommand::Bookmarks))
        {
            requests.push((PdfCommand::Bookmarks, Priority::Interactive));
        }
        if !self.search_query.is_empty()
            && !self.search_limited
            && self.search_next < reader.info.page_count
            && !self
                .pending_pdf
                .values()
                .any(|cmd| matches!(cmd, PdfCommand::Search { .. }))
        {
            requests.push((
                PdfCommand::Search {
                    page: self.search_next,
                    query: self.search_query.clone(),
                    case_sensitive: false,
                },
                Priority::Background,
            ));
        }
        let near_start = visible.first().copied().unwrap_or(page).saturating_sub(1);
        let near_end =
            (visible.last().copied().unwrap_or(page) + 1).min(reader.info.page_count - 1);
        for page in near_start..=near_end {
            if !reader.known_sizes.contains(&page)
                && !self
                    .pending_pdf
                    .values()
                    .any(|cmd| matches!(cmd,PdfCommand::PageInfo{page:p} if *p==page))
            {
                requests.push((PdfCommand::PageInfo { page }, Priority::Prefetch));
            }
        }
        for key in reader
            .tile_keys(reader.viewport.height as f64 * 0.5)
            .into_iter()
            .take(64)
        {
            prefetch_candidates.push(key);
            if !reader.tiles.contains(&key)
                && !self
                    .pending_pdf
                    .values()
                    .any(|cmd| matches!(cmd,PdfCommand::RenderTile{key:k} if *k==key))
                && !requests
                    .iter()
                    .any(|(cmd, _)| matches!(cmd,PdfCommand::RenderTile{key:k} if *k==key))
            {
                requests.push((PdfCommand::RenderTile { key }, Priority::Prefetch));
            }
        }
        if self.show_navigation && self.thumbnails {
            let start = (self.nav_scroll / 210.0) as usize;
            for page in start..(start + 6).min(reader.info.page_count) {
                if !reader.known_sizes.contains(&page) {
                    if !self
                        .pending_pdf
                        .values()
                        .any(|cmd| matches!(cmd,PdfCommand::PageInfo{page:p} if *p==page))
                    {
                        requests.push((PdfCommand::PageInfo { page }, Priority::Background));
                    }
                    continue;
                }
                let key = TileKey {
                    page,
                    scale_milli: (144.0 / reader.sizes[page].width * 1000.0) as u32,
                    x: 0,
                    y: 0,
                };
                prefetch_candidates.push(key);
                if !reader.tiles.contains(&key)
                    && !self
                        .pending_pdf
                        .values()
                        .any(|cmd| matches!(cmd,PdfCommand::RenderTile{key:k} if *k==key))
                {
                    requests.push((PdfCommand::RenderTile { key }, Priority::Background));
                }
            }
        }
        if let Some((_, path)) = self.metadata.front()
            && !self
                .pending_pdf
                .values()
                .any(|cmd| matches!(cmd, PdfCommand::Metadata { .. }))
        {
            requests.push((
                PdfCommand::Metadata { path: path.clone() },
                Priority::Background,
            ));
        }
        reader.begin_prefetch_view(&prefetch_candidates);
        requests.retain(|(command, priority)| match command {
            PdfCommand::RenderTile { key } if *priority != Priority::Visible => {
                reader.needs_prefetch(key)
            }
            _ => true,
        });
        let pixels_blocked = reader.tiles.blocked_on_pixels();
        for (command, priority) in requests {
            // Retired frame/upload handles remain charged until their last
            // owner drops. A frame retries; never re-render in a tight loop.
            if pixels_blocked && matches!(command, PdfCommand::RenderTile { .. }) {
                continue;
            }
            let content = match &command {
                PdfCommand::Text { page } => Some((*page, true)),
                PdfCommand::Links { page } => Some((*page, false)),
                _ => None,
            };
            let prefetch = match &command {
                PdfCommand::RenderTile { key } if priority != Priority::Visible => Some(*key),
                _ => None,
            };
            if !self.send_pdf(command, priority) {
                break;
            }
            if let Some((page, text)) = content
                && let Some(reader) = &mut self.reader
            {
                if text {
                    reader.mark_text_requested(page);
                } else {
                    reader.mark_links_requested(page);
                }
            }
            if let Some(key) = prefetch
                && let Some(reader) = &mut self.reader
            {
                reader.mark_prefetch_requested(key);
            }
        }
    }
    fn dirty(&mut self) {
        self.edit_revision += 1;
        self.dirty = true;
        self.last_change = Instant::now();
        let until_max = Duration::from_secs(2).saturating_sub(self.last_saved.elapsed());
        self.timers
            .after(Timer::Save, Duration::from_millis(500).min(until_max));
    }
    fn save(&mut self, clean: bool) {
        if let Some(state) = self
            .reader
            .as_ref()
            .filter(|_| self.dirty || clean)
            .map(Reader::state)
            && !self.removing(&state.document_id)
            && !self.saves.iter().any(|save| {
                save.edit_revision == self.edit_revision
                    && save.snapshot.state == state
                    && (!clean || save.snapshot.clean_shutdown)
            })
        {
            let in_flight = self
                .pending_store
                .values()
                .find_map(|purpose| match purpose {
                    Purpose::Save { version } => Some(*version),
                    _ => None,
                });
            // Coalesce only unsent states for this document. Never mutate a
            // state which the worker may already have written.
            self.saves.retain(|save| {
                save.snapshot.state.document_id != state.document_id
                    || Some(save.snapshot.version) == in_flight
            });
            self.version += 1;
            self.saves.push_back(PendingSave {
                snapshot: SessionSnapshot {
                    version: self.version,
                    state,
                    clean_shutdown: clean,
                },
                edit_revision: self.edit_revision,
            });
        }
        self.pump_saves();
    }
    fn removing(&self, id: &str) -> bool {
        self.pending_store.values().any(|purpose| matches!(purpose, Purpose::Remove { document_id } | Purpose::Relink { document_id } if document_id == id))
    }
    fn pump_saves(&mut self) {
        if self.store_failed {
            return;
        }
        if self
            .pending_store
            .values()
            .any(|purpose| matches!(purpose, Purpose::Save { .. }))
        {
            return;
        }
        if let Some(retry) = self.save_retry_at {
            if retry > Instant::now() {
                self.timers
                    .after(Timer::Save, retry.saturating_duration_since(Instant::now()));
                return;
            }
            self.save_retry_at = None;
        }
        let Some(save) = self.saves.front() else {
            return;
        };
        if self.removing(&save.snapshot.state.document_id) {
            return;
        }
        let snapshot = save.snapshot.clone();
        let version = snapshot.version;
        if !self.try_store(
            StoreCommand::SaveSnapshot(snapshot),
            Purpose::Save { version },
        ) {
            self.retry_save();
        }
    }
    fn retry_save(&mut self) {
        // Disk/permission errors may last indefinitely. Retain every pending
        // state, but avoid repeated writes and UI warnings twice per second.
        let delay = Duration::from_millis((500_u64 << self.save_failures.min(6)).min(30_000));
        self.save_failures = self.save_failures.saturating_add(1);
        self.save_retry_at = Some(Instant::now() + delay);
        self.timers.after(Timer::Save, delay);
    }
    fn removed(&mut self, id: &str) {
        self.saves
            .retain(|save| save.snapshot.state.document_id != id);
        self.metadata.retain(|(document_id, _)| document_id != id);
        if self
            .restore
            .as_ref()
            .is_some_and(|state| state.document_id == id)
        {
            self.restore = None;
        }
        let current = self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.document_id == id);
        let opening = self
            .pending_open
            .as_ref()
            .is_some_and(|(_, _, document_id)| document_id == id);
        if current || opening {
            self.invalidate_view();
            self.send_pdf(PdfCommand::Close, Priority::Visible);
            self.reader = None;
            self.pending_open = None;
            self.opening = false;
            self.dirty = false;
            self.bookmarks.clear();
            self.password = None;
            self.show_library = true;
            self.show_navigation = false;
            let _ = self.restore_guard.clear();
        }
        if self.library.selected.as_deref() == Some(id) {
            self.library.selected = None;
        }
    }
    pub fn update(&mut self, message: Message) -> Task<Message> {
        if let Message::Bridged { message, received } = message {
            let task = self.update(*message);
            let _ = received.try_send(());
            return task;
        }
        // Keep receiving acknowledgements while exiting, but freeze changes to
        // the document whose final state is being committed.
        if self.quitting
            && !matches!(
                message,
                Message::Store(_)
                    | Message::QuitFinished(_)
                    | Message::Timer(Timer::Save)
                    | Message::TileAllocated { .. }
            )
        {
            if let Message::Pdf(event) = &message {
                // A failed exit returns to reading. Discarded responses must
                // not leave requests permanently marked as still in flight.
                self.pending_pdf.remove(&event.id);
                if matches!(event.result, Ok(PdfResponse::Ready)) {
                    self.pdf_ready = true;
                }
            }
            return Task::none();
        }
        let mut task = Task::none();
        match message {
            Message::Bridged { .. } => unreachable!(),
            Message::Frame(_at) => {
                self.uploads.on_frame();
                if let Some(reader) = &mut self.reader {
                    reader.tiles.resume_after_frame();
                }
                let first = !self.measurements.contains_key("first_redraw_ms");
                self.record("first_redraw_ms");
                if first
                    && self.reader.is_none()
                    && !self.opening
                    && self.config.screenshot.is_some()
                {
                    self.timers
                        .after(Timer::Capture, Duration::from_millis(300));
                }
            }
            Message::TileAllocated {
                key,
                handle_id,
                result,
            } => {
                self.uploads.finish(handle_id);
                if let Some(reader) = &mut self.reader {
                    let current = reader
                        .tiles
                        .entries
                        .get(&key)
                        .is_some_and(|tile| tile.handle.id() == handle_id);
                    if current {
                        if let Err(error) = &result {
                            self.error = Some(format!(
                                "无法分配页面纹理：{error}。可以使用 --software 启动。"
                            ));
                            self.pdf_failed = true;
                        }
                        reader.tiles.finish_allocate(key, handle_id, result);
                    }
                }
                self.visible_ready();
            }
            Message::Pdf(event) => self.pdf_event(event),
            Message::Store(event) => self.store_event(event),
            Message::Open => services::dialog(self.output.clone(), None),
            Message::FilesPicked { paths, relink } => {
                if let Some(document_id) = relink {
                    if let Some(path) = paths.first() {
                        self.send_store(
                            StoreCommand::Relink {
                                document_id: document_id.clone(),
                                path: path.clone(),
                            },
                            Purpose::Relink { document_id },
                        );
                    }
                } else if !paths.is_empty() {
                    self.send_store(StoreCommand::Import(paths), Purpose::Import { open: true });
                }
            }
            Message::Library(action) => {
                self.library.apply_local(&action);
                if matches!(
                    action,
                    library_panel::Action::SearchChanged(_)
                        | library_panel::Action::TagFilterChanged(_)
                        | library_panel::Action::ToggleFavorites
                        | library_panel::Action::SortChanged(_)
                ) {
                    task = iced::widget::operation::scroll_to(
                        library_panel::SCROLL_ID,
                        scrollable::AbsoluteOffset::<f32>::default(),
                    );
                }
                match action {
                    library_panel::Action::Import => services::dialog(self.output.clone(), None),
                    library_panel::Action::SearchChanged(_)
                    | library_panel::Action::TagFilterChanged(_) => {
                        self.timers.after(Timer::Query, Duration::from_millis(150))
                    }
                    library_panel::Action::ToggleFavorites
                    | library_panel::Action::SortChanged(_) => self.query_library(false),
                    library_panel::Action::LoadMore => self.query_library(true),
                    library_panel::Action::Open(id) => {
                        if let Some(item) = self.library.items.iter().find(|i| i.id == id).cloned()
                        {
                            self.open_token += 1;
                            self.send_store(
                                StoreCommand::GetReadingState(id),
                                Purpose::OpenState {
                                    item,
                                    token: self.open_token,
                                },
                            );
                        }
                    }
                    library_panel::Action::ToggleFavorite(document_id, favorite) => self
                        .send_store(
                            StoreCommand::SetFavorite {
                                document_id,
                                favorite,
                            },
                            Purpose::Mutate,
                        ),
                    library_panel::Action::SaveTags(document_id, tags) => self
                        .send_store(StoreCommand::SetTags { document_id, tags }, Purpose::Mutate),
                    library_panel::Action::Remove(id) => self.send_store(
                        StoreCommand::Remove(id.clone()),
                        Purpose::Remove { document_id: id },
                    ),
                    library_panel::Action::Relink(id) => {
                        services::dialog(self.output.clone(), Some(id))
                    }
                    _ => {}
                }
            }
            Message::Reader(input) => task = self.reader_input(input),
            Message::ToggleLibrary => {
                self.show_library = !self.show_library;
                if self.show_library {
                    self.show_navigation = false;
                    self.query_library(false);
                }
            }
            Message::ToggleNavigation => {
                self.show_navigation = !self.show_navigation;
                if self.show_navigation {
                    self.show_library = false;
                }
                self.schedule();
            }
            Message::TogglePlugins => {
                self.show_plugins = !self.show_plugins;
                if self.show_plugins {
                    self.plugin_action(plugin_panel::Action::Discover);
                }
            }
            Message::Plugin(action) => self.plugin_action(action),
            Message::PluginEvent(event) => {
                self.plugin_panel.busy = false;
                match event {
                    PluginEvent::List(Ok(plugins)) => {
                        self.plugin_panel.plugins = plugins;
                        self.plugin_panel.error = None;
                    }
                    PluginEvent::Result(Ok(result)) => {
                        self.plugin_panel.result = result.text;
                        self.plugin_panel.error = None;
                        if let Some(page) = result.navigate_to_page {
                            task = self.update(Message::Jump(page as usize));
                        }
                    }
                    PluginEvent::List(Err(error)) | PluginEvent::Result(Err(error)) => {
                        self.plugin_panel.error = Some(error)
                    }
                }
            }
            Message::Theme => {
                self.dark = match self.dark {
                    None => Some(false),
                    Some(false) => Some(true),
                    Some(true) => None,
                };
            }
            Message::Zoom(zoom) => {
                if let Some(reader) = &mut self.reader {
                    reader.set_zoom(zoom, None);
                    self.invalidate_view();
                    self.dirty();
                }
            }
            Message::ZoomStep(factor) => {
                if let Some(reader) = &self.reader {
                    task = self.update(Message::Zoom(Zoom::Scale(
                        (reader.scale() * factor).clamp(0.1, 8.0),
                    )));
                }
            }
            Message::PageInput(value) => self.page_input = value,
            Message::GoPage => {
                if let Ok(page) = self.page_input.parse::<usize>() {
                    task = self.update(Message::Jump(page.saturating_sub(1)));
                }
            }
            Message::Jump(page) => {
                self.page_input.clear();
                if let Some(reader) = &mut self.reader {
                    reader.jump(page);
                    self.invalidate_view();
                    self.dirty();
                }
            }
            Message::PreviousPage => {
                if let Some(reader) = &self.reader {
                    task = self.update(Message::Jump(reader.current_page().saturating_sub(1)));
                }
            }
            Message::NextPage => {
                if let Some(reader) = &self.reader {
                    task = self.update(Message::Jump(reader.current_page() + 1));
                }
            }
            Message::SearchInput(value) => self.search_input = value,
            Message::Search => {
                self.search_query = self.search_input.trim().into();
                self.search_next = 0;
                self.search_index = usize::MAX;
                self.search_bytes = 0;
                self.search_limited = false;
                if let Some(reader) = &mut self.reader {
                    reader.matches.clear();
                }
                self.invalidate_view();
            }
            Message::ClearSearch => {
                self.search_bytes = 0;
                self.search_limited = false;
                self.search_query.clear();
                self.search_input.clear();
                if let Some(reader) = &mut self.reader {
                    reader.matches.clear();
                }
                self.invalidate_view();
            }
            Message::SearchNext(forward) => {
                if let Some(reader) = &mut self.reader {
                    let mut matches: Vec<_> = reader
                        .matches
                        .iter()
                        .flat_map(|(page, hits)| {
                            hits.iter()
                                .map(move |hit| (*page, hit.bounds.first().copied()))
                        })
                        .collect();
                    matches.sort_by_key(|(page, _)| *page);
                    if !matches.is_empty() {
                        self.search_index = if forward {
                            self.search_index.checked_add(1).unwrap_or(0) % matches.len()
                        } else {
                            self.search_index
                                .checked_add(matches.len() - 1)
                                .map_or(matches.len() - 1, |index| index % matches.len())
                        };
                        let (page, rect) = matches[self.search_index];
                        reader.jump(page);
                        if let Some(rect) = rect {
                            reader.scroll(
                                0.0,
                                (rect.y * reader.scale() - reader.viewport.height * 0.25).max(0.0),
                            );
                        }
                        self.invalidate_view();
                        self.dirty();
                    }
                }
            }
            Message::PasswordInput(value) => self.password = Some(value),
            Message::Unlock => {
                let password = self.password.take();
                if let Some((path, _, _)) = &self.pending_open {
                    self.send_pdf(
                        PdfCommand::Open {
                            path: path.clone(),
                            password,
                        },
                        Priority::Visible,
                    );
                }
            }
            Message::NavMode(thumbnails) => {
                self.thumbnails = thumbnails;
                self.nav_scroll = 0.0;
                task = iced::widget::operation::scroll_to(
                    "navigation",
                    scrollable::AbsoluteOffset::<f32>::default(),
                );
            }
            Message::NavScroll(y) => self.nav_scroll = y,
            Message::DismissError => self.error = None,
            Message::Dpi(dpi) => {
                self.dpi = dpi;
                if let Some(reader) = &mut self.reader
                    && (reader.dpi - dpi).abs() > 0.01
                {
                    reader.dpi = dpi;
                    self.invalidate_view();
                }
            }
            Message::Restore => {
                if let Some(state) = self.restore.clone() {
                    self.open_pdf(state.path.clone(), Some(state.clone()), state.document_id);
                }
            }
            Message::RetryFile => {
                if let Some(reader) = &self.reader {
                    let state = reader.state();
                    self.open_pdf(state.path.clone(), Some(state.clone()), state.document_id);
                } else if let Some((path, state, id)) = self.pending_open.clone() {
                    self.open_pdf(path, state, id);
                }
            }
            Message::Instance(event) => {
                match event {
                    InstanceMessage::Open(path) => self.send_store(
                        StoreCommand::Import(vec![path]),
                        Purpose::Import { open: true },
                    ),
                    InstanceMessage::Focus => {}
                }
                if let Some(id) = self.window {
                    task = window::gain_focus(id);
                }
            }
            Message::Timer(Timer::Save) => {
                self.pump_saves();
                if self.last_change.elapsed() >= Duration::from_millis(490)
                    || self.last_saved.elapsed() >= Duration::from_millis(1990)
                {
                    self.save(false);
                } else {
                    self.timers.after(Timer::Save, Duration::from_millis(100));
                }
            }
            Message::Timer(Timer::Query) => self.query_library(false),
            Message::Timer(Timer::Quit) => task = self.quit(),
            Message::Timer(Timer::Capture) => {
                if let Some(id) = self.window {
                    task = window::screenshot(id).map(Message::Captured);
                }
            }
            Message::Timer(Timer::Smoke) => task = self.smoke(),
            Message::Captured(screenshot) => {
                self.record("capture_complete_ms");
                self.captured = true;
                if let Some(path) = self.config.screenshot.clone() {
                    let output = self.output.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = services::write_screenshot(path, screenshot) {
                            let _ = output
                                .send_blocking(Message::PluginEvent(PluginEvent::Result(Err(e))));
                        }
                    });
                }
            }
            Message::QuitFinished(Ok(())) => {
                self.write_metrics();
                task = iced::exit();
            }
            Message::QuitFinished(Err(error)) => {
                self.quitting = false;
                self.error = Some(format!("退出保存失败：{error}。阅读状态仍保留在窗口内。"));
                // Shutdown consumes the worker. Reconnect a fresh actor and
                // bridge so a failed exit never leaves a read-only window.
                if self.store.is_none() {
                    match StoreWorker::spawn(self.paths.clone()) {
                        Ok(store) => {
                            self._bridge = services::bridge(
                                &self.pdf,
                                &store,
                                &self._instance,
                                self.output.clone(),
                            );
                            self.store = Some(store);
                            self.store_failed = false;
                        }
                        Err(error) => self.error = Some(format!("无法重新启动存储：{error}")),
                    }
                }
            }
            Message::Event(event, status, id) => {
                self.window = Some(id);
                match event {
                    Event::Window(window::Event::Opened { .. }) => {
                        self.record("window_opened_ms");
                        task = window::scale_factor(id).map(Message::Dpi);
                    }
                    Event::Window(window::Event::Resized(_)) => {
                        task = window::scale_factor(id).map(Message::Dpi)
                    }
                    Event::Window(window::Event::CloseRequested) => task = self.quit(),
                    Event::Window(window::Event::FileDropped(path)) => self.send_store(
                        StoreCommand::Import(vec![path]),
                        Purpose::Import { open: true },
                    ),
                    Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        key, modifiers, ..
                    }) => {
                        use iced::keyboard::{Key, key::Named};
                        let command = modifiers.command();
                        match key.as_ref() {
                            Key::Character("o") if command => {
                                services::dialog(self.output.clone(), None)
                            }
                            Key::Character("f") if command => {
                                task = iced::widget::operation::focus("document-search")
                            }
                            Key::Character("c")
                                if command && status == iced::event::Status::Ignored =>
                            {
                                if let Some(reader) = &self.reader {
                                    task = iced::clipboard::write(reader.selected_text());
                                }
                            }
                            Key::Character("+") | Key::Character("=") if command => {
                                task = self.update(Message::ZoomStep(1.15))
                            }
                            Key::Character("-") if command => {
                                task = self.update(Message::ZoomStep(1.0 / 1.15))
                            }
                            Key::Named(Named::PageDown)
                                if status == iced::event::Status::Ignored =>
                            {
                                if let Some(reader) = &self.reader {
                                    task = self.update(Message::Reader(Input::Scroll {
                                        x: 0.0,
                                        y: reader.viewport.height * 0.9,
                                    }));
                                }
                            }
                            Key::Named(Named::PageUp) if status == iced::event::Status::Ignored => {
                                if let Some(reader) = &self.reader {
                                    task = self.update(Message::Reader(Input::Scroll {
                                        x: 0.0,
                                        y: -reader.viewport.height * 0.9,
                                    }));
                                }
                            }
                            Key::Named(Named::Home) if command => {
                                task = self.update(Message::Jump(0))
                            }
                            Key::Named(Named::End) if command => {
                                if let Some(reader) = &self.reader {
                                    task = self.update(Message::Jump(reader.info.page_count - 1));
                                }
                            }
                            Key::Named(Named::Escape) => {
                                self.error = None;
                                task = self.update(Message::ClearSearch);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        self.schedule();
        self.finish_quit_if_ready();
        Task::batch([task, self.allocate_tiles()])
    }
    fn pdf_event(&mut self, event: PdfEvent) {
        let command = self.pending_pdf.remove(&event.id);
        if event.id != 0 && event.generation != self.generation {
            return;
        }
        match event.result {
            Ok(PdfResponse::Ready) => {
                self.pdf_ready = true;
                self.record("pdfium_ready_ms");
            }
            Ok(PdfResponse::Opened(info)) => {
                self.record("document_opened_ms");
                self.opening = false;
                if let Some((_, restore, id)) = self.pending_open.take() {
                    let title = info.title.clone();
                    self.reader = Some(Reader::new(info, id.clone(), restore.as_ref()));
                    if let Some(reader) = &mut self.reader {
                        reader.dpi = self.dpi;
                        reader.software = self.config.software;
                    }
                    if let Some(title) = title {
                        self.send_store(
                            StoreCommand::UpdateTitle {
                                document_id: id,
                                title,
                            },
                            Purpose::Mutate,
                        );
                    }
                }
                self.dirty();
            }
            Ok(PdfResponse::PageInfo { page, size }) => {
                if let Some(reader) = &mut self.reader {
                    reader.set_page_size(page, size);
                }
            }
            Ok(PdfResponse::Tile(tile)) => {
                if let Some(reader) = &mut self.reader {
                    let visible = reader.tile_keys(0.0).into_iter().collect();
                    reader.tiles.insert(tile, &visible);
                }
                self.visible_ready();
            }
            Ok(PdfResponse::Text(text)) => {
                if let Some(reader) = &mut self.reader {
                    reader.text.insert(text.page, text);
                    reader.trim_text();
                }
            }
            Ok(PdfResponse::Links { page, links }) => {
                if let Some(reader) = &mut self.reader {
                    reader.links.insert(page, links);
                    reader.trim_text();
                }
            }
            Ok(PdfResponse::Bookmarks(bookmarks)) => {
                self.bookmarks = bookmarks;
                self.bookmarks_loaded = true;
            }
            Ok(PdfResponse::Search { page, hits }) => {
                if let Some(reader) = &mut self.reader {
                    let mut accepted = Vec::new();
                    let count: usize = reader.matches.values().map(Vec::len).sum();
                    for (count, hit) in (count..).zip(hits) {
                        let bytes = std::mem::size_of::<finch_pdf::SearchHit>() * 2
                            + hit.bounds.capacity() * std::mem::size_of::<finch_core::Rect>();
                        if count >= 10_000 || self.search_bytes + bytes > 4 * 1024 * 1024 {
                            self.search_limited = true;
                            break;
                        }
                        self.search_bytes += bytes;
                        accepted.push(hit);
                    }
                    if !accepted.is_empty() {
                        reader.matches.insert(page, accepted);
                    }
                }
                self.search_next = page + 1;
            }
            Ok(PdfResponse::Metadata { path, title, .. }) => {
                if let Some((id, queued)) = self.metadata.front().cloned()
                    && queued == path
                {
                    self.metadata.pop_front();
                    if let Some(title) = title {
                        self.send_store(
                            StoreCommand::UpdateTitle {
                                document_id: id,
                                title,
                            },
                            Purpose::Mutate,
                        );
                    }
                }
            }
            Ok(PdfResponse::Closed) => {}
            Err(PdfError::Cancelled) => {}
            Err(PdfError::PasswordRequired) => {
                self.password = Some(String::new());
                self.error = Some("此 PDF 需要密码，或输入的密码不正确。".into());
            }
            Err(error) => {
                if matches!(command, Some(PdfCommand::Metadata { .. })) {
                    self.metadata.pop_front();
                } else {
                    self.error = Some(error.to_string());
                    self.pdf_failed = true;
                    if self.opening {
                        self.show_library = true;
                        self.opening = false;
                        let _ = self.restore_guard.clear();
                    }
                }
            }
        }
    }
    fn visible_ready(&mut self) {
        let Some(reader) = &self.reader else {
            return;
        };
        let visible = reader.tile_keys(0.0);
        if visible.is_empty()
            || reader
                .visible_pages(0.0)
                .any(|p| !reader.known_sizes.contains(&p))
            || !visible.iter().all(|key| {
                reader
                    .tiles
                    .entries
                    .get(key)
                    .is_some_and(|tile| tile.drawable(reader.software))
            })
        {
            return;
        }
        let first = !self.measurements.contains_key("visible_tiles_ready_ms");
        self.record("visible_tiles_ready_ms");
        if let Err(error) = self.restore_guard.clear() {
            self.error = Some(error.to_string());
        }
        if first {
            if self.config.screenshot.is_some() || self.config.metrics.is_some() {
                self.timers
                    .after(Timer::Capture, Duration::from_millis(120));
            }
            if self.config.smoke {
                self.timers.after(Timer::Smoke, Duration::from_millis(400));
            }
        }
    }
    fn store_event(&mut self, event: StoreEvent) {
        if event.id == 0 && event.result.is_err() {
            // Initialization failed before any queued command could run. Those
            // requests can never receive replies; retained saves stay in memory.
            self.store_failed = true;
            self.pending_store.clear();
            self.save_retry_at = None;
        }
        let purpose = self.pending_store.remove(&event.id);
        match event.result {
            Ok(StoreResponse::Ready(_)) => {
                self.record("store_ready_ms");
                self.query_library(false);
            }
            Ok(StoreResponse::Library(items)) => {
                if let Some(Purpose::Query { revision, append }) = purpose
                    && revision == self.query_revision
                {
                    self.library.set_results(items, append);
                }
            }
            Ok(StoreResponse::Imported(report)) => {
                if !report.failed.is_empty() {
                    self.error = Some(
                        report
                            .failed
                            .iter()
                            .map(|f| format!("{}：{}", f.path.display(), f.message))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                for item in &report.imported {
                    self.metadata
                        .push_back((item.id.clone(), item.path.clone()));
                }
                if !self.quitting
                    && matches!(purpose, Some(Purpose::Import { open: true }))
                    && let Some(item) = report.imported.first().or(report.existing.first()).cloned()
                {
                    self.open_token += 1;
                    self.send_store(
                        StoreCommand::GetReadingState(item.id.clone()),
                        Purpose::OpenState {
                            item,
                            token: self.open_token,
                        },
                    );
                }
                self.query_library(false);
            }
            Ok(StoreResponse::ReadingState(state)) => {
                if let Some(Purpose::OpenState { item, token }) = purpose
                    && token == self.open_token
                    && !self.quitting
                    && !self.removing(&item.id)
                {
                    self.library.selected = Some(item.id.clone());
                    self.open_pdf(item.path, state, item.id);
                }
            }
            Ok(StoreResponse::Relinked(item)) => {
                for save in &mut self.saves {
                    if save.snapshot.state.document_id == item.id {
                        save.snapshot.state.path = item.path.clone();
                        save.snapshot.state.revision = item.revision;
                    }
                }
                if let Some(restore) = &mut self.restore
                    && restore.document_id == item.id
                {
                    restore.path = item.path.clone();
                    restore.revision = item.revision;
                }
                let active = self
                    .reader
                    .as_ref()
                    .filter(|reader| reader.document_id == item.id)
                    .map(Reader::state)
                    .or_else(|| {
                        self.pending_open
                            .as_ref()
                            .filter(|(_, _, id)| *id == item.id)
                            .and_then(|(_, state, _)| state.clone())
                    });
                if let Some(mut state) = active {
                    state.path = item.path.clone();
                    state.revision = item.revision;
                    self.reader = None;
                    self.dirty = false;
                    if !self.quitting {
                        self.open_pdf(item.path, Some(state.clone()), item.id);
                    }
                    self.version += 1;
                    self.saves.push_back(PendingSave {
                        snapshot: SessionSnapshot {
                            version: self.version,
                            state,
                            clean_shutdown: self.quitting,
                        },
                        edit_revision: self.edit_revision,
                    });
                } else if self.reader.is_none()
                    && self.pending_open.is_none()
                    && let Some(state) = self
                        .restore
                        .as_ref()
                        .filter(|state| state.document_id == item.id)
                        .cloned()
                {
                    self.version += 1;
                    self.saves.push_back(PendingSave {
                        snapshot: SessionSnapshot {
                            version: self.version,
                            state,
                            clean_shutdown: self.quitting,
                        },
                        edit_revision: self.edit_revision,
                    });
                }
                if !self.quitting {
                    self.query_library(false);
                }
            }
            Ok(StoreResponse::Done) => {
                if let Some(Purpose::Remove { document_id }) = &purpose {
                    self.removed(document_id);
                }
                if !self.quitting
                    && matches!(purpose, Some(Purpose::Mutate | Purpose::Remove { .. }))
                {
                    self.query_library(false);
                }
            }
            Ok(StoreResponse::Saved(_)) => {
                if let Some(Purpose::Save { version }) = purpose
                    && let Some(index) = self
                        .saves
                        .iter()
                        .position(|save| save.snapshot.version == version)
                {
                    self.save_failures = 0;
                    self.save_retry_at = None;
                    let saved = self.saves.remove(index).unwrap();
                    self.restore = Some(saved.snapshot.state.clone());
                    if saved.edit_revision == self.edit_revision
                        && self.reader.as_ref().is_some_and(|reader| {
                            reader.document_id == saved.snapshot.state.document_id
                        })
                    {
                        self.dirty = false;
                    }
                    self.last_saved = Instant::now();
                }
            }
            Err(error) => {
                self.error = Some(error.clone());
                self.library.error = Some(error);
                self.library.loading = false;
                if matches!(purpose, Some(Purpose::Save { .. })) {
                    self.retry_save();
                }
                if self.quitting {
                    self.quitting = false;
                    self.error = self
                        .error
                        .take()
                        .map(|error| format!("退出保存失败：{error}。状态已保留，将重试。"));
                }
            }
        }
        self.pump_saves();
    }
    fn reader_input(&mut self, input: Input) -> Task<Message> {
        let Some(reader) = &mut self.reader else {
            return Task::none();
        };
        let mut changed = true;
        match input {
            Input::Resize(size) => reader.resize(size),
            Input::Scroll { x, y } => reader.scroll(x, y),
            Input::Zoom { delta, at } => reader.set_zoom(
                Zoom::Scale((reader.scale() * (1.0 + delta.clamp(-0.5, 0.5))).clamp(0.1, 8.0)),
                Some(at),
            ),
            Input::ScrollTo(ratio) => reader.scroll_to(ratio),
            Input::SelectStart(at) => {
                reader.select_start(at);
                changed = false;
            }
            Input::SelectMove(at) => {
                reader.select_move(at);
                changed = false;
            }
            Input::SelectEnd(at) => {
                reader.select_move(at);
                reader.selection_dragging = false;
                changed = false;
                let clicked = reader
                    .selection_start
                    .take()
                    .is_some_and(|start| (start.x - at.x).abs() + (start.y - at.y).abs() < 4.0);
                if clicked {
                    reader.selection = None;
                    if let Some((page, point)) = reader.viewport_to_page(at)
                        && let Some(target) = reader
                            .links
                            .get(&page)
                            .and_then(|links| links.iter().find(|link| link.bounds.contains(point)))
                            .map(|link| link.target.clone())
                    {
                        match target {
                            LinkTarget::Page(page) => reader.jump(page),
                            LinkTarget::Uri(uri) => {
                                if uri.starts_with("https://") || uri.starts_with("http://") {
                                    std::thread::spawn(move || {
                                        let _ = webbrowser::open(&uri);
                                    });
                                }
                            }
                        }
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.invalidate_view();
            self.dirty();
        }
        Task::none()
    }
    fn plugin_action(&mut self, action: plugin_panel::Action) {
        if matches!(action, plugin_panel::Action::Dismiss) {
            self.show_plugins = false;
            return;
        }
        let context = PluginContext {
            document: self.reader.as_ref().map(|r| DocumentContext {
                id: r.document_id.clone(),
                title: r.info.title.clone().unwrap_or_default(),
                page_index: r.current_page() as u32,
            }),
            selected_text: self
                .reader
                .as_ref()
                .map_or_else(String::new, Reader::selected_text),
        };
        let plugins = self
            .plugins
            .get_or_insert_with(|| Plugins::spawn(&self.paths, self.output.clone()));
        self.plugin_panel.busy = true;
        if let Err(error) = plugins.send(action, context) {
            self.plugin_panel.busy = false;
            self.plugin_panel.error = Some(error);
        }
    }
    fn quit(&mut self) -> Task<Message> {
        if self.quitting {
            return Task::none();
        }
        if self.store_failed && (self.reader.is_some() || !self.saves.is_empty()) {
            // Retrying an explicit close also retries initialization after the
            // user repairs the database/directory; automatic timers stay idle.
            match StoreWorker::spawn(self.paths.clone()) {
                Ok(store) => {
                    self._bridge =
                        services::bridge(&self.pdf, &store, &self._instance, self.output.clone());
                    self.store = Some(store);
                    self.store_failed = false;
                }
                Err(error) => {
                    self.error = Some(format!(
                        "无法启动存储：{error}。尚未保存的阅读状态仍保留在窗口中。"
                    ));
                    return Task::none();
                }
            }
        }
        self.quitting = true;
        self.invalidate_view();
        // A deliberate close is an immediate final-save attempt even if the
        // automatic retry of a previous failure was backed off.
        self.save_retry_at = None;
        self.save_failures = 0;
        self.save(true);
        self.finish_quit_if_ready();
        Task::none()
    }
    fn finish_quit_if_ready(&mut self) {
        if !self.quitting
            || self.store.is_none()
            || !self.saves.is_empty()
            || !self.pending_store.is_empty()
        {
            return;
        }
        let store = self.store.take();
        let output = self.output.clone();
        let plugins = self.plugins.take();
        let failed_before_start = self.store_failed;
        std::thread::Builder::new()
            .name("finch-exit".into())
            .spawn(move || {
                // Join both services even when the first one reports an error.
                let stored = store.map_or(Ok(()), |worker| {
                    worker.shutdown().map_err(|e| e.to_string())
                });
                let stopped = plugins.map_or(Ok(()), Plugins::shutdown);
                // With no reader or retained saves, a known initialization
                // failure has no pending user data and must not trap the window.
                let result = if failed_before_start { Ok(()) } else { stored }.and(stopped);
                let _ = output.send_blocking(Message::QuitFinished(result));
            })
            .expect("create exit worker");
    }
    fn write_metrics(&self) {
        if let Some(path) = &self.config.metrics {
            let stats = self.pdf.stats();
            let report = serde_json::json!({"renderer_backend":if self.config.software {"tiny-skia"} else {"wgpu"},"software_renderer_effective":self.config.software,"stages_ms":self.measurements,"tile_cache_bytes":self.reader.as_ref().map_or(0,|r|r.tiles.bytes),"pdf_worker":{"started_renders":stats.started_renders,"progressive_pauses":stats.progressive_pauses,"cancelled_renders":stats.cancelled_renders},"reading_state":self.reader.as_ref().map(Reader::state),"smoke_steps":self.smoke_step,"screenshot_captured":self.captured,"error":self.error});
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(bytes) = serde_json::to_vec_pretty(&report) {
                let _ = std::fs::write(path, bytes);
            }
        }
    }
    fn smoke(&mut self) -> Task<Message> {
        let action = match self.smoke_step {
            0 => Message::Jump(self.reader.as_ref().map_or(0, |r| r.info.page_count / 2)),
            1 => Message::Zoom(Zoom::Scale(1.35)),
            2 => Message::Reader(Input::Scroll { x: 0.0, y: 220.0 }),
            3 => {
                self.search_input = "Finch".into();
                Message::Search
            }
            4 => Message::ToggleLibrary,
            5 => Message::ToggleNavigation,
            6 => Message::TogglePlugins,
            _ => {
                self.record("smoke_completed_ms");
                self.timers
                    .after(Timer::Capture, Duration::from_millis(300));
                return Task::none();
            }
        };
        self.smoke_step += 1;
        self.timers.after(Timer::Smoke, Duration::from_millis(450));
        self.update(action)
    }
    pub fn view(&self) -> Element<'_, Message> {
        let toolbar = row![
            text("Finch").size(24),
            button("文献库")
                .on_press(Message::ToggleLibrary)
                .style(button::text),
            button("打开 PDF")
                .on_press(Message::Open)
                .style(button::secondary),
            Space::new().width(Length::Fill),
            button("目录")
                .on_press(Message::ToggleNavigation)
                .style(button::text),
            button("插件")
                .on_press(Message::TogglePlugins)
                .style(button::text),
            button(match self.dark {
                None => "主题：系统",
                Some(false) => "主题：浅色",
                Some(true) => "主题：深色",
            })
            .on_press(Message::Theme)
            .style(button::text)
        ]
        .spacing(12)
        .padding([12, 20])
        .align_y(iced::Alignment::Center);
        let mut main = row![].height(Length::Fill);
        if self.show_library {
            main = main.push(
                container(library_panel::view(&self.library).map(Message::Library))
                    .width(300)
                    .height(Length::Fill),
            );
        }
        if self.show_navigation && self.reader.is_some() {
            main = main.push(self.navigation());
        }
        let center: Element<'_, Message> = if self.quitting {
            container(text("正在保存阅读位置…").size(20))
                .center(Length::Fill)
                .into()
        } else if let Some(password) = &self.password {
            container(
                column![
                    text("此文档需要密码").size(24),
                    text_input("PDF 密码", password)
                        .secure(true)
                        .on_input(Message::PasswordInput)
                        .on_submit(Message::Unlock),
                    button("解锁").on_press(Message::Unlock)
                ]
                .spacing(18)
                .width(320),
            )
            .center(Length::Fill)
            .into()
        } else if let Some(reader) = &self.reader {
            let page = reader.current_page() + 1;
            let hit_count: usize = reader.matches.values().map(Vec::len).sum();
            let tools = row![
                button("‹")
                    .on_press(Message::PreviousPage)
                    .style(button::text),
                text_input(&page.to_string(), &self.page_input)
                    .on_input(Message::PageInput)
                    .on_submit(Message::GoPage)
                    .width(54),
                text(format!("/ {}", reader.info.page_count)),
                button("›").on_press(Message::NextPage).style(button::text),
                Space::new().width(10),
                button("−")
                    .on_press(Message::ZoomStep(1.0 / 1.15))
                    .style(button::text),
                text(format!("{:.0}%", reader.scale() * 100.0)),
                button("＋")
                    .on_press(Message::ZoomStep(1.15))
                    .style(button::text),
                button("适宽")
                    .on_press(Message::Zoom(Zoom::FitWidth))
                    .style(button::text),
                button("整页")
                    .on_press(Message::Zoom(Zoom::FitPage))
                    .style(button::text),
                Space::new().width(Length::Fill)
            ]
            .spacing(6)
            .padding([8, 12])
            .align_y(iced::Alignment::Center);
            let search_tools = row![
                text_input("搜索文内文字", &self.search_input)
                    .id("document-search")
                    .on_input(Message::SearchInput)
                    .on_submit(Message::Search)
                    .width(Length::Fill),
                button("查找").on_press(Message::Search).style(button::text)
            ]
            .spacing(6)
            .padding([8, 12])
            .align_y(iced::Alignment::Center);
            let mut reading = column![tools, search_tools];
            if !self.search_query.is_empty() {
                reading = reading.push(
                    row![
                        text(format!(
                            "{} 处匹配 · 已搜索 {}/{} 页{}",
                            hit_count,
                            self.search_next,
                            reader.info.page_count,
                            if self.search_limited {
                                " · 结果已达上限，请缩小关键词范围"
                            } else {
                                ""
                            }
                        ))
                        .size(12),
                        button("上一处")
                            .on_press(Message::SearchNext(false))
                            .style(button::text),
                        button("下一处")
                            .on_press(Message::SearchNext(true))
                            .style(button::text),
                        button("关闭搜索")
                            .on_press(Message::ClearSearch)
                            .style(button::text)
                    ]
                    .spacing(8)
                    .padding([0, 16]),
                );
            }
            reading
                .push(Element::from(Viewport(reader)).map(Message::Reader))
                .height(Length::Fill)
                .width(Length::Fill)
                .into()
        } else if self.opening {
            container(
                column![
                    text("正在打开论文…").size(24),
                    text("恢复阅读位置，按需加载页面。")
                ]
                .spacing(12),
            )
            .center(Length::Fill)
            .into()
        } else {
            let mut empty = column![
                text("专注于下一页。 ").size(38),
                text("本地论文阅读与管理").size(18),
                Space::new().height(16),
                button("打开 PDF").on_press(Message::Open).padding([12, 24]),
                text("拖入论文即可开始 · Ctrl / ⌘ O").size(13)
            ]
            .spacing(12);
            if self.restore.is_some() {
                empty = empty.push(
                    button("恢复上次阅读")
                        .on_press(Message::Restore)
                        .style(button::text),
                );
            }
            container(empty).center(Length::Fill).into()
        };
        main = main.push(center);
        if self.show_plugins {
            main = main.push(
                container(self.plugin_panel.view().map(Message::Plugin))
                    .width(300)
                    .height(Length::Fill),
            );
        }
        let mut root = column![toolbar];
        if let Some(error) = &self.error {
            root = root.push(
                container(
                    row![
                        text(error).size(13).width(Length::Fill),
                        button("重新打开")
                            .on_press(Message::RetryFile)
                            .style(button::text),
                        button("关闭")
                            .on_press(Message::DismissError)
                            .style(button::text)
                    ]
                    .spacing(12)
                    .align_y(iced::Alignment::Center),
                )
                .padding([8, 20])
                .style(container::bordered_box),
            );
        }
        root = root.push(main);
        let status = self.reader.as_ref().map_or_else(
            || "所有文献保存在本地".into(),
            |r| {
                format!(
                    "{}    ·    第 {} 页    ·    {}",
                    r.info
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    r.current_page() + 1,
                    if self.dirty || !self.saves.is_empty() {
                        "正在保存位置"
                    } else {
                        "阅读位置已保存"
                    }
                )
            },
        );
        root.push(container(text(status).size(11)).padding([6, 20]))
            .height(Length::Fill)
            .into()
    }
    fn navigation(&self) -> Element<'_, Message> {
        let reader = self.reader.as_ref().unwrap();
        let mut items = column![
            row![
                button("目录")
                    .on_press(Message::NavMode(false))
                    .style(button::text),
                button("缩略图")
                    .on_press(Message::NavMode(true))
                    .style(button::text)
            ]
            .spacing(8)
        ];
        let mut list = column![].spacing(0);
        if self.thumbnails {
            let start = (self.nav_scroll / 210.0) as usize;
            let end = (start + 6).min(reader.info.page_count);
            list = list.push(Space::new().height(start as f32 * 210.0));
            for page in start..end {
                let key = TileKey {
                    page,
                    scale_milli: (144.0 / reader.sizes[page].width * 1000.0) as u32,
                    x: 0,
                    y: 0,
                };
                let content: Element<'_, Message> = if let Some(tile) = reader
                    .tiles
                    .entries
                    .get(&key)
                    .filter(|tile| tile.drawable(reader.software))
                {
                    iced::widget::image(tile.handle.clone())
                        .width(144)
                        .height(175)
                        .into()
                } else {
                    container(text((page + 1).to_string()))
                        .width(144)
                        .height(175)
                        .center_x(144)
                        .center_y(175)
                        .style(container::bordered_box)
                        .into()
                };
                list = list.push(
                    container(
                        column![
                            button(content)
                                .on_press(Message::Jump(page))
                                .style(button::text),
                            text(format!("第 {} 页", page + 1)).size(11)
                        ]
                        .align_x(iced::Alignment::Center),
                    )
                    .height(210),
                );
            }
            list = list.push(Space::new().height((reader.info.page_count - end) as f32 * 210.0));
        } else if self.bookmarks.is_empty() {
            list = list.push(
                text(if self.bookmarks_loaded {
                    "此 PDF 没有目录"
                } else {
                    "正在读取目录…"
                })
                .size(13),
            );
        } else {
            let start = (self.nav_scroll / 40.0) as usize;
            let end = (start + 30).min(self.bookmarks.len());
            list = list.push(Space::new().height(start as f32 * 40.0));
            for bookmark in self.bookmarks.iter().take(end).skip(start) {
                let mut link = button(
                    text(&bookmark.title)
                        .size(13)
                        .wrapping(iced::widget::text::Wrapping::None),
                )
                .style(button::text)
                .width(Length::Fill);
                if let Some(page) = bookmark.page {
                    link = link.on_press(Message::Jump(page));
                }
                list = list.push(container(link).height(40).padding(iced::Padding {
                    left: (bookmark.depth.min(6) * 10) as f32,
                    ..Default::default()
                }));
            }
            list = list.push(Space::new().height((self.bookmarks.len() - end) as f32 * 40.0));
        }
        items = items.push(
            scrollable(list)
                .id("navigation")
                .on_scroll(|v| Message::NavScroll(v.absolute_offset().y))
                .height(Length::Fill),
        );
        container(items)
            .padding(12)
            .width(200)
            .height(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn reader(id: &str) -> Reader {
        Reader::new(
            finch_pdf::DocumentInfo {
                path: PathBuf::from(format!("{id}.pdf")),
                page_count: 10,
                first_page: finch_core::PageSize {
                    width: 600.0,
                    height: 800.0,
                },
                revision: finch_core::FileRevision::default(),
                title: None,
            },
            id.into(),
            None,
        )
    }

    #[test]
    fn full_queue_and_failed_save_preserve_old_documents_and_removal_cancels_final_save() {
        let temp = tempfile::TempDir::new().unwrap();
        let library = temp.path().join("invalid-pdfium-test-library");
        std::fs::write(&library, b"not a dynamic library").unwrap();
        let paths = AppPaths::in_directory(temp.path().join("data"));
        let crate::platform::InstanceClaim::Primary(instance) =
            Instance::claim(&paths, None).unwrap()
        else {
            panic!("test instance")
        };
        let config = Config {
            pdfium: Some(library),
            ..Default::default()
        };
        let mut app = App::new(config, paths, instance, Instant::now()).unwrap();
        // Saturate both actor queues while the UI does not consume its events.
        // No native PDF library, GUI renderer, or timing assumptions are needed.
        app._bridge.stop_for_test().unwrap();
        let sender = app.store.as_ref().unwrap().sender();
        let responses = app.store.as_ref().unwrap().events();
        sender
            .send(StoreRequest {
                id: 9_999,
                command: StoreCommand::Flush,
            })
            .unwrap();
        while responses.recv_timeout(Duration::from_secs(2)).unwrap().id != 9_999 {}
        // 64 responses, one blocked publication, then 64 queued requests.
        for id in 10_000..10_129 {
            sender
                .send_timeout(
                    StoreRequest {
                        id,
                        command: StoreCommand::Query(Default::default()),
                    },
                    Duration::from_secs(2),
                )
                .unwrap();
        }
        assert!(sender.is_full() && responses.is_full());
        app.reader = Some(reader("first"));
        app.dirty();
        app.save(false);
        assert!(app.dirty, "a full queue cannot acknowledge unsaved state");
        assert_eq!(app.saves.len(), 1);
        assert!(
            !app.pending_store
                .values()
                .any(|purpose| matches!(purpose, Purpose::Save { .. }))
        );
        drop(sender);
        drop(responses);
        app.store.take().unwrap().shutdown().unwrap();

        app.quitting = true;
        app.pending_pdf
            .insert(25_000, PdfCommand::PageInfo { page: 0 });
        let (received, acknowledgement) = async_channel::bounded(1);
        let _ = app.update(Message::Bridged {
            message: Box::new(Message::Pdf(PdfEvent {
                id: 25_000,
                generation: app.generation,
                result: Ok(PdfResponse::Closed),
            })),
            received,
        });
        assert!(
            acknowledgement.try_recv().is_ok(),
            "discarded PDF events still release the event stream"
        );
        assert!(
            app.pending_pdf.is_empty(),
            "discarded work cannot suppress renders after a failed exit"
        );
        let _ = app.update(Message::Pdf(PdfEvent {
            id: 0,
            generation: 0,
            result: Ok(PdfResponse::Ready),
        }));
        assert!(
            app.pdf_ready,
            "a readiness event cannot be lost during a failed exit"
        );
        app.pdf_ready = false;
        app.quitting = false;

        app.open_pdf(PathBuf::from("second.pdf"), None, "second".into());
        app.reader = Some(reader("second"));
        app.dirty();
        app.save(false);
        assert_eq!(
            app.saves.len(),
            2,
            "switching cannot discard an unsent first-document state"
        );
        let old = app.saves.front().unwrap().snapshot.clone();
        app.pending_store.insert(
            30_000,
            Purpose::Save {
                version: old.version,
            },
        );
        app.store_event(StoreEvent {
            id: 30_000,
            result: Err("temporary disk failure".into()),
        });
        assert_eq!(app.saves.front().unwrap().snapshot, old);
        assert!(app.save_retry_at.is_some());
        for _ in 0..10 {
            app.retry_save();
        }
        let delay = app
            .save_retry_at
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(delay > Duration::from_secs(29) && delay <= Duration::from_secs(30));
        app.pending_store.insert(30_099, Purpose::Mutate);
        app.store_event(StoreEvent {
            id: 0,
            result: Err("database cannot open".into()),
        });
        assert!(app.store_failed && app.pending_store.is_empty());
        assert_eq!(
            app.saves.front().unwrap().snapshot,
            old,
            "initialization errors retain unsaved state"
        );
        assert!(
            app.save_retry_at.is_none(),
            "a dead actor is not polled forever"
        );
        app.store_failed = false;
        app.pending_store.insert(
            30_001,
            Purpose::Save {
                version: old.version,
            },
        );
        app.store_event(StoreEvent {
            id: 30_001,
            result: Ok(StoreResponse::Saved(finch_store::SaveOutcome::Saved)),
        });
        assert!(
            app.dirty,
            "an old document acknowledgement cannot clear a new document's edits"
        );
        assert_eq!(app.saves.len(), 1);

        app.pending_store.insert(
            30_002,
            Purpose::Remove {
                document_id: "second".into(),
            },
        );
        app.save(true);
        app.store_event(StoreEvent {
            id: 30_002,
            result: Ok(StoreResponse::Done),
        });
        assert!(app.reader.is_none());
        assert!(app.pending_open.is_none());
        assert!(app.saves.is_empty());
        app.save(true);
        assert!(
            app.saves.is_empty(),
            "exit must not recreate a removed startup session"
        );
        drop(app);
        temp.close().unwrap();
    }
}
