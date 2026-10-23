//! Unsafe PDFium boundary. All functions run on the single PDF actor thread.
//! Handles are owned by guards and closed before their parents. File, bitmap,
//! pause and search buffers remain alive until PDFium releases their pointers.

use super::*;
use pdfium_render::prelude::*;
use std::{
    cell::Cell,
    collections::HashSet,
    ffi::{c_int, c_uchar, c_ulong, c_void},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    ptr,
    time::{Duration, Instant},
};

const MAX_CHARS: usize = 250_000;
const MAX_BOOKMARKS: usize = 20_000;
const MAX_TEXT_BYTES: usize = 1024 * 1024;

pub(super) struct Channels {
    pub requests: Receiver<PdfRequest>,
    pub events: Sender<PdfEvent>,
    pub stop: Receiver<()>,
}

pub(super) fn run(
    library_path: Option<PathBuf>,
    channels: Channels,
    generation: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    cache_bytes: usize,
    counters: Arc<RenderCounters>,
) {
    let Channels {
        requests,
        events,
        stop,
    } = channels;
    let library = match Library::load(library_path) {
        Ok(library) => library,
        Err(error) => {
            let _ = events.send(PdfEvent {
                id: 0,
                generation: 0,
                result: Err(error),
            });
            return;
        }
    };
    let emit = |event| crossbeam_channel::select! { send(events, event) -> result => result.is_ok(), recv(stop) -> _ => false };
    if !emit(PdfEvent {
        id: 0,
        generation: 0,
        result: Ok(PdfResponse::Ready),
    }) {
        return;
    }
    let mut document = None;
    let mut pending = Vec::<PdfRequest>::with_capacity(REQUEST_CAPACITY);
    let mut cache = TileCache::new(cache_bytes);
    loop {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        if pending.is_empty() {
            crossbeam_channel::select! {
                recv(stop) -> _ => break,
                recv(requests) -> request => match request { Ok(request) => pending.push(request), Err(_) => break },
            }
        }
        while pending.len() < REQUEST_CAPACITY {
            match requests.try_recv() {
                Ok(request) => pending.push(request),
                Err(_) => break,
            }
        }
        // All queues are bounded; recent visible requests take precedence over
        // prefetch and metadata. FIFO is retained within the same priority.
        let index = pending
            .iter()
            .enumerate()
            .min_by_key(|(_, request)| {
                (
                    if matches!(request.command, PdfCommand::Open { .. } | PdfCommand::Close) {
                        0
                    } else {
                        1
                    },
                    request.priority,
                    request.id,
                )
            })
            .map(|(index, _)| index)
            .unwrap();
        let request = pending.remove(index);
        if request.generation != generation.load(Ordering::Acquire) {
            if !emit(PdfEvent {
                id: request.id,
                generation: request.generation,
                result: Err(PdfError::Cancelled),
            }) {
                break;
            }
            continue;
        }
        let cancellation = Cancellation {
            generation: &generation,
            expected: request.generation,
            stopping: &stopping,
            counters: &counters,
        };
        let result = match request.command {
            PdfCommand::Open { path, password } => {
                cache.clear();
                // Release the old file before opening the replacement, including
                // on password failure, so later requests cannot read the old PDF.
                document = None;
                match Document::open(library.bindings.as_ref(), &path, password.as_deref()) {
                    Ok(opened) => {
                        let info = opened.info();
                        document = Some(opened);
                        info.map(PdfResponse::Opened)
                    }
                    Err(error) => Err(error),
                }
            }
            PdfCommand::Close => {
                document = None;
                cache.clear();
                Ok(PdfResponse::Closed)
            }
            PdfCommand::Metadata { path } => Document::open(library.bindings.as_ref(), &path, None)
                .map(|document| {
                    let title = document.title();
                    PdfResponse::Metadata {
                        path,
                        title,
                        revision: document.revision,
                        page_count: document.count,
                    }
                }),
            command => document
                .as_mut()
                .ok_or(PdfError::NoDocument)
                .and_then(|document| document.handle(command, &mut cache, &cancellation)),
        };
        let result = if cancellation.cancelled() {
            Err(PdfError::Cancelled)
        } else {
            result
        };
        if !emit(PdfEvent {
            id: request.id,
            generation: request.generation,
            result,
        }) {
            break;
        }
    }
}

