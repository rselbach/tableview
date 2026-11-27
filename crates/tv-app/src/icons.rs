use rune::Icon;

/// TableView geometric mark (table grid with subtle accent header).
pub static LOGO: Icon = Icon {
    name: "tv-logo",
    view: 24.0,
    body: r#"<rect x="2.5" y="3.5" width="19" height="17" rx="3" stroke="currentColor" stroke-width="1.6" fill="none"/><path d="M2.5 8.5H21.5M9 8.5V20.5M15.5 8.5V20.5M2.5 14.5H21.5" stroke="currentColor" stroke-width="1.4"/>"#,
};

pub static DATABASE: Icon = Icon {
    name: "tv-database",
    view: 16.0,
    body: r#"<ellipse cx="8" cy="4" rx="5.5" ry="2.2" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M2.5 4v8c0 1.2 2.46 2.2 5.5 2.2s5.5-1 5.5-2.2V4" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M2.5 8c0 1.2 2.46 2.2 5.5 2.2s5.5-1 5.5-2.2" stroke="currentColor" stroke-width="1.2" fill="none"/>"#,
};

pub static TABLE: Icon = Icon {
    name: "tv-table",
    view: 16.0,
    body: r#"<rect x="2" y="2.5" width="12" height="11" rx="1.5" stroke="currentColor" stroke-width="1.15" fill="none"/><path d="M2 6H14M6.5 6V13.5M2 9.75H14" stroke="currentColor" stroke-width="1.1"/>"#,
};

pub static VIEW_EYE: Icon = Icon {
    name: "tv-view",
    view: 16.0,
    body: r#"<path d="M1.5 8s2.3-4.2 6.5-4.2S14.5 8 14.5 8s-2.3 4.2-6.5 4.2S1.5 8 1.5 8Z" stroke="currentColor" stroke-width="1.2" fill="none" stroke-linejoin="round"/><circle cx="8" cy="8" r="1.8" stroke="currentColor" stroke-width="1.2" fill="none"/>"#,
};

