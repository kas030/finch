use crate::gpu_tiles::{
    Allocation, Error, PixelBudget, SHARED_PIXEL_BYTES, TileAllocation, texture_bytes,
};
use finch_core::{PageSize, Point, ReadingAnchor, ReadingState, Zoom};
use finch_pdf::{DocumentInfo, Link, PageText, SearchHit, TILE_SIZE, TileBitmap, TileKey};
use iced::advanced::image::Id;
use iced::widget::image::Handle;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

// All admitted source RGBA owners share 35 MiB. The actor, bridge and
// acknowledged UI stream reserve up to 12 MiB more, including the current event.
// Renderer staging, software conversion buffers and framebuffers are separate.
pub const TILE_BYTES: usize = SHARED_PIXEL_BYTES;
const TEXT_BYTES: usize = 16 * 1024 * 1024;
const LINK_BYTES: usize = 2 * 1024 * 1024;
const CONTENT_PAGE_LIMIT: usize = 32;
// Leave room for prefetch, thumbnails and retired frame/upload owners. Lower
// raster resolution, never visible coverage, when an unusually large viewport
// would otherwise make the pinned working set larger than the cache itself.
const VISIBLE_TEXTURE_BYTES: usize = 28 * 1024 * 1024;
const GAP: f64 = 24.0;
const MARGIN: f64 = 28.0;

#[derive(Debug, Clone, Copy)]
pub enum Input {
    Resize(PageSize),
    Scroll { x: f32, y: f32 },
    Zoom { delta: f32, at: Point },
    SelectStart(Point),
    SelectMove(Point),
    SelectEnd(Point),
    ScrollTo(f32),
}

pub struct CachedTile {
    pub handle: Handle,
    pub width: u32,
    pub height: u32,
    bytes: usize,
    texture_bytes: usize,
    allocation: TileAllocation,
}
impl CachedTile {
    pub fn drawable(&self, software: bool) -> bool {
        software || self.allocation.ready()
    }
}

/// One CPU owner per cached bitmap. The PDF actor is configured without a second cache.
pub struct TileCache {
    pub entries: HashMap<TileKey, CachedTile>,
    order: VecDeque<TileKey>,
    pub bytes: usize,
    /// Reserved square texture bytes, including tiles not uploaded yet.
    pub texture_bytes: usize,
    limit: usize,
    pixels: Arc<PixelBudget>,
    blocked_pixel_bytes: usize,
}
impl TileCache {
    pub fn new(limit: usize) -> Self {
        Self::with_pixel_budget(limit, PixelBudget::shared())
    }
    pub fn with_pixel_budget(limit: usize, pixels: Arc<PixelBudget>) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            texture_bytes: 0,
            limit,
            pixels,
            blocked_pixel_bytes: 0,
        }
    }
    /// On pressure, the app should defer tile requests until a new render frame
    /// releases retired handles. Do not spin or retain another result bitmap.
    pub fn blocked_on_pixels(&self) -> bool {
        self.blocked_pixel_bytes != 0
    }
    pub fn resume_after_frame(&mut self) {
        if self.blocked_pixel_bytes <= self.pixels.available() {
            self.blocked_pixel_bytes = 0;
        }
    }
    pub fn contains(&self, key: &TileKey) -> bool {
        self.entries.contains_key(key)
    }
    pub fn request_handle(&mut self, key: TileKey) -> Option<Handle> {
        let tile = self.entries.get_mut(&key)?;
        tile.allocation.request(&tile.handle)
    }
    pub fn finish_allocate(
        &mut self,
        key: TileKey,
        id: Id,
        result: Result<Allocation, Error>,
    ) -> bool {
        self.entries
            .get_mut(&key)
            .is_some_and(|tile| tile.allocation.finish(&tile.handle, id, result))
    }
    pub fn touch(&mut self, key: TileKey) {
        if self.entries.contains_key(&key) {
            self.order.retain(|k| *k != key);
            self.order.push_back(key);
        }
    }
    pub fn insert(&mut self, tile: TileBitmap, pinned: &HashSet<TileKey>) -> bool {
        let bytes = tile.rgba.capacity();
        let Some(texture_bytes) = texture_bytes(tile.width, tile.height) else {
            return false;
        };
        let expected = (tile.width as usize)
            .checked_mul(tile.height as usize)
            .and_then(|pixels| pixels.checked_mul(4));
        if tile.width == 0
            || tile.height == 0
            || expected != Some(tile.rgba.len())
            || bytes > self.limit
            || texture_bytes > self.limit
        {
            return false;
        }
        if self.entries.contains_key(&tile.key) {
            return true;
        }
        if self.blocked_on_pixels() {
            return false;
        }
        let initial_shortfall = bytes.saturating_sub(self.pixels.available());
        let mut retired = 0usize;
        while self.bytes + bytes > self.limit || self.texture_bytes + texture_bytes > self.limit {
            let Some(evicted) = self.evict_one(pinned) else {
                break;
            };
            retired += evicted;
        }
        // Drop enough LRU owners to make room. A previous frame may still hold
        // those same buffers; keep their charge and retry only after a frame.
        let mut shortfall = initial_shortfall
            .saturating_sub(retired)
            .min(bytes.saturating_sub(self.pixels.available()));
        while shortfall > 0 {
            let Some(evicted) = self.evict_one(pinned) else {
                break;
            };
            shortfall = shortfall.saturating_sub(evicted);
        }
        let Some(pixels) = self.pixels.wrap(tile.rgba) else {
            self.blocked_pixel_bytes = bytes;
            return false;
        };
        self.blocked_pixel_bytes = 0;
        self.entries.insert(
            tile.key,
            CachedTile {
                handle: Handle::from_rgba(tile.width, tile.height, pixels),
                width: tile.width,
                height: tile.height,
                bytes,
                texture_bytes,
                allocation: TileAllocation::default(),
            },
        );
        self.order.push_back(tile.key);
        self.bytes += bytes;
        self.texture_bytes += texture_bytes;
        true
    }
    fn evict_one(&mut self, pinned: &HashSet<TileKey>) -> Option<usize> {
        let candidate = self
            .order
            .iter()
            .position(|key| !pinned.contains(key))
            .unwrap_or(0);
        let key = self.order.remove(candidate)?;
        let old = self.entries.remove(&key)?;
        self.bytes -= old.bytes;
        self.texture_bytes -= old.texture_bytes;
        Some(old.bytes)
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
        self.texture_bytes = 0;
        self.blocked_pixel_bytes = 0;
    }
}

