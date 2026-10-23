<p align="center">
  <img src=".github/assets/finch-icon.png" alt="Finch 图标" width="128">
</p>

<h1 align="center">Finch</h1>

<p align="center">轻量、快速的本地 PDF 论文阅读器</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-dea584?style=flat-square&amp;logo=rust&amp;logoColor=black" alt="Rust">
  <img src="https://img.shields.io/badge/UI-Iced-5b8def?style=flat-square" alt="UI: Iced">
  <img src="https://img.shields.io/badge/PDF-PDFium-e85d4a?style=flat-square" alt="PDF: PDFium">
  <img src="https://img.shields.io/badge/平台-Windows%20%7C%20macOS-64748b?style=flat-square" alt="平台: Windows / macOS">
  <img src="https://img.shields.io/badge/状态-开发中-e9a23b?style=flat-square" alt="状态: 开发中">
</p>

## 简介

Finch 是一款使用 Rust 构建的本地 PDF 论文阅读与管理工具。无需账户或云服务，不依赖 WebView，围绕阅读位置恢复、按需渲染和有限缓存设计。

目前处于早期开发阶段，面向 Windows 与 macOS；macOS 已配置构建与 CI，仍待实机验证。

## 功能

- ⚡ **轻量快速**：原生桌面界面，按需分块渲染、优先加载可见页面，以有限缓存控制资源占用。
- 📖 **PDF 阅读**：连续滚动、页码跳转、缩放、适宽与整页显示，支持目录、缩略图和浅色 / 深色主题。
- 🔍 **查找与摘录**：文内搜索、文字选择与复制，以及文档内部导航和外部链接。
- 🔖 **接续阅读**：恢复上次文档、缩放方式与页内阅读位置。
- 📚 **文献整理**：按标题、文件名或标签搜索，支持收藏、最近阅读和丢失文件重新定位。
- 🧩 **本地插件**：手动安装、按需启用，提供[文本统计示例](examples/text-stats)。

当前采用单窗口、单文档模式，暂不支持 PDF 批注编辑、OCR 或云同步。

## 本地运行

安装 Rust 及平台构建工具：Windows 推荐 Visual Studio C++ Build Tools（MSVC），macOS 需要 Xcode Command Line Tools。工具链由 [rust-toolchain.toml](rust-toolchain.toml) 固定，依赖由 `Cargo.lock` 锁定。

在仓库根目录执行以下命令。下载脚本会获取固定版本的 PDFium 并校验 SHA-256。

**Windows（PowerShell）**

```powershell
./scripts/fetch-pdfium.ps1
cargo run --locked -p finch-app -- tests/fixtures/finch-demo.pdf
```

**macOS（Apple Silicon / Intel）**

```sh
sh scripts/fetch-pdfium.sh
cargo run --locked -p finch-app -- tests/fixtures/finch-demo.pdf
```

## 使用

点击「打开 PDF」或拖入文件开始阅读。Windows 使用 `Ctrl`，macOS 使用 `Cmd`：

| 快捷键 | 操作 |
| --- | --- |
| `Ctrl/Cmd + O` | 打开 PDF |
| `Ctrl/Cmd + F` | 搜索文内文字 |
| `Ctrl/Cmd + C` | 复制选中文字 |
| `Ctrl/Cmd + + / -` | 放大 / 缩小 |

文献库引用原始 PDF，移除库记录不会删除原文件。阅读状态和文献库保存在本地，可通过 `--data-dir <目录>` 指定数据位置。更多启动选项见 `cargo run --locked -p finch-app -- --help`。

本地插件以当前用户权限运行，仅启用可信来源的插件。

## 许可

许可证见 [LICENSE](LICENSE)，第三方许可见 [licenses/third-party](licenses/third-party)。
