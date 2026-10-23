//! UI-independent identifiers, reading state and coordinate math.

use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::UNIX_EPOCH};

pub type DocumentId = String;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileRevision {
    pub len: u64,
    pub modified_ms: u64,
}

impl FileRevision {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified_ms: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| {
                    duration.as_millis().min(u64::MAX as u128) as u64
                }),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum Zoom {
    #[default]
    FitWidth,
    FitPage,
    /// Logical display pixels per PDF point (one point is 1/72 inch).
    Scale(f32),
}

impl Zoom {
    pub fn scale(self, page: PageSize, viewport: PageSize) -> f32 {
        let page = page.sanitized();
        let viewport = viewport.sanitized();
        let value = match self {
            Self::FitWidth => viewport.width / page.width,
            Self::FitPage => (viewport.width / page.width).min(viewport.height / page.height),
            Self::Scale(scale) => scale,
        };
        if value.is_finite() {
            value.clamp(0.05, 16.0)
        } else {
            1.0
        }
    }
}

/// A page point anchored to a point in the viewport; all values are normalized.
/// Coordinates use the top-left origin, including PDF-derived text and links.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReadingAnchor {
    pub page_x: f32,
    pub page_y: f32,
    pub viewport_x: f32,
    pub viewport_y: f32,
}

impl Default for ReadingAnchor {
    fn default() -> Self {
        Self {
            page_x: 0.5,
            page_y: 0.0,
            viewport_x: 0.5,
            viewport_y: 0.0,
        }
    }
}

impl ReadingAnchor {
    pub fn sanitized(self) -> Self {
        fn unit(value: f32) -> f32 {
            if value.is_finite() {
                value.clamp(0.0, 1.0)
            } else {
                0.0
            }
        }
        Self {
            page_x: unit(self.page_x),
            page_y: unit(self.page_y),
            viewport_x: unit(self.viewport_x),
            viewport_y: unit(self.viewport_y),
        }
    }

    /// Absolute scroll offset before clamping to the actual content bounds.
    pub fn scroll_offset(
        self,
        page_origin: Point,
        page_size: PageSize,
        viewport: PageSize,
        scale: f32,
    ) -> Point {
        let anchor = self.sanitized();
        Point {
            x: page_origin.x + anchor.page_x * page_size.width * scale
                - anchor.viewport_x * viewport.width,
            y: page_origin.y + anchor.page_y * page_size.height * scale
                - anchor.viewport_y * viewport.height,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReadingState {
    pub document_id: DocumentId,
    pub path: PathBuf,
    pub revision: FileRevision,
    pub page: usize,
    pub zoom: Zoom,
    pub anchor: ReadingAnchor,
}

impl ReadingState {
    /// Restoring after a document replacement must never index a missing page.
    pub fn clamp_page(&mut self, page_count: usize) {
        self.page = self.page.min(page_count.saturating_sub(1));
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

impl Default for PageSize {
    fn default() -> Self {
        Self {
            width: 612.0,
            height: 792.0,
        }
    }
}

impl PageSize {
    pub fn sanitized(self) -> Self {
        let default = Self::default();
        Self {
            width: if self.width.is_finite() && self.width > 0.0 {
                self.width
            } else {
                default.width
            },
            height: if self.height.is_finite() && self.height > 0.0 {
                self.height
            } else {
                default.height
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x <= self.x + self.width
            && point.y <= self.y + self.height
    }
    pub fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
    /// Convert PDF bottom-left coordinates into Finch's top-left coordinates.
    pub fn from_pdf(left: f32, bottom: f32, right: f32, top: f32, page_height: f32) -> Self {
        Self {
            x: left.min(right),
            y: page_height - top.max(bottom),
            width: (right - left).abs(),
            height: (top - bottom).abs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_anchor_survives_scale_and_viewport_changes() {
        let anchor = ReadingAnchor {
            page_x: 0.5,
            page_y: 0.75,
            viewport_x: 0.5,
            viewport_y: 0.25,
        };
        let page = PageSize {
            width: 600.0,
            height: 800.0,
        };
        for (scale, viewport) in [
            (
                1.0,
                PageSize {
                    width: 600.0,
                    height: 400.0,
                },
            ),
            (
                2.0,
                PageSize {
                    width: 900.0,
                    height: 700.0,
                },
            ),
        ] {
            let origin = Point { x: 12.0, y: 900.0 };
            let scroll = anchor.scroll_offset(origin, page, viewport, scale);
            assert!(
                (origin.y + page.height * scale * anchor.page_y
                    - scroll.y
                    - viewport.height * anchor.viewport_y)
                    .abs()
                    < 0.01
            );
            assert!(
                (origin.x + page.width * scale * anchor.page_x
                    - scroll.x
                    - viewport.width * anchor.viewport_x)
                    .abs()
                    < 0.01
            );
        }
    }

    #[test]
    fn unsafe_restore_values_are_sanitized() {
        assert_eq!(
            Zoom::Scale(f32::NAN).scale(PageSize::default(), PageSize::default()),
            1.0
        );
        assert_eq!(
            Zoom::Scale(-100.0).scale(PageSize::default(), PageSize::default()),
            0.05
        );
        let mut state = ReadingState {
            page: 500,
            ..Default::default()
        };
        state.clamp_page(3);
        assert_eq!(state.page, 2);
        state.clamp_page(0);
        assert_eq!(state.page, 0);
    }

    #[test]
    fn state_round_trips_unicode_paths_and_zoom() {
        let state = ReadingState {
            path: PathBuf::from("论文/阅读.pdf"),
            zoom: Zoom::Scale(1.25),
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(state, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn pdf_rect_has_top_left_origin() {
        assert_eq!(
            Rect::from_pdf(10.0, 100.0, 30.0, 120.0, 800.0),
            Rect {
                x: 10.0,
                y: 680.0,
                width: 20.0,
                height: 20.0
            }
        );
    }
}
