//! Top-level navigation destinations. Layouts follow `design/mockups/index.html`.

use gpui_kit::component::IconName;

pub mod library;
pub mod live;
pub mod people;
pub mod schedule;
pub mod settings;
pub mod today;

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
}
