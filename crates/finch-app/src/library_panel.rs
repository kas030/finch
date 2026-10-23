//! Library sidebar: presentation state only, with bounded visible row creation.
//! Filesystem and database operations are emitted as actions for the host.

use finch_store::{LibraryItem, LibraryQuery, LibrarySort};
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input, tooltip};
use iced::{Element, Length, Theme};

pub const SCROLL_ID: &str = "finch-library-scroll";
const ROW_HEIGHT: f32 = 104.0;
const OVERSCAN_ROWS: usize = 3;
const PAGE_SIZE: usize = 200;

#[derive(Debug, Clone)]
pub enum Action {
    SearchChanged(String),
    TagFilterChanged(String),
    ToggleFavorites,
    SortChanged(LibrarySort),
    Import,
    Open(String),
    ToggleFavorite(String, bool),
    Relink(String),
    Remove(String),
    EditTags(String),
    TagsChanged(String),
    SaveTags(String, Vec<String>),
    CancelTags,
    LoadMore,
    Scrolled { y: f32, height: f32 },
}

pub struct LibraryPanel {
    pub items: Vec<LibraryItem>,
    pub query: String,
    pub favorites_only: bool,
    pub sort: LibrarySort,
    pub selected: Option<String>,
    pub tag_filter: String,
    pub editing_tags: Option<(String, String)>,
    pub has_more: bool,
    pub loading: bool,
    pub error: Option<String>,
    scroll_y: f32,
    viewport_height: f32,
}

impl Default for LibraryPanel {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            query: String::new(),
            favorites_only: false,
            sort: LibrarySort::RecentlyRead,
            selected: None,
            tag_filter: String::new(),
            editing_tags: None,
            has_more: false,
            loading: false,
            error: None,
            scroll_y: 0.0,
            viewport_height: 900.0,
        }
    }
}

impl LibraryPanel {
    /// Apply presentation state before dispatching the corresponding host command.
    /// Query changes also require resetting the scrollable with SCROLL_ID.
    pub fn apply_local(&mut self, action: &Action) {
        match action {
            Action::SearchChanged(value) => {
                self.query.clone_from(value);
                self.reset_scroll();
            }
            Action::TagFilterChanged(value) => {
                self.tag_filter.clone_from(value);
                self.reset_scroll();
            }
            Action::ToggleFavorites => {
                self.favorites_only = !self.favorites_only;
                self.reset_scroll();
            }
            Action::SortChanged(sort) => {
                self.sort = *sort;
                self.reset_scroll();
            }
            Action::Open(id) => self.selected = Some(id.clone()),
            Action::EditTags(id) => {
                if let Some(item) = self.items.iter().find(|item| item.id == *id) {
                    self.editing_tags = Some((id.clone(), item.tags.join(", ")));
                }
            }
            Action::TagsChanged(value) => {
                if let Some((_, tags)) = &mut self.editing_tags {
                    tags.clone_from(value);
                }
            }
            Action::SaveTags(_, _) | Action::CancelTags => self.editing_tags = None,
            Action::Scrolled { y, height } => {
                self.scroll_y = y.max(0.0);
                self.viewport_height = height.max(1.0);
            }
            _ => {}
        }
    }

    pub fn query(&self) -> LibraryQuery {
        LibraryQuery {
            text: self.query.clone(),
            tag: if self.tag_filter.trim().is_empty() {
                None
            } else {
                Some(self.tag_filter.trim().into())
            },
            favorites_only: self.favorites_only,
            sort: self.sort,
            limit: PAGE_SIZE,
            offset: 0,
        }
    }

    pub fn next_query(&self) -> LibraryQuery {
        LibraryQuery {
            offset: self.items.len(),
            ..self.query()
        }
    }

    /// `append` corresponds to a LoadMore request. The host should discard stale
    /// responses when a later search/filter request has superseded the query.
    pub fn set_results(&mut self, items: Vec<LibraryItem>, append: bool) {
        self.has_more = items.len() == PAGE_SIZE;
        self.loading = false;
        self.error = None;
        if append {
            self.items.extend(items);
        } else {
            self.items = items;
        }
        self.scroll_y = self
            .scroll_y
            .min((self.items.len() as f32 * ROW_HEIGHT - self.viewport_height).max(0.0));
    }