pub struct Reader {
    pub info: DocumentInfo,
    pub document_id: String,
    pub zoom: Zoom,
    pub viewport: PageSize,
    pub dpi: f32,
    pub software: bool,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub sizes: Vec<PageSize>,
    pub known_sizes: HashSet<usize>,
    fit_page: usize,
    pending_restore: Option<(usize, ReadingAnchor)>,
    viewport_ready: bool,
    prefix: Vec<f64>,
    content_width: f64,
    pub tiles: TileCache,
    pub text: HashMap<usize, PageText>,
    pub links: HashMap<usize, Vec<Link>>,
    content_generation: Option<u64>,
    text_attempted: HashSet<usize>,
    links_attempted: HashSet<usize>,
    prefetch_attempted: HashSet<TileKey>,
    pub matches: HashMap<usize, Vec<SearchHit>>,
    pub selection: Option<((usize, usize), (usize, usize))>,
    pub selection_dragging: bool,
    pub selection_start: Option<Point>,
}

impl Reader {
    pub fn new(info: DocumentInfo, document_id: String, restore: Option<&ReadingState>) -> Self {
        let sizes = vec![info.first_page; info.page_count];
        let restore =
            restore.map(|state| (state.page.min(info.page_count.saturating_sub(1)), state));
        let mut reader = Self {
            info,
            document_id,
            sizes,
            known_sizes: HashSet::from([0]),
            zoom: restore.map_or(Zoom::FitWidth, |(_, state)| state.zoom),
            fit_page: restore.map_or(0, |(page, _)| page),
            pending_restore: restore.map(|(page, state)| (page, state.anchor.sanitized())),
            viewport_ready: false,
            viewport: PageSize {
                width: 900.0,
                height: 700.0,
            },
            dpi: 1.0,
            software: false,
            scroll_x: 0.0,
            scroll_y: 0.0,
            prefix: Vec::new(),
            content_width: 0.0,
            tiles: TileCache::new(TILE_BYTES),
            text: HashMap::new(),
            links: HashMap::new(),
            content_generation: None,
            text_attempted: HashSet::new(),
            links_attempted: HashSet::new(),
            prefetch_attempted: HashSet::new(),
            matches: HashMap::new(),
            selection: None,
            selection_dragging: false,
            selection_start: None,
        };
        reader.rebuild();
        if let Some((page, state)) = restore {
            reader.apply_anchor(page, state.anchor);
        }
        reader
    }
    pub fn scale(&self) -> f32 {
        self.zoom.scale(
            self.sizes
                .get(self.fit_page)
                .copied()
                .unwrap_or(self.info.first_page),
            PageSize {
                width: (self.viewport.width - MARGIN as f32 * 2.0 - 12.0).max(80.0),
                height: (self.viewport.height - MARGIN as f32 * 2.0).max(80.0),
            },
        )
    }
    pub fn scale_milli(&self) -> u32 {
        let dpi = if self.dpi.is_finite() && self.dpi > 0.0 {
            self.dpi
        } else {
            1.0
        };
        let requested = (self.scale() * dpi * 1000.0).round().clamp(10.0, 64_000.0) as u32;
        if self.visible_texture_bytes(requested) <= VISIBLE_TEXTURE_BYTES {
            return requested;
        }
        let mut low = 10;
        let mut high = requested;
        // Tile alignment makes the cost a staircase. Every chosen lower bound
        // is checked, so the result remains feasible even at alignment changes.
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if self.visible_texture_bytes(middle) <= VISIBLE_TEXTURE_BYTES {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        low
    }
    pub fn total_height(&self) -> f64 {
        self.prefix.last().copied().unwrap_or(0.0) + MARGIN
    }
    pub fn max_scroll_y(&self) -> f64 {
        (self.total_height() - self.viewport.height as f64).max(0.0)
    }
    fn content_width(&self) -> f64 {
        self.content_width
    }
    pub fn page_origin(&self, page: usize) -> (f64, f64) {
        let width = self.sizes[page].width as f64 * self.scale() as f64;
        ((self.content_width() - width) / 2.0, self.prefix[page])
    }
    pub fn page_at_y(&self, y: f64) -> usize {
        self.prefix
            .partition_point(|top| *top <= y)
            .saturating_sub(1)
            .min(self.sizes.len().saturating_sub(1))
    }
    pub fn current_page(&self) -> usize {
        if let Some((page, _)) = self.pending_restore {
            return page;
        }
        self.page_at_y(self.scroll_y + self.viewport.height as f64 * 0.25)
    }
    pub fn visible_pages(&self, margin: f64) -> std::ops::RangeInclusive<usize> {
        let start = self.page_at_y((self.scroll_y - margin).max(0.0));
        let end = self.page_at_y(self.scroll_y + self.viewport.height as f64 + margin);
        start..=end
    }
    fn rebuild(&mut self) {
        let scale = self.scale() as f64;
        self.prefix.clear();
        let mut top = MARGIN;
        let mut width = (self.viewport.width as f64 - MARGIN * 2.0).max(0.0);
        for size in &self.sizes {
            self.prefix.push(top);
            top += size.height as f64 * scale + GAP;
            width = width.max(size.width as f64 * scale);
        }
        self.content_width = width + MARGIN * 2.0;
        self.prefix.push(top - GAP);
        self.clamp_scroll();
    }
    fn clamp_scroll(&mut self) {
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll_y());
        self.scroll_x = self.scroll_x.clamp(
            0.0,
            (self.content_width() - self.viewport.width as f64).max(0.0),
        );
    }
    pub fn capture_at(&self, at: Point) -> (usize, ReadingAnchor) {
        let page = self.page_at_y(self.scroll_y + at.y as f64);
        let (x, y) = self.page_origin(page);
        let size = self.sizes[page];
        let scale = self.scale() as f64;
        let px = ((self.scroll_x + at.x as f64 - x) / (size.width as f64 * scale)).clamp(0.0, 1.0);
        let py = ((self.scroll_y + at.y as f64 - y) / (size.height as f64 * scale)).clamp(0.0, 1.0);
        (
            page,
            ReadingAnchor {
                page_x: px as f32,
                page_y: py as f32,
                viewport_x: ((x + px * size.width as f64 * scale - self.scroll_x)
                    / self.viewport.width as f64)
                    .clamp(0.0, 1.0) as f32,
                viewport_y: ((y + py * size.height as f64 * scale - self.scroll_y)
                    / self.viewport.height as f64)
                    .clamp(0.0, 1.0) as f32,
            },
        )
    }
    pub fn state(&self) -> ReadingState {
        let (page, anchor) = self.pending_restore.unwrap_or_else(|| {
            self.capture_at(Point {
                x: self.viewport.width / 2.0,
                y: self.viewport.height * 0.25,
            })
        });
        ReadingState {
            document_id: self.document_id.clone(),
            path: self.info.path.clone(),
            revision: self.info.revision,
            page,
            zoom: self.zoom,
            anchor,
        }
    }
    pub fn restore_anchor(&mut self, page: usize, anchor: ReadingAnchor) {
        self.pending_restore = None;
        self.update_fit_page(page);
        self.apply_anchor(page, anchor);
    }
    fn apply_anchor(&mut self, page: usize, anchor: ReadingAnchor) {
        if self.sizes.is_empty() {
            return;
        }
        let page = page.min(self.sizes.len() - 1);
        let (x, y) = self.page_origin(page);
        let size = self.sizes[page];
        let anchor = anchor.sanitized();
        self.scroll_x = x + anchor.page_x as f64 * size.width as f64 * self.scale() as f64
            - anchor.viewport_x as f64 * self.viewport.width as f64;
        self.scroll_y = y + anchor.page_y as f64 * size.height as f64 * self.scale() as f64
            - anchor.viewport_y as f64 * self.viewport.height as f64;
        self.clamp_scroll();
    }
    pub fn resize(&mut self, size: PageSize) {
        let size = size.sanitized();
        if self.viewport == size && self.viewport_ready {
            return;
        }
        let state = self.state();
        self.viewport_ready = true;
        self.viewport = size;
        self.rebuild();
        self.apply_anchor(state.page, state.anchor);
        self.finish_restore();
    }
    pub fn has_viewport(&self) -> bool {
        self.viewport_ready
    }
    pub fn set_page_size(&mut self, page: usize, size: PageSize) {
        if page >= self.sizes.len() {
            return;
        }
        let size = size.sanitized();
        self.known_sizes.insert(page);
        // Confirming an accurate estimate does not change the layout.
        if self.sizes[page] == size {
            self.finish_restore();
            return;
        }
        let state = self.state();
        self.sizes[page] = size;
        self.rebuild();
        self.apply_anchor(state.page, state.anchor);
        self.finish_restore();
    }
    pub fn set_zoom(&mut self, zoom: Zoom, at: Option<Point>) {
        self.pending_restore = None;
        let (page, anchor) = self.capture_at(at.unwrap_or(Point {
            x: self.viewport.width / 2.0,
            y: self.viewport.height * 0.25,
        }));
        self.zoom = zoom;
        self.fit_page = page;
        self.rebuild();
        self.apply_anchor(page, anchor);
    }
    pub fn scroll(&mut self, x: f32, y: f32) {
        self.pending_restore = None;
        if x.is_finite() {
            self.scroll_x += x as f64;
        }
        if y.is_finite() {
            self.scroll_y += y as f64;
        }
        self.clamp_scroll();
        self.refit_current_page();
    }
    pub fn scroll_to(&mut self, ratio: f32) {
        if !ratio.is_finite() {
            return;
        }
        self.pending_restore = None;
        self.scroll_y = self.max_scroll_y() * ratio.clamp(0.0, 1.0) as f64;
        self.refit_current_page();
    }
    pub fn jump(&mut self, page: usize) {
        self.pending_restore = None;
        let page = page.min(self.sizes.len().saturating_sub(1));
        self.update_fit_page(page);
        self.scroll_y = self.prefix[page] - MARGIN;
        if !matches!(self.zoom, Zoom::Scale(_)) {
            let (x, _) = self.page_origin(page);
            self.scroll_x = x + self.sizes[page].width as f64 * self.scale() as f64 / 2.0
                - self.viewport.width as f64 / 2.0;
        }
        self.clamp_scroll();
        self.selection = None;
    }
    fn finish_restore(&mut self) {
        if self.viewport_ready
            && self
                .pending_restore
                .is_some_and(|(page, _)| self.known_sizes.contains(&page))
        {
            self.pending_restore = None;
        }
    }
    fn update_fit_page(&mut self, page: usize) {
        let page = page.min(self.sizes.len().saturating_sub(1));
        if !matches!(self.zoom, Zoom::Scale(_)) && self.fit_page != page {
            self.fit_page = page;
            self.rebuild();
        }
    }
    fn refit_current_page(&mut self) {
        if matches!(self.zoom, Zoom::Scale(_)) {
            return;
        }
        let state = self.state();
        if self.fit_page != state.page {
            self.update_fit_page(state.page);
            self.apply_anchor(state.page, state.anchor);
        }
    }
    pub fn tile_keys(&self, margin: f64) -> Vec<TileKey> {
        let mut keys = Vec::new();
        let render_scale = self.scale_milli();
        for page in self.visible_pages(margin) {
            if !self.known_sizes.contains(&page) {
                continue;
            }
            let Some(span) = self.tile_span(page, render_scale, margin) else {
                continue;
            };
            for y in (span.top..span.bottom).step_by(TILE_SIZE as usize) {
                for x in (span.left..span.right).step_by(TILE_SIZE as usize) {
                    keys.push(TileKey {
                        page,
                        scale_milli: render_scale,
                        x,
                        y,
                    });
                }
            }
        }
        keys
    }
    fn tile_span(&self, page: usize, render_scale: u32, margin: f64) -> Option<TileSpan> {
        let ratio = render_scale as f64 / 1000.0 / self.scale() as f64;
        let (px, py) = self.page_origin(page);
        let key = TileKey {
            page,
            scale_milli: render_scale,
            x: 0,
            y: 0,
        };
        let (_, _, width, height) = key.dimensions(self.sizes[page]).ok()?;
        let left = ((self.scroll_x - px).max(0.0) * ratio) as u32 / TILE_SIZE * TILE_SIZE;
        let top = ((self.scroll_y - margin - py).max(0.0) * ratio) as u32 / TILE_SIZE * TILE_SIZE;
        let right = (((self.scroll_x + self.viewport.width as f64 - px).max(0.0) * ratio).ceil()
            as u32)
            .min(width);
        let bottom =
            (((self.scroll_y + self.viewport.height as f64 + margin - py).max(0.0) * ratio).ceil()
                as u32)
                .min(height);
        (left < right && top < bottom).then_some(TileSpan {
            left,
            top,
            right,
            bottom,
            width,
            height,
        })
    }
    fn visible_texture_bytes(&self, scale: u32) -> usize {
        self.visible_pages(0.0)
            .filter_map(|page| self.tile_span(page, scale, 0.0))
            .fold(0usize, |bytes, span| {
                bytes.saturating_add(span.texture_bytes())
            })
    }
    pub fn viewport_to_page(&self, at: Point) -> Option<(usize, Point)> {
        let page = self.page_at_y(self.scroll_y + at.y as f64);
        let (x, y) = self.page_origin(page);
        let scale = self.scale() as f64;
        let point = Point {
            x: ((at.x as f64 + self.scroll_x - x) / scale) as f32,
            y: ((at.y as f64 + self.scroll_y - y) / scale) as f32,
        };
        let size = self.sizes[page];
        (point.x >= 0.0 && point.y >= 0.0 && point.x <= size.width && point.y <= size.height)
            .then_some((page, point))
    }
    fn char_at(&self, at: Point) -> Option<(usize, usize)> {
        let (page, point) = self.viewport_to_page(at)?;
        self.nearest_char(page, point)
    }
    fn drag_char_at(&self, at: Point) -> Option<(usize, usize)> {
        if !at.x.is_finite() || !at.y.is_finite() {
            return None;
        }
        let world_y = self.scroll_y + at.y as f64;
        let mut page = self.page_at_y(world_y);
        if page + 1 < self.sizes.len() {
            let bottom = self.prefix[page] + self.sizes[page].height as f64 * self.scale() as f64;
            if world_y > (bottom + self.prefix[page + 1]) / 2.0 {
                page += 1;
            }
        }
        let (x, y) = self.page_origin(page);
        let scale = self.scale() as f64;
        let point = Point {
            x: ((at.x as f64 + self.scroll_x - x) / scale).clamp(0.0, self.sizes[page].width as f64)
                as f32,
            y: ((world_y - y) / scale).clamp(0.0, self.sizes[page].height as f64) as f32,
        };
        self.nearest_char(page, point)
    }
    fn nearest_char(&self, page: usize, point: Point) -> Option<(usize, usize)> {
        let text = self.text.get(&page)?;
        let ch = text.chars.iter().min_by(|a, b| {
            let distance = |ch: &finch_pdf::TextChar| {
                let rect = ch.bounds;
                let dx = (point.x - point.x.clamp(rect.x, rect.x + rect.width)).abs();
                let dy = (point.y - point.y.clamp(rect.y, rect.y + rect.height)).abs();
                dy * dy * 4.0 + dx * dx
            };
            distance(a).total_cmp(&distance(b))
        })?;
        Some((page, ch.index))
    }
    pub fn select_start(&mut self, at: Point) {
        self.selection_start = Some(at);
        self.selection_dragging = true;
        self.selection = self.char_at(at).map(|ch| (ch, ch));
    }
    pub fn select_move(&mut self, at: Point) {
        if self.selection_dragging
            && let (Some((first, _)), Some(last)) = (self.selection, self.drag_char_at(at))
        {
            self.selection = Some((first, last));
        }
    }
    pub fn selected_range(&self) -> Option<((usize, usize), (usize, usize))> {
        self.selection
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
    }
    pub fn selected_text(&self) -> String {
        let Some((first, last)) = self.selected_range() else {
            return String::new();
        };
        // Missing page text must never silently produce a partial quotation.
        if !(first.0..=last.0).all(|page| self.text.contains_key(&page)) {
            return String::new();
        }
        let mut out = String::new();
        for page in first.0..=last.0 {
            if let Some(text) = self.text.get(&page) {
                let start = if page == first.0 { first.1 } else { 0 };
                let end = if page == last.0 { last.1 } else { usize::MAX };
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text.selected_text(start, end));
            }
        }
        out
    }
    /// Visible candidates are bounded and current-page first. This also bounds
    /// attempt tracking on unusual documents containing many tiny pages.
    pub fn content_pages(&self) -> Vec<usize> {
        let visible = self.visible_pages(0.0);
        let first = *visible.start();
        let last = *visible.end();
        let center = self.current_page().clamp(first, last);
        let start = center.saturating_sub(CONTENT_PAGE_LIMIT / 2).max(first);
        let end = start.saturating_add(CONTENT_PAGE_LIMIT - 1).min(last);
        let start = end.saturating_sub(CONTENT_PAGE_LIMIT - 1).max(first);
        let mut pages: Vec<_> = (start..=end).collect();
        pages.sort_unstable_by_key(|page| (page.abs_diff(center), *page));
        pages
    }
    pub fn begin_content_view(&mut self, generation: u64) {
        if self.content_generation != Some(generation) {
            self.content_generation = Some(generation);
            self.text_attempted.clear();
            self.links_attempted.clear();
            self.prefetch_attempted.clear();
        }
        let pages = self.content_pages();
        self.text_attempted.retain(|page| pages.contains(page));
        self.links_attempted.retain(|page| pages.contains(page));
    }
    pub fn needs_text(&self, page: usize) -> bool {
        !self.text.contains_key(&page) && !self.text_attempted.contains(&page)
    }
    pub fn needs_links(&self, page: usize) -> bool {
        !self.links.contains_key(&page) && !self.links_attempted.contains(&page)
    }
    /// Only mark requests accepted by the bounded worker queue.
    pub fn mark_text_requested(&mut self, page: usize) {
        self.text_attempted.insert(page);
    }
    pub fn mark_links_requested(&mut self, page: usize) {
        self.links_attempted.insert(page);
    }
    pub fn begin_prefetch_view(&mut self, candidates: &[TileKey]) {
        self.prefetch_attempted
            .retain(|key| candidates.contains(key));
    }
    pub fn needs_prefetch(&self, key: &TileKey) -> bool {
        !self.prefetch_attempted.contains(key)
    }
    pub fn mark_prefetch_requested(&mut self, key: TileKey) {
        self.prefetch_attempted.insert(key);
    }
    pub fn trim_text(&mut self) {
        let page = self.current_page();
        let selection = self.selected_range();
        let visible = self.content_pages();
        self.text.retain(|index, _| {
            visible.contains(index)
                || index.abs_diff(page) <= 3
                || selection.is_some_and(|(a, b)| *index >= a.0 && *index <= b.0 && b.0 - a.0 <= 12)
        });
        self.links
            .retain(|index, _| visible.contains(index) || index.abs_diff(page) <= 3);
        let mut candidates: Vec<_> = self.text.keys().copied().collect();
        candidates.sort_unstable_by_key(|index| {
            (
                selection.is_none_or(|(a, b)| *index < a.0 || *index > b.0),
                index.abs_diff(page),
            )
        });
        let mut bytes = 0;
        for index in candidates {
            let text = &self.text[&index];
            let cost = text.text.capacity()
                + text.chars.capacity() * std::mem::size_of::<finch_pdf::TextChar>();
            if bytes + cost <= TEXT_BYTES {
                bytes += cost;
            } else {
                self.text.remove(&index);
                if selection.is_some_and(|(a, b)| index >= a.0 && index <= b.0) {
                    self.selection = None;
                }
            }
        }
        let mut candidates: Vec<_> = self.links.keys().copied().collect();
        candidates.sort_unstable_by_key(|index| index.abs_diff(page));
        let mut bytes = 0;
        for index in candidates {
            let links = &self.links[&index];
            let cost = links.capacity() * std::mem::size_of::<Link>()
                + links
                    .iter()
                    .map(|link| match &link.target {
                        finch_pdf::LinkTarget::Uri(uri) => uri.capacity(),
                        _ => 0,
                    })
                    .sum::<usize>();
            if bytes + cost <= LINK_BYTES {
                bytes += cost;
            } else {
                self.links.remove(&index);
            }
        }
    }
}