struct Library {
    bindings: Box<dyn PdfiumLibraryBindings>,
}

impl Library {
    fn load(explicit: Option<PathBuf>) -> Result<Self, PdfError> {
        let path = if let Some(path) = explicit {
            path
        } else if let Some(path) = std::env::var_os("FINCH_PDFIUM_PATH") {
            PathBuf::from(path)
        } else {
            let executable =
                std::env::current_exe().map_err(|error| PdfError::Io(error.to_string()))?;
            let folder = executable.parent().unwrap_or(Path::new("."));
            let name = Pdfium::pdfium_platform_library_name();
            let candidates = [
                folder.join(&name),
                folder.join("../Frameworks").join(&name),
                folder.join("pdfium").join(&name),
            ];
            candidates
                .into_iter()
                .find(|path| path.is_file())
                .ok_or_else(|| {
                    PdfError::Library(format!(
                        "place {} next to Finch, or set FINCH_PDFIUM_PATH",
                        name.to_string_lossy()
                    ))
                })?
        };
        let path = if path.is_dir() {
            path.join(Pdfium::pdfium_platform_library_name())
        } else {
            path
        };
        let bindings =
            Pdfium::bind_to_library(path).map_err(|error| PdfError::Library(error.to_string()))?;
        // SAFETY: The process-wide PdfWorker guard prohibits a second library,
        // and this thread owns all subsequent PDFium calls and finalization.
        unsafe {
            bindings.FPDF_InitLibrary();
        }
        Ok(Self { bindings })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            self.bindings.FPDF_DestroyLibrary();
        }
    }
}

struct Document<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    handle: FPDF_DOCUMENT,
    // Stable address passed to the custom random-access callback. Dropped only
    // after FPDF_CloseDocument returns, which stops all file callbacks.
    _file: Box<File>,
    path: PathBuf,
    revision: FileRevision,
    count: usize,
    pages: VecDeque<(usize, FPDF_PAGE)>,
}

unsafe extern "C" fn read_block(
    parameter: *mut c_void,
    position: c_ulong,
    buffer: *mut c_uchar,
    size: c_ulong,
) -> c_int {
    // No unwinding may cross the C ABI; invalid inputs return a read failure.
    if parameter.is_null() || buffer.is_null() || size as u128 > isize::MAX as u128 {
        return 0;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: Document owns this Box<File> throughout native access. PDFium
        // invokes callbacks synchronously on the actor thread, never concurrently.
        let file = unsafe { &mut *parameter.cast::<File>() };
        let output = unsafe { std::slice::from_raw_parts_mut(buffer, size as usize) };
        file.seek(SeekFrom::Start(position as u64))
            .and_then(|_| file.read_exact(output))
            .is_ok() as c_int
    }))
    .unwrap_or(0)
}

impl<'a> Document<'a> {
    fn open(
        bindings: &'a dyn PdfiumLibraryBindings,
        path: &Path,
        password: Option<&str>,
    ) -> Result<Self, PdfError> {
        if password.is_some_and(|value| value.contains('\0')) {
            return Err(PdfError::PasswordRequired);
        }
        let mut file = Box::new(File::open(path).map_err(|error| PdfError::Io(error.to_string()))?);
        let metadata = file
            .metadata()
            .map_err(|error| PdfError::Io(error.to_string()))?;
        let revision = FileRevision::from_metadata(&metadata);
        let len = c_ulong::try_from(revision.len).map_err(|_| {
            PdfError::Limit(
                "file exceeds this platform's PDFium random-access file size limit".into(),
            )
        })?;
        let mut access = FPDF_FILEACCESS {
            m_FileLen: len,
            m_GetBlock: Some(read_block),
            m_Param: (&mut *file as *mut File).cast(),
        };
        // SAFETY: File pointer is stable, callback follows PDFium's block API,
        // and the access structure itself is copied by FPDF_LoadCustomDocument.
        let handle = unsafe { bindings.FPDF_LoadCustomDocument(&mut access, password) };
        if handle.is_null() {
            let code = unsafe { bindings.FPDF_GetLastError() };
            return Err(if code == 4 {
                PdfError::PasswordRequired
            } else {
                PdfError::Native(format!("could not open PDF (PDFium error {code})"))
            });
        }
        let count = unsafe { bindings.FPDF_GetPageCount(handle) }.max(0) as usize;
        let document = Self {
            bindings,
            handle,
            _file: file,
            path: path.to_owned(),
            revision,
            count,
            pages: VecDeque::new(),
        };
        if count == 0 {
            return Err(PdfError::Native(
                "document contains no readable pages".into(),
            ));
        }
        Ok(document)
    }

