use crate::reader::{Input, Reader};
use finch_core::{PageSize, Point as PdfPoint};
use iced::advanced::Renderer as _;
use iced::advanced::image::Renderer as _;
use iced::{
    Color, Element, Event, Length, Point, Rectangle, Size, Theme,
    advanced::{
        Clipboard, Layout, Shell, Widget, image, layout, mouse, renderer,
        widget::{self, Tree},
    },
    keyboard,
};

pub struct Viewport<'a>(pub &'a Reader);
#[derive(Default)]
struct State {
    modifiers: keyboard::Modifiers,
    scrollbar_drag: Option<f32>,
    panning: Option<Point>,
    viewport_size: Option<Size>,
}

impl Widget<Input, Theme, iced::Renderer> for Viewport<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State::default())
    }
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }
    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let reader = self.0;
        let scale = reader.scale();
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                ..Default::default()
            },
            theme.extended_palette().background.strong.color,
        );
        renderer.with_layer(bounds, |renderer| {
            for page in reader.visible_pages(0.0) {
                let (px, py) = reader.page_origin(page);
                let rect = Rectangle {
                    x: bounds.x + (px - reader.scroll_x) as f32,
                    y: bounds.y + (py - reader.scroll_y) as f32,
                    width: reader.sizes[page].width * scale,
                    height: reader.sizes[page].height * scale,
                };
                let Some(clip) = rect.intersection(&bounds) else {
                    continue;
                };
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: clip,
                        ..Default::default()
                    },
                    Color::WHITE,
                );
                // Draw coarse fallbacks first, then sharper ones, then exact
                // scale tiles. HashMap order must never choose visible quality.
                let current_scale = reader.scale_milli();
                let mut tiles: Vec<_> = reader
                    .tiles
                    .entries
                    .iter()
                    .filter(|(key, tile)| key.page == page && tile.drawable(reader.software))
                    .collect();
                tiles.sort_unstable_by_key(|(key, _)| {
                    (
                        key.scale_milli == current_scale,
                        key.scale_milli,
                        key.y,
                        key.x,
                    )
                });
                for (key, tile) in tiles {
                    let ratio = scale / key.scale();
                    let target = Rectangle {
                        x: rect.x + key.x as f32 * ratio,
                        y: rect.y + key.y as f32 * ratio,
                        width: tile.width as f32 * ratio,
                        height: tile.height as f32 * ratio,
                    };
                    if target.intersects(&clip) {
                        renderer.draw_image(image::Image::new(tile.handle.clone()), target, clip);
                    }
                }
                if let Some(hits) = reader.matches.get(&page) {
                    for hit in hits {
                        for highlight in &hit.bounds {
                            fill_highlight(
                                renderer,
                                *highlight,
                                rect,
                                scale,
                                clip,
                                Color::from_rgba(1.0, 0.72, 0.12, 0.35),
                            );
                        }
                    }
                }
                if let (Some((first, last)), Some(text)) =
                    (reader.selected_range(), reader.text.get(&page))
                    && page >= first.0
                    && page <= last.0
                {
                    for ch in &text.chars {
                        if (page > first.0 || ch.index >= first.1)
                            && (page < last.0 || ch.index <= last.1)
                        {
                            fill_highlight(
                                renderer,
                                ch.bounds,
                                rect,
                                scale,
                                clip,
                                Color::from_rgba(0.18, 0.48, 0.90, 0.30),
                            );
                        }
                    }
                }
            }
            if reader.max_scroll_y() > 0.0 {
                let (top, thumb_height) = scrollbar(reader, bounds.height);
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x + bounds.width - 9.0,
                            y: bounds.y + top,
                            width: 5.0,
                            height: thumb_height,
                        },
                        border: iced::Border {
                            radius: 3.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    theme
                        .extended_palette()
                        .background
                        .strong
                        .text
                        .scale_alpha(0.28),
                );
            }
        });
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Input>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();
        if state.viewport_size != Some(bounds.size()) || !self.0.has_viewport() {
            state.viewport_size = Some(bounds.size());
            shell.publish(Input::Resize(PageSize {
                width: bounds.width,
                height: bounds.height,
            }));
        }
        let at = cursor.position().map(|p| PdfPoint {
            x: p.x - bounds.x,
            y: p.y - bounds.y,
        });
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let (x, y) = match delta {
                    mouse::ScrollDelta::Lines { x, y } => (*x * 48.0, *y * 48.0),
                    mouse::ScrollDelta::Pixels { x, y } => (*x, *y),
                };
                if state.modifiers.control() || state.modifiers.logo() {
                    shell.publish(Input::Zoom {
                        delta: y / 400.0,
                        at: at.unwrap_or_default(),
                    });
                } else if state.modifiers.shift() {
                    shell.publish(Input::Scroll { x: -y, y: -x });
                } else {
                    shell.publish(Input::Scroll { x: -x, y: -y });
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(bounds) =>
            {
                if let Some(at) = at {
                    if at.x > bounds.width - 14.0 {
                        let (top, height) = scrollbar(self.0, bounds.height);
                        let offset = if at.y >= top && at.y <= top + height {
                            at.y - top
                        } else {
                            height / 2.0
                        };
                        state.scrollbar_drag = Some(offset);
                        shell.publish(Input::ScrollTo(scrollbar_ratio(
                            at.y,
                            offset,
                            bounds.height,
                            height,
                        )));
                    } else {
                        shell.publish(Input::SelectStart(at));
                    }
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle))
                if cursor.is_over(bounds) =>
            {
                state.panning = cursor.position();
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if let Some(offset) = state.scrollbar_drag {
                    let (_, height) = scrollbar(self.0, bounds.height);
                    shell.publish(Input::ScrollTo(scrollbar_ratio(
                        position.y - bounds.y,
                        offset,
                        bounds.height,
                        height,
                    )));
                    shell.capture_event();
                } else if let Some(previous) = state.panning {
                    shell.publish(Input::Scroll {
                        x: previous.x - position.x,
                        y: previous.y - position.y,
                    });
                    state.panning = Some(*position);
                    shell.capture_event();
                } else if self.0.selection_dragging
                    && let Some(at) = at
                {
                    shell.publish(Input::SelectMove(at));
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.scrollbar_drag = None;
                if self.0.selection_dragging {
                    shell.publish(Input::SelectEnd(at.unwrap_or_default()));
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Middle)) => {
                state.panning = None;
            }
            Event::Window(iced::window::Event::Unfocused) => {
                state.modifiers = keyboard::Modifiers::default();
                state.panning = None;
                state.scrollbar_drag = None;
                if self.0.selection_dragging {
                    shell.publish(Input::SelectEnd(
                        at.or(self.0.selection_start).unwrap_or_default(),
                    ));
                }
            }
            _ => {}
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if !cursor.is_over(layout.bounds()) {
            return mouse::Interaction::None;
        }
        if tree.state.downcast_ref::<State>().panning.is_some() {
            return mouse::Interaction::Grabbing;
        }
        let bounds = layout.bounds();
        if let Some(p) = cursor.position()
            && let Some((page, point)) = self.0.viewport_to_page(PdfPoint {
                x: p.x - bounds.x,
                y: p.y - bounds.y,
            })
            && self
                .0
                .links
                .get(&page)
                .is_some_and(|links| links.iter().any(|link| link.bounds.contains(point)))
        {
            return mouse::Interaction::Pointer;
        }
        mouse::Interaction::Text
    }
}
fn scrollbar(reader: &Reader, height: f32) -> (f32, f32) {
    let thumb = (height as f64 / reader.total_height() * height as f64)
        .max(28.0)
        .min(height as f64) as f32;
    let top = if reader.max_scroll_y() > 0.0 {
        (reader.scroll_y / reader.max_scroll_y()) as f32 * (height - thumb)
    } else {
        0.0
    };
    (top, thumb)
}
fn scrollbar_ratio(y: f32, offset: f32, viewport: f32, thumb: f32) -> f32 {
    if viewport <= thumb {
        return 0.0;
    }
    ((y - offset) / (viewport - thumb)).clamp(0.0, 1.0)
}

