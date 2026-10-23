//! Presentation only: root routes actions to the dedicated plugin worker.
use finch_plugin::PluginInfo;
use iced::widget::{button, checkbox, column, container, row, scrollable, text};
use iced::{Element, Length};

#[derive(Debug, Clone)]
pub enum Action {
    Discover,
    Install,
    SetEnabled {
        id: String,
        enabled: bool,
    },
    Run {
        plugin_id: String,
        command_id: String,
    },
    Dismiss,
}

#[derive(Debug, Default)]
pub struct PluginPanel {
    pub plugins: Vec<PluginInfo>,
    pub result: String,
    pub busy: bool,
    pub error: Option<String>,
}

impl PluginPanel {
    pub fn view(&self) -> Element<'_, Action> {
        let header = row![
            text("本地插件").size(21).width(Length::Fill),
            button("收起").on_press(Action::Dismiss).style(button::text),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        let actions = row![
            button("安装插件…").on_press_maybe((!self.busy).then_some(Action::Install)),
            button("刷新")
                .on_press_maybe((!self.busy).then_some(Action::Discover))
                .style(button::secondary),
        ]
        .spacing(8);
        let mut contents = column![
            header,
            text("按需启动，空闲后自动退出。翻译与笔记等能力可通过插件扩展。")
                .size(13),
            actions,
            container(text("启用代表你信任此插件。插件是本地程序，可以访问你的文件和网络；进程隔离不等于安全沙箱。").size(12))
                .padding(10)
                .style(container::bordered_box),
        ]
        .spacing(14)
        .width(Length::Fill);

        if self.busy {
            contents = contents.push(text("正在处理插件请求…").size(13));
        }
        if let Some(error) = &self.error {
            contents = contents.push(
                container(
                    column![text("插件操作未完成").size(14), text(error).size(12)].spacing(5),
                )
                .padding(10)
                .style(container::bordered_box),
            );
        }
        if self.plugins.is_empty() && !self.busy {
            contents = contents.push(
                container(
                    column![
                        text("还没有安装插件").size(16),
                        text(
                            "选择包含 plugin.json 和可执行文件的插件包，即可安装。安装后默认关闭。"
                        )
                        .size(13),
                    ]
                    .spacing(8),
                )
                .padding(14)
                .style(container::rounded_box),
            );
        }
        for plugin in &self.plugins {
            let id = plugin.manifest.id.clone();
            let enabled = checkbox(plugin.enabled)
                .label(if plugin.enabled {
                    "已启用"
                } else {
                    "信任并启用"
                })
                .on_toggle_maybe((!self.busy).then_some(move |enabled| Action::SetEnabled {
                    id: id.clone(),
                    enabled,
                }));
            let mut card = column![
                text(&plugin.manifest.name).size(16),
                text(format!(
                    "{} · v{}",
                    plugin.manifest.id, plugin.manifest.version
                ))
                .size(11),
                enabled,
            ]
            .spacing(8);
            if !plugin.available {
                card = card.push(text("此插件包不包含当前平台的可执行文件。").size(12));
            }
            for command in &plugin.manifest.commands {
                let action =
                    (!self.busy && plugin.enabled && plugin.available).then(|| Action::Run {
                        plugin_id: plugin.manifest.id.clone(),
                        command_id: command.id.clone(),
                    });
                card = card.push(
                    button(text(&command.title).size(13))
                        .on_press_maybe(action)
                        .width(Length::Fill)
                        .style(button::secondary),
                );
            }
            contents = contents.push(
                container(card)
                    .padding(12)
                    .width(Length::Fill)
                    .style(container::bordered_box),
            );
        }
        if !self.result.is_empty() {
            contents = contents.push(
                container(
                    column![text("运行结果").size(16), text(&self.result).size(13)].spacing(10),
                )
                .padding(12)
                .width(Length::Fill)
                .style(container::rounded_box),
            );
        }
        container(scrollable(contents).height(Length::Fill))
            .padding(16)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}
