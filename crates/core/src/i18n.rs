//! Static UI strings. Two locales, `en` (default) and `zh`. Lookup is a
//! plain match — the string set is small and fixed, so a heavyweight i18n
//! framework is not warranted.
//!
//! Scope: these keys cover settings/panel/menu/CSV-header text. Status-bar
//! metric *prefixes* are user config and deliberately NOT here: changing
//! language must never rewrite a user's custom prefix.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Zh,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::En, Language::Zh];

    /// Native name shown in the language picker.
    pub fn label(self) -> &'static str {
        match self {
            Language::En => "English",
            Language::Zh => "中文",
        }
    }
}

/// Translate `key` for `lang`. The return borrows from `key`, so callers pass
/// string literals (which they always do). A missing key returns the key
/// itself, making a forgotten string visible during development rather than
/// silently empty.
pub fn t(lang: Language, key: &str) -> &str {
    match (lang, key) {
        (Language::En, "settings.title") => "SysVitals Settings",
        (Language::Zh, "settings.title") => "SysVitals 设置",
        (Language::En, "settings.general") => "General",
        (Language::Zh, "settings.general") => "通用",
        (Language::En, "settings.display") => "Display",
        (Language::Zh, "settings.display") => "显示",
        (Language::En, "settings.language") => "Language",
        (Language::Zh, "settings.language") => "语言",
        (Language::En, "settings.theme") => "Indicator theme",
        (Language::Zh, "settings.theme") => "指示器主题",
        (Language::En, "theme.auto") => "Follow system",
        (Language::Zh, "theme.auto") => "跟随系统",
        (Language::En, "theme.light") => "Light",
        (Language::Zh, "theme.light") => "亮色",
        (Language::En, "theme.dark") => "Dark",
        (Language::Zh, "theme.dark") => "暗色",
        (Language::En, "settings.sample_interval") => "Sample interval",
        (Language::Zh, "settings.sample_interval") => "采样间隔",
        (Language::En, "settings.prefixes") => "Prefixes",
        (Language::Zh, "settings.prefixes") => "前缀",
        (Language::En, "settings.prefix_hint") => "Independent of language; up to 8 characters.",
        (Language::Zh, "settings.prefix_hint") => "不随语言变化；最多 8 个字符。",
        (Language::En, "settings.dual_row") => "Dual-row layout",
        (Language::Zh, "settings.dual_row") => "双行显示",
        (Language::En, "settings.item_spacing") => "Item spacing",
        (Language::Zh, "settings.item_spacing") => "项目间距",
        (Language::En, "settings.group_resources") => "CPU / MEM",
        (Language::Zh, "settings.group_resources") => "CPU / 内存",
        (Language::En, "settings.group_network") => "Upload / Download",
        (Language::Zh, "settings.group_network") => "上传 / 下载",
        (Language::En, "settings.layout") => "Layout",
        (Language::Zh, "settings.layout") => "布局",
        (Language::En, "settings.unit_seconds") => "s",
        (Language::Zh, "settings.unit_seconds") => "秒",
        (Language::En, "settings.cpu") => "CPU",
        (Language::Zh, "settings.cpu") => "CPU",
        (Language::En, "settings.mem") => "MEM",
        (Language::Zh, "settings.mem") => "内存",
        (Language::En, "settings.upload") => "Upload",
        (Language::Zh, "settings.upload") => "上传",
        (Language::En, "settings.download") => "Download",
        (Language::Zh, "settings.download") => "下载",
        (Language::En, "settings.anchor") => "Taskbar position",
        (Language::Zh, "settings.anchor") => "任务栏位置",
        (Language::En, "settings.left_margin") => "Left margin",
        (Language::Zh, "settings.left_margin") => "左侧边距",
        (Language::En, "settings.top_margin") => "Top margin",
        (Language::Zh, "settings.top_margin") => "顶部边距",
        (Language::En, "settings.save_failed") => "Settings could not be saved",
        (Language::Zh, "settings.save_failed") => "设置保存失败",
        (Language::En, "anchor.right") => "Left of tray",
        (Language::Zh, "anchor.right") => "托盘左侧",
        (Language::En, "anchor.left") => "Far left (Win11 centered only)",
        (Language::Zh, "anchor.left") => "最左侧（仅 Win11 居中时）",

        (Language::En, "chart.title") => "History",
        (Language::Zh, "chart.title") => "历史走势",
        (Language::En, "chart.cpu") => "CPU",
        (Language::Zh, "chart.cpu") => "CPU",
        (Language::En, "chart.mem") => "Memory",
        (Language::Zh, "chart.mem") => "内存",
        (Language::En, "chart.range.5m") => "5 min",
        (Language::Zh, "chart.range.5m") => "5 分钟",
        (Language::En, "chart.range.1h") => "1 hour",
        (Language::Zh, "chart.range.1h") => "1 小时",
        (Language::En, "chart.range.1d") => "1 day",
        (Language::Zh, "chart.range.1d") => "1 天",
        (Language::En, "chart.range.1mo") => "1 month",
        (Language::Zh, "chart.range.1mo") => "1 个月",
        (Language::En, "chart.copy_csv") => "Copy CSV",
        (Language::Zh, "chart.copy_csv") => "复制 CSV",
        (Language::En, "chart.copied") => "Copied to clipboard",
        (Language::Zh, "chart.copied") => "已复制到剪贴板",
        (Language::En, "chart.copy_failed") => "Copy failed",
        (Language::Zh, "chart.copy_failed") => "复制失败",

        (Language::En, "menu.settings") => "Settings",
        (Language::Zh, "menu.settings") => "设置",
        // Right-click menu entry that launches the per-platform system
        // resource viewer. Previously reachable only via double-click on the
        // indicator, which forced every left-click on the indicator to wait
        // the full system double-click threshold before opening the trend
        // chart; moving this to the context menu lets the left click fire
        // instantly. Two locale-neutral names because the two OSes ship
        // under different labels.
        (Language::En, "menu.task_manager") => "Task Manager",
        (Language::Zh, "menu.task_manager") => "任务管理器",
        (Language::En, "menu.activity_monitor") => "Activity Monitor",
        (Language::Zh, "menu.activity_monitor") => "活动监视器",
        (Language::En, "menu.restart") => "Restart",
        (Language::Zh, "menu.restart") => "重启",
        (Language::En, "menu.quit") => "Quit",
        (Language::Zh, "menu.quit") => "退出",

        // CSV headers follow the UI language; keys used directly.
        (Language::En, "csv.header.timestamp") => "timestamp",
        (Language::Zh, "csv.header.timestamp") => "时间",
        (Language::En, "csv.header.cpu") => "cpu_pct",
        (Language::Zh, "csv.header.cpu") => "CPU占用",
        (Language::En, "csv.header.mem") => "mem_pct",
        (Language::Zh, "csv.header.mem") => "内存占用",

        (_, key) => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_keys_differ_between_locales() {
        assert_ne!(
            t(Language::En, "settings.title"),
            t(Language::Zh, "settings.title")
        );
        assert_eq!(t(Language::En, "settings.title"), "SysVitals Settings");
        assert_eq!(t(Language::Zh, "settings.title"), "SysVitals 设置");
    }

    #[test]
    fn csv_headers_localized() {
        assert_eq!(t(Language::En, "csv.header.cpu"), "cpu_pct");
        assert_eq!(t(Language::Zh, "csv.header.cpu"), "CPU占用");
        assert_eq!(t(Language::Zh, "csv.header.timestamp"), "时间");
    }

    #[test]
    fn menu_task_viewers_localized() {
        // Context-menu labels for the per-platform system resource viewer.
        // Both locales must resolve (no key-passthrough), and the two OSes
        // ship under different names so both pairs need their own keys.
        assert_eq!(t(Language::En, "menu.task_manager"), "Task Manager");
        assert_eq!(t(Language::Zh, "menu.task_manager"), "任务管理器");
        assert_eq!(t(Language::En, "menu.activity_monitor"), "Activity Monitor");
        assert_eq!(t(Language::Zh, "menu.activity_monitor"), "活动监视器");
    }

    #[test]
    fn unknown_key_returns_key() {
        assert_eq!(t(Language::En, "does.not.exist"), "does.not.exist");
    }

    #[test]
    fn language_default() {
        assert_eq!(Language::default(), Language::En);
    }
}
