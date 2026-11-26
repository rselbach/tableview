use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Local;
use rune::{ColorScheme, PointerPress, TextEdit, Tx};
use rune_kit::shell::TabDragState;
use rune_kit::{PaneState, SortDir, TableState};
use tv_core::{
    AccentColor, ColumnSummary, ConnectionId, ConnectionProfile, ConnectionStore, DatabaseInfo, DbObject, DbObjectKind,
    EnvTag, ExecutedSql, PageRequest, PasswordMode, RequestId, SafetyPolicy, SchemaInfo, SessionId, SortOrder,
    SortSpec, SslMode, StoreData, TabId, TablePage, TableStructure,
};

use crate::worker::{WorkerBridge, WorkerCmd, WorkerEvent};

/// Active top-level screen in TableView's single-window shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Launcher,
    Workspace(SessionId),
}

/// Active sidebar section inside a workspace session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarTab {
    #[default]
    Items,
    Queries,
    History,
}

impl SidebarTab {
    pub const ALL: [Self; 3] = [Self::Items, Self::Queries, Self::History];

    pub const fn index(self) -> usize {
        match self {
            Self::Items => 0,
            Self::Queries => 1,
            Self::History => 2,
        }
    }

    pub const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Queries,
            2 => Self::History,
            _ => Self::Items,
        }
    }
}

/// Loading state for an expanded object's columns in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnLoadState {
    Loading,
    Loaded(Vec<ColumnSummary>),
    Error(String),
}

/// Catalog state populated once a session connects to PostgreSQL.
#[derive(Clone, Debug)]
pub struct ConnectedData {
    pub server_version: String,
    pub databases: Vec<DatabaseInfo>,
    pub active_database: Option<String>,
    pub schemas: Vec<SchemaInfo>,
    pub active_schema: Option<String>,
    pub objects: Vec<DbObject>,
    pub expanded_columns: HashMap<(String, String), ColumnLoadState>,
    pub selected_object: Option<(String, String)>,
}

/// Connection lifecycle state machine driving toolbar status, sidebar state, and actions.
#[derive(Clone, Debug)]
pub enum ConnectionState {
    Disconnected { reason: Option<String> },
    Connecting,
    Connected(Box<ConnectedData>),
    Reconnecting,
    Failed(String),
}

impl ConnectionState {
    pub fn as_connected(&self) -> Option<&ConnectedData> {
        match self {
            Self::Connected(data) => Some(data),
            _ => None,
        }
    }

    pub fn as_connected_mut(&mut self) -> Option<&mut ConnectedData> {
        match self {
            Self::Connected(data) => Some(data),
            _ => None,
        }
    }
}

/// Asynchronous test status in the connection editor dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestStatus {
    Idle,
    Testing(RequestId),
    Ok(String),
    Err(String),
}

/// Tracks which `Select` dropdown inside the connection form is currently open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpenFormSelect {
    #[default]
    None,
    Env,
    Safety,
    PasswordMode,
    SslMode,
}

/// Editable fields and validation state for creating or editing a PostgreSQL connection.
#[derive(Clone, Debug)]
pub struct ConnectionFormState {
    pub editing_id: Option<ConnectionId>,
    pub name: TextEdit,
    pub folder: TextEdit,
    pub host: TextEdit,
    pub port: TextEdit,
    pub user: TextEdit,
    pub password: TextEdit,
    pub database: TextEdit,
    pub ca_cert: TextEdit,
    pub cert: TextEdit,
    pub key: TextEdit,
    pub env: EnvTag,
    pub color: AccentColor,
    pub safety: SafetyPolicy,
    pub password_mode: PasswordMode,
    pub ssl_mode: SslMode,
    pub open_select: OpenFormSelect,
    pub test_status: TestStatus,
    pub validation_error: Option<String>,
    pub loading_credential: bool,
    pub password_touched: bool,
}

impl ConnectionFormState {
    pub fn new_blank() -> Self {
        Self {
            editing_id: None,
            name: TextEdit::new(""),
            folder: TextEdit::new(""),
            host: TextEdit::new("127.0.0.1"),
            port: TextEdit::new("5432"),
            user: TextEdit::new("postgres"),
            password: TextEdit::new(""),
            database: TextEdit::new(""),
            ca_cert: TextEdit::new(""),
            cert: TextEdit::new(""),
            key: TextEdit::new(""),
            env: EnvTag::Local,
            color: AccentColor::Green,
            safety: SafetyPolicy::Unrestricted,
            password_mode: PasswordMode::Keychain,
            ssl_mode: SslMode::Prefer,
            open_select: OpenFormSelect::None,
            test_status: TestStatus::Idle,
            validation_error: None,
            loading_credential: false,
            password_touched: false,
        }
    }

