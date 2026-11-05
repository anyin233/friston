//! Top-level navigation destinations. Layouts follow `design/mockups/index.html`.

use gpui_kit::component::IconName;

/// A page reachable from the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Today: upcoming meetings, quick record, recent minutes.
    Today,
    /// Calendar-backed schedule (EventKit) with capture presets per meeting.
    Schedule,
    /// Live meeting: capture controls, live captions with speakers, notes editor.
    Live,
    /// Minutes library with filters and multimodal search.
    Library,
    /// People and voiceprints (enrollment, clips, spatial cues).
    People,
    /// Providers, models, capture, Obsidian vault.
    Settings,
}

impl Page {
    /// Every page in sidebar order.
    pub const ALL: [Page; 6] = [
        Page::Today,
        Page::Schedule,
        Page::Live,
        Page::Library,
        Page::People,
        Page::Settings,
    ];

    /// Sidebar label, matching `design/mockups/index.html`.
    ///
    /// Returns: the label text.
    pub fn label(self) -> &'static str {
        match self {
            Page::Today => "今天",
            Page::Schedule => "日程",
            Page::Live => "会议中",
            Page::Library => "纪要库",
            Page::People => "说话人",
            Page::Settings => "设置",
        }
    }

    /// Sidebar icon from the bundled Lucide set.
    ///
    /// Returns: the icon name.
    pub fn icon(self) -> IconName {
        match self {
            Page::Today => IconName::LayoutDashboard,
            Page::Schedule => IconName::Calendar,
            Page::Live => IconName::Mic,
            Page::Library => IconName::BookOpen,
            Page::People => IconName::CircleUser,
            Page::Settings => IconName::Settings,
        }
    }

    /// One-line description shown in the page header until the real page lands.
    ///
    /// Returns: the description.
    pub fn summary(self) -> &'static str {
        match self {
            Page::Today => "接下来的会议、一键开始记录、最近生成的纪要和待办。",
            Page::Schedule => "来自系统日历的会议，每场会议可以设置记录方式和纪要模板。",
            Page::Live => "窗口录制、实时字幕与说话人、边开会边记笔记。",
            Page::Library => "纪要、转写、截图、录音和录屏，一个搜索框全部找到。",
            Page::People => "提前录制或从录音截取声纹，让字幕直接显示名字。",
            Page::Settings => "纪要生成方式、本地模型、录制与字幕、Obsidian 库和搜索索引。",
        }
    }
}