    fn info(&self) -> Result<DocumentInfo, PdfError> {
        Ok(DocumentInfo {
            path: self.path.clone(),
            page_count: self.count,
            first_page: self.size(0)?,
            revision: self.revision,
            title: self.title(),
        })
    }

    fn title(&self) -> Option<String> {
        read_utf16(|pointer, len| unsafe {
            self.bindings
                .FPDF_GetMetaText(self.handle, "Title", pointer, len)
        })
        .filter(|value| !value.trim().is_empty())
    }

    fn size(&self, page: usize) -> Result<PageSize, PdfError> {
        if page >= self.count {
            return Err(PdfError::InvalidPage(page));
        }
        let mut size = FS_SIZEF {
            width: 0.0,
            height: 0.0,
        };
        let success = unsafe {
            self.bindings
                .FPDF_GetPageSizeByIndexF(self.handle, page as c_int, &mut size)
        };
        if success == 0
            || !size.width.is_finite()
            || !size.height.is_finite()
            || size.width <= 0.0
            || size.height <= 0.0
        {
            return Err(PdfError::Native("invalid page dimensions".into()));
        }
        Ok(PageSize {
            width: size.width,
            height: size.height,
        })
    }

    fn page(&mut self, index: usize) -> Result<FPDF_PAGE, PdfError> {
        if index >= self.count {
            return Err(PdfError::InvalidPage(index));
        }
        if let Some(position) = self.pages.iter().position(|(page, _)| *page == index) {
            let entry = self.pages.remove(position).unwrap();
            let handle = entry.1;
            self.pages.push_back(entry);
            return Ok(handle);
        }
        let handle = unsafe { self.bindings.FPDF_LoadPage(self.handle, index as c_int) };
        if handle.is_null() {
            return Err(PdfError::Native(format!(
                "could not load page {}",
                index + 1
            )));
        }
        if self.pages.len() >= 4
            && let Some((_, old)) = self.pages.pop_front()
        {
            unsafe {
                self.bindings.FPDF_ClosePage(old);
            }
        }
        self.pages.push_back((index, handle));
        Ok(handle)
    }

    fn handle(
        &mut self,
        command: PdfCommand,
        cache: &mut TileCache,
        cancel: &Cancellation<'_>,
    ) -> Result<PdfResponse, PdfError> {
        // Detection is demand-driven, with no filesystem watcher or background
        // poller. Never combine old parsed content with bytes from a changed PDF.
        let metadata =
            std::fs::metadata(&self.path).map_err(|error| PdfError::Io(error.to_string()))?;
        if FileRevision::from_metadata(&metadata) != self.revision {
            cache.clear();
            return Err(PdfError::FileChanged);
        }
        match command {
            PdfCommand::PageInfo { page } => Ok(PdfResponse::PageInfo {
                page,
                size: self.size(page)?,
            }),
            PdfCommand::RenderTile { key } => {
                if let Some(bitmap) = cache.get(key) {
                    return Ok(PdfResponse::Tile(bitmap));
                }
                let size = self.size(key.page)?;
                let page = self.page(key.page)?;
                let bitmap = render(self.bindings, page, size, key, cancel)?;
                cache.insert(bitmap.clone());
                Ok(PdfResponse::Tile(bitmap))
            }
            PdfCommand::Text { page } => {
                let handle = self.page(page)?;
                text(self.bindings, handle, page, cancel).map(PdfResponse::Text)
            }
            PdfCommand::Search {
                page,
                query,
                case_sensitive,
            } => {
                let handle = self.page(page)?;
                search(self.bindings, handle, &query, case_sensitive, cancel)
                    .map(|hits| PdfResponse::Search { page, hits })
            }
            PdfCommand::Bookmarks => bookmarks(self.bindings, self.handle, self.count, cancel)
                .map(PdfResponse::Bookmarks),
            PdfCommand::Links { page } => {
                let handle = self.page(page)?;
                links(self.bindings, self.handle, handle, self.count, cancel)
                    .map(|links| PdfResponse::Links { page, links })
            }
            PdfCommand::Open { .. } | PdfCommand::Metadata { .. } | PdfCommand::Close => {
                unreachable!("handled by actor")
            }
        }
    }
}