pub static MATERIALIZED_VIEW: Icon = Icon {
    name: "tv-mat-view",
    view: 16.0,
    body: r#"<rect x="2" y="2.5" width="12" height="11" rx="1.5" stroke="currentColor" stroke-width="1.15" fill="none"/><path d="M4.5 9.5L7 6.5L9.2 8.8L11.5 5.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static PLUG: Icon = Icon {
    name: "tv-plug",
    view: 16.0,
    body: r#"<path d="M6 2.5V5.5M10 2.5V5.5M4.5 5.5H11.5V8.5C11.5 10.4 9.9 12 8 12C6.1 12 4.5 10.4 4.5 8.5V5.5ZM8 12V14.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static PLUS: Icon = Icon {
    name: "tv-plus",
    view: 16.0,
    body: r#"<path d="M8 3V13M3 8H13" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

pub static REFRESH: Icon = Icon {
    name: "tv-refresh",
    view: 16.0,
    body: r#"<path d="M13 8A5 5 0 1 1 11.5 4.5M13 2.5V5.5H10" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static TERMINAL: Icon = Icon {
    name: "tv-terminal",
    view: 16.0,
    body: r#"<rect x="1.8" y="2.8" width="12.4" height="10.4" rx="1.6" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M4.5 6.2L6.8 8.2L4.5 10.2M8.2 10.2H11.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static INSPECTOR: Icon = Icon {
    name: "tv-inspector",
    view: 16.0,
    body: r#"<rect x="2" y="3" width="12" height="10" rx="2" stroke="currentColor" stroke-width="1.15" fill="none"/><path d="M10 3.5V12.5" stroke="currentColor" stroke-width="1.15"/>"#,
};

pub static SIDEBAR_ICON: Icon = Icon {
    name: "tv-sidebar",
    view: 16.0,
    body: r#"<rect x="2" y="3" width="12" height="10" rx="2" stroke="currentColor" stroke-width="1.15" fill="none"/><path d="M6 3.5V12.5" stroke="currentColor" stroke-width="1.15"/>"#,
};

pub static KEY_ICON: Icon = Icon {
    name: "tv-key",
    view: 16.0,
    body: r#"<circle cx="5.5" cy="7.5" r="2.7" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M7.8 8.8L13.5 12.5M11.2 11L12.5 9.6" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>"#,
};

pub static LINK_IMPORT: Icon = Icon {
    name: "tv-import",
    view: 16.0,
    body: r#"<path d="M6.5 9.5L9.5 6.5M5.2 7.5L3.8 8.9A2.6 2.6 0 0 0 7.5 12.6L8.9 11.2M7.1 4.8L8.5 3.4A2.6 2.6 0 0 1 12.2 7.1L10.8 8.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" fill="none"/>"#,
};

pub static SUN_MOON: Icon = Icon {
    name: "tv-theme",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="5" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M8 3A5 5 0 0 1 8 13Z" fill="currentColor"/>"#,
};

pub static SHIELD: Icon = Icon {
    name: "tv-shield",
    view: 16.0,
    body: r#"<path d="M8 2L13 4.2V7.8C13 10.9 10.9 13.1 8 14C5.1 13.1 3 10.9 3 7.8V4.2L8 2Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/>"#,
};

pub static FILTER: Icon = Icon {
    name: "tv-filter",
    view: 16.0,
    body: r#"<path d="M2.5 4H13.5M4.8 8H11.2M6.8 12H9.2" stroke="currentColor" stroke-width="1.25" stroke-linecap="round"/>"#,
};

pub static COPY: Icon = Icon {
    name: "tv-copy",
    view: 16.0,
    body: r#"<rect x="5.5" y="5.5" width="8" height="8" rx="1.5" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M10.5 5.5V4A1.5 1.5 0 0 0 9 2.5H4A1.5 1.5 0 0 0 2.5 4V9A1.5 1.5 0 0 0 4 10.5H5.5" stroke="currentColor" stroke-width="1.2" fill="none"/>"#,
};

pub static CODE_DDL: Icon = Icon {
    name: "tv-code-ddl",
    view: 16.0,
    body: r#"<path d="M5.5 4.5L2 8L5.5 11.5M10.5 4.5L14 8L10.5 11.5M9 3.5L7 12.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static INDEX_BOLT: Icon = Icon {
    name: "tv-index-bolt",
    view: 16.0,
    body: r#"<path d="M9 2L4.5 8.5H8L7 14L11.5 7.5H8L9 2Z" stroke="currentColor" stroke-width="1.15" stroke-linejoin="round" fill="none"/>"#,
};

pub static CHEVRON_LEFT: Icon = Icon {
    name: "tv-chevron-left",
    view: 16.0,
    body: r#"<path d="M10 3.5L5.5 8L10 12.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static CHEVRON_RIGHT: Icon = Icon {
    name: "tv-chevron-right",
    view: 16.0,
    body: r#"<path d="M6 3.5L10.5 8L6 12.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static SQL_QUERY: Icon = Icon {
    name: "tv-sql-query",
    view: 16.0,
    body: r#"<rect x="2" y="2.5" width="12" height="11" rx="2" stroke="currentColor" stroke-width="1.3" fill="none"/><path d="M5 6.2L7 8L5 9.8M8.5 9.8H11" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static PLAY: Icon = Icon {
    name: "tv-play",
    view: 16.0,
    body: r#"<path d="M5 3.4L12.2 8L5 12.6V3.4Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" fill="currentColor"/>"#,
};

pub static STOP_SQUARE: Icon = Icon {
    name: "tv-stop-square",
    view: 16.0,
    body: r#"<rect x="4" y="4" width="8" height="8" rx="1.4" fill="currentColor"/>"#,
};

pub static BOOKMARK: Icon = Icon {
    name: "tv-bookmark",
    view: 16.0,
    body: r#"<path d="M4.2 2.5H11.8C12.2 2.5 12.5 2.8 12.5 3.2V13.5L8 10.6L3.5 13.5V3.2C3.5 2.8 3.8 2.5 4.2 2.5Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" fill="none"/>"#,
};

pub static CLOCK: Icon = Icon {
    name: "tv-clock",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="5.5" stroke="currentColor" stroke-width="1.3" fill="none"/><path d="M8 5V8.3L10.3 9.7" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static TRASH: Icon = Icon {
    name: "tv-trash",
    view: 16.0,
    body: r#"<path d="M3 4.5H13M6 4.5V3H10V4.5M5 4.5V12.5C5 13.1 5.4 13.5 6 13.5H10C10.6 13.5 11 13.1 11 12.5V4.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static ARROW_UP_RIGHT: Icon = Icon {
    name: "tv-arrow-up-right",
    view: 16.0,
    body: r#"<path d="M5 11L11.5 4.5M6.5 4.5H11.5V9.5" stroke="currentColor" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static DOWNLOAD: Icon = Icon {
    name: "tv-download",
    view: 16.0,
    body: r#"<path d="M8 2.8V10.2M5.2 7.6L8 10.5L10.8 7.6M3 12.8H13" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static DIFF_CHANGES: Icon = Icon {
    name: "tv-diff-changes",
    view: 16.0,
    body: r#"<rect x="2.2" y="2.5" width="11.6" height="11" rx="1.8" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M8 5.2V8.8M6.2 7H9.8M6.2 10.8H9.8" stroke="currentColor" stroke-width="1.25" stroke-linecap="round"/>"#,
};

pub static EXPLAIN_TREE: Icon = Icon {
    name: "tv-explain-tree",
    view: 16.0,
    body: r#"<rect x="5.5" y="2" width="5" height="3" rx="0.9" stroke="currentColor" stroke-width="1.2" fill="none"/><rect x="2" y="10.5" width="4.5" height="3" rx="0.9" stroke="currentColor" stroke-width="1.2" fill="none"/><rect x="9.5" y="10.5" width="4.5" height="3" rx="0.9" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M8 5V7.8M4.25 10.5V7.8H11.75V10.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static CHECK: Icon = Icon {
    name: "tv-check",
    view: 16.0,
    body: r#"<path d="M3.2 8.4L6.4 11.6L12.8 4.4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static UNDO: Icon = Icon {
    name: "tv-undo",
    view: 16.0,
    body: r#"<path d="M5.5 5H10C11.9 5 13.5 6.6 13.5 8.5C13.5 10.4 11.9 12 10 12H5.5M5.5 5L3.2 7.3L5.5 9.6" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

pub static EDIT_PENCIL: Icon = Icon {
    name: "tv-edit-pencil",
    view: 16.0,
    body: r#"<path d="M10.8 2.7L13.3 5.2L5.5 13H3V10.5L10.8 2.7Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/>"#,
};

pub static UPDATE_CIRCLE: Icon = Icon {
    name: "tv-update-circle",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="6" stroke="currentColor" stroke-width="1.25" fill="none"/><path d="M8 10.6V5.4M5.7 7.6L8 5.3L10.3 7.6" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};
