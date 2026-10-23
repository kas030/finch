//! Explicit allocations keep PDF tiles out of Iced's growing shared atlas.
//!
//! The pinned Iced 0.14 asynchronous `image::allocate` creates one square texture
//! per image. Drawing an image before allocation uses the shared atlas instead.
//! Keep each allocation alive for exactly as long as its cached tile.

use iced::advanced::image::Id;
use iced::widget::image::Handle;
pub use iced_runtime::image::{Allocation, Error, allocate};
use std::{
    collections::HashSet,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

pub const MAX_UPLOADS: usize = 2;
pub const SHARED_PIXEL_BYTES: usize = 35 * 1024 * 1024;

/// Source RGBA allocations retained by every reader, frame and upload combined.
/// Renderer staging/framebuffers and tiny-skia's converted pixels are separate.
pub struct PixelBudget {
    limit: usize,
    used: AtomicUsize,
}

impl PixelBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    pub fn shared() -> Arc<Self> {
        static BUDGET: OnceLock<Arc<PixelBudget>> = OnceLock::new();
        BUDGET.get_or_init(|| Self::new(SHARED_PIXEL_BYTES)).clone()
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub fn available(&self) -> usize {
        self.limit.saturating_sub(self.used())
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<PixelReservation> {
        let mut used = self.used();
        loop {
            let total = used
                .checked_add(bytes)
                .filter(|total| *total <= self.limit)?;
            match self
                .used
                .compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(current) => used = current,
            }
        }
        Some(PixelReservation {
            budget: self.clone(),
            bytes,
        })
    }

    /// Retains the original Vec without copying and releases the charge only
    /// after the last clone of its Iced Bytes/Handle/Allocation has been dropped.
    pub fn wrap(
        self: &Arc<Self>,
        pixels: Arc<Vec<u8>>,
    ) -> Option<iced::advanced::graphics::core::Bytes> {
        let reservation = self.reserve(pixels.capacity())?;
        Some(iced::advanced::graphics::core::Bytes::from_owner(
            BudgetedPixels {
                pixels,
                _reservation: reservation,
            },
        ))
    }
}

struct PixelReservation {
    budget: Arc<PixelBudget>,
    bytes: usize,
}

impl Drop for PixelReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct BudgetedPixels {
    // Fields drop in declaration order: release our Vec before its charge.
    pixels: Arc<Vec<u8>>,
    _reservation: PixelReservation,
}

impl AsRef<[u8]> for BudgetedPixels {
    fn as_ref(&self) -> &[u8] {
        self.pixels.as_slice()
    }
}

/// The dedicated texture is square, including narrow page-edge tiles.
pub fn texture_bytes(width: u32, height: u32) -> Option<usize> {
    let side = usize::try_from(width.max(height)).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    side.checked_mul(side)?.checked_mul(4)
}

#[derive(Default)]
pub struct TileAllocation {
    allocation: Option<Allocation>,
    pending: bool,
    failed: bool,
}

impl TileAllocation {
    pub fn ready(&self) -> bool {
        self.allocation.is_some()
    }

    pub fn request(&mut self, handle: &Handle) -> Option<Handle> {
        if self.pending || self.failed || self.ready() {
            return None;
        }
        self.pending = true;
        Some(handle.clone())
    }

    /// Unique image IDs distinguish identical page/tile keys in different files.
    pub fn finish(&mut self, handle: &Handle, id: Id, result: Result<Allocation, Error>) -> bool {
        if handle.id() != id || !self.pending {
            return false;
        }
        self.pending = false;
        match result {
            Ok(allocation) if allocation.handle().id() == id => {
                self.allocation = Some(allocation);
                true
            }
            _ => {
                // Never retry a permanent allocation failure every frame.
                self.failed = true;
                false
            }
        }
    }
}

/// Bound both pending work and uploads between renderer cache-trim passes.
/// Keep one gate in the app, shared across document changes and thumbnails.
#[derive(Default)]
pub struct UploadGate {
    in_flight: HashSet<Id>,
    started_this_frame: usize,
}

impl UploadGate {
    /// Call once for each window `RedrawRequested`.
    pub fn on_frame(&mut self) {
        self.started_this_frame = 0;
    }

    pub fn can_start(&self) -> bool {
        self.in_flight.len() < MAX_UPLOADS && self.started_this_frame < MAX_UPLOADS
    }

    /// Call after `can_start`, immediately before creating the upload task.
    pub fn start(&mut self, handle: &Handle) -> bool {
        if !self.can_start() || !self.in_flight.insert(handle.id()) {
            return false;
        }
        self.started_this_frame += 1;
        true
    }

    /// Finish stale-document results too: their upload still consumed a slot.
    pub fn finish(&mut self, id: Id) {
        self.in_flight.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> Handle {
        Handle::from_rgba(1, 1, vec![0; 4])
    }

    #[test]
    fn narrow_tiles_charge_the_entire_square_texture() {
        assert_eq!(texture_bytes(512, 1), Some(1024 * 1024));
        assert_eq!(texture_bytes(144, 192), Some(192 * 192 * 4));
        assert_eq!(texture_bytes(0, 512), None);
        assert_eq!(texture_bytes(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn gate_limits_completed_bursts_and_pending_work_across_frames() {
        let mut gate = UploadGate::default();
        let first = handle();
        let second = handle();
        let third = handle();
        assert!(gate.start(&first));
        assert!(!gate.start(&first));
        assert!(gate.start(&second));
        gate.finish(first.id());
        gate.finish(second.id());
        assert!(
            !gate.can_start(),
            "completion must not bypass a frame budget"
        );
        gate.on_frame();
        assert!(gate.start(&first));
        assert!(gate.start(&second));
        gate.on_frame();
        assert!(
            !gate.start(&third),
            "frames must not bypass pending uploads"
        );
        gate.finish(first.id());
        assert!(gate.start(&third));
    }

    #[test]
    fn stale_results_cannot_complete_a_reused_tile_key() {
        let old = handle();
        let current = handle();
        let mut state = TileAllocation::default();
        assert!(state.request(&current).is_some());
        assert!(!state.finish(&current, old.id(), Err(Error::Empty)));
        assert!(state.pending);
        assert!(!state.finish(&current, current.id(), Err(Error::OutOfMemory)));
        assert!(!state.ready());
        assert!(state.request(&current).is_none());
    }

    #[test]
    fn pixel_charge_follows_the_last_owner_without_copying() {
        let budget = PixelBudget::new(32);
        let mut pixels = Vec::with_capacity(32);
        pixels.resize(16, 7);
        let original = Arc::new(pixels);
        let pointer = original.as_ptr();
        let bytes = budget.wrap(original).unwrap();
        assert_eq!(bytes.as_ptr(), pointer);
        assert_eq!(budget.used(), 32, "reserve capacity, not length");
        let handle = Handle::from_rgba(2, 2, bytes.clone());
        let upload = handle.clone();
        drop(bytes);
        drop(handle);
        assert_eq!(budget.used(), 32, "an upload retains the same allocation");
        assert!(budget.wrap(Arc::new(vec![0; 4])).is_none());
        drop(upload);
        assert_eq!(budget.used(), 0);
        assert!(budget.wrap(Arc::new(vec![0; 32])).is_some());
    }

    #[test]
    fn concurrent_reservations_never_oversubscribe_the_shared_limit() {
        let budget = PixelBudget::new(32);
        let barrier = Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let budget = budget.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..1000 {
                        let reservation = budget.reserve(8);
                        assert!(budget.used() <= 32);
                        std::thread::yield_now();
                        drop(reservation);
                    }
                });
            }
        });
        assert_eq!(budget.used(), 0);
        assert!(budget.reserve(usize::MAX).is_none());
    }
}