impl Drop for Document<'_> {
    fn drop(&mut self) {
        unsafe {
            for (_, handle) in self.pages.drain(..) {
                self.bindings.FPDF_ClosePage(handle);
            }
            self.bindings.FPDF_CloseDocument(self.handle);
        }
    }
}

struct Cancellation<'a> {
    generation: &'a AtomicU64,
    expected: u64,
    stopping: &'a AtomicBool,
    counters: &'a RenderCounters,
}
impl Cancellation<'_> {
    fn cancelled(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) != self.expected
    }
    fn check(&self) -> Result<(), PdfError> {
        if self.cancelled() {
            Err(PdfError::Cancelled)
        } else {
            Ok(())
        }
    }
}
struct PauseContext<'a, 'b> {
    cancel: &'a Cancellation<'b>,
    deadline: Cell<Instant>,
}

unsafe extern "C" fn should_pause(pause: *mut IFSDK_PAUSE) -> c_int {
    // SAFETY: render keeps both structs on its stack until RenderPage_Close.
    let context = unsafe { &*((*pause).user.cast::<PauseContext<'_, '_>>()) };
    (context.cancel.cancelled() || Instant::now() >= context.deadline.get()) as c_int
}

fn render(
    bindings: &dyn PdfiumLibraryBindings,
    page: FPDF_PAGE,
    size: PageSize,
    key: TileKey,
    cancel: &Cancellation<'_>,
) -> Result<TileBitmap, PdfError> {
    cancel.check()?;
    let (width, height, full_width, full_height) = key.dimensions(size)?;
    let mut rgba = vec![255_u8; width as usize * height as usize * 4];
    // SAFETY: rgba has exactly height rows of width*4 bytes and remains fixed
    // until bitmap destruction. Format 4 is BGRA; reverse-byte-order renders
    // directly to RGBA, avoiding a second allocation or encoding step.
    let bitmap = unsafe {
        bindings.FPDFBitmap_CreateEx(
            width as c_int,
            height as c_int,
            4,
            rgba.as_mut_ptr().cast(),
            (width * 4) as c_int,
        )
    };
    if bitmap.is_null() {
        return Err(PdfError::Native("could not allocate tile bitmap".into()));
    }
    struct RenderGuard<'a> {
        bindings: &'a dyn PdfiumLibraryBindings,
        page: FPDF_PAGE,
        bitmap: FPDF_BITMAP,
    }
    impl Drop for RenderGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                self.bindings.FPDF_RenderPage_Close(self.page);
                self.bindings.FPDFBitmap_Destroy(self.bitmap);
            }
        }
    }
    let mut context = PauseContext {
        cancel,
        deadline: Cell::new(Instant::now() + Duration::from_millis(8)),
    };
    let mut pause = IFSDK_PAUSE {
        version: 1,
        NeedToPauseNow: Some(should_pause),
        user: (&mut context as *mut PauseContext<'_, '_>).cast(),
    };
    let guard = RenderGuard {
        bindings,
        page,
        bitmap,
    };
    cancel.counters.started.fetch_add(1, Ordering::Relaxed);
    // Flags: noninteractive annotations, reverse byte order, bounded native
    // image cache. PDFium clips to the small bitmap despite full-page scale.
    let mut status = unsafe {
        bindings.FPDF_RenderPageBitmap_Start(
            bitmap,
            page,
            -(key.x as c_int),
            -(key.y as c_int),
            full_width as c_int,
            full_height as c_int,
            0,
            1 | 16 | 512,
            &mut pause,
        )
    };
    while status == 1 {
        cancel.counters.pauses.fetch_add(1, Ordering::Relaxed);
        if cancel.cancelled() {
            cancel.counters.cancelled.fetch_add(1, Ordering::Relaxed);
            return Err(PdfError::Cancelled);
        }
        std::thread::yield_now();
        context
            .deadline
            .set(Instant::now() + Duration::from_millis(8));
        status = unsafe { bindings.FPDF_RenderPage_Continue(page, &mut pause) };
    }
    drop(guard);
    if cancel.cancelled() {
        cancel.counters.cancelled.fetch_add(1, Ordering::Relaxed);
        return Err(PdfError::Cancelled);
    }
    if status != 2 {
        return Err(PdfError::Native(format!(
            "progressive rendering failed (status {status})"
        )));
    }
    Ok(TileBitmap {
        key,
        width,
        height,
        rgba: Arc::new(rgba),
    })
}

