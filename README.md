# SysVitals

[English](#english) | [中文](#中文)

## English

A lightweight CPU / memory / network monitor living in your taskbar (Windows) and menu bar (macOS).

SysVitals shows live percentages and transfer rates directly in the system status bar, follows
your system font, DPI and light/dark theme, and opens a small history chart when clicked. It is
built in Rust and aims to stay visually native and negligibly cheap to run 24/7.

### Status

**Early development, no releases yet.** The core logic and the first working display layers are
implemented:

- `crates/core` — sampling (CPU / memory / network differentials), formatting, configuration,
  history store, logging and i18n; pure Rust, unit-tested on any platform
- `crates/app-win` — Windows taskbar indicator (topmost layered overlay + GDI text)
- `crates/app-mac` — macOS menu-bar indicator (`NSStatusItem`)
- `crates/app-settings` — settings window (GPUI), launched as a separate process
- `crates/app-history` — CPU / memory trend-chart popup (GPUI), with one-click CSV copy
- `crates/ui-common` — placeholder for a future shared GPUI settings/tray layer

Nothing is tagged or packaged yet; the Windows and macOS layers are still being validated on real
desktops.

### Features

- Taskbar (Windows) / menu bar (macOS) indicator for CPU, memory, upload and download
- Configurable prefixes, single-line or dual-row layout, tray-adjacent or far-left anchoring on
  Windows, vertical-taskbar support
- Binary (1024-base) units with adaptive `B/s` / `K/s` / `M/s` / `G/s` rate formatting
- Light / dark theme following the system
- Click the indicator to open a CPU / memory history chart (5 min / 1 h / 1 d / 1 mo), retained
  for up to a month, with one-click CSV copy to the clipboard
- English and Chinese UI (switchable; prefixes are independent of language)

### Building the core

Requirements: a stable Rust toolchain (`rust-toolchain.toml` pins it).

```sh
cargo build            # cross-platform crates only (core + ui-common)
cargo test             # core unit tests
cargo run -p sysvitals-core --example monitor   # terminal demo of the sampling loop
```

Platform display crates are isolated from the default build and are compiled explicitly on
their own OS:

```sh
cargo build -p app-win        # Windows, MSVC toolchain
cargo build -p app-mac        # macOS
cargo build -p app-settings   # GPUI settings window (Windows / macOS)
cargo build -p app-history    # GPUI history chart popup (Windows / macOS)
```

### Design notes

SysVitals keeps all error-prone logic — counter differencing, wrap-around protection, NIC
selection, unit selection, layout — in the pure-Rust `core` crate, where it is unit-tested on
any platform. The status-bar surfaces themselves are drawn by thin, platform-native glue:
on Windows a topmost layered popup aligned to the taskbar geometry (the taskbar is rendered by
XAML Islands, so an embedded child window cannot draw crisply — verified empirically), on macOS
an `NSStatusItem`. The settings window and history chart are separate GPUI processes launched by
the indicator, so the resident indicator stays minimal (no UI toolkit, no modal loops in its own
message pump).

### License

Released under the MIT License. See [LICENSE](LICENSE).

---

## 中文

常驻 Windows 任务栏 / macOS 菜单栏的轻量 CPU、内存、网速监控，Rust 构建。

SysVitals 直接在系统状态栏里实时显示占用百分比与上下行速度，字号跟随系统、支持亮暗主题、
DPI 自适应，点击状态栏项可弹出 CPU / 内存历史走势图。目标是视觉上像系统自带、长期运行
开销可忽略。

### 状态

**开发早期，尚无发布版本。** 核心层与首版显示层已实现：

- `crates/core` —— 采样（CPU / 内存 / 网络差分）、格式化、配置、历史存储、日志与国际化；纯 Rust，可在任意平台单元测试
- `crates/app-win` —— Windows 任务栏指示器（topmost 分层窗口 + GDI 文本）
- `crates/app-mac` —— macOS 菜单栏指示器（`NSStatusItem`）
- `crates/app-settings` —— 设置窗口（GPUI），作为独立进程拉起
- `crates/app-history` —— CPU / 内存走势图弹窗（GPUI），一键复制 CSV
- `crates/ui-common` —— 未来共享 GPUI 设置/托盘层的占位

尚未打 tag 或出包；Windows 与 macOS 显示层仍在真实桌面上验证。

### 特性

- 任务栏（Windows）/ 菜单栏（macOS）指示器：CPU、内存、上行、下行
- 前缀可自定义，单行 / 双行布局，Windows 上可选托盘旁或最左侧锚位，支持垂直任务栏
- 1024 进制，网速按 `B/s` / `K/s` / `M/s` / `G/s` 四档自适应
- 亮 / 暗主题跟随系统
- 点击指示器弹出 CPU / 内存走势图（5 分钟 / 1 小时 / 1 天 / 1 个月），历史最长保留一个月，一键复制 CSV 到剪贴板
- 中英文界面（可切换；前缀不受语言影响）

### 构建核心层

要求：stable Rust 工具链（版本由 `rust-toolchain.toml` 锁定）。

```sh
cargo build            # 仅跨平台 crate（core + ui-common）
cargo test             # 核心层单元测试
cargo run -p sysvitals-core --example monitor   # 终端版采样循环演示
```

平台显示 crate 已从默认构建隔离，需在各自系统上显式编译：

```sh
cargo build -p app-win        # Windows，需 MSVC 工具链
cargo build -p app-mac        # macOS
cargo build -p app-settings   # GPUI 设置窗（Windows / macOS）
cargo build -p app-history    # GPUI 走势图弹窗（Windows / macOS）
```

### 设计说明

SysVitals 把所有容易出错的逻辑——计数器差分、回绕保护、网卡选择与聚合、单位换算、排版——
收敛在纯 Rust 的 `core` crate 里，任何平台都能单元测试。状态栏表面本身由薄的平台原生胶水
绘制：Windows 用贴合任务栏几何的 topmost 分层悬浮窗（任务栏由 XAML Islands 渲染，嵌入子窗
无法清晰绘制——已实测证伪），macOS 用 `NSStatusItem`。设置窗与走势图是指示器拉起的独立 GPUI
进程，常驻指示器进程因此保持精简（自身消息循环里没有 UI 工具包，也没有模态循环）。

### 许可

基于 MIT 许可发布，见 [LICENSE](LICENSE)。