    pub fn from_profile(profile: &ConnectionProfile, loading_credential: bool) -> Self {
        Self {
            editing_id: Some(profile.id),
            name: TextEdit::new(&profile.name),
            folder: TextEdit::new(profile.folder.as_deref().unwrap_or("")),
            host: TextEdit::new(&profile.host),
            port: TextEdit::new(profile.port.to_string()),
            user: TextEdit::new(&profile.user),
            password: TextEdit::new(""),
            database: TextEdit::new(profile.database.as_deref().unwrap_or("")),
            ca_cert: TextEdit::new(
                profile
                    .ssl
                    .ca_cert_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            cert: TextEdit::new(
                profile
                    .ssl
                    .client_cert_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            key: TextEdit::new(
                profile
                    .ssl
                    .client_key_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            env: profile.env,
            color: profile.color,
            safety: profile.safety,
            password_mode: profile.password_mode,
            ssl_mode: profile.ssl.mode,
            open_select: OpenFormSelect::None,
            test_status: TestStatus::Idle,
            validation_error: None,
            loading_credential,
            password_touched: false,
        }
    }

    pub fn to_profile(&self, existing: Option<&ConnectionProfile>) -> Result<ConnectionProfile, String> {
        let host = self.host.text.trim();
        let port = if host.starts_with('/') && self.port.text.trim().is_empty() {
            5432
        } else {
            self.port
                .text
                .trim()
                .parse::<u16>()
                .map_err(|_| "Port must be a number between 1 and 65535".to_owned())?
        };
        let mut profile = ConnectionProfile {
            id: self.editing_id.unwrap_or_default(),
            name: self.name.text.trim().to_owned(),
            folder: non_empty_opt(&self.folder.text),
            env: self.env,
            color: self.color,
            safety: self.safety,
            host: host.to_owned(),
            port,
            user: self.user.text.trim().to_owned(),
            password_mode: self.password_mode,
            database: non_empty_opt(&self.database.text),
            ssl: tv_core::SslConfig {
                mode: self.ssl_mode,
                ca_cert_path: non_empty_opt(&self.ca_cert.text).map(Into::into),
                client_cert_path: non_empty_opt(&self.cert.text).map(Into::into),
                client_key_path: non_empty_opt(&self.key.text).map(Into::into),
            },
            created_at: existing.map(|p| p.created_at).unwrap_or_else(chrono::Utc::now),
            last_connected_at: existing.and_then(|p| p.last_connected_at),
        };
        if profile.name.is_empty() && !profile.host.is_empty() {
            profile.name = profile.host.clone();
        }
        profile.validate().map_err(|e| e.to_string())?;
        Ok(profile)
    }
}

fn non_empty_opt(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Which quick palette overlay is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Database,
    Connection,
    QuickJump,
}

/// State for `Cmd+K`, `Cmd+Shift+K`, and `Cmd+P` modal pickers.
#[derive(Clone, Debug)]
pub struct PickerState {
    pub kind: PickerKind,
    pub query: TextEdit,
    pub highlighted: usize,
}

impl PickerState {
    pub fn new(kind: PickerKind) -> Self {
        Self {
            kind,
            query: TextEdit::new(""),
            highlighted: 0,
        }
    }
}

/// Modal state for importing a `postgres://` URL into a new connection form.
#[derive(Clone, Debug)]
pub struct ImportUrlState {
    pub url: TextEdit,
    pub error: Option<String>,
}

/// Modal state for prompting a password when `PasswordMode::AskEveryTime` is used.
#[derive(Clone, Debug)]
pub struct PasswordPromptState {
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub database_override: Option<String>,
    pub password: TextEdit,
    pub error: Option<String>,
}

/// Active modal dialog overlay. At most one modal is open at a time.
#[derive(Clone, Debug, Default)]
pub enum OverlayState {
    #[default]
    None,
    ConnectionForm(Box<ConnectionFormState>),
    ImportUrl(ImportUrlState),
    Picker(PickerState),
    PasswordPrompt(PasswordPromptState),
    DeleteConfirm(ConnectionId),
}

/// One SQL query entry recorded in a workspace session's bottom console log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleLogEntry {
    pub timestamp: String,
    pub sql: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

impl ConsoleLogEntry {
    pub fn from_executed(exec: ExecutedSql) -> Self {
        Self {
            timestamp: Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            sql: exec.sql,
            duration_ms: exec.duration_ms,
            error: exec.error,
        }
    }
}

/// Active sub-view within a table or view workspace tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableViewMode {
    #[default]
    Data,
    Structure,
    Indexes,
}

impl TableViewMode {
    pub const fn index(self) -> usize {
        match self {
            Self::Data => 0,
            Self::Structure => 1,
            Self::Indexes => 2,
        }
    }

    pub const fn from_index(idx: usize) -> Self {
        match idx {
            1 => Self::Structure,
            2 => Self::Indexes,
            _ => Self::Data,
        }
    }
}

/// Interactive state for one open table or view tab (`Data | Structure | Indexes` + inspector state).
pub struct TableTabState {
    pub schema: String,
    pub object_name: String,
    pub object_kind: DbObjectKind,
    pub view_mode: TableViewMode,
    pub filter_input: TextEdit,
    pub applied_filter: Option<String>,
    pub filter_error: Option<String>,
    pub sort: Option<SortSpec>,
    pub page_size: i64,
    pub page_size_open: bool,
    pub offset: i64,
    pub page_loading: bool,
    pub structure_loading: bool,
    pub latest_page_req: Option<RequestId>,
    pub latest_structure_req: Option<RequestId>,
    pub page: Option<TablePage>,
    pub page_error: Option<String>,
    pub structure: Option<TableStructure>,
    pub structure_error: Option<String>,
    pub data_table: TableState,
    pub structure_table: TableState,
    pub indexes_table: TableState,
    pub selected_row: Option<usize>,
    pub inspector_search: TextEdit,
    pub inspector_json_pretty: bool,
    pub header_menu: Option<(String, PointerPress)>,
    pub ddl_expanded: bool,
}

impl TableTabState {
    pub fn new(
        tab_id: TabId,
        schema: String,
        object_name: String,
        object_kind: DbObjectKind,
        page_req: Option<RequestId>,
        structure_req: Option<RequestId>,
    ) -> Self {
        let tid = tab_id.get();
        Self {
            schema,
            object_name,
            object_kind,
            view_mode: TableViewMode::Data,
            filter_input: TextEdit::new(""),
            applied_filter: None,
            filter_error: None,
            sort: None,
            page_size: 300,
            page_size_open: false,
            offset: 0,
            page_loading: page_req.is_some(),
            structure_loading: structure_req.is_some(),
            latest_page_req: page_req,
            latest_structure_req: structure_req,
            page: None,
            page_error: None,
            structure: None,
            structure_error: None,
            data_table: TableState::new().key(format!("tv-data-{tid}")),
            structure_table: TableState::new().key(format!("tv-struct-{tid}")),
            indexes_table: TableState::new().key(format!("tv-idx-{tid}")),
            selected_row: None,
            inspector_search: TextEdit::new(""),
            inspector_json_pretty: true,
            header_menu: None,
            ddl_expanded: true,
        }
    }

    pub fn page_request(&self) -> PageRequest {
        PageRequest {
            schema: self.schema.clone(),
            table: self.object_name.clone(),
            filter_where: self.applied_filter.clone(),
            sort: self.sort.clone(),
            limit: self.page_size,
            offset: self.offset,
        }
    }
}

/// Content variants hosted inside a workspace tab.
pub enum TabContent {
    Table(Box<TableTabState>),
}

/// A tab open inside a [`SessionState`].
pub struct WorkspaceTab {
    pub id: TabId,
    pub content: TabContent,
}

impl WorkspaceTab {
    pub fn new_table(
        id: TabId,
        schema: String,
        object_name: String,
        object_kind: DbObjectKind,
        page_req: Option<RequestId>,
        structure_req: Option<RequestId>,
    ) -> Self {
        Self {
            id,
            content: TabContent::Table(Box::new(TableTabState::new(
                id,
                schema,
                object_name,
                object_kind,
                page_req,
                structure_req,
            ))),
        }
    }

    pub fn title(&self) -> &str {
        match &self.content {
            TabContent::Table(t) => &t.object_name,
        }
    }

    pub fn schema_and_name(&self) -> Option<(&str, &str)> {
        match &self.content {
            TabContent::Table(t) => Some((&t.schema, &t.object_name)),
        }
    }

    pub fn object_kind(&self) -> Option<DbObjectKind> {
        match &self.content {
            TabContent::Table(t) => Some(t.object_kind),
        }
    }

    pub fn is_loading(&self) -> bool {
        match &self.content {
            TabContent::Table(t) => t.page_loading || t.structure_loading,
        }
    }

    pub fn has_error(&self) -> bool {
        match &self.content {
            TabContent::Table(t) => t.page_error.is_some() || t.filter_error.is_some() || t.structure_error.is_some(),
        }
    }

    pub fn as_table(&self) -> Option<&TableTabState> {
        match &self.content {
            TabContent::Table(t) => Some(t),
        }
    }

    pub fn as_table_mut(&mut self) -> Option<&mut TableTabState> {
        match &mut self.content {
            TabContent::Table(t) => Some(t),
        }
    }
}

/// Parses a `TabId::to_string()` formatted key (`"tab-12"`) back into [`TabId`].
pub fn parse_tab_id(raw: &str) -> Option<TabId> {
    raw.strip_prefix("tab-")
        .and_then(|num| num.parse::<u64>().ok())
        .map(TabId::new)
}

/// Self-contained state for a single database workspace session.
pub struct SessionState {
    pub id: SessionId,
    pub profile: ConnectionProfile,
    pub session_password: Option<String>,
    pub latest_req_id: RequestId,
    pub status: ConnectionState,
    pub sidebar_tab: SidebarTab,
    pub sidebar_search: TextEdit,
    pub show_system_schemas: bool,
    pub console_open: bool,
    pub console_entries: Vec<ConsoleLogEntry>,
    pub schema_dropdown_open: bool,
    pub sidebar_pane: PaneState,
    pub inspector_pane: PaneState,
    pub tabs: Vec<WorkspaceTab>,
    pub active_tab: Option<TabId>,
    pub tab_drag: TabDragState,
}

impl SessionState {
    pub fn new(id: SessionId, profile: ConnectionProfile, req_id: RequestId) -> Self {
        Self {
            id,
            profile,
            session_password: None,
            latest_req_id: req_id,
            status: ConnectionState::Connecting,
            sidebar_tab: SidebarTab::Items,
            sidebar_search: TextEdit::new(""),
            show_system_schemas: false,
            console_open: true,
            console_entries: Vec::new(),
            schema_dropdown_open: false,
            sidebar_pane: PaneState::new(260.0).range(180.0, 440.0),
            inspector_pane: PaneState::new(292.0).range(220.0, 480.0),
            tabs: Vec::new(),
            active_tab: None,
            tab_drag: TabDragState::new(),
        }
    }

    pub fn active_tab(&self) -> Option<&WorkspaceTab> {
        let active = self.active_tab?;
        self.tabs.iter().find(|t| t.id == active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut WorkspaceTab> {
        let active = self.active_tab?;
        self.tabs.iter_mut().find(|t| t.id == active)
    }

    pub fn active_table_tab(&self) -> Option<&TableTabState> {
        self.active_tab().and_then(WorkspaceTab::as_table)
    }

    pub fn active_table_tab_mut(&mut self) -> Option<&mut TableTabState> {
        self.active_tab_mut().and_then(WorkspaceTab::as_table_mut)
    }

    pub fn tab_mut(&mut self, tab_id: TabId) -> Option<&mut WorkspaceTab> {
        self.tabs.iter_mut().find(|t| t.id == tab_id)
    }

    pub fn filtered_objects(&self) -> Vec<&DbObject> {
        let Some(connected) = self.status.as_connected() else {
            return Vec::new();
        };
        let query = self.sidebar_search.text.trim().to_ascii_lowercase();
        connected
            .objects
            .iter()
            .filter(|obj| query.is_empty() || obj.name.to_ascii_lowercase().contains(&query))
            .collect()
    }

    pub fn visible_schemas(&self) -> Vec<&SchemaInfo> {
        let Some(connected) = self.status.as_connected() else {
            return Vec::new();
        };
        connected
            .schemas
            .iter()
            .filter(|s| self.show_system_schemas || !s.is_system)
            .collect()
    }
}

/// Top-level application state owned by Rune.
pub struct AppState {
    pub store_path: PathBuf,
    pub persist_enabled: bool,
    pub connections: Vec<ConnectionProfile>,
    pub folders: Vec<String>,
    pub sort_order: SortOrder,
    pub sort_dropdown_open: bool,
    pub env_filter: Option<EnvTag>,
    pub launcher_search: TextEdit,
    pub launcher_selected: Option<ConnectionId>,
    pub launcher_context_menu: Option<(ConnectionId, PointerPress)>,
    pub sessions: HashMap<SessionId, SessionState>,
    pub screen: Screen,
    pub overlay: OverlayState,
    pub appearance: ColorScheme,
    pub status_notice: Option<(String, bool)>,
    pub worker: Option<WorkerBridge>,
    next_session_id: u64,
    next_tab_id: u64,
    next_req_id: u64,
    pub active_test_req_id: Option<RequestId>,
    pub active_cred_req_id: Option<RequestId>,
    pub fallback_pane: PaneState,
    pub fallback_edit: TextEdit,
    pub fallback_table: TableState,
    pub fallback_tab_drag: TabDragState,
}

impl AppState {
    /// Creates application state loaded from the default platform store (or memory fallback).
    pub fn init_default() -> Self {
        let (store_path, store_data) = match ConnectionStore::open_default() {
            Ok(store) => {
                let path = store.path().to_path_buf();
                let data = store.load().unwrap_or_default();
                (path, data)
            }
            Err(_) => (PathBuf::from("connections.json"), StoreData::default()),
        };
        let mut state = Self::from_store_data(store_path, store_data, true);
        state.worker = WorkerBridge::spawn();
        state
    }

    /// Creates an isolated in-memory application state for tests and headless scenes.
    pub fn init_in_memory(data: StoreData) -> Self {
        Self::from_store_data(PathBuf::from("in-memory-connections.json"), data, false)
    }

    fn from_store_data(store_path: PathBuf, mut data: StoreData, persist_enabled: bool) -> Self {
        data.sort(SortOrder::Name);
        let launcher_selected = data.connections.first().map(|c| c.id);
        Self {
            store_path,
            persist_enabled,
            connections: data.connections,
            folders: data.folders,
            sort_order: SortOrder::Name,
            sort_dropdown_open: false,
            env_filter: None,
            launcher_search: TextEdit::new(""),
            launcher_selected,
            launcher_context_menu: None,
            sessions: HashMap::new(),
            screen: Screen::Launcher,
            overlay: OverlayState::None,
            appearance: ColorScheme::System,
            status_notice: None,
            worker: None,
            next_session_id: 1,
            next_tab_id: 1,
            next_req_id: 1,
            active_test_req_id: None,
            active_cred_req_id: None,
            fallback_pane: PaneState::new(260.0),
            fallback_edit: TextEdit::new(""),
            fallback_table: TableState::new().key("tv-fallback-table"),
            fallback_tab_drag: TabDragState::new(),
        }
    }

    pub fn sidebar_search_mut(&mut self) -> &mut TextEdit {
        match self.screen {
            Screen::Workspace(id) => match self.sessions.get_mut(&id) {
                Some(sess) => &mut sess.sidebar_search,
                None => &mut self.fallback_edit,
            },
            Screen::Launcher => &mut self.fallback_edit,
        }
    }

    fn active_table_edit_mut(&mut self, pick: fn(&mut TableTabState) -> &mut TextEdit) -> &mut TextEdit {
        if let Screen::Workspace(sid) = self.screen
            && self.sessions.get(&sid).and_then(SessionState::active_table_tab).is_some()
            && let Some(tab) = self.sessions.get_mut(&sid).and_then(SessionState::active_table_tab_mut)
        {
            return pick(tab);
        }
        &mut self.fallback_edit
    }

    fn active_table_state_mut(&mut self, pick: fn(&mut TableTabState) -> &mut TableState) -> &mut TableState {
        if let Screen::Workspace(sid) = self.screen
            && self.sessions.get(&sid).and_then(SessionState::active_table_tab).is_some()
            && let Some(tab) = self.sessions.get_mut(&sid).and_then(SessionState::active_table_tab_mut)
        {
            return pick(tab);
        }
        &mut self.fallback_table
    }

    pub fn active_filter_input_mut(&mut self) -> &mut TextEdit {
        self.active_table_edit_mut(|tab| &mut tab.filter_input)
    }

    pub fn active_inspector_search_mut(&mut self) -> &mut TextEdit {
        self.active_table_edit_mut(|tab| &mut tab.inspector_search)
    }

    pub fn tab_drag_mut(&mut self, sid: SessionId) -> &mut TabDragState {
        match self.sessions.get_mut(&sid) {
            Some(session) => &mut session.tab_drag,
            None => &mut self.fallback_tab_drag,
        }
    }

    pub fn active_data_table(&self) -> &TableState {
        self.active_session()
            .and_then(SessionState::active_table_tab)
            .map_or(&self.fallback_table, |t| &t.data_table)
    }

    pub fn active_data_table_mut(&mut self) -> &mut TableState {
        self.active_table_state_mut(|tab| &mut tab.data_table)
    }

    pub fn active_structure_table(&self) -> &TableState {
        self.active_session()
            .and_then(SessionState::active_table_tab)
            .map_or(&self.fallback_table, |t| &t.structure_table)
    }

    pub fn active_structure_table_mut(&mut self) -> &mut TableState {
        self.active_table_state_mut(|tab| &mut tab.structure_table)
    }

    pub fn active_indexes_table(&self) -> &TableState {
        self.active_session()
            .and_then(SessionState::active_table_tab)
            .map_or(&self.fallback_table, |t| &t.indexes_table)
    }

    pub fn active_indexes_table_mut(&mut self) -> &mut TableState {
        self.active_table_state_mut(|tab| &mut tab.indexes_table)
    }

    pub fn form_field_mut(&mut self, pick: fn(&mut ConnectionFormState) -> &mut TextEdit) -> &mut TextEdit {
        match &mut self.overlay {
            OverlayState::ConnectionForm(f) => pick(f),
            _ => &mut self.fallback_edit,
        }
    }

    pub fn picker_query_mut(&mut self) -> &mut TextEdit {
        match &mut self.overlay {
            OverlayState::Picker(p) => &mut p.query,
            _ => &mut self.fallback_edit,
        }
    }

    pub fn import_url_mut(&mut self) -> &mut TextEdit {
        match &mut self.overlay {
            OverlayState::ImportUrl(m) => &mut m.url,
            _ => &mut self.fallback_edit,
        }
    }

    pub fn prompt_password_mut(&mut self) -> &mut TextEdit {
        match &mut self.overlay {
            OverlayState::PasswordPrompt(p) => &mut p.password,
            _ => &mut self.fallback_edit,
        }
    }

    pub fn sidebar_pane_mut(&mut self, id: SessionId) -> &mut PaneState {
        if let Some(session) = self.sessions.get_mut(&id) {
            &mut session.sidebar_pane
        } else {
            &mut self.fallback_pane
        }
    }

    pub fn inspector_pane_mut(&mut self, id: SessionId) -> &mut PaneState {
        if let Some(session) = self.sessions.get_mut(&id) {
            &mut session.inspector_pane
        } else {
            &mut self.fallback_pane
        }
    }

    pub fn alloc_session_id(&mut self) -> SessionId {
        let id = SessionId::new(self.next_session_id);
        self.next_session_id += 1;
        id
    }

    pub fn alloc_tab_id(&mut self) -> TabId {
        let id = TabId::new(self.next_tab_id);
        self.next_tab_id += 1;
        id
    }

    pub fn alloc_req_id(&mut self) -> RequestId {
        let id = RequestId::new(self.next_req_id);
        self.next_req_id += 1;
        id
    }

    pub fn active_session(&self) -> Option<&SessionState> {
        match self.screen {
            Screen::Workspace(id) => self.sessions.get(&id),
            Screen::Launcher => None,
        }
    }

    pub fn active_session_mut(&mut self) -> Option<&mut SessionState> {
        match self.screen {
            Screen::Workspace(id) => self.sessions.get_mut(&id),
            Screen::Launcher => None,
        }
    }

    pub fn filtered_connections(&self) -> Vec<&ConnectionProfile> {
        let q = self.launcher_search.text.trim().to_ascii_lowercase();
        self.connections
            .iter()
            .filter(|c| self.env_filter.is_none_or(|env| c.env == env))
            .filter(|c| {
                q.is_empty()
                    || c.name.to_ascii_lowercase().contains(&q)
                    || c.host.to_ascii_lowercase().contains(&q)
                    || c.user.to_ascii_lowercase().contains(&q)
                    || c.database
                        .as_deref()
                        .is_some_and(|d| d.to_ascii_lowercase().contains(&q))
            })
            .collect()
    }

    pub fn persist_connections(&self) {
        if !self.persist_enabled {
            return;
        }
        let store = ConnectionStore::at(self.store_path.clone());
        let data = StoreData {
            version: tv_core::storage::STORE_SCHEMA_VERSION,
            folders: self.folders.clone(),
            connections: self.connections.clone(),
        };
        drop(store.save(&data));
    }

    pub fn sort_connections(&mut self, order: SortOrder) {
        self.sort_order = order;
        let mut data = StoreData {
            version: tv_core::storage::STORE_SCHEMA_VERSION,
            folders: std::mem::take(&mut self.folders),
            connections: std::mem::take(&mut self.connections),
        };
        data.sort(order);
        self.folders = data.folders;
        self.connections = data.connections;
    }

    pub fn find_session_for_connection(&self, connection_id: ConnectionId) -> Option<SessionId> {
        self.sessions
            .values()
            .find(|s| s.profile.id == connection_id)
            .map(|s| s.id)
    }

    pub fn open_new_connection_form(&mut self, tx: &mut Tx) {
        self.launcher_context_menu = None;
        self.overlay = OverlayState::ConnectionForm(Box::new(ConnectionFormState::new_blank()));
        tx.focus_when_shown("form-name", true);
    }

    pub fn open_edit_connection_form(&mut self, connection_id: ConnectionId, tx: &mut Tx) {
        let Some(profile) = self.connections.iter().find(|c| c.id == connection_id).cloned() else {
            return;
        };
        self.launcher_context_menu = None;
        let load_cred = profile.password_mode == PasswordMode::Keychain && self.worker.is_some();
        let form = ConnectionFormState::from_profile(&profile, load_cred);
        self.overlay = OverlayState::ConnectionForm(Box::new(form));
        if load_cred && self.worker.is_some() {
            let req_id = self.alloc_req_id();
            self.active_cred_req_id = Some(req_id);
            if let Some(worker) = &self.worker {
                worker.send(WorkerCmd::LoadCredentialForEdit {
                    req_id,
                    connection_id,
                    service: profile.credential_service(),
                });
            }
        }
        tx.focus_when_shown("form-name", true);
    }

    pub fn open_import_url_dialog(&mut self, tx: &mut Tx) {
        self.launcher_context_menu = None;
        self.overlay = OverlayState::ImportUrl(ImportUrlState {
            url: TextEdit::new(""),
            error: None,
        });
        tx.focus_when_shown("import-url-input", true);
    }

    pub fn save_connection_form(&mut self, connect_after: bool, tx: &mut Tx) {
        let OverlayState::ConnectionForm(ref mut form) = self.overlay else {
            return;
        };
        let existing = form
            .editing_id
            .and_then(|id| self.connections.iter().find(|c| c.id == id));
        let profile = match form.to_profile(existing) {
            Ok(p) => p,
            Err(msg) => {
                form.validation_error = Some(msg);
                return;
            }
        };

        let secret = form.password.text.clone();
        let password_touched = form.password_touched;
        let is_new = form.editing_id.is_none();

        if let Some(worker) = &self.worker {
            match profile.password_mode {
                PasswordMode::Keychain if password_touched || is_new || !secret.is_empty() => {
                    worker.send(WorkerCmd::SaveCredential {
                        service: profile.credential_service(),
                        account: profile.user.clone(),
                        secret: secret.clone(),
                    });
                }
                PasswordMode::None | PasswordMode::AskEveryTime => {
                    worker.send(WorkerCmd::DeleteCredential {
                        service: profile.credential_service(),
                    });
                }
                PasswordMode::Keychain => {}
            }
        }

        let mut data = StoreData {
            version: tv_core::storage::STORE_SCHEMA_VERSION,
            folders: std::mem::take(&mut self.folders),
            connections: std::mem::take(&mut self.connections),
        };
        data.upsert(profile.clone());
        data.sort(self.sort_order);
        self.folders = data.folders;
        self.connections = data.connections;
        self.launcher_selected = Some(profile.id);
        self.persist_connections();

        for session in self.sessions.values_mut() {
            if session.profile.id == profile.id {
                session.profile = profile.clone();
            }
        }

        self.overlay = OverlayState::None;
        if connect_after {
            let explicit_pw = (!secret.is_empty()).then_some(secret);
            self.start_or_focus_connection(profile.id, explicit_pw, None, tx);
        } else {
            self.set_notice(format!("Saved connection \"{}\"", profile.name), false);
        }
    }

    pub fn test_connection_form(&mut self) {
        let req_id = self.alloc_req_id();
        let OverlayState::ConnectionForm(ref mut form) = self.overlay else {
            return;
        };
        let existing = form
            .editing_id
            .and_then(|id| self.connections.iter().find(|c| c.id == id));
        let profile = match form.to_profile(existing) {
            Ok(p) => p,
            Err(msg) => {
                form.validation_error = Some(msg);
                return;
            }
        };
        form.validation_error = None;
        form.test_status = TestStatus::Testing(req_id);
        self.active_test_req_id = Some(req_id);
        let pw_override = (!form.password.text.is_empty()).then(|| form.password.text.clone());

        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::TestConnection {
                req_id,
                profile,
                password_override: pw_override,
            });
        }
    }

    pub fn submit_import_url(&mut self, tx: &mut Tx) {
        let OverlayState::ImportUrl(ref mut modal) = self.overlay else {
            return;
        };
        let (profile, secret) = match ConnectionProfile::from_postgres_url(&modal.url.text) {
            Ok(res) => res,
            Err(err) => {
                modal.error = Some(err.to_string());
                return;
            }
        };
        let mut form = ConnectionFormState::from_profile(&profile, false);
        form.editing_id = None;
        if let Some(pw) = secret {
            form.password = TextEdit::new(&pw);
            form.password_touched = true;
        }
        self.overlay = OverlayState::ConnectionForm(Box::new(form));
        tx.focus_when_shown("form-name", true);
    }

    pub fn duplicate_connection(&mut self, id: ConnectionId) {
        let mut data = StoreData {
            version: tv_core::storage::STORE_SCHEMA_VERSION,
            folders: std::mem::take(&mut self.folders),
            connections: std::mem::take(&mut self.connections),
        };
        if let Some(copy) = data.duplicate(id) {
            self.launcher_selected = Some(copy.id);
            self.set_notice(format!("Duplicated as \"{}\"", copy.name), false);
        }
        self.folders = data.folders;
        self.connections = data.connections;
        self.launcher_context_menu = None;
        self.persist_connections();
    }

    pub fn copy_connection_url(&mut self, id: ConnectionId, tx: &mut Tx) {
        let Some(profile) = self.connections.iter().find(|c| c.id == id) else {
            return;
        };
        let url = profile.to_postgres_url(true);
        tx.copy(url);
        self.launcher_context_menu = None;
        self.set_notice("Copied connection URL (password masked)", false);
    }

    pub fn delete_connection(&mut self, id: ConnectionId) {
        let service = self
            .connections
            .iter()
            .find(|c| c.id == id)
            .map(ConnectionProfile::credential_service);
        let mut data = StoreData {
            version: tv_core::storage::STORE_SCHEMA_VERSION,
            folders: std::mem::take(&mut self.folders),
            connections: std::mem::take(&mut self.connections),
        };
        data.delete(id);
        self.folders = data.folders;
        self.connections = data.connections;
        if self.launcher_selected == Some(id) {
            self.launcher_selected = self.connections.first().map(|c| c.id);
        }
        if let Some(sid) = self.find_session_for_connection(id) {
            if let Some(worker) = &self.worker {
                worker.send(WorkerCmd::Disconnect { session_id: sid });
            }
            self.sessions.remove(&sid);
            if self.screen == Screen::Workspace(sid) {
                self.screen = Screen::Launcher;
            }
        }
        if let (Some(worker), Some(service)) = (&self.worker, service) {
            worker.send(WorkerCmd::DeleteCredential { service });
        }
        self.overlay = OverlayState::None;
        self.launcher_context_menu = None;
        self.persist_connections();
    }

    pub fn start_or_focus_connection(
        &mut self,
        connection_id: ConnectionId,
        explicit_password: Option<String>,
        database_override: Option<String>,
        tx: &mut Tx,
    ) {
        self.launcher_context_menu = None;
        if let Some(existing_sid) = self.find_session_for_connection(connection_id)
            && database_override.is_none()
            && explicit_password.is_none()
        {
            self.screen = Screen::Workspace(existing_sid);
            self.overlay = OverlayState::None;
            self.sync_window_title(tx);
            return;
        }

        let Some(profile) = self.connections.iter().find(|c| c.id == connection_id).cloned() else {
            return;
        };

        if profile.password_mode == PasswordMode::AskEveryTime && explicit_password.is_none() {
            self.overlay = OverlayState::PasswordPrompt(PasswordPromptState {
                connection_id,
                connection_name: profile.name.clone(),
                database_override,
                password: TextEdit::new(""),
                error: None,
            });
            tx.focus_when_shown("password-prompt-input", true);
            return;
        }

        let session_id = self
            .find_session_for_connection(connection_id)
            .unwrap_or_else(|| self.alloc_session_id());
        let req_id = self.alloc_req_id();
        let mut session = SessionState::new(session_id, profile.clone(), req_id);
        session.session_password = explicit_password.clone();
        self.sessions.insert(session_id, session);
        self.screen = Screen::Workspace(session_id);
        self.overlay = OverlayState::None;
        self.sync_window_title(tx);

        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::Connect {
                session_id,
                req_id,
                profile,
                prompt_password: explicit_password,
                database: database_override,
            });
        }
    }