struct TextPage<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    handle: FPDF_TEXTPAGE,
}
impl<'a> TextPage<'a> {
    fn load(bindings: &'a dyn PdfiumLibraryBindings, page: FPDF_PAGE) -> Result<Self, PdfError> {
        let handle = unsafe { bindings.FPDFText_LoadPage(page) };
        if handle.is_null() {
            Err(PdfError::Native("could not load page text".into()))
        } else {
            Ok(Self { bindings, handle })
        }
    }
}
impl Drop for TextPage<'_> {
    fn drop(&mut self) {
        unsafe {
            self.bindings.FPDFText_ClosePage(self.handle);
        }
    }
}

/// Map every PDF rectangle through PDFium's actual page transform, including
/// intrinsic rotation and cropbox offsets, into top-left oriented page points.
fn display_rect(
    bindings: &dyn PdfiumLibraryBindings,
    page: FPDF_PAGE,
    left: f64,
    bottom: f64,
    right: f64,
    top: f64,
) -> Rect {
    let width = unsafe { bindings.FPDF_GetPageWidthF(page) };
    let height = unsafe { bindings.FPDF_GetPageHeightF(page) };
    let (device_width, device_height) = (
        (width * 100.0).round() as c_int,
        (height * 100.0).round() as c_int,
    );
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for (x, y) in [(left, bottom), (left, top), (right, bottom), (right, top)] {
        let (mut dx, mut dy) = (0, 0);
        let ok = unsafe {
            bindings.FPDF_PageToDevice(
                page,
                0,
                0,
                device_width,
                device_height,
                0,
                x,
                y,
                &mut dx,
                &mut dy,
            )
        };
        if ok == 0 {
            return Rect::default();
        }
        min_x = min_x.min(dx);
        max_x = max_x.max(dx);
        min_y = min_y.min(dy);
        max_y = max_y.max(dy);
    }
    Rect {
        x: min_x as f32 / 100.0,
        y: min_y as f32 / 100.0,
        width: (max_x as f64 - min_x as f64) as f32 / 100.0,
        height: (max_y as f64 - min_y as f64) as f32 / 100.0,
    }
}

fn text(
    bindings: &dyn PdfiumLibraryBindings,
    page: FPDF_PAGE,
    index: usize,
    cancel: &Cancellation<'_>,
) -> Result<PageText, PdfError> {
    let text_page = TextPage::load(bindings, page)?;
    let count = unsafe { bindings.FPDFText_CountChars(text_page.handle) };
    if count < 0 {
        return Err(PdfError::Native("could not count page characters".into()));
    }
    if count as usize > MAX_CHARS {
        return Err(PdfError::Limit(
            "more than 250,000 characters on one page".into(),
        ));
    }
    let mut chars = Vec::with_capacity(count as usize);
    let mut text = String::with_capacity(count as usize);
    for index in 0..count {
        if index % 256 == 0 {
            cancel.check()?;
        }
        let character =
            char::from_u32(unsafe { bindings.FPDFText_GetUnicode(text_page.handle, index) })
                .unwrap_or('\u{fffd}');
        let (mut left, mut right, mut bottom, mut top) = (0.0, 0.0, 0.0, 0.0);
        let valid = unsafe {
            bindings.FPDFText_GetCharBox(
                text_page.handle,
                index,
                &mut left,
                &mut right,
                &mut bottom,
                &mut top,
            )
        };
        let bounds = if valid != 0 {
            display_rect(bindings, page, left, bottom, right, top)
        } else {
            Rect::default()
        };
        text.push(character);
        chars.push(TextChar {
            index: index as usize,
            character,
            bounds,
        });
    }
    Ok(PageText {
        page: index,
        text,
        chars,
    })
}