    fn reset_scroll(&mut self) {
        self.scroll_y = 0.0;
        self.editing_tags = None;
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let first = (self.scroll_y / ROW_HEIGHT) as usize;
        let start = first.saturating_sub(OVERSCAN_ROWS).min(self.items.len());
        let end = (first + (self.viewport_height / ROW_HEIGHT).ceil() as usize + OVERSCAN_ROWS + 1)
            .min(self.items.len());
        start..end.max(start)
    }
}

pub fn view(panel: &LibraryPanel) -> Element<'_, Action> {
    let heading = row![
        column![text("文献库").size(22), text("安静阅读，专注发现").size(11)].spacing(3),
        Space::new().width(Length::Fill),
        button(text("＋ 导入").size(12))
            .padding([7, 10])
            .on_press(Action::Import),
    ]
    .align_y(iced::Alignment::Center);
    let search = text_input("搜索标题、文件名、标签", &panel.query)
        .on_input(Action::SearchChanged)
        .padding(9)
        .size(13);
    let filters = row![
        button(
            text(if panel.favorites_only {
                "★ 已收藏"
            } else {
                "☆ 收藏"
            })
            .size(11)
        )
        .on_press(Action::ToggleFavorites)
        .style(move |theme, status| if panel.favorites_only {
            button::primary(theme, status)
        } else {
            button::secondary(theme, status)
        }),
        text_input("筛选标签", &panel.tag_filter)
            .on_input(Action::TagFilterChanged)
            .padding(6)
            .size(11),
    ]
    .spacing(7);
    let mut sort_controls = row![].spacing(3);
    for (sort, label) in [
        (LibrarySort::RecentlyRead, "最近阅读"),
        (LibrarySort::RecentlyAdded, "最近添加"),
        (LibrarySort::Title, "标题"),
    ] {
        sort_controls = sort_controls.push(
            button(text(label).size(11))
                .padding([5, 7])
                .on_press(Action::SortChanged(sort))
                .style(move |theme, status| {
                    if panel.sort == sort {
                        button::secondary(theme, status)
                    } else {
                        button::text(theme, status)
                    }
                }),
        );
    }
    let mut content = column![heading, search, filters, sort_controls].spacing(10);
    if let Some(message) = &panel.error {
        content = content.push(
            text(message)
                .size(12)
                .color(iced::Color::from_rgb8(192, 79, 65)),
        );
    }
    if let Some((id, value)) = &panel.editing_tags {
        let tags = parse_tags(value);
        content = content.push(
            container(
                column![
                    text("编辑标签 · 用逗号分隔").size(12),
                    text_input("例如：机器学习, 待读", value)
                        .on_input(Action::TagsChanged)
                        .on_submit(Action::SaveTags(id.clone(), tags.clone()))
                        .padding(7)
                        .size(12),
                    row![
                        button(text("保存").size(12)).on_press(Action::SaveTags(id.clone(), tags)),
                        button(text("取消").size(12))
                            .on_press(Action::CancelTags)
                            .style(button::text)
                    ]
                    .spacing(7),
                ]
                .spacing(7),
            )
            .padding(10)
            .width(Length::Fill)
            .style(container::rounded_box),
        );
    }
    if panel.items.is_empty() {
        let message = if panel.loading {
            "正在读取文献库…"
        } else if panel.query.is_empty() && panel.tag_filter.is_empty() && !panel.favorites_only {
            "将 PDF 拖入窗口\n或点击“导入”开始阅读\n\n文件保留在原来的位置"
        } else {
            "没有符合条件的文献\n试试其他关键词或标签"
        };
        content = content.push(
            container(text(message).size(13))
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .padding(18),
        );
    } else {
        let range = panel.visible_range();
        let mut list =
            column![Space::new().height(range.start as f32 * ROW_HEIGHT)].width(Length::Fill);
        for item in &panel.items[range.clone()] {
            list = list.push(library_row(
                item,
                panel.selected.as_deref() == Some(item.id.as_str()),
            ));
        }
        list = list.push(Space::new().height((panel.items.len() - range.end) as f32 * ROW_HEIGHT));
        if panel.has_more {
            let more = button(
                text(if panel.loading {
                    "加载中…"
                } else {
                    "加载更多"
                })
                .size(12),
            )
            .width(Length::Fill)
            .style(button::text);
            list = list.push(if panel.loading {
                more
            } else {
                more.on_press(Action::LoadMore)
            });
        }
        content = content.push(
            scrollable(list)
                .id(SCROLL_ID)
                .height(Length::Fill)
                .on_scroll(|viewport| Action::Scrolled {
                    y: viewport.absolute_offset().y,
                    height: viewport.bounds().height,
                }),
        );
    }
    content = content.push(
        text(if panel.loading {
            "正在更新…".into()
        } else {
            format!(
                "{} 篇文献{}",
                panel.items.len(),
                if panel.has_more {
                    " · 可继续加载"
                } else {
                    ""
                }
            )
        })
        .size(11),
    );
    container(content)
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn library_row(item: &LibraryItem, selected: bool) -> Element<'_, Action> {
    let title = truncate(&item.title, 32);
    let file_name = truncate(&item.file_name, 40);
    let open = button(
        column![
            text(title).size(13).wrapping(text::Wrapping::None),
            text(file_name).size(10).wrapping(text::Wrapping::None),
        ]
        .spacing(2),
    )
    .width(Length::Fill)
    .padding(0)
    .style(button::text)
    .on_press(if item.missing {
        Action::Relink(item.id.clone())
    } else {
        Action::Open(item.id.clone())
    });
    let favorite = tooltip(
        button(text(if item.favorite { "★" } else { "☆" }).size(15))
            .padding([2, 7])
            .style(button::text)
            .on_press(Action::ToggleFavorite(item.id.clone(), !item.favorite)),
        text("收藏").size(11),
        tooltip::Position::Top,
    );
    let tag_summary = if item.missing {
        "源文件已移动 · 点击重新定位".into()
    } else if item.tags.is_empty() {
        "未添加标签".into()
    } else {
        truncate(&item.tags.join(" · "), 35)
    };
    let mut actions = row![
        button(text("标签").size(10))
            .padding([2, 5])
            .style(button::text)
            .on_press(Action::EditTags(item.id.clone())),
        Space::new().width(Length::Fill),
    ]
    .spacing(2)
    .align_y(iced::Alignment::Center);
    if item.missing {
        actions = actions.push(
            button(text("重新定位").size(10))
                .padding([2, 5])
                .style(button::text)
                .on_press(Action::Relink(item.id.clone())),
        );
    }
    actions = actions.push(tooltip(
        button(text("移除").size(10))
            .padding([2, 5])
            .style(button::text)
            .on_press(Action::Remove(item.id.clone())),
        text("从文献库移除，保留源文件").size(11),
        tooltip::Position::Top,
    ));
    container(
        column![
            row![open, favorite].spacing(2),
            text(tag_summary).size(10).wrapping(text::Wrapping::None),
            actions
        ]
        .spacing(4),
    )
    .padding([9, 10])
    .height(ROW_HEIGHT)
    .width(Length::Fill)
    .style(move |theme: &Theme| {
        let palette = theme.extended_palette();
        container::Style {
            background: selected.then_some(palette.primary.weak.color.into()),
            border: iced::Border {
                radius: 7.0.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    })
    .into()
}

fn parse_tags(value: &str) -> Vec<String> {
    value
        .split([',', '，'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn truncate(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let mut result: String = chars
        .by_ref()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_filters_and_tag_editor_preserve_user_intent() {
        let mut panel = LibraryPanel::default();
        panel.apply_local(&Action::SearchChanged("论文".into()));
        panel.apply_local(&Action::TagFilterChanged(" Rust ".into()));
        panel.apply_local(&Action::ToggleFavorites);
        let query = panel.query();
        assert_eq!(query.text, "论文");
        assert_eq!(query.tag.as_deref(), Some("Rust"));
        assert!(query.favorites_only);
        assert_eq!(parse_tags("论文， Rust, ,阅读"), ["论文", "Rust", "阅读"]);
    }

    #[test]
    fn truncation_respects_unicode_character_boundaries() {
        assert_eq!(truncate("中文论文标题", 3), "中文论…");
        assert_eq!(truncate("PDF", 3), "PDF");
    }

    #[test]
    fn a_large_library_creates_only_viewport_rows() {
        let item = LibraryItem {
            id: "document".into(),
            path: "paper.pdf".into(),
            title: "Paper".into(),
            file_name: "paper.pdf".into(),
            tags: Vec::new(),
            favorite: false,
            added_at_ms: 0,
            last_opened_at_ms: None,
            revision: Default::default(),
            missing: false,
        };
        let mut panel = LibraryPanel {
            items: vec![item; 10_000],
            ..LibraryPanel::default()
        };
        panel.apply_local(&Action::Scrolled {
            y: 5000.0 * ROW_HEIGHT,
            height: 500.0,
        });
        let range = panel.visible_range();
        assert!(range.contains(&5000));
        assert!(range.len() <= 12);
        assert!(range.start > 0 && range.end < panel.items.len());
    }
}