    pub fn submit_password_prompt(&mut self, tx: &mut Tx) {
        let OverlayState::PasswordPrompt(ref prompt) = self.overlay else {
            return;
        };
        let pw = prompt.password.text.clone();
        let connection_id = prompt.connection_id;
        let database_override = prompt.database_override.clone();
        self.start_or_focus_connection(connection_id, Some(pw), database_override, tx);
    }

    pub fn select_database(&mut self, database: String, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.latest_req_id = req_id;
        let profile = session.profile.clone();
        let prompt_password = session.session_password.clone();
        if let ConnectionState::Connected(ref mut data) = session.status {
            data.active_database = Some(database.clone());
            data.selected_object = None;
            data.expanded_columns.clear();
        } else {
            session.status = ConnectionState::Connecting;
        }
        session.tabs.clear();
        session.active_tab = None;
        session.tab_drag = TabDragState::new();
        self.overlay = OverlayState::None;
        self.sync_window_title(tx);

        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::SelectDatabase {
                session_id,
                req_id,
                profile,
                prompt_password,
                database,
            });
        }
    }

    pub fn select_schema(&mut self, schema: String) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.latest_req_id = req_id;
        session.schema_dropdown_open = false;
        if let ConnectionState::Connected(ref mut data) = session.status {
            data.active_schema = Some(schema.clone());
            data.selected_object = None;
            data.expanded_columns.clear();
        }
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::SelectSchema {
                session_id,
                req_id,
                schema,
            });
        }
    }

    pub fn toggle_object_expand(&mut self, schema: String, object: String) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let key = (schema.clone(), object.clone());
        let need_load = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let Some(connected) = session.status.as_connected_mut() else {
                return;
            };
            match connected.expanded_columns.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    entry.remove();
                    false
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(ColumnLoadState::Loading);
                    true
                }
            }
        };
        if need_load && self.worker.is_some() {
            let req_id = self.alloc_req_id();
            if let Some(worker) = &self.worker {
                worker.send(WorkerCmd::LoadColumns {
                    session_id,
                    req_id,
                    schema,
                    object,
                });
            }
        }
    }

    pub fn select_object(&mut self, schema: String, object: String, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let key = (schema.clone(), object.clone());
        let has_worker = self.worker.is_some();

        let existing_tab_id = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let Some(connected) = session.status.as_connected_mut() else {
                return;
            };
            connected.selected_object = Some(key);
            session
                .tabs
                .iter()
                .find(|t| t.schema_and_name() == Some((schema.as_str(), object.as_str())))
                .map(|t| t.id)
        };

        if let Some(tab_id) = existing_tab_id {
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.active_tab = Some(tab_id);
            }
            self.sync_window_title(tx);
            return;
        }

        let tab_id = self.alloc_tab_id();
        let page_req = has_worker.then(|| self.alloc_req_id());
        let struct_req = has_worker.then(|| self.alloc_req_id());

        let page_request = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let kind = session
                .status
                .as_connected()
                .and_then(|c| {
                    c.objects
                        .iter()
                        .find(|o| o.schema == schema && o.name == object)
                        .map(|o| o.kind)
                })
                .unwrap_or(DbObjectKind::Table);
            let tab = WorkspaceTab::new_table(
                tab_id,
                schema.clone(),
                object.clone(),
                kind,
                page_req,
                struct_req,
            );
            let req = tab.as_table().map(TableTabState::page_request);
            session.tabs.push(tab);
            session.active_tab = Some(tab_id);
            req
        };

        self.sync_window_title(tx);
        if let Some(worker) = &self.worker {
            if let (Some(req_id), Some(request)) = (page_req, page_request) {
                worker.send(WorkerCmd::FetchTablePage {
                    session_id,
                    tab_id,
                    req_id,
                    request,
                });
            }
            if let Some(req_id) = struct_req {
                worker.send(WorkerCmd::DescribeTable {
                    session_id,
                    tab_id,
                    req_id,
                    schema,
                    table: object,
                });
            }
        }
    }

    pub fn activate_tab(&mut self, tab_id: TabId, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(target) = session
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .and_then(|t| t.schema_and_name().map(|(s, n)| (s.to_owned(), n.to_owned())))
        else {
            return;
        };
        session.active_tab = Some(tab_id);
        if let ConnectionState::Connected(ref mut data) = session.status {
            data.selected_object = Some(target);
        }
        self.sync_window_title(tx);
    }

    pub fn close_tab(&mut self, tab_id: TabId, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(pos) = session.tabs.iter().position(|t| t.id == tab_id) else {
            return;
        };
        session.tabs.remove(pos);
        if session.active_tab == Some(tab_id) {
            let next_id = if pos < session.tabs.len() {
                Some(session.tabs[pos].id)
            } else {
                session.tabs.last().map(|t| t.id)
            };
            session.active_tab = next_id;
        }
        let next_obj = session
            .active_tab()
            .and_then(|t| t.schema_and_name().map(|(s, n)| (s.to_owned(), n.to_owned())));
        if let ConnectionState::Connected(ref mut data) = session.status {
            data.selected_object = next_obj;
        }
        self.sync_window_title(tx);
    }

    pub fn close_active_tab(&mut self, tx: &mut Tx) -> bool {
        let Some(tab_id) = self.active_session().and_then(|s| s.active_tab) else {
            return false;
        };
        self.close_tab(tab_id, tx);
        true
    }

    pub fn cycle_tab(&mut self, delta: isize, tx: &mut Tx) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        let len = session.tabs.len();
        if len < 2 {
            return false;
        }
        let cur = session
            .active_tab
            .and_then(|id| session.tabs.iter().position(|t| t.id == id))
            .unwrap_or(0);
        let next_idx = (cur as isize + delta).rem_euclid(len as isize) as usize;
        let next_id = session.tabs[next_idx].id;
        self.activate_tab(next_id, tx);
        true
    }

    pub fn reorder_tabs(&mut self, from: usize, to: usize) {
        let Some(session) = self.active_session_mut() else {
            return;
        };
        if from >= session.tabs.len() {
            return;
        }
        let tab = session.tabs.remove(from);
        let dest = to.min(session.tabs.len());
        session.tabs.insert(dest, tab);
    }

    pub fn set_active_tab_view_mode(&mut self, mode: TableViewMode) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let has_worker = self.worker.is_some();
        let req_id = has_worker.then(|| self.alloc_req_id());
        let desc_target = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let Some(tab_id) = session.active_tab else {
                return;
            };
            let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
                return;
            };
            tab.view_mode = mode;
            tab.header_menu = None;
            tab.page_size_open = false;
            if has_worker
                && matches!(mode, TableViewMode::Structure | TableViewMode::Indexes)
                && tab.structure.is_none()
                && !tab.structure_loading
                && let Some(rid) = req_id
            {
                tab.structure_loading = true;
                tab.latest_structure_req = Some(rid);
                Some((tab_id, rid, tab.schema.clone(), tab.object_name.clone()))
            } else {
                None
            }
        };
        if let (Some(worker), Some((tab_id, rid, schema, table))) = (&self.worker, desc_target) {
            worker.send(WorkerCmd::DescribeTable {
                session_id,
                tab_id,
                req_id: rid,
                schema,
                table,
            });
        }
    }

    fn dispatch_active_tab_page(&mut self) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let has_worker = self.worker.is_some();
        let req_id = has_worker.then(|| self.alloc_req_id());
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(tab_id) = session.active_tab else {
            return;
        };
        let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
            return;
        };
        tab.header_menu = None;
        tab.page_size_open = false;
        let Some(rid) = req_id else {
            return;
        };
        tab.page_loading = true;
        tab.latest_page_req = Some(rid);
        let request = tab.page_request();
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::FetchTablePage {
                session_id,
                tab_id,
                req_id: rid,
                request,
            });
        }
    }

    pub fn apply_active_tab_filter(&mut self) {
        {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            let trimmed = tab.filter_input.text.trim();
            tab.applied_filter = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            tab.filter_error = None;
            tab.offset = 0;
            tab.selected_row = None;
            tab.data_table.clear_selection();
        }
        self.dispatch_active_tab_page();
    }

    pub fn clear_active_tab_filter(&mut self) {
        {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            tab.filter_input = TextEdit::new("");
            tab.applied_filter = None;
            tab.filter_error = None;
            tab.offset = 0;
            tab.selected_row = None;
            tab.data_table.clear_selection();
        }
        self.dispatch_active_tab_page();
    }

    pub fn sort_active_tab(&mut self, column: String, ascending: bool) {
        {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            let dir = if ascending {
                SortDir::Ascending
            } else {
                SortDir::Descending
            };
            tab.data_table.set_sort(column.clone(), dir);
            tab.sort = Some(SortSpec { column, ascending });
            tab.offset = 0;
            tab.selected_row = None;
            tab.data_table.clear_selection();
        }
        self.dispatch_active_tab_page();
    }

    pub fn clear_active_tab_sort(&mut self) {
        {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            tab.sort = None;
            tab.data_table.clear_sort();
            tab.offset = 0;
            tab.selected_row = None;
            tab.data_table.clear_selection();
        }
        self.dispatch_active_tab_page();
    }

    pub fn set_active_tab_page_size(&mut self, page_size: i64) {
        {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            tab.page_size = page_size.clamp(10, 5000);
            tab.page_size_open = false;
            tab.offset = 0;
            tab.selected_row = None;
            tab.data_table.clear_selection();
        }
        self.dispatch_active_tab_page();
    }

    pub fn paginate_active_tab(&mut self, next: bool) {
        let changed = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if next {
                let has_more = tab.page.as_ref().is_some_and(|p| p.has_more);
                if !has_more {
                    false
                } else {
                    tab.offset += tab.page_size;
                    tab.selected_row = None;
                    tab.data_table.clear_selection();
                    true
                }
            } else if tab.offset > 0 {
                tab.offset = (tab.offset - tab.page_size).max(0);
                tab.selected_row = None;
                tab.data_table.clear_selection();
                true
            } else {
                false
            }
        };
        if changed {
            self.dispatch_active_tab_page();
        }
    }

    pub fn reload_active_tab(&mut self) {
        self.dispatch_active_tab_page();
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        if self.worker.is_none() {
            return;
        }
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(tab_id) = session.active_tab else {
            return;
        };
        let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
            return;
        };
        tab.structure_loading = true;
        tab.latest_structure_req = Some(req_id);
        let schema = tab.schema.clone();
        let table = tab.object_name.clone();
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::DescribeTable {
                session_id,
                tab_id,
                req_id,
                schema,
                table,
            });
        }
    }

    pub fn select_active_tab_row(&mut self, row_id: &str) {
        let Some(idx) = row_id.strip_prefix("r-").and_then(|s| s.parse::<usize>().ok()) else {
            return;
        };
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.selected_row = Some(idx);
        }
    }

    pub fn clear_active_tab_row_selection(&mut self) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.selected_row = None;
            tab.data_table.clear_selection();
        }
    }

    pub fn reload_workspace(&mut self) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let active_schema = session
            .status
            .as_connected()
            .and_then(|c| c.active_schema.clone());
        session.latest_req_id = req_id;
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::ReloadWorkspace {
                session_id,
                req_id,
                active_schema,
            });
        }
    }

    pub fn reconnect_active(&mut self, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let active_db = session
            .status
            .as_connected()
            .and_then(|c| c.active_database.clone())
            .or_else(|| session.profile.database.clone());
        let profile = session.profile.clone();
        let pw = session.session_password.clone();
        if profile.password_mode == PasswordMode::AskEveryTime && pw.is_none() {
            let conn_id = profile.id;
            let name = profile.name.clone();
            self.overlay = OverlayState::PasswordPrompt(PasswordPromptState {
                connection_id: conn_id,
                connection_name: name,
                database_override: active_db,
                password: TextEdit::new(""),
                error: None,
            });
            tx.focus_when_shown("password-prompt-input", true);
            return;
        }
        session.latest_req_id = req_id;
        session.status = ConnectionState::Reconnecting;
        self.sync_window_title(tx);
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::Connect {
                session_id,
                req_id,
                profile,
                prompt_password: pw,
                database: active_db,
            });
        }
    }

    pub fn disconnect_active(&mut self, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.status = ConnectionState::Disconnected { reason: None };
        session.session_password = None;
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::Disconnect { session_id });
        }
        self.sync_window_title(tx);
    }

    pub fn open_picker(&mut self, kind: PickerKind, tx: &mut Tx) {
        if kind != PickerKind::Connection {
            let Some(session) = self.active_session() else {
                return;
            };
            if session.status.as_connected().is_none() {
                return;
            }
            if kind == PickerKind::QuickJump
                && session
                    .status
                    .as_connected()
                    .and_then(|c| c.active_database.as_ref())
                    .is_none()
            {
                return;
            }
        }
        self.overlay = OverlayState::Picker(PickerState::new(kind));
        tx.focus_when_shown("picker-search", true);
    }

    pub fn apply_worker_events(&mut self, tx: &mut Tx) -> bool {
        let Some(ref worker) = self.worker else {
            return false;
        };
        let events = worker.drain_events();
        if events.is_empty() {
            return false;
        }
        for event in events {
            self.handle_worker_event(event, tx);
        }
        true
    }

    fn handle_worker_event(&mut self, event: WorkerEvent, tx: &mut Tx) {
        match event {
            WorkerEvent::Connected {
                session_id,
                req_id,
                server_info,
                databases,
                active_database,
                schemas,
                active_schema,
                objects,
                logs,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if session.latest_req_id != req_id {
                    return;
                }
                append_logs(&mut session.console_entries, logs);
                session.status = ConnectionState::Connected(Box::new(ConnectedData {
                    server_version: server_info.version,
                    databases,
                    active_database,
                    schemas,
                    active_schema,
                    objects,
                    expanded_columns: HashMap::new(),
                    selected_object: None,
                }));
                let conn_id = session.profile.id;
                if let Some(profile) = self.connections.iter_mut().find(|c| c.id == conn_id) {
                    profile.last_connected_at = Some(chrono::Utc::now());
                    self.persist_connections();
                }
                if self.screen == Screen::Workspace(session_id) {
                    self.sync_window_title(tx);
                }
            }

            WorkerEvent::ConnectFailed {
                session_id,
                req_id,
                error,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if session.latest_req_id != req_id {
                    return;
                }
                if error.is_auth_failure() && session.profile.password_mode == PasswordMode::AskEveryTime {
                    let conn_id = session.profile.id;
                    let name = session.profile.name.clone();
                    let db = session.profile.database.clone();
                    session.session_password = None;
                    session.status = ConnectionState::Disconnected {
                        reason: Some(error.to_string()),
                    };
                    self.overlay = OverlayState::PasswordPrompt(PasswordPromptState {
                        connection_id: conn_id,
                        connection_name: name,
                        database_override: db,
                        password: TextEdit::new(""),
                        error: Some(error.to_string()),
                    });
                    tx.focus_when_shown("password-prompt-input", true);
                } else {
                    session.status = ConnectionState::Failed(error.to_string());
                }
                if self.screen == Screen::Workspace(session_id) {
                    self.sync_window_title(tx);
                }
            }

            WorkerEvent::SchemasAndObjectsLoaded {
                session_id,
                req_id,
                databases,
                active_database,
                schemas,
                active_schema,
                objects,
                logs,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if session.latest_req_id != req_id {
                    return;
                }
                append_logs(&mut session.console_entries, logs);
                if let ConnectionState::Connected(ref mut data) = session.status {
                    data.databases = databases;
                    data.active_database = Some(active_database);
                    data.schemas = schemas;
                    data.active_schema = active_schema;
                    data.objects = objects;
                    data.expanded_columns.clear();
                    data.selected_object = None;
                }
                session.tabs.clear();
                session.active_tab = None;
                session.tab_drag = TabDragState::new();
                if self.screen == Screen::Workspace(session_id) {
                    self.sync_window_title(tx);
                }
            }

            WorkerEvent::ObjectsLoaded {
                session_id,
                req_id,
                schema,
                objects,
                logs,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if session.latest_req_id != req_id {
                    return;
                }
                append_logs(&mut session.console_entries, logs);
                if let ConnectionState::Connected(ref mut data) = session.status
                    && data.active_schema.as_deref() == Some(&schema)
                {
                    data.objects = objects;
                }
            }

            WorkerEvent::ColumnsLoaded {
                session_id,
                req_id: _,
                schema,
                object,
                columns,
                logs,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                append_logs(&mut session.console_entries, logs);
                if let ConnectionState::Connected(ref mut data) = session.status {
                    let key = (schema, object);
                    if data.expanded_columns.contains_key(&key) {
                        let next = match columns {
                            Ok(cols) => ColumnLoadState::Loaded(cols),
                            Err(err) => ColumnLoadState::Error(err.to_string()),
                        };
                        data.expanded_columns.insert(key, next);
                    }
                }
            }

            WorkerEvent::TablePageLoaded {
                session_id,
                tab_id,
                req_id,
                result,
                logs,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                append_logs(&mut session.console_entries, logs);
                let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
                    return;
                };
                if tab.latest_page_req != Some(req_id) {
                    return;
                }
                tab.page_loading = false;
                match result {
                    Ok(page) => {
                        let row_count = page.rows.len();
                        tab.page = Some(page);
                        tab.page_error = None;
                        tab.filter_error = None;
                        if let Some(sel) = tab.selected_row
                            && sel >= row_count
                        {
                            tab.selected_row = None;
                            tab.data_table.clear_selection();
                        }
                    }
                    Err(err) => {
                        if matches!(err, tv_core::DbError::Disconnected) {
                            session.status = ConnectionState::Disconnected {
                                reason: Some(err.to_string()),
                            };
                            if self.screen == Screen::Workspace(session_id) {
                                self.sync_window_title(tx);
                            }
                        } else if tab.applied_filter.is_some() && tab.page.is_some() {
                            tab.filter_error = Some(err.to_string());
                        } else {
                            tab.page_error = Some(err.to_string());
                        }
                    }
                }
            }

            WorkerEvent::TableStructureLoaded {
                session_id,
                tab_id,
                req_id,
                result,
                logs,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                append_logs(&mut session.console_entries, logs);
                let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
                    return;
                };
                if tab.latest_structure_req != Some(req_id) {
                    return;
                }
                tab.structure_loading = false;
                match result {
                    Ok(structure) => {
                        tab.structure = Some(structure);
                        tab.structure_error = None;
                    }
                    Err(err) => {
                        tab.structure_error = Some(err.to_string());
                    }
                }
            }

            WorkerEvent::OperationFailed {
                session_id,
                req_id,
                error,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if session.latest_req_id != req_id {
                    return;
                }
                if matches!(error, tv_core::DbError::Disconnected) {
                    session.status = ConnectionState::Disconnected {
                        reason: Some(error.to_string()),
                    };
                }
                self.set_notice(error.to_string(), true);
                if self.screen == Screen::Workspace(session_id) {
                    self.sync_window_title(tx);
                }
            }

            WorkerEvent::TestResult { req_id, result } => {
                if self.active_test_req_id != Some(req_id) {
                    return;
                }
                if let OverlayState::ConnectionForm(ref mut form) = self.overlay
                    && form.test_status == TestStatus::Testing(req_id)
                {
                    form.test_status = match result {
                        Ok(info) => TestStatus::Ok(format!("Connected: {}", info.version)),
                        Err(err) => TestStatus::Err(err.to_string()),
                    };
                }
            }

            WorkerEvent::CredentialLoaded {
                req_id,
                connection_id,
                secret,
            } => {
                if self.active_cred_req_id != Some(req_id) {
                    return;
                }
                if let OverlayState::ConnectionForm(ref mut form) = self.overlay
                    && form.editing_id == Some(connection_id)
                {
                    form.loading_credential = false;
                    if !form.password_touched
                        && let Some(pw) = secret
                    {
                        form.password = TextEdit::new(&pw);
                    }
                }
            }
        }
    }

    pub fn set_notice(&mut self, message: impl Into<String>, is_error: bool) {
        self.status_notice = Some((message.into(), is_error));
    }

    pub fn sync_window_title(&self, tx: &mut Tx) {
        let title = match self.active_session() {
            None => "TableView".to_owned(),
            Some(session) => match &session.status {
                ConnectionState::Connected(data) => {
                    let active_obj = session
                        .active_tab()
                        .and_then(WorkspaceTab::schema_and_name)
                        .or_else(|| data.selected_object.as_ref().map(|(s, o)| (s.as_str(), o.as_str())));
                    match (&data.active_database, active_obj) {
                        (Some(db), Some((schema, obj))) => {
                            format!("{} — {} — {}.{}", session.profile.name, db, schema, obj)
                        }
                        (Some(db), None) => format!("{} — {}", session.profile.name, db),
                        (None, _) => format!("{} — TableView", session.profile.name),
                    }
                }
                _ => format!("{} (Disconnected) — TableView", session.profile.name),
            },
        };
        tx.set_title(title);
    }
}

fn append_logs(entries: &mut Vec<ConsoleLogEntry>, logs: Vec<ExecutedSql>) {
    for log in logs {
        entries.push(ConsoleLogEntry::from_executed(log));
    }
    const MAX_LOGS: usize = 300;
    if entries.len() > MAX_LOGS {
        let excess = entries.len() - MAX_LOGS;
        entries.drain(0..excess);
    }
}