fn search(
    bindings: &dyn PdfiumLibraryBindings,
    page: FPDF_PAGE,
    query: &str,
    case_sensitive: bool,
    cancel: &Cancellation<'_>,
) -> Result<Vec<SearchHit>, PdfError> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    if query.len() > 16_384 || query.contains('\0') {
        return Err(PdfError::Limit(
            "search term is too long or contains NUL".into(),
        ));
    }
    let text = TextPage::load(bindings, page)?;
    // Keep the UTF-16 query alive for the entire native search, independent of
    // whether a particular PDFium release happens to copy it at FindStart.
    let query: Vec<u16> = query.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        bindings.FPDFText_FindStart(text.handle, query.as_ptr(), case_sensitive as c_ulong, 0)
    };
    if handle.is_null() {
        return Err(PdfError::Native("could not begin text search".into()));
    }
    struct SearchGuard<'a> {
        bindings: &'a dyn PdfiumLibraryBindings,
        handle: FPDF_SCHHANDLE,
    }
    impl Drop for SearchGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                self.bindings.FPDFText_FindClose(self.handle);
            }
        }
    }
    let _guard = SearchGuard { bindings, handle };
    let mut hits = Vec::new();
    let mut rectangle_count = 0usize;
    while unsafe { bindings.FPDFText_FindNext(handle) } != 0 {
        cancel.check()?;
        if hits.len() >= 10_000 {
            return Err(PdfError::Limit(
                "more than 10,000 search matches on one page".into(),
            ));
        }
        let start = unsafe { bindings.FPDFText_GetSchResultIndex(handle) };
        let count = unsafe { bindings.FPDFText_GetSchCount(handle) };
        let rectangles =
            unsafe { bindings.FPDFText_CountRects(text.handle, start, count) }.clamp(0, 4096);
        rectangle_count += rectangles as usize;
        if rectangle_count > 65_536 {
            return Err(PdfError::Limit(
                "search highlight geometry exceeds one MiB on one page".into(),
            ));
        }
        let mut bounds = Vec::with_capacity(rectangles as usize);
        for rect in 0..rectangles {
            let (mut left, mut top, mut right, mut bottom) = (0.0, 0.0, 0.0, 0.0);
            if unsafe {
                bindings.FPDFText_GetRect(
                    text.handle,
                    rect,
                    &mut left,
                    &mut top,
                    &mut right,
                    &mut bottom,
                )
            } != 0
            {
                bounds.push(display_rect(bindings, page, left, bottom, right, top));
            }
        }
        if start >= 0 && count > 0 {
            hits.push(SearchHit {
                char_index: start as usize,
                char_count: count as usize,
                bounds,
            });
        }
    }
    Ok(hits)
}