struct TileSpan {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    width: u32,
    height: u32,
}
impl TileSpan {
    /// All tiles except the page's bottom-right corner have at least one full
    /// side; Iced's dedicated texture is square even for narrow edge tiles.
    fn texture_bytes(&self) -> usize {
        let columns = (self.right - self.left).div_ceil(TILE_SIZE) as usize;
        let rows = (self.bottom - self.top).div_ceil(TILE_SIZE) as usize;
        let full = TILE_SIZE as usize * TILE_SIZE as usize * 4;
        let mut bytes = columns.saturating_mul(rows).saturating_mul(full);
        let last_x = (self.width - 1) / TILE_SIZE * TILE_SIZE;
        let last_y = (self.height - 1) / TILE_SIZE * TILE_SIZE;
        if (self.left..self.right).contains(&last_x) && (self.top..self.bottom).contains(&last_y) {
            let corner = (self.width - last_x).max(self.height - last_y) as usize;
            bytes = bytes.saturating_sub(full - corner * corner * 4);
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reader() -> Reader {
        Reader::new(
            DocumentInfo {
                path: "test.pdf".into(),
                page_count: 1500,
                first_page: PageSize::default(),
                revision: Default::default(),
                title: None,
            },
            "test".into(),
            None,
        )
    }
    #[test]
    fn restored_deep_page_stays_at_same_pixel() {
        let mut reader = reader();
        reader.jump(1250);
        reader.scroll(0.0, 271.75);
        let state = reader.state();
        let y = reader.scroll_y;
        reader.jump(0);
        reader.restore_anchor(state.page, state.anchor);
        assert!((reader.scroll_y - y).abs() < 0.1);
    }
    #[test]
    fn resolving_previous_page_geometry_preserves_anchor() {
        let mut reader = reader();
        reader.jump(1200);
        reader.scroll(0.0, 180.0);
        let before = reader.state();
        reader.set_page_size(
            5,
            PageSize {
                width: 900.0,
                height: 1300.0,
            },
        );
        let after = reader.state();
        assert_eq!(before.page, after.page);
        assert!((before.anchor.page_y - after.anchor.page_y).abs() < 0.0001);
    }
    #[test]
    fn unknown_target_geometry_does_not_erase_the_saved_anchor() {
        for geometry_first in [false, true] {
            let info = reader().info;
            let anchor = ReadingAnchor {
                page_x: 0.70,
                page_y: 0.67,
                viewport_x: 0.50,
                viewport_y: 0.25,
            };
            let state = ReadingState {
                document_id: "test".into(),
                path: info.path.clone(),
                revision: info.revision,
                page: 700,
                zoom: Zoom::Scale(2.0),
                anchor,
            };
            let mut restored = Reader::new(info, "test".into(), Some(&state));
            assert_eq!(
                restored.state().anchor,
                anchor,
                "estimated layout clamps must not rewrite persisted state"
            );
            let size = PageSize {
                width: 1600.0,
                height: 2500.0,
            };
            if geometry_first {
                restored.set_page_size(700, size);
            }
            restored.resize(PageSize {
                width: 1200.0,
                height: 740.0,
            });
            restored.set_page_size(
                5,
                PageSize {
                    width: 900.0,
                    height: 1400.0,
                },
            );
            if !geometry_first {
                restored.set_page_size(700, size);
            }
            assert!(restored.pending_restore.is_none());
            let (x, y) = restored.page_origin(700);
            let actual_x = x + anchor.page_x as f64 * size.width as f64 * restored.scale() as f64
                - restored.scroll_x;
            let actual_y = y + anchor.page_y as f64 * size.height as f64 * restored.scale() as f64
                - restored.scroll_y;
            assert!((actual_x - anchor.viewport_x as f64 * 1200.0).abs() < 0.1);
            assert!((actual_y - anchor.viewport_y as f64 * 740.0).abs() < 0.1);
        }
    }

    #[test]
    fn fit_modes_use_the_actual_target_page_and_follow_page_changes() {
        let mut reader = reader();
        reader.resize(PageSize {
            width: 900.0,
            height: 700.0,
        });
        reader.set_page_size(
            8,
            PageSize {
                width: 1400.0,
                height: 500.0,
            },
        );
        reader.set_page_size(
            9,
            PageSize {
                width: 500.0,
                height: 1800.0,
            },
        );
        reader.jump(8);
        reader.set_zoom(Zoom::FitPage, None);
        assert!((reader.scale() - 832.0 / 1400.0).abs() < 0.00001);
        reader.jump(9);
        assert!((reader.scale() - 644.0 / 1800.0).abs() < 0.00001);
        let (x, y) = reader.page_origin(9);
        assert!(x - reader.scroll_x >= -0.1);
        assert!(x - reader.scroll_x + 500.0 * reader.scale() as f64 <= 900.1);
        assert!((y - reader.scroll_y - MARGIN).abs() < 0.1);
        reader.set_zoom(Zoom::FitWidth, None);
        assert!((reader.scale() - 832.0 / 500.0).abs() < 0.00001);
        reader.scroll_to(0.0);
        assert_eq!(reader.current_page(), 0);
        assert!((reader.scale() - 832.0 / 612.0).abs() < 0.00001);
    }

    #[test]
    fn fit_page_restore_waits_for_target_dimensions_and_yields_to_navigation() {
        let info = reader().info;
        let state = ReadingState {
            document_id: "test".into(),
            path: info.path.clone(),
            revision: info.revision,
            page: 700,
            zoom: Zoom::FitPage,
            anchor: ReadingAnchor {
                page_x: 0.5,
                page_y: 0.4,
                viewport_x: 0.5,
                viewport_y: 0.25,
            },
        };
        let mut restored = Reader::new(info.clone(), "test".into(), Some(&state));
        restored.resize(PageSize {
            width: 1200.0,
            height: 740.0,
        });
        restored.set_page_size(
            700,
            PageSize {
                width: 1600.0,
                height: 2500.0,
            },
        );
        assert!((restored.scale() - 684.0 / 2500.0).abs() < 0.00001);
        assert_eq!(restored.current_page(), 700);
        let (_, y) = restored.page_origin(700);
        let anchor_y =
            y + state.anchor.page_y as f64 * 2500.0 * restored.scale() as f64 - restored.scroll_y;
        assert!((anchor_y - 185.0).abs() < 0.1);
        let mut interrupted = Reader::new(info, "test".into(), Some(&state));
        interrupted.jump(10);
        interrupted.set_page_size(
            700,
            PageSize {
                width: 1600.0,
                height: 2500.0,
            },
        );
        interrupted.resize(PageSize {
            width: 1200.0,
            height: 740.0,
        });
        assert_eq!(interrupted.current_page(), 10);
        assert!(interrupted.pending_restore.is_none());
    }

    #[test]
    fn cross_page_selection_handles_reverse_drags_and_page_gaps() {
        let mut info = reader().info;
        info.page_count = 3;
        info.first_page = PageSize {
            width: 200.0,
            height: 200.0,
        };
        let mut reader = Reader::new(info, "selection".into(), None);
        reader.set_zoom(Zoom::Scale(1.0), None);
        reader.resize(PageSize {
            width: 400.0,
            height: 500.0,
        });
        reader.jump(0);
        for (page, chars) in [(0, ['甲', '乙']), (1, ['丙', '丁'])] {
            reader.text.insert(
                page,
                PageText {
                    page,
                    text: chars.into_iter().collect(),
                    chars: chars
                        .into_iter()
                        .enumerate()
                        .map(|(index, character)| finch_pdf::TextChar {
                            index,
                            character,
                            bounds: finch_core::Rect {
                                x: 20.0,
                                y: if index == 0 { 20.0 } else { 180.0 },
                                width: 10.0,
                                height: 10.0,
                            },
                        })
                        .collect(),
                },
            );
        }
        let point = |reader: &Reader, page, y| {
            let (x, top) = reader.page_origin(page);
            Point {
                x: (x - reader.scroll_x) as f32 + 25.0,
                y: (top - reader.scroll_y) as f32 + y,
            }
        };
        let first = point(&reader, 0, 185.0);
        let last = point(&reader, 1, 25.0);
        reader.select_start(first);
        reader.select_move(last);
        assert_eq!(reader.selected_text(), "乙\n丙");
        reader.select_start(last);
        reader.select_move(first);
        assert_eq!(reader.selected_text(), "乙\n丙");
        reader.select_start(first);
        reader.select_move(point(&reader, 0, 202.0));
        assert_eq!(reader.selected_text(), "乙");
        reader.select_move(point(&reader, 1, -2.0));
        assert_eq!(reader.selected_text(), "乙\n丙");
    }

    #[test]
    fn fractional_trackpad_scroll_is_not_rounded_on_deep_pages() {
        let mut reader = reader();
        reader.set_zoom(Zoom::Scale(1.0), None);
        reader.jump(1200);
        let initial = reader.scroll_y;
        for _ in 0..1000 {
            reader.scroll(0.0, 0.125);
        }
        assert!((reader.scroll_y - initial - 125.0).abs() < 0.000001);
        reader.scroll(f32::NAN, f32::INFINITY);
        assert!(reader.scroll_x.is_finite() && reader.scroll_y.is_finite());
    }
    #[test]
    fn visible_work_is_independent_of_document_length() {
        let reader = reader();
        assert!(reader.visible_pages(0.0).count() < 4);
        assert!(reader.tile_keys(0.0).len() < 20);
    }
    #[test]
    fn cache_rejects_oversized_tiles() {
        let mut cache = TileCache::new(8);
        cache.insert(
            TileBitmap {
                key: TileKey {
                    page: 0,
                    scale_milli: 1000,
                    x: 0,
                    y: 0,
                },
                width: 2,
                height: 2,
                rgba: std::sync::Arc::new(vec![0; 16]),
            },
            &HashSet::new(),
        );
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn edge_tiles_are_evicted_by_texture_size_not_only_pixel_bytes() {
        let mut cache = TileCache::new(2 * 1024 * 1024);
        let key = |x| TileKey {
            page: 0,
            scale_milli: 1000,
            x,
            y: 0,
        };
        for x in 0..3 {
            cache.insert(
                TileBitmap {
                    key: key(x),
                    width: 512,
                    height: 1,
                    rgba: std::sync::Arc::new(vec![0; 512 * 4]),
                },
                &HashSet::new(),
            );
        }
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.bytes, 2 * 512 * 4);
        assert_eq!(cache.texture_bytes, 2 * 1024 * 1024);
        assert!(!cache.contains(&key(0)));
        assert!(!cache.entries[&key(1)].drawable(false));
        assert!(cache.entries[&key(1)].drawable(true));
        let pending = cache.request_handle(key(1)).unwrap();
        assert!(cache.request_handle(key(1)).is_none());
        cache.clear();
        assert_eq!(cache.texture_bytes, 0);
        cache.insert(
            TileBitmap {
                key: key(1),
                width: 512,
                height: 1,
                rgba: std::sync::Arc::new(vec![0; 512 * 4]),
            },
            &HashSet::new(),
        );
        assert!(cache.request_handle(key(1)).is_some());
        assert!(!cache.finish_allocate(key(1), pending.id(), Err(Error::Empty)));
        // A stale completion leaves the replacement tile's upload pending.
        assert!(cache.request_handle(key(1)).is_none());
    }

    #[test]
    fn retired_frame_and_upload_owners_share_the_next_documents_pixel_budget() {
        let budget = PixelBudget::new(48);
        let tile = |x| TileBitmap {
            key: TileKey {
                page: 0,
                scale_milli: 1000,
                x,
                y: 0,
            },
            width: 2,
            height: 2,
            rgba: Arc::new(vec![0; 16]),
        };
        let mut previous = TileCache::with_pixel_budget(48, budget.clone());
        assert!(previous.insert(tile(0), &HashSet::new()));
        assert!(previous.insert(tile(1), &HashSet::new()));
        assert!(previous.insert(tile(2), &HashSet::new()));
        let old_frame: Vec<_> = previous
            .entries
            .values()
            .map(|tile| tile.handle.clone())
            .collect();
        let upload = previous.request_handle(tile(0).key).unwrap();
        drop(previous);
        assert_eq!(budget.used(), 48);
        let mut current = TileCache::with_pixel_budget(48, budget.clone());
        assert!(!current.insert(tile(0), &HashSet::new()));
        assert!(current.blocked_on_pixels());
        assert_eq!(current.bytes, 0);
        drop(old_frame);
        assert_eq!(budget.used(), 16, "the old upload still owns one tile");
        current.resume_after_frame();
        assert!(!current.blocked_on_pixels());
        assert!(current.insert(tile(0), &HashSet::new()));
        assert!(current.insert(tile(1), &HashSet::new()));
        assert_eq!(budget.used(), 48);
        drop(upload);
        assert!(current.insert(tile(2), &HashSet::new()));
        assert_eq!(current.bytes, 48);
        current.clear();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn memory_pressure_evicts_once_then_waits_for_the_frame_owner() {
        let budget = PixelBudget::new(48);
        let tile = |x| TileBitmap {
            key: TileKey {
                page: 0,
                scale_milli: 1000,
                x,
                y: 0,
            },
            width: 2,
            height: 2,
            rgba: Arc::new(vec![0; 16]),
        };
        let mut cache = TileCache::with_pixel_budget(48, budget.clone());
        for x in 0..3 {
            assert!(cache.insert(tile(x), &HashSet::new()));
        }
        let frame: Vec<_> = cache
            .entries
            .values()
            .map(|tile| tile.handle.clone())
            .collect();
        assert!(!cache.insert(tile(3), &HashSet::new()));
        assert_eq!(cache.entries.len(), 2);
        assert!(!cache.insert(tile(4), &HashSet::new()));
        assert_eq!(
            cache.entries.len(),
            2,
            "queued results must not drain the cache"
        );
        assert_eq!(budget.used(), 48);
        drop(frame);
        assert_eq!(budget.used(), 32);
        cache.resume_after_frame();
        assert!(cache.insert(tile(3), &HashSet::new()));
        assert_eq!(budget.used(), 48);
    }

    #[test]
    fn mixed_page_zoom_and_dpi_keep_the_same_reading_point() {
        let mut reader = reader();
        reader.set_page_size(
            700,
            PageSize {
                width: 840.0,
                height: 1190.0,
            },
        );
        reader.set_zoom(Zoom::Scale(1.375), None);
        reader.jump(700);
        reader.scroll(120.0, 500.0);
        let cursor = Point { x: 320.0, y: 240.0 };
        let (page, point) = reader.viewport_to_page(cursor).unwrap();
        reader.set_zoom(Zoom::Scale(2.4), Some(cursor));
        let (new_page, new_point) = reader.viewport_to_page(cursor).unwrap();
        assert_eq!(page, new_page);
        assert!((point.x - new_point.x).abs() < 0.01);
        assert!((point.y - new_point.y).abs() < 0.01);
        let scroll = (reader.scroll_x, reader.scroll_y);
        let state = reader.state();
        reader.dpi = 1.75;
        assert_eq!(reader.scale_milli(), 4200);
        assert_eq!((reader.scroll_x, reader.scroll_y), scroll);
        assert_eq!(reader.state(), state);
        reader.resize(PageSize {
            width: 1100.0,
            height: 900.0,
        });
        let restored = reader.state();
        assert_eq!(restored.page, state.page);
        assert!((restored.anchor.page_y - state.anchor.page_y).abs() < 0.00001);
        let (x, y) = reader.page_origin(state.page);
        let logical_y = y + state.anchor.page_y as f64
            * reader.sizes[state.page].height as f64
            * reader.scale() as f64
            - reader.scroll_y;
        let logical_x = x + state.anchor.page_x as f64
            * reader.sizes[state.page].width as f64
            * reader.scale() as f64
            - reader.scroll_x;
        assert!((logical_y - state.anchor.viewport_y as f64 * 900.0).abs() < 0.1);
        assert!((logical_x - state.anchor.viewport_x as f64 * 1100.0).abs() < 0.1);
    }

    #[test]
    fn fractional_dpi_never_requests_tiles_beyond_native_page_edges() {
        for dpi in [1.0, 1.25, 1.5, 1.75, 2.25] {
            for zoom in [0.334, 0.83, 1.367, 2.003] {
                let mut reader = reader();
                reader.resize(PageSize {
                    width: 2000.0,
                    height: 1000.0,
                });
                reader.set_zoom(Zoom::Scale(zoom), None);
                reader.dpi = dpi;
                let scale = reader.scale_milli() as f32 / 1000.0;
                reader.set_page_size(
                    0,
                    PageSize {
                        width: 512.0 / scale,
                        height: 513.0 / scale,
                    },
                );
                reader.jump(0);
                let keys = reader.tile_keys(0.0);
                assert!(!keys.is_empty());
                assert_eq!(
                    keys.len(),
                    keys.iter().copied().collect::<HashSet<_>>().len()
                );
                for key in keys {
                    assert!(
                        key.dimensions(reader.sizes[key.page]).is_ok(),
                        "invalid tile {key:?} at DPI {dpi}"
                    );
                }
            }
        }
    }

    #[test]
    fn bitmap_lru_respects_pins_and_owns_the_original_allocation() {
        let tile = |x| TileBitmap {
            key: TileKey {
                page: 0,
                scale_milli: 1000,
                x,
                y: 0,
            },
            width: 2,
            height: 2,
            rgba: std::sync::Arc::new(vec![x as u8; 16]),
        };
        let first = tile(0);
        let retained = first.rgba.clone();
        let pointer = retained.as_ptr();
        let mut cache = TileCache::new(48);
        cache.insert(first, &HashSet::new());
        match &cache.entries[&tile(0).key].handle {
            Handle::Rgba { pixels, .. } => assert_eq!(pixels.as_ptr(), pointer),
            _ => panic!("tile must remain raw RGBA"),
        }
        cache.insert(tile(1), &HashSet::new());
        cache.insert(tile(2), &HashSet::new());
        cache.touch(tile(0).key);
        cache.insert(tile(3), &HashSet::from([tile(1).key]));
        assert_eq!(cache.bytes, 48);
        assert!(cache.contains(&tile(1).key));
        assert!(!cache.contains(&tile(2).key));
        cache.insert(tile(3), &HashSet::new());
        assert_eq!(cache.bytes, 48);
        cache.clear();
        assert_eq!(cache.bytes, 0);
        assert_eq!(std::sync::Arc::strong_count(&retained), 1);
    }

    #[test]
    fn text_cache_has_a_byte_bound_and_never_copies_an_incomplete_selection() {
        let mut reader = reader();
        reader.jump(1);
        for page in 0..3 {
            reader.text.insert(
                page,
                PageText {
                    page,
                    text: String::with_capacity(6 * 1024 * 1024),
                    chars: Vec::new(),
                },
            );
        }
        reader.selection = Some(((0, 0), (2, 0)));
        reader.trim_text();
        let bytes: usize = reader
            .text
            .values()
            .map(|text| {
                text.text.capacity()
                    + text.chars.capacity() * std::mem::size_of::<finch_pdf::TextChar>()
            })
            .sum();
        assert!(bytes <= TEXT_BYTES);
        assert_eq!(reader.text.len(), 2);
        assert!(reader.text.contains_key(&1));
        assert!(reader.selection.is_none());
        reader.selection = Some(((0, 0), (2, 0)));
        assert!(reader.selected_text().is_empty());
    }

    #[test]
    fn low_zoom_visible_text_is_loaded_once_and_stays_selectable() {
        let mut reader = reader();
        reader.resize(PageSize {
            width: 900.0,
            height: 900.0,
        });
        reader.set_zoom(Zoom::Scale(0.05), None);
        reader.jump(0);
        let candidates = reader.content_pages();
        assert!(candidates.len() > 9);
        let mut text_requests = 0;
        let mut link_requests = 0;
        for _ in 0..100 {
            reader.begin_content_view(7);
            for page in reader.content_pages() {
                if reader.needs_text(page) {
                    reader.mark_text_requested(page);
                    text_requests += 1;
                    reader.text.insert(
                        page,
                        PageText {
                            page,
                            text: format!("page {page}"),
                            chars: Vec::new(),
                        },
                    );
                }
                if reader.needs_links(page) {
                    reader.mark_links_requested(page);
                    link_requests += 1;
                    reader.links.insert(page, Vec::new());
                }
            }
            reader.trim_text();
        }
        assert_eq!(text_requests, candidates.len());
        assert_eq!(link_requests, candidates.len());
        assert!(candidates.iter().all(|page| reader.text.contains_key(page)));
        assert!(
            candidates
                .iter()
                .all(|page| reader.links.contains_key(page))
        );
    }

    #[test]
    fn over_budget_text_and_links_are_not_reparsed_while_idle() {
        let mut reader = reader();
        reader.resize(PageSize {
            width: 900.0,
            height: 900.0,
        });
        reader.set_zoom(Zoom::Scale(0.5), None);
        reader.jump(0);
        reader.begin_content_view(8);
        assert!(reader.content_pages().contains(&1));
        for page in 0..2 {
            assert!(reader.needs_text(page));
            reader.mark_text_requested(page);
            reader.mark_links_requested(page);
            reader.text.insert(
                page,
                PageText {
                    page,
                    text: String::with_capacity(9 * 1024 * 1024),
                    chars: Vec::new(),
                },
            );
            reader.links.insert(
                page,
                vec![Link {
                    bounds: Default::default(),
                    target: finch_pdf::LinkTarget::Uri(String::with_capacity(1200 * 1024)),
                }],
            );
        }
        reader.trim_text();
        assert_eq!(reader.text.len(), 1);
        assert_eq!(reader.links.len(), 1);
        for _ in 0..100 {
            reader.begin_content_view(8);
            reader.trim_text();
            assert!(!(0..2).any(|page| reader.needs_text(page) || reader.needs_links(page)));
        }
        reader.begin_content_view(9);
        assert_eq!((0..2).filter(|page| reader.needs_text(*page)).count(), 1);
        assert_eq!((0..2).filter(|page| reader.needs_links(*page)).count(), 1);
    }

    #[test]
    fn content_and_prefetch_attempt_tracking_is_bounded_and_queue_failure_can_retry() {
        let mut reader = reader();
        reader.resize(PageSize {
            width: 900.0,
            height: 10_000.0,
        });
        reader.set_zoom(Zoom::Scale(0.05), None);
        reader.begin_content_view(1);
        assert_eq!(reader.content_pages().len(), CONTENT_PAGE_LIMIT);
        let unqueued = reader.current_page();
        assert!(reader.needs_text(unqueued));
        reader.begin_content_view(1);
        assert!(reader.needs_text(unqueued));
        for page in reader.content_pages() {
            reader.mark_text_requested(page);
            reader.mark_links_requested(page);
        }
        reader.jump(500);
        reader.begin_content_view(1);
        assert!(reader.text_attempted.is_empty());
        assert!(reader.links_attempted.is_empty());
        let key = TileKey {
            page: 500,
            scale_milli: 50,
            x: 0,
            y: 0,
        };
        reader.begin_prefetch_view(&[key]);
        assert!(reader.needs_prefetch(&key));
        reader.mark_prefetch_requested(key);
        for _ in 0..100 {
            // A completed prefetch that was evicted is not an idle retry.
            reader.begin_content_view(1);
            reader.begin_prefetch_view(&[key]);
            assert!(!reader.needs_prefetch(&key));
        }
        reader.begin_prefetch_view(&[]);
        assert!(reader.prefetch_attempted.is_empty());
        reader.mark_prefetch_requested(key);
        reader.begin_content_view(2);
        assert!(reader.needs_prefetch(&key));
    }

    #[test]
    fn ordinary_dpi_keeps_native_sampling_and_large_views_fit_without_missing_tiles() {
        let mut ordinary = reader();
        ordinary.dpi = 1.25;
        ordinary.set_zoom(Zoom::Scale(1.0), None);
        assert_eq!(ordinary.scale_milli(), 1250);

        for (width, height, dpi, zoom) in [
            (3840.0, 2160.0, 3.0, 2.0),
            (16_384.0, 16_384.0, 4.0, 1.0),
            (32_768.0, 8192.0, 3.0, 16.0),
            (8000.0, 32_768.0, 4.0, 0.05),
        ] {
            let mut reader = reader();
            reader.resize(PageSize { width, height });
            reader.set_zoom(Zoom::Scale(zoom), None);
            reader.dpi = dpi;
            reader.known_sizes.extend(0..reader.info.page_count);
            reader.jump(20);
            let scale = reader.scale_milli();
            let keys = reader.tile_keys(0.0);
            let cost: usize = keys
                .iter()
                .map(|key| {
                    let (w, h, _, _) = key.dimensions(reader.sizes[key.page]).unwrap();
                    texture_bytes(w, h).unwrap()
                })
                .sum();
            assert!(
                cost <= VISIBLE_TEXTURE_BYTES,
                "{width}x{height} DPI {dpi}: {cost} bytes"
            );
            assert_eq!(cost, reader.visible_texture_bytes(scale));
            assert!(!keys.is_empty());
            let set: HashSet<_> = keys.iter().copied().collect();
            // Verify every grid cell intersecting every page is covered. A
            // truncated key list would satisfy the byte check but fail here.
            for page in reader.visible_pages(0.0) {
                let Some(span) = reader.tile_span(page, scale, 0.0) else {
                    continue;
                };
                for y in (span.top..span.bottom).step_by(TILE_SIZE as usize) {
                    for x in (span.left..span.right).step_by(TILE_SIZE as usize) {
                        assert!(set.contains(&TileKey {
                            page,
                            scale_milli: scale,
                            x,
                            y
                        }));
                    }
                }
            }
        }
    }

    #[test]
    fn visible_square_texture_estimate_matches_fractional_edge_tiles() {
        for (width, height) in [
            (13.7, 47.9),
            (500.3, 2048.1),
            (2048.1, 500.3),
            (1024.3, 1536.7),
        ] {
            let mut reader = reader();
            reader.resize(PageSize {
                width: 1800.0,
                height: 1800.0,
            });
            reader.set_zoom(Zoom::Scale(1.0), None);
            reader.set_page_size(0, PageSize { width, height });
            reader.known_sizes.extend(reader.visible_pages(0.0));
            reader.dpi = 1.25;
            for scroll in [0.0, 173.7, 510.3] {
                reader.scroll(0.0, scroll);
                reader.known_sizes.extend(reader.visible_pages(0.0));
                let cost: usize = reader
                    .tile_keys(0.0)
                    .iter()
                    .map(|key| {
                        let (w, h, _, _) = key.dimensions(reader.sizes[key.page]).unwrap();
                        texture_bytes(w, h).unwrap()
                    })
                    .sum();
                assert_eq!(cost, reader.visible_texture_bytes(reader.scale_milli()));
            }
        }
    }

    #[test]
    fn hundred_thousand_pages_have_bounded_visible_work() {
        let mut info = reader().info;
        info.page_count = 100_000;
        let mut reader = Reader::new(info, "large".into(), None);
        reader.known_sizes.extend(49_998..50_003);
        reader.jump(50_000);
        assert!(reader.visible_pages(0.0).count() < 4);
        assert!(reader.tile_keys(0.0).len() < 20);
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(reader.page_origin(50_000));
            std::hint::black_box(reader.visible_pages(0.0));
        }
        println!(
            "100,000-page geometry: 10,000 visible-frame queries in {:?}",
            start.elapsed()
        );
        let before = reader.state();
        reader.set_page_size(10, PageSize::default());
        assert_eq!(reader.state(), before);
    }
}
