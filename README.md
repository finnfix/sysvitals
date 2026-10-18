# SysVitals

[English](#english) | [中文](#中文)

## English

A lightweight CPU / memory / network monitor living in your taskbar (Windows) and menu bar (macOS).

SysVitals shows live percentages and transfer rates directly in the system status bar, follows
your system font, DPI and light/dark theme, and opens a small history chart when clicked. It is
built in Rust and aims to stay visually native and negligibly cheap to run 24/7.

### Status

**Early development.** The platform-independent core is implemented and unit-tested:

- `crates/core` — sampling (CPU / memory / network differentials), formatting, configuration and i18n
- `crates/app-win` — Windows taskbar display layer (planned)
- `crates/app-mac` — macOS menu-bar display layer (planned)
- `crates/ui-common` — shared settings window and tray (planned, GPUI)

No tagged or packaged releases yet.

### Planned features

- Taskbar (Windows) / menu bar (macOS) indicator for CPU, memory, upload and download
- Configurable prefixes, single-line or dual-row layout, tray-adjacent or far-left anchoring on Windows
- Binary units with `K/s` and `M/s` rate formatting
- Light / dark theme following the system
- Click to open a CPU/memory history chart, with one-click CSV copy
- English and Chinese UI

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
cargo build -p app-win    # Windows, MSVC toolchain
cargo build -p app-mac    # macOS
```

### Design notes

SysVitals keeps all error-prone logic — counter differencing, wrap-around protection, NIC
selection, unit selection, layout — in the pure-Rust `core` crate, where it is unit-tested on
any platform. The status-bar surfaces themselves are drawn by thin, platform-native glue:
a Win32 child window embedded in the taskbar (Windows) and `NSStatusItem` (macOS). Taskbar
embedding is not an officially supported API; the app treats it as best-effort and degrades
to a tray item if the system layout changes.

### License

Released under the MIT License. See [LICENSE](LICENSE).

---

## 中文

常驻 Windows 任务栏 / macOS 菜单栏的轻量 CPU、内存、网速监控，Rust 构建。

SysVitals 直接在系统状态栏里实时显示占用百分比与上下行速度，字号跟随系统、支持亮暗主题、
DPI 自适应，点击状态栏项可弹出 CPU / 内存历史走势图。目标是视觉上像系统自带、长期运行
开销可忽略。

### 状态

**开发早期。** 跨平台核心层已实现并通过单元测试：

- `crates/core` —— 采样（CPU / 内存 / 网络差分）、格式化、配置与国际化
- `crates/app-win` —— Windows 任务栏显示层（规划中）
- `crates/app-mac` —— macOS 菜单栏显示层（规划中）
- `crates/ui-common` —— 共享的设置窗口与托盘（规划中，GPUI）

暂无发布版本。

### 规划特性

- 任务栏（Windows）/ 菜单栏（macOS）指示器：CPU、内存、上行、下行
- 前缀可自定义，单行 / 双行布局，Windows 上可选托盘旁或最左侧锚位
- 1024 进制，网速按 `K/s` / `M/s` 自适应
- 亮 / 暗主题跟随系统
- 点击查看 CPU / 内存走势图，一键复制 CSV
- 中英文界面

### 构建核心层

要求：stable Rust 工具链（版本由 `rust-toolchain.toml` 锁定）。

```sh
cargo build            # 仅跨平台 crate（core + ui-common）
cargo test             # 核心层单元测试
cargo run -p sysvitals-core --example monitor   # 终端版采样循环演示
```

平台显示 crate 已从默认构建隔离，需在各自系统上显式编译：

```sh
cargo build -p app-win    # Windows，需 MSVC 工具链
cargo build -p app-mac    # macOS
```

### 设计说明

SysVitals 把所有容易出错的逻辑——计数器差分、回绕保护、网卡选择与聚合、单位换算、排版——
收敛在纯 Rust 的 `core` crate 里，任何平台都能单元测试。状态栏表面本身由薄的平台原生胶水
绘制：Windows 用嵌入任务栏的 Win32 子窗口，macOS 用 `NSStatusItem`。任务栏嵌入不是官方
支持的 API，程序按 best-effort 处理：系统结构变化时自动降级为托盘图标。

### 许可

基于 MIT 许可发布，见 [LICENSE](LICENSE)。