fn read_utf16(mut read: impl FnMut(*mut c_void, c_ulong) -> c_ulong) -> Option<String> {
    let bytes = read(ptr::null_mut(), 0) as usize;
    if !(2..=MAX_TEXT_BYTES).contains(&bytes) || !bytes.is_multiple_of(2) {
        return None;
    }
    let mut buffer = vec![0u16; bytes / 2];
    let written = read(buffer.as_mut_ptr().cast(), bytes as c_ulong) as usize;
    if written > bytes {
        return None;
    }
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

fn destination_page(
    bindings: &dyn PdfiumLibraryBindings,
    document: FPDF_DOCUMENT,
    destination: FPDF_DEST,
    count: usize,
) -> Option<usize> {
    if destination.is_null() {
        return None;
    }
    let index = unsafe { bindings.FPDFDest_GetDestPageIndex(document, destination) };
    (index >= 0 && (index as usize) < count).then_some(index as usize)
}

fn bookmarks(
    bindings: &dyn PdfiumLibraryBindings,
    document: FPDF_DOCUMENT,
    count: usize,
    cancel: &Cancellation<'_>,
) -> Result<Vec<Bookmark>, PdfError> {
    let first = unsafe { bindings.FPDFBookmark_GetFirstChild(document, ptr::null_mut()) };
    let mut stack = vec![(first, 0usize)];
    let mut visited = HashSet::new();
    let mut result = Vec::new();
    let mut title_bytes = 0usize;
    while let Some((bookmark, depth)) = stack.pop() {
        cancel.check()?;
        if bookmark.is_null() || !visited.insert(bookmark as usize) {
            continue;
        }
        if result.len() >= MAX_BOOKMARKS || depth > 128 {
            return Err(PdfError::Limit(
                "bookmark tree is too large or deeply nested".into(),
            ));
        }
        let title = read_utf16(|buffer, len| unsafe {
            bindings.FPDFBookmark_GetTitle(bookmark, buffer, len)
        })
        .unwrap_or_default();
        title_bytes += title.capacity();
        if title_bytes > 2 * 1024 * 1024 {
            return Err(PdfError::Limit("bookmark titles exceed two MiB".into()));
        }
        let mut destination = unsafe { bindings.FPDFBookmark_GetDest(document, bookmark) };
        if destination.is_null() {
            let action = unsafe { bindings.FPDFBookmark_GetAction(bookmark) };
            if !action.is_null() && unsafe { bindings.FPDFAction_GetType(action) } == 1 {
                destination = unsafe { bindings.FPDFAction_GetDest(document, action) };
            }
        }
        result.push(Bookmark {
            title,
            page: destination_page(bindings, document, destination, count),
            depth,
        });
        stack.push((
            unsafe { bindings.FPDFBookmark_GetNextSibling(document, bookmark) },
            depth,
        ));
        stack.push((
            unsafe { bindings.FPDFBookmark_GetFirstChild(document, bookmark) },
            depth + 1,
        ));
    }
    Ok(result)
}

fn links(
    bindings: &dyn PdfiumLibraryBindings,
    document: FPDF_DOCUMENT,
    page: FPDF_PAGE,
    count: usize,
    cancel: &Cancellation<'_>,
) -> Result<Vec<Link>, PdfError> {
    let mut position = 0;
    let mut handle = ptr::null_mut();
    let mut links = Vec::new();
    let mut link_bytes = 0usize;
    while unsafe { bindings.FPDFLink_Enumerate(page, &mut position, &mut handle) } != 0 {
        cancel.check()?;
        if position > 20_000 {
            return Err(PdfError::Limit("too many links on one page".into()));
        }
        let mut rect = FS_RECTF {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
        if unsafe { bindings.FPDFLink_GetAnnotRect(handle, &mut rect) } == 0 {
            continue;
        }
        let direct = unsafe { bindings.FPDFLink_GetDest(document, handle) };
        let mut target = destination_page(bindings, document, direct, count).map(LinkTarget::Page);
        if target.is_none() {
            let action = unsafe { bindings.FPDFLink_GetAction(handle) };
            if !action.is_null() {
                match unsafe { bindings.FPDFAction_GetType(action) } {
                    1 => {
                        let destination = unsafe { bindings.FPDFAction_GetDest(document, action) };
                        target = destination_page(bindings, document, destination, count)
                            .map(LinkTarget::Page);
                    }
                    3 => {
                        let bytes = unsafe {
                            bindings.FPDFAction_GetURIPath(document, action, ptr::null_mut(), 0)
                        } as usize;
                        if bytes > 0 && bytes <= 16_384 {
                            let mut buffer = vec![0u8; bytes];
                            let written = unsafe {
                                bindings.FPDFAction_GetURIPath(
                                    document,
                                    action,
                                    buffer.as_mut_ptr().cast(),
                                    bytes as c_ulong,
                                )
                            } as usize;
                            if written <= bytes {
                                let end = buffer
                                    .iter()
                                    .position(|byte| *byte == 0)
                                    .unwrap_or(buffer.len());
                                if let Ok(uri) = std::str::from_utf8(&buffer[..end]) {
                                    target = Some(LinkTarget::Uri(uri.to_owned()));
                                }
                            }
                        }
                    }
                    _ => {} // Launch, JavaScript and remote-file actions are never executed.
                }
            }
        }
        if let Some(target) = target {
            link_bytes += std::mem::size_of::<Link>()
                + match &target {
                    LinkTarget::Uri(uri) => uri.capacity(),
                    LinkTarget::Page(_) => 0,
                };
            if link_bytes > 2 * 1024 * 1024 {
                return Err(PdfError::Limit(
                    "link data exceeds two MiB on one page".into(),
                ));
            }
            links.push(Link {
                bounds: display_rect(
                    bindings,
                    page,
                    rect.left as f64,
                    rect.bottom as f64,
                    rect.right as f64,
                    rect.top as f64,
                ),
                target,
            });
        }
    }
    Ok(links)
}