fn fill_highlight(
    renderer: &mut iced::Renderer,
    rect: finch_core::Rect,
    page: Rectangle,
    scale: f32,
    clip: Rectangle,
    color: Color,
) {
    let target = Rectangle {
        x: page.x + rect.x * scale,
        y: page.y + rect.y * scale,
        width: rect.width * scale,
        height: rect.height * scale,
    };
    if let Some(bounds) = target.intersection(&clip) {
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                ..Default::default()
            },
            color,
        );
    }
}
impl<'a> From<Viewport<'a>> for Element<'a, Input> {
    fn from(widget: Viewport<'a>) -> Self {
        Self::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollbar_grab_preserves_the_thumb_position_and_reaches_both_ends() {
        let viewport = 700.0;
        let thumb = 120.0;
        let top = 250.0;
        let offset = 31.0;
        let ratio = scrollbar_ratio(top + offset, offset, viewport, thumb);
        assert!((ratio * (viewport - thumb) - top).abs() < 0.001);
        assert_eq!(scrollbar_ratio(offset, offset, viewport, thumb), 0.0);
        assert_eq!(
            scrollbar_ratio(viewport - thumb + offset, offset, viewport, thumb),
            1.0
        );
        assert_eq!(scrollbar_ratio(10.0, offset, viewport, viewport), 0.0);
    }
}
