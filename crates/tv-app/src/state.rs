use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use rune::{ColorScheme, FileDialogOptions, PointerPress, TextEdit, Tx};
use rune_kit::diff::{DiffMode, DiffState, FileDiff};
use rune_kit::shell::TabDragState;
use rune_kit::{PaneState, SortDir, TableState};
use tv_core::{
    AccentColor, AppSettings, AppearancePreference, CellValue, ColumnMeta, ColumnSummary, ConnectionId,
    ConnectionProfile, ConnectionStore, DatabaseInfo, DbEngine, DbObject, DbObjectKind, DraftRowId, EnvTag,
    ExecutedSql, ExplainPlan, ExportFormat, FkPeekRow, ForeignKeyRef, PageRequest, PasswordMode, QueryBatchResult,
    QueryHistoryEntry, QueryStatementResult, RequestId, SafetyDecision, SafetyPolicy, SavedQuery, SavedQueryId,
    SchemaInfo, SessionId, SettingsStore, SortOrder, SortSpec, SslMode, StagedCellValue, StagedChanges,
    StatementInspection, StorageError, StoreData, TabId, TablePage, TableStructure, atomic_write_bytes,
    build_explain_sql_for_engine, build_fk_where_clause_for_engine, default_export_filename,
    evaluate_sql_safety_for_engine_with_session_mode, evaluate_staged_commit_safety, export_rows_for_engine,
};
use tv_core::sql::{
    MysqlQuoteMode, PostgresSessionMode, SqlSessionMode, batch_sets_credentials_with_session_mode, cursor_line_col,
    format_sql, mysql_slice_has_multiple_statements, postgres_slice_has_multiple_statements,
    split_statements_for_engine_with_session_mode, statement_at_cursor_for_engine_with_session_mode,
    trim_sql_whitespace_for_engine,
};

use tv_update::{UpdateFailure, UpdateStatus};

use crate::update::{UpdateBridge, UpdateHostEvent, UpdateUiState, manual_check_notice};
use crate::views::table_scroll::TableHScroll;
use crate::worker::{WorkerBridge, WorkerCmd, WorkerEvent};

pub const STATUS_NOTICE_DURATION_SECS: f64 = 4.0;
pub const STATUS_NOTICE_ERROR_DURATION_SECS: f64 = 6.0;
pub const STATUS_NOTICE_LEAVE_GRACE_SECS: f64 = 1.5;

/// Countdown state for the transient status toast banner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StatusNoticeTimer {
    Pending { duration: f64 },
    Running { due: f64 },
    Paused { remaining: f64 },
    Pinned,
}

/// Active status toast message and its auto-dismiss lifecycle.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusNotice {
    pub message: String,
    pub is_error: bool,
    pub hovered: bool,
    pub timer: StatusNoticeTimer,
}

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
    Loading(RequestId),
    Loaded(Vec<ColumnSummary>),
    Error(String),
}

/// Catalog state populated once a session connects to a database.
#[derive(Clone, Debug)]
pub struct ConnectedData {
    pub server_version: String,
    pub mode_seq: u64,
    pub mysql_quote_mode: Option<MysqlQuoteMode>,
    pub postgres_session_mode: Option<PostgresSessionMode>,
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

/// Editable fields and validation state for creating or editing a database connection.
#[derive(Clone, Debug)]
pub struct ConnectionFormState {
    pub editing_id: Option<ConnectionId>,
    pub engine: DbEngine,
    pub name: TextEdit,
    pub folder: TextEdit,
    pub host: TextEdit,
    pub port: TextEdit,
    pub user: TextEdit,
    pub password: TextEdit,
    pub database: TextEdit,
    pub file_path: TextEdit,
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
    pub saving_req_id: Option<RequestId>,
}

impl ConnectionFormState {
    pub fn new_blank() -> Self {
        Self {
            editing_id: None,
            engine: DbEngine::Postgres,
            name: TextEdit::new(""),
            folder: TextEdit::new(""),
            host: TextEdit::new("127.0.0.1"),
            port: TextEdit::new("5432"),
            user: TextEdit::new("postgres"),
            password: TextEdit::new(""),
            database: TextEdit::new(""),
            file_path: TextEdit::new(""),
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
            saving_req_id: None,
        }
    }

    pub fn set_engine(&mut self, engine: DbEngine) {
        if self.editing_id.is_some() || self.engine == engine {
            return;
        }
        self.engine = engine;
        if engine != DbEngine::Sqlite {
            let port_trimmed = self.port.text.trim();
            if port_trimmed.is_empty() || matches!(port_trimmed, "0" | "5432" | "3306") {
                self.port = TextEdit::new(engine.default_port().to_string());
            }
            let user_trimmed = self.user.text.trim();
            if user_trimmed.is_empty() || matches!(user_trimmed, "postgres" | "root") {
                self.user = TextEdit::new(engine.default_user());
            }
        }
        self.open_select = OpenFormSelect::None;
        self.validation_error = None;
        self.test_status = TestStatus::Idle;
    }

    pub fn from_profile(profile: &ConnectionProfile, loading_credential: bool) -> Self {
        Self {
            editing_id: Some(profile.id),
            engine: profile.engine,
            name: TextEdit::new(&profile.name),
            folder: TextEdit::new(profile.folder.as_deref().unwrap_or("")),
            host: TextEdit::new(&profile.host),
            port: if profile.engine == DbEngine::Sqlite {
                TextEdit::new("")
            } else {
                TextEdit::new(profile.port.to_string())
            },
            user: TextEdit::new(&profile.user),
            password: TextEdit::new(""),
            database: TextEdit::new(profile.database.as_deref().unwrap_or("")),
            file_path: TextEdit::new(
                profile
                    .file_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
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
            saving_req_id: None,
        }
    }

    pub fn to_profile(&self, existing: Option<&ConnectionProfile>) -> Result<ConnectionProfile, String> {
        let created_at = existing.map(|p| p.created_at).unwrap_or_else(chrono::Utc::now);
        let last_connected_at = existing.and_then(|p| p.last_connected_at);

        let mut profile = if self.engine == DbEngine::Sqlite {
            let file_path = non_empty_opt(&self.file_path.text).map(PathBuf::from);
            let mut name = self.name.text.trim().to_owned();
            if name.is_empty()
                && let Some(ref fp) = file_path
            {
                let path_str = fp.to_string_lossy();
                if path_str == ":memory:" {
                    name = "SQLite (memory)".to_owned();
                } else if let Some(stem) = fp.file_stem().and_then(|s| s.to_str())
                    && !stem.trim().is_empty()
                {
                    name = stem.to_owned();
                } else {
                    name = path_str.into_owned();
                }
            }
            ConnectionProfile {
                id: self.editing_id.unwrap_or_default(),
                name,
                engine: DbEngine::Sqlite,
                folder: non_empty_opt(&self.folder.text),
                env: self.env,
                color: self.color,
                safety: self.safety,
                host: String::new(),
                port: 0,
                user: String::new(),
                password_mode: PasswordMode::None,
                database: None,
                file_path,
                ssl: tv_core::SslConfig {
                    mode: SslMode::Disable,
                    ..Default::default()
                },
                created_at,
                last_connected_at,
            }
        } else {
            let host = self.host.text.trim();
            let port = if host.starts_with('/') && self.port.text.trim().is_empty() {
                self.engine.default_port()
            } else {
                self.port
                    .text
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| "Port must be a number between 1 and 65535".to_owned())?
            };
            ConnectionProfile {
                id: self.editing_id.unwrap_or_default(),
                name: self.name.text.trim().to_owned(),
                engine: self.engine,
                folder: non_empty_opt(&self.folder.text),
                env: self.env,
                color: self.color,
                safety: self.safety,
                host: host.to_owned(),
                port,
                user: self.user.text.trim().to_owned(),
                password_mode: self.password_mode,
                database: non_empty_opt(&self.database.text),
                file_path: None,
                ssl: tv_core::SslConfig {
                    mode: self.ssl_mode,
                    ca_cert_path: non_empty_opt(&self.ca_cert.text).map(Into::into),
                    client_cert_path: non_empty_opt(&self.cert.text).map(Into::into),
                    client_key_path: non_empty_opt(&self.key.text).map(Into::into),
                },
                created_at,
                last_connected_at,
            }
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

/// Modal state for importing a connection URL (`postgresql://`, `mysql://`, or `sqlite://`) into a new connection form.
#[derive(Clone, Debug)]
pub struct ImportUrlState {
    pub url: TextEdit,
    pub error: Option<String>,
}

/// Modal state for prompting a password when `PasswordMode::AskEveryTime` is used.
#[derive(Clone, Debug)]
pub struct PasswordPromptState {
    pub session_id: Option<SessionId>,
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub database_override: Option<String>,
    pub preferred_schema: Option<String>,
    pub password: TextEdit,
    pub error: Option<String>,
}

/// Modal state for saving or naming a SQL query in the local SQLite library.
#[derive(Clone, Debug)]
pub struct SaveQueryModalState {
    pub session_id: SessionId,
    pub tab_id: TabId,
    pub editing_id: Option<SavedQueryId>,
    pub name: TextEdit,
    pub folder: TextEdit,
    pub scope_all_connections: bool,
    pub error: Option<String>,
}

/// Action held until the user confirms a safety guardrail dialog.
#[derive(Clone, Debug)]
pub enum PendingProtectedAction {
    ExecuteSql {
        statements: Vec<String>,
        executed_sql_text: String,
        row_limit: Option<usize>,
        approved_sql_mode: SqlSessionMode,
    },
    ExplainAnalyze {
        statement: String,
        approved_sql_mode: SqlSessionMode,
    },
}

/// Modal state for confirming mutating or destructive SQL on protected connections.
#[derive(Clone, Debug)]
pub struct SafetyConfirmModalState {
    pub session_id: SessionId,
    pub connect_req_id: RequestId,
    pub tab_id: TabId,
    pub policy: SafetyPolicy,
    pub env: EnvTag,
    pub reason: String,
    pub statements: Vec<StatementInspection>,
    pub is_destructive: bool,
    pub destructive_ack: bool,
    pub pending: PendingProtectedAction,
}

/// Modal state for previewing and exporting tabular rows as CSV, JSON, or SQL INSERT.
#[derive(Clone, Debug)]
pub struct ExportModalState {
    pub engine: DbEngine,
    pub source_label: String,
    pub schema: Option<String>,
    pub table_name: TextEdit,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<CellValue>>,
    pub format: ExportFormat,
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
    SaveQuery(Box<SaveQueryModalState>),
    SafetyConfirm(Box<SafetyConfirmModalState>),
    Export(Box<ExportModalState>),
    DeleteConfirm(ConnectionId),
    Settings,
}

pub const CONSOLE_DEFAULT_HEIGHT: f32 = 184.0;
pub const CONSOLE_MIN_HEIGHT: f32 = 96.0;
pub const CONSOLE_MIN_STAGE_HEIGHT: f32 = 320.0;

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

/// Distinguishes loaded page rows (`0..page.rows.len()`) from staged draft inserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    Existing(usize),
    Draft(DraftRowId),
}

/// Inline editor session for one cell in a table tab.
#[derive(Clone, Debug)]
pub struct ActiveCellEdit {
    pub row_key: RowKey,
    pub col_idx: usize,
    pub edit: TextEdit,
    pub set_null: bool,
    pub set_default: bool,
}

/// Live or loading state for a foreign-key peek popover anchored next to a cell.
#[derive(Clone, Debug)]
pub struct FkPeekState {
    pub anchor_key: String,
    pub fk: ForeignKeyRef,
    pub value: CellValue,
    pub source_col: ColumnMeta,
    pub req_id: Option<RequestId>,
    pub loading: bool,
    pub result: Option<FkPeekRow>,
    pub error: Option<String>,
}

/// Interactive state for one open table or view tab (`Data | Structure | Indexes` + inspector state).
pub struct TableTabState {
    pub engine: DbEngine,
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
    pub pending_page: Option<TablePage>,
    pub page_error: Option<String>,
    pub reload_on_reconnect: bool,
    pub structure: Option<TableStructure>,
    pub structure_error: Option<String>,
    pub data_table: TableState,
    pub data_hscroll: TableHScroll,
    pub structure_table: TableState,
    pub structure_hscroll: TableHScroll,
    pub indexes_table: TableState,
    pub indexes_hscroll: TableHScroll,
    pub selected_row: Option<usize>,
    pub selected_row_key: Option<RowKey>,
    pub selected_cell: Option<(RowKey, usize)>,
    pub editing_cell: Option<ActiveCellEdit>,
    pub row_menu: Option<(RowKey, PointerPress)>,
    pub staged: StagedChanges,
    pub staged_gen: u64,
    pub commit_staged_gen: Option<u64>,
    pub diff_drawer_open: bool,
    pub diff_state: DiffState,
    pub committing: bool,
    pub commit_req_id: Option<RequestId>,
    pub commit_error: Option<String>,
    pub fk_peek: Option<FkPeekState>,
    pub auto_select_first_row: bool,
    pub inspector_search: TextEdit,
    pub inspector_json_pretty: bool,
    pub header_menu: Option<(String, PointerPress)>,
    pub ddl_expanded: bool,
    pub tab_id: TabId,
}

impl TableTabState {
    pub fn new(
        tab_id: TabId,
        engine: DbEngine,
        schema: String,
        object_name: String,
        object_kind: DbObjectKind,
        page_req: Option<RequestId>,
        structure_req: Option<RequestId>,
    ) -> Self {
        let tid = tab_id.get();
        Self {
            engine,
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
            pending_page: None,
            page_error: None,
            reload_on_reconnect: false,
            structure: None,
            structure_error: None,
            data_table: TableState::new().key(format!("tv-data-{tid}")),
            data_hscroll: TableHScroll::default(),
            structure_table: TableState::new().key(format!("tv-struct-{tid}")),
            structure_hscroll: TableHScroll::default(),
            indexes_table: TableState::new().key(format!("tv-idx-{tid}")),
            indexes_hscroll: TableHScroll::default(),
            selected_row: None,
            selected_row_key: None,
            selected_cell: None,
            editing_cell: None,
            row_menu: None,
            staged: StagedChanges::new(),
            staged_gen: 0,
            commit_staged_gen: None,
            diff_drawer_open: false,
            diff_state: DiffState::default(),
            committing: false,
            commit_req_id: None,
            commit_error: None,
            fk_peek: None,
            auto_select_first_row: false,
            inspector_search: TextEdit::new(""),
            inspector_json_pretty: true,
            header_menu: None,
            ddl_expanded: true,
            tab_id,
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

    pub fn mark_staged_mutated(&mut self) {
        self.staged_gen = self.staged_gen.wrapping_add(1);
    }

    pub fn apply_page(&mut self, page: TablePage, clear_staged: bool) {
        let row_count = page.rows.len();
        let auto_select = self.auto_select_first_row;
        self.auto_select_first_row = false;
        self.page = Some(page);
        self.pending_page = None;
        self.page_error = None;
        self.filter_error = None;
        self.reload_on_reconnect = false;
        if clear_staged {
            self.editing_cell = None;
            self.staged.clear();
            self.mark_staged_mutated();
            self.rebuild_diff_state();
        }
        if auto_select && row_count > 0 {
            self.selected_row = Some(0);
            self.selected_row_key = Some(RowKey::Existing(0));
            self.selected_cell = Some((RowKey::Existing(0), 0));
            self.data_table.select("r-0".to_owned());
        } else if let Some(sel) = self.selected_row
            && sel >= row_count
        {
            self.selected_row = None;
            self.selected_row_key = None;
            self.selected_cell = None;
            self.data_table.clear_selection();
        }
    }

    pub fn apply_pending_page_if_clean(&mut self) {
        if !self.committing
            && !self.staged.has_changes()
            && let Some(deferred) = self.pending_page.take()
        {
            self.apply_page(deferred, true);
        }
    }

    pub fn active_row_key(&self) -> Option<RowKey> {
        self.selected_row_key.or_else(|| self.selected_row.map(RowKey::Existing))
    }

    pub fn columns(&self) -> Option<&[ColumnMeta]> {
        self.page
            .as_ref()
            .map(|p| p.columns.as_slice())
            .or_else(|| self.structure.as_ref().map(|s| s.columns.as_slice()))
    }

    pub fn can_edit_table(&self, policy: SafetyPolicy) -> Result<(), &'static str> {
        if policy == SafetyPolicy::ReadOnly {
            return Err("Connection is Read-Only; editing is disabled");
        }
        if self.object_kind != DbObjectKind::Table {
            return Err("Only base tables support inline row editing");
        }
        Ok(())
    }

    pub fn has_primary_key(&self) -> bool {
        self.columns()
            .is_some_and(|cols| cols.iter().any(|c| c.is_primary_key))
    }

    pub fn visual_row_count(&self) -> usize {
        self.staged.inserts.len() + self.page.as_ref().map_or(0, |p| p.rows.len())
    }

    pub fn visual_row_key(&self, visual_idx: usize) -> Option<RowKey> {
        if let Some(draft) = self.staged.inserts.get(visual_idx) {
            return Some(RowKey::Draft(draft.id));
        }
        let page_idx = visual_idx.checked_sub(self.staged.inserts.len())?;
        let page_len = self.page.as_ref().map_or(0, |p| p.rows.len());
        (page_idx < page_len).then_some(RowKey::Existing(page_idx))
    }

    pub fn row_id_string(&self, visual_idx: usize) -> String {
        match self.visual_row_key(visual_idx) {
            Some(RowKey::Draft(id)) => id.to_string(),
            Some(RowKey::Existing(i)) => format!("r-{i}"),
            None => format!("r-missing-{visual_idx}"),
        }
    }

    pub fn row_key_to_visual_idx(&self, row_key: RowKey) -> Option<usize> {
        match row_key {
            RowKey::Draft(id) => self.staged.inserts.iter().position(|d| d.id == id),
            RowKey::Existing(idx) => {
                let page_len = self.page.as_ref().map_or(0, |p| p.rows.len());
                (idx < page_len).then_some(self.staged.inserts.len() + idx)
            }
        }
    }

    /// Returns the effective cell preview (applying pending staged values) for display & inspector.
    pub fn effective_cell(&self, row_key: RowKey, col_idx: usize) -> CellValue {
        let Some(cols) = self.columns() else {
            return CellValue::Null;
        };
        let Some(col) = cols.get(col_idx) else {
            return CellValue::Null;
        };
        match row_key {
            RowKey::Draft(id) => self
                .staged
                .insert_row(id)
                .and_then(|d| d.values.get(col_idx))
                .map_or(CellValue::Null, |v| v.to_preview_cell(col)),
            RowKey::Existing(idx) => {
                if let Some(staged_val) = self.staged.effective_existing_cell(idx, col_idx) {
                    staged_val.to_preview_cell(col)
                } else {
                    self.page
                        .as_ref()
                        .and_then(|p| p.rows.get(idx))
                        .and_then(|r| r.get(col_idx))
                        .cloned()
                        .unwrap_or(CellValue::Null)
                }
            }
        }
    }

    /// Recomputes `diff_state` from pending `staged` row changes and transactional SQL.
    pub fn rebuild_diff_state(&mut self) {
        if self.staged.is_empty() {
            self.diff_state.set_files(Vec::new());
            return;
        }
        let Some(cols) = self.columns() else {
            self.diff_state.set_files(Vec::new());
            return;
        };
        let (before, after) = self.staged.build_diff_texts(&self.schema, &self.object_name, cols);
        let row_file = FileDiff::from_texts(format!("{}.{}.rows", self.schema, self.object_name), &before, &after);
        let mut files = vec![row_file];
        match self
            .staged
            .build_transaction_sql_for_engine(self.engine, &self.schema, &self.object_name, cols)
        {
            Ok(sql) => {
                files.push(FileDiff::between(
                    None,
                    Some(format!("{}.{}.sql", self.schema, self.object_name)),
                    "",
                    &sql,
                ));
            }
            Err(err) => {
                files.push(FileDiff::between(
                    None,
                    Some(format!("{}.{}.warning.txt", self.schema, self.object_name)),
                    "",
                    &err.to_string(),
                ));
            }
        }
        self.diff_state.set_files(files);
    }
}

/// Row-limit presets offered in the SQL workbench action bar.
pub const QUERY_LIMIT_OPTIONS: &[(Option<usize>, &str)] = &[
    (Some(100), "Limit 100"),
    (Some(500), "Limit 500"),
    (Some(1000), "Limit 1,000"),
    (Some(5000), "Limit 5,000"),
    (None, "No limit"),
];

/// One statement result pane rendered under the SQL editor.
pub struct QueryResultTabState {
    pub statement: QueryStatementResult,
    pub table_key: String,
    pub table: TableState,
    pub hscroll: TableHScroll,
    pub selected_row: Option<usize>,
}

impl QueryResultTabState {
    pub fn new(tab_id: TabId, result_gen: u64, idx: usize, statement: QueryStatementResult) -> Self {
        let table_key = if result_gen == 0 {
            format!("tv-qres-{}-{idx}", tab_id.get())
        } else {
            format!("tv-qres-{}-{result_gen}-{idx}", tab_id.get())
        };
        Self {
            statement,
            table: TableState::new().key(table_key.clone()),
            table_key,
            hscroll: TableHScroll::default(),
            selected_row: None,
        }
    }
}

#[derive(Clone, Debug)]
struct HistoryIndicatorCache {
    text: String,
    engine: DbEngine,
    sql_mode: SqlSessionMode,
    current_key: Option<(usize, usize)>,
    current_excluded: bool,
    buffer_excluded: Option<bool>,
}

/// Interactive state for one SQL query workbench tab (editor, live timer, multi-result grids).
pub struct QueryTabState {
    pub tab_id: TabId,
    pub title: String,
    pub saved_query_id: Option<SavedQueryId>,
    pub editor: TextEdit,
    pub saved_snapshot: String,
    pub row_limit: Option<usize>,
    pub limit_dropdown_open: bool,
    pub record_history_override: Option<bool>,
    pub split_ratio: f32,
    pub running: bool,
    pub cancelling: bool,
    pub active_req_id: Option<RequestId>,
    pub elapsed_ms: u64,
    pub result_gen: u64,
    pub results: Vec<QueryResultTabState>,
    pub active_result: usize,
    pub batch_error: Option<String>,
    pub batch_cancelled: bool,
    pub total_duration_ms: Option<u64>,
    pub explain_plan: Option<ExplainPlan>,
    pub explain_loading: bool,
    pub explain_req_id: Option<RequestId>,
    pub explain_error: Option<String>,
    pub show_explain: bool,
    pub explain_raw_json: bool,
    pub inspector_search: TextEdit,
    pub inspector_json_pretty: bool,
    history_indicator_cache: RefCell<Option<HistoryIndicatorCache>>,
}

impl QueryTabState {
    pub fn new(
        tab_id: TabId,
        title: String,
        initial_sql: String,
        saved_query_id: Option<SavedQueryId>,
    ) -> Self {
        let editor = TextEdit::new(&initial_sql).with_line_home_end(true);
        Self {
            tab_id,
            title,
            saved_query_id,
            editor,
            saved_snapshot: initial_sql,
            row_limit: Some(1000),
            limit_dropdown_open: false,
            record_history_override: None,
            split_ratio: 0.42,
            running: false,
            cancelling: false,
            active_req_id: None,
            elapsed_ms: 0,
            result_gen: 0,
            results: Vec::new(),
            active_result: 0,
            batch_error: None,
            batch_cancelled: false,
            total_duration_ms: None,
            explain_plan: None,
            explain_loading: false,
            explain_req_id: None,
            explain_error: None,
            show_explain: false,
            explain_raw_json: false,
            inspector_search: TextEdit::new(""),
            inspector_json_pretty: true,
            history_indicator_cache: RefCell::new(None),
        }
    }

    pub fn is_dirty(&self) -> bool {
        if self.saved_query_id.is_some() {
            self.editor.text != self.saved_snapshot
        } else {
            !self.editor.text.trim().is_empty() && self.editor.text != self.saved_snapshot
        }
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        cursor_line_col(&self.editor.text, self.editor.focus)
    }

    pub fn row_limit_label(&self) -> &'static str {
        QUERY_LIMIT_OPTIONS
            .iter()
            .find(|(limit, _)| *limit == self.row_limit)
            .map_or("Limit 1,000", |(_, label)| *label)
    }

    pub fn effective_record_history(&self, global_enabled: bool) -> bool {
        self.record_history_override.unwrap_or(global_enabled)
    }

    pub fn history_indicator_exclusions(
        &self,
        engine: DbEngine,
        sql_mode: SqlSessionMode,
    ) -> (bool, bool) {
        let current_excluded = self.current_query_sets_credentials_with_session(engine, sql_mode.clone());
        if current_excluded {
            return (true, false);
        }
        let buffer_excluded = self.any_query_sets_credentials_with_session(engine, sql_mode);
        (false, buffer_excluded)
    }

    pub fn current_query_sets_credentials(
        &self,
        engine: DbEngine,
        mysql_mode: Option<MysqlQuoteMode>,
    ) -> bool {
        self.current_query_sets_credentials_with_session(engine, SqlSessionMode::mysql(mysql_mode))
    }

    pub fn current_query_sets_credentials_with_session(
        &self,
        engine: DbEngine,
        sql_mode: SqlSessionMode,
    ) -> bool {
        let cur_key = (self.editor.anchor, self.editor.focus);
        {
            let cache = self.history_indicator_cache.borrow();
            if let Some(entry) = cache.as_ref()
                && entry.engine == engine
                && entry.sql_mode == sql_mode
                && entry.text == self.editor.text
                && entry.current_key == Some(cur_key)
            {
                return entry.current_excluded;
            }
        }
        let computed = statements_for_current(&self.editor, engine, sql_mode.clone()).is_some_and(
            |(stmts, executed_sql_text)| {
                batch_sets_credentials_with_session_mode(engine, &stmts, &executed_sql_text, sql_mode.clone())
            },
        );
        let mut cache = self.history_indicator_cache.borrow_mut();
        match cache.as_mut() {
            Some(entry)
                if entry.engine == engine
                    && entry.sql_mode == sql_mode
                    && entry.text == self.editor.text =>
            {
                entry.current_key = Some(cur_key);
                entry.current_excluded = computed;
            }
            _ => {
                *cache = Some(HistoryIndicatorCache {
                    text: self.editor.text.clone(),
                    engine,
                    sql_mode,
                    current_key: Some(cur_key),
                    current_excluded: computed,
                    buffer_excluded: None,
                });
            }
        }
        computed
    }

    pub fn any_query_sets_credentials(
        &self,
        engine: DbEngine,
        mysql_mode: Option<MysqlQuoteMode>,
    ) -> bool {
        self.any_query_sets_credentials_with_session(engine, SqlSessionMode::mysql(mysql_mode))
    }

    pub fn any_query_sets_credentials_with_session(
        &self,
        engine: DbEngine,
        sql_mode: SqlSessionMode,
    ) -> bool {
        {
            let cache = self.history_indicator_cache.borrow();
            if let Some(entry) = cache.as_ref()
                && entry.engine == engine
                && entry.sql_mode == sql_mode
                && entry.text == self.editor.text
                && let Some(cached) = entry.buffer_excluded
            {
                return cached;
            }
        }
        let computed = statements_for_all(&self.editor.text, engine, sql_mode.clone()).is_some_and(
            |(stmts, executed_sql_text)| {
                batch_sets_credentials_with_session_mode(engine, &stmts, &executed_sql_text, sql_mode.clone())
            },
        );
        let mut cache = self.history_indicator_cache.borrow_mut();
        match cache.as_mut() {
            Some(entry)
                if entry.engine == engine
                    && entry.sql_mode == sql_mode
                    && entry.text == self.editor.text =>
            {
                entry.buffer_excluded = Some(computed);
            }
            _ => {
                *cache = Some(HistoryIndicatorCache {
                    text: self.editor.text.clone(),
                    engine,
                    sql_mode,
                    current_key: None,
                    current_excluded: false,
                    buffer_excluded: Some(computed),
                });
            }
        }
        computed
    }

    pub fn active_result_state(&self) -> Option<&QueryResultTabState> {
        self.results.get(self.active_result)
    }

    pub fn active_result_state_mut(&mut self) -> Option<&mut QueryResultTabState> {
        self.results.get_mut(self.active_result)
    }

    pub fn replace_results(&mut self, statements: Vec<QueryStatementResult>) {
        self.result_gen = self.result_gen.wrapping_add(1);
        let tab_id = self.tab_id;
        let result_gen = self.result_gen;
        self.results = statements
            .into_iter()
            .enumerate()
            .map(|(idx, statement)| QueryResultTabState::new(tab_id, result_gen, idx, statement))
            .collect();
        self.active_result = 0;
    }

    pub fn selected_result_row(&self) -> Option<(&[ColumnMeta], &[CellValue])> {
        let res = self.active_result_state()?;
        let row_idx = res.selected_row?;
        let row = res.statement.rows.get(row_idx)?;
        Some((&res.statement.columns, row.as_slice()))
    }
}

/// Content variants hosted inside a workspace tab.
pub enum TabContent {
    Table(Box<TableTabState>),
    Query(Box<QueryTabState>),
}

/// A tab open inside a [`SessionState`].
pub struct WorkspaceTab {
    pub id: TabId,
    pub content: TabContent,
}

impl WorkspaceTab {
    pub fn new_table(
        id: TabId,
        engine: DbEngine,
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
                engine,
                schema,
                object_name,
                object_kind,
                page_req,
                structure_req,
            ))),
        }
    }

    pub fn new_query(
        id: TabId,
        title: String,
        initial_sql: String,
        saved_query_id: Option<SavedQueryId>,
    ) -> Self {
        Self {
            id,
            content: TabContent::Query(Box::new(QueryTabState::new(
                id,
                title,
                initial_sql,
                saved_query_id,
            ))),
        }
    }

    pub fn title(&self) -> &str {
        match &self.content {
            TabContent::Table(t) => &t.object_name,
            TabContent::Query(q) => &q.title,
        }
    }

    pub fn schema_and_name(&self) -> Option<(&str, &str)> {
        match &self.content {
            TabContent::Table(t) => Some((&t.schema, &t.object_name)),
            TabContent::Query(_) => None,
        }
    }

    pub fn object_kind(&self) -> Option<DbObjectKind> {
        match &self.content {
            TabContent::Table(t) => Some(t.object_kind),
            TabContent::Query(_) => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        match &self.content {
            TabContent::Table(t) => t.page_loading || t.structure_loading || t.committing,
            TabContent::Query(q) => q.running || q.explain_loading,
        }
    }

    pub fn has_error(&self) -> bool {
        match &self.content {
            TabContent::Table(t) => {
                t.page_error.is_some()
                    || t.filter_error.is_some()
                    || t.structure_error.is_some()
                    || t.commit_error.is_some()
            }
            TabContent::Query(q) => {
                q.batch_error.is_some()
                    || q.explain_error.is_some()
                    || q.results.iter().any(|r| r.statement.error.is_some())
            }
        }
    }

    pub fn as_table(&self) -> Option<&TableTabState> {
        match &self.content {
            TabContent::Table(t) => Some(t),
            TabContent::Query(_) => None,
        }
    }

    pub fn as_table_mut(&mut self) -> Option<&mut TableTabState> {
        match &mut self.content {
            TabContent::Table(t) => Some(t),
            TabContent::Query(_) => None,
        }
    }

    pub fn as_query(&self) -> Option<&QueryTabState> {
        match &self.content {
            TabContent::Query(q) => Some(q),
            TabContent::Table(_) => None,
        }
    }

    pub fn as_query_mut(&mut self) -> Option<&mut QueryTabState> {
        match &mut self.content {
            TabContent::Query(q) => Some(q),
            TabContent::Table(_) => None,
        }
    }
}

/// Parses a `TabId::to_string()` formatted key (`"tab-12"`) back into [`TabId`].
pub fn parse_tab_id(raw: &str) -> Option<TabId> {
    raw.strip_prefix("tab-")
        .and_then(|num| num.parse::<u64>().ok())
        .map(TabId::new)
}

/// Preserved database, schema, and object selection used to restore workspace context on reconnect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconnectContext {
    pub active_database: Option<String>,
    pub active_schema: Option<String>,
    pub selected_object: Option<(String, String)>,
}

/// Self-contained state for a single database workspace session.
pub struct SessionState {
    pub id: SessionId,
    pub profile: ConnectionProfile,
    pub profile_out_of_sync: bool,
    pub session_password: Option<String>,
    pub latest_req_id: RequestId,
    pub active_connect_req_id: RequestId,
    pub pending_db_switch_req: Option<RequestId>,
    pub status: ConnectionState,
    pub reconnect_context: ReconnectContext,
    pub sidebar_tab: SidebarTab,
    pub sidebar_search: TextEdit,
    pub show_system_schemas: bool,
    pub console_open: bool,
    pub console_height: f32,
    console_drag_saved_height: Option<f32>,
    pub console_entries: Vec<ConsoleLogEntry>,
    pub schema_dropdown_open: bool,
    pub sidebar_pane: PaneState,
    pub inspector_pane: PaneState,
    pub tabs: Vec<WorkspaceTab>,
    pub active_tab: Option<TabId>,
    pub tab_drag: TabDragState,
    pub saved_queries: Vec<SavedQuery>,
    pub query_history: Vec<QueryHistoryEntry>,
    pub query_seq: u64,
}

impl SessionState {
    pub fn new(id: SessionId, profile: ConnectionProfile, req_id: RequestId) -> Self {
        Self {
            id,
            profile,
            profile_out_of_sync: false,
            session_password: None,
            latest_req_id: req_id,
            active_connect_req_id: req_id,
            pending_db_switch_req: None,
            status: ConnectionState::Connecting,
            reconnect_context: ReconnectContext::default(),
            sidebar_tab: SidebarTab::Items,
            sidebar_search: TextEdit::new(""),
            show_system_schemas: false,
            console_open: true,
            console_height: CONSOLE_DEFAULT_HEIGHT,
            console_drag_saved_height: None,
            console_entries: Vec::new(),
            schema_dropdown_open: false,
            sidebar_pane: PaneState::new(260.0).range(180.0, 440.0),
            inspector_pane: PaneState::new(292.0).range(220.0, 480.0),
            tabs: Vec::new(),
            active_tab: None,
            tab_drag: TabDragState::new(),
            saved_queries: Vec::new(),
            query_history: Vec::new(),
            query_seq: 1,
        }
    }

    /// Updates `status` while snapshotting active database, schema, and object selection when leaving [`ConnectionState::Connected`].
    pub fn set_status(&mut self, next: ConnectionState) {
        if let ConnectionState::Connected(ref data) = self.status {
            self.reconnect_context = ReconnectContext {
                active_database: data.active_database.clone(),
                active_schema: data.active_schema.clone(),
                selected_object: data.selected_object.clone(),
            };
        }
        self.status = next;
    }

    /// Returns live MySQL quoting flags reported by the connected worker session, if any.
    pub fn mysql_quote_mode(&self) -> Option<MysqlQuoteMode> {
        self.status.as_connected().and_then(|c| c.mysql_quote_mode)
    }

    /// Returns live PostgreSQL session string-escaping and transaction state reported by the worker.
    pub fn postgres_session_mode(&self) -> Option<PostgresSessionMode> {
        self.status
            .as_connected()
            .and_then(|c| c.postgres_session_mode.clone())
    }

    /// Returns live PostgreSQL `standard_conforming_strings` reported by the connected worker session.
    pub fn postgres_standard_conforming_strings(&self) -> Option<bool> {
        self.postgres_session_mode()
            .map(|mode| mode.standard_conforming_strings)
    }

    /// Returns combined live SQL session dialect flags for statement splitting, safety, and history.
    pub fn sql_mode(&self) -> SqlSessionMode {
        SqlSessionMode {
            mysql: self.mysql_quote_mode(),
            postgres: self.postgres_session_mode(),
        }
    }

    /// Returns the active database, schema, and object selection, or the last snapshotted context if disconnected.
    pub fn current_reconnect_context(&self) -> ReconnectContext {
        match &self.status {
            ConnectionState::Connected(data) => ReconnectContext {
                active_database: data.active_database.clone(),
                active_schema: data.active_schema.clone(),
                selected_object: data.selected_object.clone(),
            },
            _ => self.reconnect_context.clone(),
        }
    }

    pub fn first_dirty_table_tab_id(&self) -> Option<TabId> {
        self.tabs
            .iter()
            .filter_map(WorkspaceTab::as_table)
            .find(|t| t.staged.has_changes() || t.committing)
            .map(|t| t.tab_id)
    }

    pub fn has_staged_edits_or_committing(&self) -> bool {
        self.first_dirty_table_tab_id().is_some()
    }

    pub fn check_table_mutation_ready(&self, tab: &TableTabState) -> Result<(), &'static str> {
        if tab.committing {
            return Err("Wait for staged changes commit to finish before editing");
        }
        if self.pending_db_switch_req.is_some() {
            return Err("Wait for database switch to finish before editing");
        }
        Ok(())
    }

    pub fn console_max_height(container_h: f32) -> f32 {
        (container_h - CONSOLE_MIN_STAGE_HEIGHT).max(CONSOLE_MIN_HEIGHT)
    }

    pub fn clamped_console_height(&self, container_h: f32) -> f32 {
        let max_h = Self::console_max_height(container_h);
        if self.console_height.is_finite() {
            self.console_height.round().clamp(CONSOLE_MIN_HEIGHT, max_h)
        } else {
            CONSOLE_DEFAULT_HEIGHT.clamp(CONSOLE_MIN_HEIGHT, max_h)
        }
    }

    pub fn begin_console_drag(&mut self) {
        self.console_drag_saved_height = Some(self.console_height);
    }

    pub fn cancel_console_drag(&mut self) {
        if let Some(saved) = self.console_drag_saved_height.take() {
            self.console_height = saved;
        }
    }

    pub fn finish_console_drag(&mut self) {
        self.console_drag_saved_height = None;
    }

    pub fn set_console_height(&mut self, height: f32, container_h: f32) {
        if height.is_finite() {
            let max_h = Self::console_max_height(container_h);
            self.console_height = height.round().clamp(CONSOLE_MIN_HEIGHT, max_h);
        }
    }

    pub fn reset_console_height(&mut self) {
        self.console_height = CONSOLE_DEFAULT_HEIGHT;
        self.console_drag_saved_height = None;
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

    pub fn active_query_tab(&self) -> Option<&QueryTabState> {
        self.active_tab().and_then(WorkspaceTab::as_query)
    }

    pub fn active_query_tab_mut(&mut self) -> Option<&mut QueryTabState> {
        self.active_tab_mut().and_then(WorkspaceTab::as_query_mut)
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

/// Tracks an in-flight connection profile save waiting on keychain confirmation.
#[derive(Clone, Debug)]
pub struct PendingProfileSave {
    pub req_id: RequestId,
    pub profile: ConnectionProfile,
    pub previous_profile: Option<ConnectionProfile>,
    pub previous_folders: Vec<String>,
    pub next_store: StoreData,
    pub connect_after: bool,
    pub password_mutated: bool,
    pub deleting_credential: bool,
    pub cancelled: bool,
}

/// Tracks an in-flight connection deletion waiting on keychain removal.
#[derive(Clone, Debug)]
pub struct PendingProfileDelete {
    pub req_id: RequestId,
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub previous_profile: ConnectionProfile,
    pub previous_folders: Vec<String>,
    pub next_store: StoreData,
}

/// Top-level application state owned by Rune.
pub struct AppState {
    pub store_path: PathBuf,
    pub settings_path: PathBuf,
    pub persist_enabled: bool,
    pub store_load_error: Option<String>,
    pub connections: Vec<ConnectionProfile>,
    pub folders: Vec<String>,
    pub sort_order: SortOrder,
    pub sort_dropdown_open: bool,
    pub env_filter: Option<EnvTag>,
    pub launcher_search: TextEdit,
    pub launcher_selected: Option<ConnectionId>,
    pub launcher_context_menu: Option<(ConnectionId, PointerPress)>,
    pub delete_confirm_error: Option<String>,
    pub pending_profile_save: Option<PendingProfileSave>,
    pub pending_profile_delete: Option<PendingProfileDelete>,
    pub pending_credential_deletes: HashSet<ConnectionId>,
    pub sessions: HashMap<SessionId, SessionState>,
    pub screen: Screen,
    pub overlay: OverlayState,
    pub appearance: ColorScheme,
    pub record_query_history: bool,
    pub settings_save_error: Option<String>,
    pub status_notice: Option<StatusNotice>,
    pub worker: Option<WorkerBridge>,
    pub update: UpdateUiState,
    pub update_bridge: Option<UpdateBridge>,
    next_session_id: u64,
    next_tab_id: u64,
    next_req_id: u64,
    pub active_test_req_id: Option<RequestId>,
    pub active_cred_req_id: Option<RequestId>,
    pub fallback_pane: PaneState,
    pub fallback_edit: TextEdit,
    pub fallback_table: TableState,
    pub fallback_tab_drag: TabDragState,
    pub fallback_diff: DiffState,
}

/// Converts a persisted [`AppearancePreference`] into a Rune [`ColorScheme`].
pub const fn appearance_from_pref(pref: AppearancePreference) -> ColorScheme {
    match pref {
        AppearancePreference::System => ColorScheme::System,
        AppearancePreference::Light => ColorScheme::Light,
        AppearancePreference::Dark => ColorScheme::Dark,
    }
}

/// Converts a Rune [`ColorScheme`] into a persisted [`AppearancePreference`].
pub const fn pref_from_appearance(scheme: ColorScheme) -> AppearancePreference {
    match scheme {
        ColorScheme::System => AppearancePreference::System,
        ColorScheme::Light => AppearancePreference::Light,
        ColorScheme::Dark => AppearancePreference::Dark,
    }
}

impl AppState {
    /// Creates application state loaded from the default platform store (or memory fallback).
    pub fn init_default() -> Self {
        let (store_path, persist_enabled, store_result) = match ConnectionStore::open_default() {
            Ok(store) => {
                let path = store.path().to_path_buf();
                let result = store.load();
                (path, true, result)
            }
            Err(err) => (PathBuf::from("connections.json"), false, Err(err)),
        };
        let (settings_path, settings_result) = match SettingsStore::open_default() {
            Ok(store) => {
                let path = store.path().to_path_buf();
                let result = store.load();
                (path, result)
            }
            Err(err) => (PathBuf::from("settings.json"), Err(err)),
        };
        let mut state = Self::from_store_data(store_path, StoreData::default(), persist_enabled);
        state.apply_connections_load(store_result);
        state.settings_path = settings_path;
        state.apply_settings_load(settings_result);
        let worker = WorkerBridge::spawn();
        let update_bridge = worker
            .as_ref()
            .and_then(|w| UpdateBridge::start(&w.runtime_handle()));
        state.worker = worker;
        if let Some(ref bridge) = update_bridge {
            state.update.running_version = bridge.running_version.clone();
            state.update.snapshot = bridge.snapshot();
        }
        state.update_bridge = update_bridge;
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
            settings_path: PathBuf::from("in-memory-settings.json"),
            persist_enabled,
            store_load_error: None,
            connections: data.connections,
            folders: data.folders,
            sort_order: SortOrder::Name,
            sort_dropdown_open: false,
            env_filter: None,
            launcher_search: TextEdit::new(""),
            launcher_selected,
            launcher_context_menu: None,
            delete_confirm_error: None,
            pending_profile_save: None,
            pending_profile_delete: None,
            pending_credential_deletes: HashSet::new(),
            sessions: HashMap::new(),
            screen: Screen::Launcher,
            overlay: OverlayState::None,
            appearance: ColorScheme::System,
            record_query_history: true,
            settings_save_error: None,
            status_notice: None,
            worker: None,
            update: UpdateUiState::default(),
            update_bridge: None,
            next_session_id: 1,
            next_tab_id: 1,
            next_req_id: 1,
            active_test_req_id: None,
            active_cred_req_id: None,
            fallback_pane: PaneState::new(260.0),
            fallback_edit: TextEdit::new(""),
            fallback_table: TableState::new().key("tv-fallback-table"),
            fallback_tab_drag: TabDragState::new(),
            fallback_diff: DiffState::default(),
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
        #[derive(Clone, Copy)]
        enum Kind {
            Table,
            Query,
        }
        let found = match self.screen {
            Screen::Workspace(sid) => self.sessions.get(&sid).and_then(|sess| {
                let tab = sess.active_tab()?;
                match &tab.content {
                    TabContent::Table(_) => Some((sid, Kind::Table)),
                    TabContent::Query(_) => Some((sid, Kind::Query)),
                }
            }),
            Screen::Launcher => None,
        };
        match found {
            Some((sid, Kind::Table)) => self
                .sessions
                .get_mut(&sid)
                .and_then(SessionState::active_table_tab_mut)
                .map_or(&mut self.fallback_edit, |t| &mut t.inspector_search),
            Some((sid, Kind::Query)) => self
                .sessions
                .get_mut(&sid)
                .and_then(SessionState::active_query_tab_mut)
                .map_or(&mut self.fallback_edit, |q| &mut q.inspector_search),
            None => &mut self.fallback_edit,
        }
    }

    pub fn active_query_editor_mut(&mut self) -> &mut TextEdit {
        if let Screen::Workspace(sid) = self.screen
            && self.sessions.get(&sid).and_then(SessionState::active_query_tab).is_some()
            && let Some(tab) = self.sessions.get_mut(&sid).and_then(SessionState::active_query_tab_mut)
        {
            return &mut tab.editor;
        }
        &mut self.fallback_edit
    }

    pub fn active_query_result_table(&self) -> &TableState {
        self.active_session()
            .and_then(SessionState::active_query_tab)
            .and_then(QueryTabState::active_result_state)
            .map_or(&self.fallback_table, |r| &r.table)
    }

    pub fn active_query_result_table_mut(&mut self) -> &mut TableState {
        if let Screen::Workspace(sid) = self.screen
            && self
                .sessions
                .get(&sid)
                .and_then(SessionState::active_query_tab)
                .and_then(QueryTabState::active_result_state)
                .is_some()
            && let Some(res) = self
                .sessions
                .get_mut(&sid)
                .and_then(SessionState::active_query_tab_mut)
                .and_then(QueryTabState::active_result_state_mut)
        {
            return &mut res.table;
        }
        &mut self.fallback_table
    }

    pub fn save_query_name_mut(&mut self) -> &mut TextEdit {
        match &mut self.overlay {
            OverlayState::SaveQuery(m) => &mut m.name,
            _ => &mut self.fallback_edit,
        }
    }

    pub fn save_query_folder_mut(&mut self) -> &mut TextEdit {
        match &mut self.overlay {
            OverlayState::SaveQuery(m) => &mut m.folder,
            _ => &mut self.fallback_edit,
        }
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

    pub fn export_table_name_mut(&mut self) -> &mut TextEdit {
        match &mut self.overlay {
            OverlayState::Export(e) => &mut e.table_name,
            _ => &mut self.fallback_edit,
        }
    }

    pub fn inline_cell_edit_mut(&mut self) -> &mut TextEdit {
        if let Screen::Workspace(sid) = self.screen
            && self
                .sessions
                .get(&sid)
                .and_then(SessionState::active_table_tab)
                .is_some_and(|t| t.editing_cell.is_some())
            && let Some(tab) = self.sessions.get_mut(&sid).and_then(SessionState::active_table_tab_mut)
            && let Some(ref mut editing) = tab.editing_cell
        {
            editing.set_null = false;
            editing.set_default = false;
            return &mut editing.edit;
        }
        &mut self.fallback_edit
    }

    pub fn active_staged_diff(&self) -> &DiffState {
        self.active_session()
            .and_then(SessionState::active_table_tab)
            .map_or(&self.fallback_diff, |t| &t.diff_state)
    }

    pub fn active_staged_diff_mut(&mut self) -> &mut DiffState {
        if let Screen::Workspace(sid) = self.screen
            && self.sessions.get(&sid).and_then(SessionState::active_table_tab).is_some()
            && let Some(tab) = self.sessions.get_mut(&sid).and_then(SessionState::active_table_tab_mut)
        {
            return &mut tab.diff_state;
        }
        &mut self.fallback_diff
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

    pub fn begin_console_drag(&mut self, id: SessionId) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.begin_console_drag();
        }
    }

    pub fn cancel_console_drag(&mut self, id: SessionId) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.cancel_console_drag();
        }
    }

    pub fn finish_console_drag(&mut self, id: SessionId) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.finish_console_drag();
        }
    }

    pub fn set_console_height(&mut self, id: SessionId, height: f32, container_h: f32) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.set_console_height(height, container_h);
        }
    }

    pub fn step_console_height(&mut self, id: SessionId, delta: f32, container_h: f32) {
        if delta.is_finite()
            && let Some(session) = self.sessions.get_mut(&id)
        {
            let next = session.clamped_console_height(container_h) + delta;
            session.set_console_height(next, container_h);
        }
    }

    pub fn reset_console_height(&mut self, id: SessionId) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.reset_console_height();
        }
    }

    pub fn alloc_session_id(&mut self) -> SessionId {
        while self.sessions.contains_key(&SessionId::new(self.next_session_id)) {
            self.next_session_id += 1;
        }
        let id = SessionId::new(self.next_session_id);
        self.next_session_id += 1;
        id
    }

    pub fn alloc_tab_id(&mut self) -> TabId {
        let max_existing = self
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter().map(|t| t.id.get()))
            .max()
            .unwrap_or(0);
        if self.next_tab_id <= max_existing {
            self.next_tab_id = max_existing + 1;
        }
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
            .filter(|c| connection_matches_search(c, &q))
            .collect()
    }

    fn current_store_data(&self) -> StoreData {
        StoreData {
            version: tv_core::storage::STORE_SCHEMA_VERSION,
            folders: self.folders.clone(),
            connections: self.connections.clone(),
        }
    }

    fn save_store_data(&self, data: &StoreData) -> Result<(), StorageError> {
        if !self.persist_enabled {
            return Ok(());
        }
        if self.store_load_error.is_some() {
            return Err(StorageError::Io(
                "connection store could not be loaded; retry loading or back up the existing file first"
                    .to_owned(),
            ));
        }
        ConnectionStore::at(self.store_path.clone()).save(data)
    }

    pub fn persist_connections(&self) -> Result<(), StorageError> {
        self.save_store_data(&self.current_store_data())
    }

    /// Loads connections from `path`, preserving unreadable or corrupt files on disk.
    pub fn load_connections_from(&mut self, path: PathBuf) {
        self.store_path = path.clone();
        self.apply_connections_load(ConnectionStore::at(path).load());
    }

    pub fn retry_load_connections(&mut self) {
        let result = if self.persist_enabled {
            ConnectionStore::at(self.store_path.clone()).load()
        } else {
            match ConnectionStore::open_default() {
                Ok(store) => {
                    self.store_path = store.path().to_path_buf();
                    self.persist_enabled = true;
                    store.load()
                }
                Err(err) => Err(err),
            }
        };
        let ok = result.is_ok();
        self.apply_connections_load(result);
        if ok {
            self.set_notice("Loaded connections from disk", false);
        }
    }

    pub fn backup_and_reset_connection_store(&mut self) {
        if !self.persist_enabled {
            return;
        }
        match ConnectionStore::at(self.store_path.clone()).backup_and_reset() {
            Ok(backup_path) => {
                self.connections.clear();
                self.folders.clear();
                self.launcher_selected = None;
                self.store_load_error = None;
                let msg = match backup_path {
                    Some(path) => format!(
                        "Backed up unreadable connections file to {} and created a fresh store",
                        path.display()
                    ),
                    None => "Initialized a fresh connections store".to_owned(),
                };
                self.set_notice(msg, false);
            }
            Err(err) => {
                let msg = format!(
                    "Failed to reset connections store ({err}). Existing file was preserved on disk."
                );
                self.store_load_error = Some(msg.clone());
                self.set_notice(msg, true);
            }
        }
    }

    fn apply_connections_load(&mut self, result: Result<StoreData, StorageError>) {
        match result {
            Ok(mut data) => {
                data.sort(self.sort_order);
                self.connections = data.connections;
                self.folders = data.folders;
                self.launcher_selected = self.connections.first().map(|c| c.id);
                let had_error = self.store_load_error.take().is_some();
                if had_error
                    && self.status_notice.as_ref().is_some_and(|n| {
                        n.is_error
                            && (n.message.starts_with("Failed to load connections")
                                || n.message.starts_with("Failed to locate connections")
                                || n.message.starts_with("Failed to reset connections"))
                    })
                {
                    self.status_notice = None;
                }
            }
            Err(err) => {
                self.connections.clear();
                self.folders.clear();
                self.launcher_selected = None;
                let msg = if self.persist_enabled {
                    format!(
                        "Failed to load connections from {} ({err}). Existing file has been preserved on disk.",
                        self.store_path.display()
                    )
                } else {
                    format!("Failed to locate connections store ({err}). Persistence is disabled.")
                };
                self.store_load_error = Some(msg.clone());
                self.set_notice(msg, true);
            }
        }
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
        let mut form = ConnectionFormState::new_blank();
        if self.screen == Screen::Launcher
            && let Some(env) = self.env_filter
        {
            form.env = env;
            form.color = env.default_color();
        }
        self.overlay = OverlayState::ConnectionForm(Box::new(form));
        tx.focus_when_shown("form-name", true);
    }

    pub fn set_form_engine(&mut self, engine: DbEngine) {
        if let OverlayState::ConnectionForm(ref mut form) = self.overlay {
            form.set_engine(engine);
        }
    }

    pub fn browse_sqlite_file(&mut self, tx: &mut Tx) {
        let mut options = FileDialogOptions {
            title: "Choose SQLite Database File".to_owned(),
            ..Default::default()
        }
        .filter("SQLite Database", &["db", "sqlite", "sqlite3", "db3"]);
        if let OverlayState::ConnectionForm(ref form) = self.overlay {
            let trimmed = form.file_path.text.trim();
            if !trimmed.is_empty() && trimmed != ":memory:" {
                let path = PathBuf::from(trimmed);
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    options.directory = Some(parent.to_path_buf());
                }
            }
        }
        tx.open_file(options, |s: &mut Self, result, _| match result {
            Ok(Some(path)) => {
                if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
                    let path_str = path.to_string_lossy().into_owned();
                    form.file_path = TextEdit::new(&path_str);
                    if form.name.text.trim().is_empty()
                        && let Some(stem) = path.file_stem().and_then(|st| st.to_str())
                        && !stem.trim().is_empty()
                    {
                        form.name = TextEdit::new(stem);
                    }
                    form.validation_error = None;
                    form.test_status = TestStatus::Idle;
                }
            }
            Ok(None) => {}
            Err(err) => {
                s.set_notice(format!("File dialog failed: {err}"), true);
            }
        });
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

    pub fn open_delete_confirm(&mut self, id: ConnectionId) {
        self.launcher_context_menu = None;
        self.delete_confirm_error = None;
        self.overlay = OverlayState::DeleteConfirm(id);
    }

    pub fn close_delete_confirm(&mut self) {
        self.delete_confirm_error = None;
        self.overlay = OverlayState::None;
    }

    pub fn dismiss_connection_form(&mut self) {
        if let Some(ref mut pending) = self.pending_profile_save
            && let OverlayState::ConnectionForm(ref form) = self.overlay
            && form.saving_req_id == Some(pending.req_id)
        {
            pending.cancelled = true;
        }
        self.overlay = OverlayState::None;
    }

    pub fn save_connection_form(&mut self, connect_after: bool, tx: &mut Tx) {
        if self.pending_profile_save.is_some() || self.pending_profile_delete.is_some() {
            return;
        }

        let (profile, previous_profile, secret, password_touched, is_new) = {
            let OverlayState::ConnectionForm(ref mut form) = self.overlay else {
                return;
            };
            if form.saving_req_id.is_some() {
                return;
            }
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
            (
                profile,
                existing.cloned(),
                form.password.text.clone(),
                form.password_touched,
                form.editing_id.is_none(),
            )
        };

        let previous_folders = self.folders.clone();
        let mut next_store = self.current_store_data();
        next_store.upsert(profile.clone());
        next_store.sort(self.sort_order);

        if let Err(err) = self.save_store_data(&next_store) {
            let msg = format!("Failed to save connection profile: {err}");
            if let OverlayState::ConnectionForm(ref mut form) = self.overlay {
                form.validation_error = Some(msg.clone());
            }
            self.set_notice(msg, true);
            return;
        }

        let switched_from_keychain = previous_profile
            .as_ref()
            .is_some_and(|p| p.password_mode == PasswordMode::Keychain);
        let needs_cleanup = self.pending_credential_deletes.contains(&profile.id);

        enum CredMutation {
            SaveSecret(String),
            DeleteSecret,
        }

        let cred_mutation = if self.worker.is_some() {
            match profile.password_mode {
                PasswordMode::Keychain if !secret.is_empty() => {
                    Some(CredMutation::SaveSecret(secret.clone()))
                }
                PasswordMode::Keychain if (password_touched && !is_new) || needs_cleanup => {
                    Some(CredMutation::DeleteSecret)
                }
                PasswordMode::None | PasswordMode::AskEveryTime
                    if switched_from_keychain || needs_cleanup =>
                {
                    Some(CredMutation::DeleteSecret)
                }
                _ => None,
            }
        } else {
            None
        };
        let password_mutated = password_touched
            || switched_from_keychain != (profile.password_mode == PasswordMode::Keychain);

        if let Some(mutation) = cred_mutation {
            let deleting_credential = matches!(mutation, CredMutation::DeleteSecret);
            let req_id = self.alloc_req_id();
            if let OverlayState::ConnectionForm(ref mut form) = self.overlay {
                form.saving_req_id = Some(req_id);
                form.validation_error = None;
            }
            let service = profile.credential_service();
            let account = profile.user.clone();
            let connection_id = profile.id;
            self.pending_profile_save = Some(PendingProfileSave {
                req_id,
                profile,
                previous_profile,
                previous_folders,
                next_store,
                connect_after,
                password_mutated,
                deleting_credential,
                cancelled: false,
            });
            if let Some(worker) = &self.worker {
                match mutation {
                    CredMutation::SaveSecret(secret_val) => {
                        worker.send(WorkerCmd::SaveCredential {
                            req_id,
                            connection_id,
                            service,
                            account,
                            secret: secret_val,
                        });
                    }
                    CredMutation::DeleteSecret => {
                        worker.send(WorkerCmd::DeleteCredential {
                            req_id,
                            connection_id,
                            service,
                        });
                    }
                }
            }
            return;
        }

        let pending = PendingProfileSave {
            req_id: RequestId::new(0),
            profile: profile.clone(),
            previous_profile,
            previous_folders,
            next_store,
            connect_after,
            password_mutated,
            deleting_credential: false,
            cancelled: false,
        };
        let explicit_pw = if matches!(
            profile.password_mode,
            PasswordMode::Keychain | PasswordMode::AskEveryTime | PasswordMode::None
        ) {
            None
        } else {
            (!secret.is_empty()).then_some(secret)
        };
        self.finish_profile_save(pending, explicit_pw, tx);
    }

    fn merge_recent_timestamps(&self, target: &mut StoreData) -> bool {
        let mut merged = false;
        for conn in &mut target.connections {
            if let Some(existing) = self.connections.iter().find(|c| c.id == conn.id)
                && existing.last_connected_at > conn.last_connected_at
            {
                conn.last_connected_at = existing.last_connected_at;
                merged = true;
            }
        }
        merged
    }

    fn persist_merged_recent_if_needed(&mut self, merged_recent: bool) {
        if merged_recent
            && self.store_load_error.is_none()
            && let Err(err) = self.persist_connections()
        {
            self.set_notice(
                format!("Updated profile, but failed to update connection metadata on disk: {err}"),
                true,
            );
        }
    }

    fn focus_dirty_table_tab(&mut self, session_id: SessionId, tab_id: TabId, tx: &mut Tx) {
        self.screen = Screen::Workspace(session_id);
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.active_tab = Some(tab_id);
            let target = session
                .tabs
                .iter()
                .find(|t| t.id == tab_id)
                .and_then(WorkspaceTab::schema_and_name)
                .map(|(s, n)| (s.to_owned(), n.to_owned()));
            if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) {
                tab.diff_drawer_open = true;
            }
            if let Some(obj) = target
                && let ConnectionState::Connected(ref mut data) = session.status
            {
                data.selected_object = Some(obj);
            }
        }
        self.sync_window_title(tx);
    }

    fn apply_saved_profile_to_state(
        &mut self,
        profile: &ConnectionProfile,
        mut next_store: StoreData,
        password_mutated: bool,
    ) -> (bool, bool) {
        let merged_recent = self.merge_recent_timestamps(&mut next_store);
        self.folders = next_store.folders;
        self.connections = next_store.connections;
        self.launcher_selected = Some(profile.id);
        if self.env_filter.is_some_and(|env| env != profile.env) {
            self.env_filter = None;
        }
        let search_q = self.launcher_search.text.trim().to_ascii_lowercase();
        if !connection_matches_search(profile, &search_q) {
            self.launcher_search = TextEdit::new("");
        }

        let mut any_out_of_sync = false;
        for session in self.sessions.values_mut() {
            if session.profile.id == profile.id {
                let requires_fresh = session.profile.connection_target_changed(profile)
                    || session.profile.safety_context_changed(profile)
                    || password_mutated;
                if requires_fresh {
                    session.profile_out_of_sync = true;
                    any_out_of_sync = true;
                } else {
                    session.profile.name = profile.name.clone();
                    session.profile.folder = profile.folder.clone();
                    session.profile.color = profile.color;
                    if session.profile_out_of_sync {
                        any_out_of_sync = true;
                    }
                    if let Some(worker) = &self.worker {
                        worker.send(WorkerCmd::UpdateSessionMetadata {
                            session_id: session.id,
                            name: profile.name.clone(),
                            folder: profile.folder.clone(),
                            color: profile.color,
                        });
                    }
                }
            }
        }
        (merged_recent, any_out_of_sync)
    }

    fn finish_profile_save(
        &mut self,
        pending: PendingProfileSave,
        explicit_pw: Option<String>,
        tx: &mut Tx,
    ) {
        let (merged_recent, any_out_of_sync) = self.apply_saved_profile_to_state(
            &pending.profile,
            pending.next_store,
            pending.password_mutated,
        );
        self.overlay = OverlayState::None;
        if pending.connect_after {
            let pw = if pending.profile.password_mode == PasswordMode::Keychain {
                None
            } else {
                explicit_pw
            };
            if let Some(existing_sid) = self.find_session_for_connection(pending.profile.id) {
                let needs_fresh = self.sessions.get(&existing_sid).is_some_and(|session| {
                    session.profile_out_of_sync
                        || session.profile.connection_target_changed(&pending.profile)
                        || session.profile.safety_context_changed(&pending.profile)
                        || pending.password_mutated
                });
                if needs_fresh {
                    if let Some(dirty_tab_id) = self
                        .sessions
                        .get(&existing_sid)
                        .and_then(SessionState::first_dirty_table_tab_id)
                    {
                        self.focus_dirty_table_tab(existing_sid, dirty_tab_id, tx);
                        self.set_notice(
                            "Saved profile changes. Commit or discard staged edits before connecting to the updated target.",
                            true,
                        );
                    } else {
                        self.start_connection_inner(pending.profile.id, pw, None, true, tx);
                    }
                } else {
                    self.start_connection_inner(pending.profile.id, pw, None, false, tx);
                }
            } else {
                self.start_connection_inner(pending.profile.id, pw, None, false, tx);
            }
        } else if any_out_of_sync {
            self.set_notice(
                format!(
                    "Saved connection \"{}\". Reconnect the active session to apply updated settings.",
                    pending.profile.name
                ),
                false,
            );
        } else {
            self.set_notice(format!("Saved connection \"{}\"", pending.profile.name), false);
        }
        self.persist_merged_recent_if_needed(merged_recent);
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
        let (profile, secret) = match ConnectionProfile::from_url(&modal.url.text) {
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
        self.launcher_context_menu = None;
        if self.pending_profile_save.is_some() || self.pending_profile_delete.is_some() {
            return;
        }
        let mut next_store = self.current_store_data();
        let Some(copy) = next_store.duplicate(id) else {
            return;
        };
        if let Err(err) = self.save_store_data(&next_store) {
            self.set_notice(format!("Failed to save duplicated connection: {err}"), true);
            return;
        }
        self.folders = next_store.folders;
        self.connections = next_store.connections;
        self.launcher_selected = Some(copy.id);
        self.set_notice(format!("Duplicated as \"{}\"", copy.name), false);
    }

    pub fn copy_connection_url(&mut self, id: ConnectionId, tx: &mut Tx) {
        let Some(profile) = self.connections.iter().find(|c| c.id == id) else {
            return;
        };
        let url = profile.to_url(true);
        tx.copy(url);
        self.launcher_context_menu = None;
        self.set_notice("Copied connection URL (password masked)", false);
    }

    pub fn delete_connection(&mut self, id: ConnectionId) {
        self.launcher_context_menu = None;
        if self.pending_profile_delete.is_some() || self.pending_profile_save.is_some() {
            return;
        }
        let Some(previous_profile) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let previous_folders = self.folders.clone();
        let mut next_store = self.current_store_data();
        if !next_store.delete(id) {
            return;
        }

        if let Err(err) = self.save_store_data(&next_store) {
            let msg = format!("Failed to save connection removal: {err}");
            self.delete_confirm_error = Some(msg.clone());
            self.set_notice(msg, true);
            return;
        }

        if (previous_profile.password_mode == PasswordMode::Keychain
            || self.pending_credential_deletes.contains(&id))
            && self.worker.is_some()
        {
            let req_id = self.alloc_req_id();
            let service = previous_profile.credential_service();
            self.delete_confirm_error = None;
            self.pending_profile_delete = Some(PendingProfileDelete {
                req_id,
                connection_id: id,
                connection_name: previous_profile.name.clone(),
                previous_profile,
                previous_folders,
                next_store,
            });
            if let Some(worker) = &self.worker {
                worker.send(WorkerCmd::DeleteCredential {
                    req_id,
                    connection_id: id,
                    service,
                });
            }
            return;
        }

        let name = previous_profile.name;
        self.finish_profile_delete(id, &name, next_store, true);
    }

    fn finish_profile_delete(
        &mut self,
        id: ConnectionId,
        name: &str,
        mut next_store: StoreData,
        persist_recent: bool,
    ) {
        let merged_recent = self.merge_recent_timestamps(&mut next_store);
        self.folders = next_store.folders;
        self.connections = next_store.connections;
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
        self.delete_confirm_error = None;
        self.overlay = OverlayState::None;
        self.launcher_context_menu = None;
        self.set_notice(format!("Deleted connection \"{name}\""), false);
        if persist_recent {
            self.persist_merged_recent_if_needed(merged_recent);
        }
    }

    pub fn start_or_focus_connection(
        &mut self,
        connection_id: ConnectionId,
        explicit_password: Option<String>,
        database_override: Option<String>,
        tx: &mut Tx,
    ) {
        self.start_connection_inner(
            connection_id,
            explicit_password,
            database_override,
            false,
            tx,
        );
    }

    fn start_connection_inner(
        &mut self,
        connection_id: ConnectionId,
        explicit_password: Option<String>,
        database_override: Option<String>,
        force_connect: bool,
        tx: &mut Tx,
    ) {
        self.launcher_context_menu = None;
        if !force_connect
            && let Some(existing_sid) = self.find_session_for_connection(connection_id)
            && database_override.is_none()
            && explicit_password.is_none()
        {
            self.screen = Screen::Workspace(existing_sid);
            self.overlay = OverlayState::None;
            let should_reconnect = self.worker.is_some()
                && self.sessions.get(&existing_sid).is_some_and(|sess| {
                    matches!(
                        sess.status,
                        ConnectionState::Disconnected { .. } | ConnectionState::Failed(_)
                    )
                });
            if should_reconnect {
                self.reconnect_active(tx);
            } else {
                self.sync_window_title(tx);
            }
            return;
        }

        let Some(profile) = self.connections.iter().find(|c| c.id == connection_id).cloned() else {
            return;
        };

        if profile.engine != DbEngine::Sqlite
            && profile.password_mode == PasswordMode::AskEveryTime
            && explicit_password.is_none()
        {
            self.overlay = OverlayState::PasswordPrompt(PasswordPromptState {
                session_id: None,
                connection_id,
                connection_name: profile.name.clone(),
                database_override,
                preferred_schema: None,
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
            worker.send(WorkerCmd::LoadQueryLibrary {
                session_id,
                connection_id,
            });
            worker.send(WorkerCmd::Connect {
                session_id,
                req_id,
                profile,
                prompt_password: explicit_password,
                database: database_override,
                preferred_schema: None,
            });
        }
    }

    pub fn submit_password_prompt(&mut self, tx: &mut Tx) {
        let OverlayState::PasswordPrompt(ref prompt) = self.overlay else {
            return;
        };
        let pw = prompt.password.text.clone();
        let session_id = prompt.session_id;
        let connection_id = prompt.connection_id;
        let database_override = prompt.database_override.clone();
        let preferred_schema = prompt.preferred_schema.clone();
        if let Some(sid) = session_id
            && self.sessions.contains_key(&sid)
        {
            let requires_nonempty = self.sessions.get(&sid).is_some_and(|s| {
                s.profile_out_of_sync
                    && s.profile.engine != DbEngine::Sqlite
                    && s.profile.password_mode == PasswordMode::Keychain
            });
            if requires_nonempty && pw.is_empty() {
                if let OverlayState::PasswordPrompt(ref mut prompt) = self.overlay {
                    prompt.error = Some(
                        "Enter a password to reconnect to the previous target, or cancel and click Apply Profile."
                            .to_owned(),
                    );
                }
                tx.focus_when_shown("password-prompt-input", true);
                return;
            }
            let req_id = self.alloc_req_id();
            let profile = {
                let Some(session) = self.sessions.get_mut(&sid) else {
                    return;
                };
                session.session_password = Some(pw.clone());
                session.latest_req_id = req_id;
                session.active_connect_req_id = req_id;
                session.pending_db_switch_req = None;
                session.set_status(ConnectionState::Reconnecting);
                session.profile.clone()
            };
            self.overlay = OverlayState::None;
            self.screen = Screen::Workspace(sid);
            self.sync_window_title(tx);
            if let Some(worker) = &self.worker {
                worker.send(WorkerCmd::Connect {
                    session_id: sid,
                    req_id,
                    profile,
                    prompt_password: Some(pw),
                    database: database_override,
                    preferred_schema,
                });
            }
            return;
        }
        self.start_or_focus_connection(connection_id, Some(pw), database_override, tx);
    }

    pub fn select_database(&mut self, database: String, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        if let Some(dirty_tab_id) = self
            .sessions
            .get(&session_id)
            .and_then(SessionState::first_dirty_table_tab_id)
        {
            self.overlay = OverlayState::None;
            self.focus_dirty_table_tab(session_id, dirty_tab_id, tx);
            self.set_notice("Commit or discard staged changes before switching database", true);
            return;
        }
        let has_worker = self.worker.is_some();
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.latest_req_id = req_id;
        let profile = session.profile.clone();
        let prompt_password = session.session_password.clone();

        if has_worker && matches!(session.status, ConnectionState::Connected(_)) {
            session.pending_db_switch_req = Some(req_id);
        } else {
            session.active_connect_req_id = req_id;
            session.pending_db_switch_req = has_worker.then_some(req_id);
            if let ConnectionState::Connected(ref mut data) = session.status {
                data.active_database = Some(database.clone());
                data.selected_object = None;
                data.expanded_columns.clear();
            } else {
                session.set_status(ConnectionState::Connecting);
                session.reconnect_context = ReconnectContext {
                    active_database: Some(database.clone()),
                    active_schema: None,
                    selected_object: None,
                };
            }
            session.tabs.retain(|t| matches!(t.content, TabContent::Query(_)));
            if let Some(active) = session.active_tab
                && !session.tabs.iter().any(|t| t.id == active)
            {
                session.active_tab = session.tabs.last().map(|t| t.id);
            }
            session.tab_drag = TabDragState::new();
            self.sync_window_title(tx);
        }

        self.overlay = OverlayState::None;

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
        let connect_req_id = session.active_connect_req_id;
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
                connect_req_id,
                schema,
            });
        }
    }

    pub fn toggle_object_expand(&mut self, schema: String, object: String) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let key = (schema.clone(), object.clone());
        let req_id = self.alloc_req_id();
        let (connect_req_id, need_load) = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let connect_req_id = session.active_connect_req_id;
            let Some(connected) = session.status.as_connected_mut() else {
                return;
            };
            let need = match connected.expanded_columns.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    entry.remove();
                    false
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(ColumnLoadState::Loading(req_id));
                    true
                }
            };
            (connect_req_id, need)
        };
        if need_load && let Some(worker) = &self.worker {
            worker.send(WorkerCmd::LoadColumns {
                session_id,
                req_id,
                connect_req_id,
                schema,
                object,
            });
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

        let (connect_req_id, page_request) = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let connect_req_id = session.active_connect_req_id;
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
                session.profile.engine,
                schema.clone(),
                object.clone(),
                kind,
                page_req,
                struct_req,
            );
            let req = tab.as_table().map(TableTabState::page_request);
            session.tabs.push(tab);
            session.active_tab = Some(tab_id);
            (connect_req_id, req)
        };

        self.sync_window_title(tx);
        if let Some(worker) = &self.worker {
            if let (Some(req_id), Some(request)) = (page_req, page_request) {
                worker.send(WorkerCmd::FetchTablePage {
                    session_id,
                    tab_id,
                    req_id,
                    connect_req_id,
                    request,
                });
            }
            if let Some(req_id) = struct_req {
                worker.send(WorkerCmd::DescribeTable {
                    session_id,
                    tab_id,
                    req_id,
                    connect_req_id,
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
        let Some(tab) = session.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        let target = tab.schema_and_name().map(|(s, n)| (s.to_owned(), n.to_owned()));
        session.active_tab = Some(tab_id);
        if let Some(obj) = target
            && let ConnectionState::Connected(ref mut data) = session.status
        {
            data.selected_object = Some(obj);
        }
        self.sync_window_title(tx);
    }

    pub fn close_tab(&mut self, tab_id: TabId, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let (is_dirty, pos) = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let is_dirty = session
                .tabs
                .iter()
                .find(|t| t.id == tab_id)
                .and_then(WorkspaceTab::as_table)
                .is_some_and(|t| t.staged.has_changes() || t.committing);
            if is_dirty {
                (true, 0)
            } else {
                let Some(pos) = session.tabs.iter().position(|t| t.id == tab_id) else {
                    return;
                };
                (false, pos)
            }
        };
        if is_dirty {
            self.focus_dirty_table_tab(session_id, tab_id, tx);
            self.set_notice("Commit or discard staged changes before closing tab", true);
            return;
        }
        let Some(session) = self.sessions.get_mut(&session_id) else {
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
            let connect_req_id = session.active_connect_req_id;
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
                Some((tab_id, rid, connect_req_id, tab.schema.clone(), tab.object_name.clone()))
            } else {
                None
            }
        };
        if let (Some(worker), Some((tab_id, rid, connect_req_id, schema, table))) =
            (&self.worker, desc_target)
        {
            worker.send(WorkerCmd::DescribeTable {
                session_id,
                tab_id,
                req_id: rid,
                connect_req_id,
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
        let connect_req_id = session.active_connect_req_id;
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
        tab.pending_page = None;
        tab.latest_page_req = Some(rid);
        let request = tab.page_request();
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::FetchTablePage {
                session_id,
                tab_id,
                req_id: rid,
                connect_req_id,
                request,
            });
        }
    }

    pub fn apply_active_tab_filter(&mut self) {
        let blocked = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if tab.staged.has_changes() || tab.committing {
                tab.diff_drawer_open = true;
                true
            } else {
                let trimmed = tab.filter_input.text.trim();
                tab.applied_filter = (!trimmed.is_empty()).then(|| trimmed.to_owned());
                tab.filter_error = None;
                tab.offset = 0;
                tab.selected_row = None;
                tab.selected_row_key = None;
                tab.selected_cell = None;
                tab.editing_cell = None;
                tab.data_table.clear_selection();
                false
            }
        };
        if blocked {
            self.set_notice("Commit or discard staged changes before filtering rows", true);
            return;
        }
        self.dispatch_active_tab_page();
    }

    pub fn clear_active_tab_filter(&mut self) {
        let blocked = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if tab.staged.has_changes() || tab.committing {
                tab.diff_drawer_open = true;
                true
            } else {
                tab.filter_input = TextEdit::new("");
                tab.applied_filter = None;
                tab.filter_error = None;
                tab.offset = 0;
                tab.selected_row = None;
                tab.selected_row_key = None;
                tab.selected_cell = None;
                tab.editing_cell = None;
                tab.data_table.clear_selection();
                false
            }
        };
        if blocked {
            self.set_notice("Commit or discard staged changes before clearing filter", true);
            return;
        }
        self.dispatch_active_tab_page();
    }

    pub fn sort_active_tab(&mut self, column: String, ascending: bool) {
        let blocked = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if tab.staged.has_changes() || tab.committing {
                tab.diff_drawer_open = true;
                true
            } else {
                let dir = if ascending {
                    SortDir::Ascending
                } else {
                    SortDir::Descending
                };
                tab.data_table.set_sort(column.clone(), dir);
                tab.sort = Some(SortSpec { column, ascending });
                tab.offset = 0;
                tab.selected_row = None;
                tab.selected_row_key = None;
                tab.selected_cell = None;
                tab.editing_cell = None;
                tab.data_table.clear_selection();
                false
            }
        };
        if blocked {
            self.set_notice("Commit or discard staged changes before sorting rows", true);
            return;
        }
        self.dispatch_active_tab_page();
    }

    pub fn clear_active_tab_sort(&mut self) {
        let blocked = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if tab.staged.has_changes() || tab.committing {
                tab.diff_drawer_open = true;
                true
            } else {
                tab.sort = None;
                tab.data_table.clear_sort();
                tab.offset = 0;
                tab.selected_row = None;
                tab.selected_row_key = None;
                tab.selected_cell = None;
                tab.editing_cell = None;
                tab.data_table.clear_selection();
                false
            }
        };
        if blocked {
            self.set_notice("Commit or discard staged changes before clearing sort", true);
            return;
        }
        self.dispatch_active_tab_page();
    }

    pub fn set_active_tab_page_size(&mut self, page_size: i64) {
        let blocked = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if tab.staged.has_changes() || tab.committing {
                tab.page_size_open = false;
                tab.diff_drawer_open = true;
                true
            } else {
                tab.page_size = page_size.clamp(10, 5000);
                tab.page_size_open = false;
                tab.offset = 0;
                tab.selected_row = None;
                tab.selected_row_key = None;
                tab.selected_cell = None;
                tab.editing_cell = None;
                tab.data_table.clear_selection();
                false
            }
        };
        if blocked {
            self.set_notice("Commit or discard staged changes before changing page size", true);
            return;
        }
        self.dispatch_active_tab_page();
    }

    pub fn paginate_active_tab(&mut self, next: bool) {
        let mut blocked = false;
        let changed = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if tab.staged.has_changes() || tab.committing {
                tab.diff_drawer_open = true;
                blocked = true;
                false
            } else if next {
                let has_more = tab.page.as_ref().is_some_and(|p| p.has_more);
                if !has_more {
                    false
                } else {
                    tab.offset += tab.page_size;
                    tab.selected_row = None;
                    tab.selected_row_key = None;
                    tab.selected_cell = None;
                    tab.editing_cell = None;
                    tab.data_table.clear_selection();
                    true
                }
            } else if tab.offset > 0 {
                tab.offset = (tab.offset - tab.page_size).max(0);
                tab.selected_row = None;
                tab.selected_row_key = None;
                tab.selected_cell = None;
                tab.editing_cell = None;
                tab.data_table.clear_selection();
                true
            } else {
                false
            }
        };
        if blocked {
            self.set_notice("Commit or discard staged changes before changing page", true);
            return;
        }
        if changed {
            self.dispatch_active_tab_page();
        }
    }

    pub fn reload_active_tab(&mut self) {
        let blocked = self
            .active_session_mut()
            .and_then(SessionState::active_table_tab_mut)
            .is_some_and(|t| {
                if t.staged.has_changes() || t.committing {
                    t.diff_drawer_open = true;
                    true
                } else {
                    false
                }
            });
        if blocked {
            self.set_notice("Commit or discard staged changes before reloading table", true);
            return;
        }
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
        let connect_req_id = session.active_connect_req_id;
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
                connect_req_id,
                schema,
                table,
            });
        }
    }

    pub fn select_active_tab_row(&mut self, row_id: &str) {
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_tab_mut) else {
            return;
        };
        match &mut tab.content {
            TabContent::Table(t) => {
                if let Some(idx) = row_id.strip_prefix("r-").and_then(|s| s.parse::<usize>().ok()) {
                    t.selected_row = Some(idx);
                    t.selected_row_key = Some(RowKey::Existing(idx));
                    t.row_menu = None;
                    if t.editing_cell.as_ref().is_some_and(|e| e.row_key != RowKey::Existing(idx)) {
                        t.editing_cell = None;
                    }
                } else if let Some(draft_num) = row_id.strip_prefix("new-").and_then(|s| s.parse::<u64>().ok()) {
                    let key = RowKey::Draft(DraftRowId(draft_num));
                    t.selected_row = None;
                    t.selected_row_key = Some(key);
                    t.row_menu = None;
                    if t.editing_cell.as_ref().is_some_and(|e| e.row_key != key) {
                        t.editing_cell = None;
                    }
                }
            }
            TabContent::Query(q) => {
                if let Some(idx) = row_id.strip_prefix("r-").and_then(|s| s.parse::<usize>().ok())
                    && let Some(res) = q.active_result_state_mut()
                {
                    res.selected_row = Some(idx);
                }
            }
        }
    }

    pub fn clear_active_tab_row_selection(&mut self) {
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_tab_mut) else {
            return;
        };
        match &mut tab.content {
            TabContent::Table(t) => {
                t.selected_row = None;
                t.selected_row_key = None;
                t.selected_cell = None;
                t.editing_cell = None;
                t.data_table.clear_selection();
            }
            TabContent::Query(q) => {
                if let Some(res) = q.active_result_state_mut() {
                    res.selected_row = None;
                    res.table.clear_selection();
                }
            }
        }
    }

    pub fn open_new_query_tab(&mut self, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let tab_id = self.alloc_tab_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let seq = session.query_seq;
        session.query_seq += 1;
        let tab = WorkspaceTab::new_query(tab_id, format!("Query {seq}"), String::new(), None);
        session.tabs.push(tab);
        session.active_tab = Some(tab_id);
        let editor_key = format!("sql-editor-{}", tab_id.get());
        self.sync_window_title(tx);
        tx.focus_when_shown(editor_key, true);
    }

    pub fn insert_newline_in_active_query_editor(&mut self) {
        if self.active_session().and_then(SessionState::active_query_tab).is_some() {
            self.active_query_editor_mut().insert("\n");
        }
    }

    pub fn format_active_query_sql(&mut self) -> bool {
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_query_tab_mut) else {
            return false;
        };
        let formatted = format_sql(&tab.editor.text);
        if formatted == tab.editor.text {
            return true;
        }
        let len = tab.editor.text.len();
        tab.editor.set_selection(0, len);
        tab.editor.insert(&formatted);
        true
    }

    pub fn set_active_query_row_limit(&mut self, limit: Option<usize>) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_query_tab_mut) {
            tab.row_limit = limit;
            tab.limit_dropdown_open = false;
        }
    }

    pub fn select_active_query_result_tab(&mut self, index: usize) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_query_tab_mut)
            && index < tab.results.len()
        {
            tab.active_result = index;
            tab.show_explain = false;
        }
    }

    pub fn show_active_query_explain_tab(&mut self) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_query_tab_mut) {
            tab.show_explain = true;
        }
    }

    pub fn set_active_query_split_ratio(&mut self, ratio: f32) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_query_tab_mut) {
            tab.split_ratio = ratio.clamp(0.18, 0.80);
        }
    }

    pub fn toggle_active_inspector_json_pretty(&mut self) {
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_tab_mut) else {
            return;
        };
        match &mut tab.content {
            TabContent::Table(t) => t.inspector_json_pretty = !t.inspector_json_pretty,
            TabContent::Query(q) => q.inspector_json_pretty = !q.inspector_json_pretty,
        }
    }

    pub fn handle_primary_save(&mut self, tx: &mut Tx) -> bool {
        let is_table = self
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some();
        if is_table {
            if let Some(tab) = self.active_session().and_then(SessionState::active_table_tab)
                && tab.staged.has_changes()
            {
                self.open_staged_diff_drawer();
            } else {
                self.set_notice("No staged changes to review", false);
            }
            return true;
        }
        self.open_save_query_modal(tx)
    }

    pub fn select_table_cell(&mut self, row_key: RowKey, col_idx: usize) {
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
            return;
        };
        let row_id = match row_key {
            RowKey::Draft(id) => {
                tab.selected_row = None;
                id.to_string()
            }
            RowKey::Existing(idx) => {
                tab.selected_row = Some(idx);
                format!("r-{idx}")
            }
        };
        tab.selected_row_key = Some(row_key);
        tab.selected_cell = Some((row_key, col_idx));
        tab.row_menu = None;
        tab.data_table.select(row_id);
        if tab
            .editing_cell
            .as_ref()
            .is_some_and(|e| e.row_key != row_key || e.col_idx != col_idx)
        {
            tab.editing_cell = None;
        }
    }

    pub fn begin_cell_edit(&mut self, row_key: RowKey, col_idx: usize, tx: &mut Tx) -> bool {
        let Screen::Workspace(sid) = self.screen else {
            return false;
        };
        let Some(session) = self.sessions.get(&sid) else {
            return false;
        };
        let policy = session.profile.safety;
        let Some(tab) = session.active_table_tab() else {
            return false;
        };
        if let Err(reason) = session.check_table_mutation_ready(tab) {
            self.set_notice(reason, true);
            return false;
        }
        if let Err(reason) = tab.can_edit_table(policy) {
            self.set_notice(reason, true);
            return false;
        }
        if matches!(row_key, RowKey::Existing(_)) && !tab.has_primary_key() {
            self.set_notice("Table requires a primary key to edit existing rows safely", true);
            return false;
        }
        let current = tab.effective_cell(row_key, col_idx);
        let initial_text = match &current {
            CellValue::Null => String::new(),
            other => other.to_edit_string(),
        };
        let is_null = current.is_null();
        let is_default = matches!(row_key, RowKey::Draft(id) if tab
            .staged
            .insert_row(id)
            .and_then(|d| d.values.get(col_idx))
            .is_some_and(|v| matches!(v, StagedCellValue::DefaultExpr)));

        self.select_table_cell(row_key, col_idx);
        if let Some(tab_mut) = self.sessions.get_mut(&sid).and_then(SessionState::active_table_tab_mut) {
            if let RowKey::Existing(idx) = row_key
                && tab_mut.staged.is_row_deleted(idx)
            {
                self.set_notice("Undo deletion on this row before editing cells", true);
                return false;
            }
            let len = initial_text.len();
            let mut edit = TextEdit::new(&initial_text);
            edit.set_selection(0, len);
            tab_mut.editing_cell = Some(ActiveCellEdit {
                row_key,
                col_idx,
                edit,
                set_null: is_null,
                set_default: is_default,
            });
            tab_mut.fk_peek = None;
            tab_mut.row_menu = None;
        }
        tx.scroll_into_view("inline-editor-anchor");
        tx.focus_when_shown("inline-cell-editor", true);
        true
    }

    pub fn cancel_inline_cell_edit(&mut self) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.editing_cell = None;
        }
    }

    pub fn commit_inline_cell_edit(&mut self) {
        let blocked_reason = {
            let Some(session) = self.active_session_mut() else {
                return;
            };
            let db_switching = session.pending_db_switch_req.is_some();
            let Some(tab) = session.active_table_tab_mut() else {
                return;
            };
            if tab.committing || db_switching {
                tab.editing_cell = None;
                Some(if tab.committing {
                    "Wait for staged changes commit to finish before editing"
                } else {
                    "Wait for database switch to finish before editing"
                })
            } else {
                let Some(editing) = tab.editing_cell.take() else {
                    return;
                };
                let new_val = if editing.set_null {
                    StagedCellValue::Null
                } else if editing.set_default {
                    StagedCellValue::DefaultExpr
                } else {
                    StagedCellValue::Literal(editing.edit.text.clone())
                };
                match editing.row_key {
                    RowKey::Draft(id) => {
                        tab.staged.stage_insert_cell_edit(id, editing.col_idx, new_val);
                    }
                    RowKey::Existing(idx) => {
                        if let Some(orig_row) = tab.page.as_ref().and_then(|p| p.rows.get(idx)).cloned()
                        {
                            tab.staged.stage_cell_edit(idx, &orig_row, editing.col_idx, new_val);
                        }
                    }
                }
                tab.mark_staged_mutated();
                tab.commit_error = None;
                tab.rebuild_diff_state();
                tab.apply_pending_page_if_clean();
                None
            }
        };
        if let Some(reason) = blocked_reason {
            self.set_notice(reason, true);
        }
    }

    pub fn stage_cell_null(&mut self, row_key: RowKey, col_idx: usize) {
        let error_notice = {
            let Some(session) = self.active_session_mut() else {
                return;
            };
            let policy = session.profile.safety;
            let db_switching = session.pending_db_switch_req.is_some();
            let Some(tab) = session.active_table_tab_mut() else {
                return;
            };
            if tab.committing {
                Some("Wait for staged changes commit to finish before editing")
            } else if db_switching {
                Some("Wait for database switch to finish before editing")
            } else if let Err(reason) = tab.can_edit_table(policy) {
                Some(reason)
            } else if matches!(row_key, RowKey::Existing(_)) && !tab.has_primary_key() {
                Some("Table requires a primary key to edit existing rows safely")
            } else if tab
                .columns()
                .and_then(|c| c.get(col_idx))
                .is_some_and(|c| !c.nullable)
            {
                Some("Column is NOT NULL")
            } else {
                tab.editing_cell = None;
                match row_key {
                    RowKey::Draft(id) => {
                        tab.staged.stage_insert_cell_edit(id, col_idx, StagedCellValue::Null);
                    }
                    RowKey::Existing(idx) => {
                        if let Some(orig_row) = tab.page.as_ref().and_then(|p| p.rows.get(idx)).cloned()
                        {
                            tab.staged
                                .stage_cell_edit(idx, &orig_row, col_idx, StagedCellValue::Null);
                        }
                    }
                }
                tab.mark_staged_mutated();
                tab.commit_error = None;
                tab.rebuild_diff_state();
                tab.apply_pending_page_if_clean();
                None
            }
        };
        if let Some(msg) = error_notice {
            self.set_notice(msg, true);
        }
    }

    pub fn insert_staged_row(&mut self, tx: &mut Tx) -> bool {
        let outcome: Result<(RowKey, usize), &'static str> = {
            let Some(session) = self.active_session_mut() else {
                return false;
            };
            let policy = session.profile.safety;
            let db_switching = session.pending_db_switch_req.is_some();
            let Some(tab) = session.active_table_tab_mut() else {
                return false;
            };
            if tab.committing {
                Err("Wait for staged changes commit to finish before editing")
            } else if db_switching {
                Err("Wait for database switch to finish before editing")
            } else if let Err(reason) = tab.can_edit_table(policy) {
                Err(reason)
            } else if let Some(cols) = tab.columns().map(|c| c.to_vec()) {
                let draft_id = tab.staged.stage_insert_row(&cols);
                let col_idx = cols
                    .iter()
                    .position(|c| c.default_expr.is_none() && !c.is_primary_key)
                    .unwrap_or(0);
                tab.mark_staged_mutated();
                tab.commit_error = None;
                tab.fk_peek = None;
                tab.row_menu = None;
                tab.rebuild_diff_state();
                Ok((RowKey::Draft(draft_id), col_idx))
            } else {
                Err("Load table columns before inserting a row")
            }
        };
        match outcome {
            Ok((row_key, col_idx)) => self.begin_cell_edit(row_key, col_idx, tx),
            Err(reason) => {
                self.set_notice(reason, true);
                false
            }
        }
    }

    pub fn toggle_delete_row(&mut self, row_key: RowKey) {
        let error_notice = {
            let Some(session) = self.active_session_mut() else {
                return;
            };
            let policy = session.profile.safety;
            let db_switching = session.pending_db_switch_req.is_some();
            let Some(tab) = session.active_table_tab_mut() else {
                return;
            };
            if tab.committing {
                Some("Wait for staged changes commit to finish before editing")
            } else if db_switching {
                Some("Wait for database switch to finish before editing")
            } else if let Err(reason) = tab.can_edit_table(policy) {
                Some(reason)
            } else {
                match row_key {
                    RowKey::Draft(id) => {
                        tab.editing_cell = None;
                        tab.row_menu = None;
                        tab.staged.remove_insert_row(id);
                        if tab.selected_row_key == Some(row_key) {
                            tab.selected_row_key = None;
                            tab.selected_cell = None;
                            tab.data_table.clear_selection();
                        }
                        tab.mark_staged_mutated();
                        tab.commit_error = None;
                        tab.rebuild_diff_state();
                        tab.apply_pending_page_if_clean();
                        None
                    }
                    RowKey::Existing(idx) => {
                        if !tab.has_primary_key() {
                            Some("Table requires a primary key to delete existing rows safely")
                        } else {
                            tab.editing_cell = None;
                            tab.row_menu = None;
                            if let Some(orig_row) =
                                tab.page.as_ref().and_then(|p| p.rows.get(idx)).cloned()
                            {
                                tab.staged.toggle_delete_row(idx, &orig_row);
                                tab.mark_staged_mutated();
                            }
                            tab.commit_error = None;
                            tab.rebuild_diff_state();
                            tab.apply_pending_page_if_clean();
                            None
                        }
                    }
                }
            }
        };
        if let Some(msg) = error_notice {
            self.set_notice(msg, true);
        }
    }

    pub fn toggle_delete_selected_row(&mut self) {
        let row_key = self
            .active_session()
            .and_then(SessionState::active_table_tab)
            .and_then(TableTabState::active_row_key);
        if let Some(rk) = row_key {
            self.toggle_delete_row(rk);
        }
    }

    pub fn open_staged_diff_drawer(&mut self) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.commit_error = None;
            tab.rebuild_diff_state();
            tab.diff_drawer_open = true;
        }
    }

    pub fn close_staged_diff_drawer(&mut self) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.diff_drawer_open = false;
        }
    }

    pub fn set_staged_diff_mode(&mut self, mode: DiffMode) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.diff_state.set_mode(mode);
        }
    }

    pub fn discard_staged_changes(&mut self) {
        let blocked_reason = {
            let Some(session) = self.active_session_mut() else {
                return;
            };
            let db_switching = session.pending_db_switch_req.is_some();
            let Some(tab) = session.active_table_tab_mut() else {
                return;
            };
            if tab.committing {
                Some("Wait for staged changes commit to finish before discarding")
            } else if db_switching {
                Some("Wait for database switch to finish before discarding")
            } else {
                tab.staged.clear();
                tab.mark_staged_mutated();
                tab.editing_cell = None;
                tab.commit_error = None;
                tab.diff_drawer_open = false;
                if matches!(tab.selected_row_key, Some(RowKey::Draft(_))) {
                    tab.selected_row_key = None;
                    tab.selected_cell = None;
                    tab.data_table.clear_selection();
                }
                tab.rebuild_diff_state();
                tab.apply_pending_page_if_clean();
                None
            }
        };
        if let Some(reason) = blocked_reason {
            self.set_notice(reason, true);
        } else {
            self.set_notice("Discarded staged changes", false);
        }
    }

    pub fn commit_staged_changes(&mut self) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let req_id = self.alloc_req_id();
        let dispatch = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            if session.pending_db_switch_req.is_some() {
                None
            } else {
                let engine = session.profile.engine;
                let policy = session.profile.safety;
                let connect_req_id = session.active_connect_req_id;
                let Some(tab_id) = session.active_tab else {
                    return;
                };
                let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
                    return;
                };
                if tab.committing {
                    return;
                }
                if let Err(msg) = evaluate_staged_commit_safety(policy) {
                    tab.commit_error = Some(msg);
                    return;
                }
                let Some(cols) = tab.columns().map(|c| c.to_vec()) else {
                    tab.commit_error = Some("Missing table columns metadata".to_owned());
                    return;
                };
                let stmts = match tab
                    .staged
                    .build_statements_for_engine(engine, &tab.schema, &tab.object_name, &cols)
                {
                    Ok(s) => s,
                    Err(err) => {
                        tab.commit_error = Some(err.to_string());
                        return;
                    }
                };
                tab.editing_cell = None;
                tab.committing = true;
                tab.commit_req_id = Some(req_id);
                tab.commit_staged_gen = Some(tab.staged_gen);
                tab.commit_error = None;
                Some((tab_id, connect_req_id, stmts))
            }
        };

        let Some((tab_id, connect_req_id, statements)) = dispatch else {
            if self
                .sessions
                .get(&session_id)
                .is_some_and(|s| s.pending_db_switch_req.is_some())
            {
                self.set_notice("Wait for database switch to finish before committing staged changes", true);
            }
            return;
        };
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::CommitStagedChanges {
                session_id,
                tab_id,
                req_id,
                connect_req_id,
                statements,
            });
        } else if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.committing = false;
            tab.commit_req_id = None;
            tab.commit_staged_gen = None;
            tab.staged.clear();
            tab.mark_staged_mutated();
            tab.diff_drawer_open = false;
            tab.rebuild_diff_state();
            self.set_notice("Committed staged changes", false);
        }
    }

    pub fn open_fk_peek(
        &mut self,
        anchor_key: String,
        fk: ForeignKeyRef,
        value: CellValue,
        source_col: ColumnMeta,
    ) {
        if value.is_null() {
            return;
        }
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let has_worker = self.worker.is_some();
        let req_id = has_worker.then(|| self.alloc_req_id());
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let connect_req_id = session.active_connect_req_id;
        let Some(tab_id) = session.active_tab else {
            return;
        };
        let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
            return;
        };
        tab.fk_peek = Some(FkPeekState {
            anchor_key,
            fk: fk.clone(),
            value: value.clone(),
            source_col: source_col.clone(),
            req_id,
            loading: has_worker,
            result: None,
            error: (!has_worker).then(|| "Connect to a live database to load referenced rows.".to_owned()),
        });
        if let (Some(worker), Some(rid)) = (&self.worker, req_id) {
            worker.send(WorkerCmd::FetchFkPeek {
                session_id,
                tab_id,
                req_id: rid,
                connect_req_id,
                fk,
                value,
                source_col,
            });
        }
    }

    pub fn close_fk_peek(&mut self) {
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.fk_peek = None;
        }
    }

    pub fn jump_to_foreign_key(
        &mut self,
        fk: ForeignKeyRef,
        value: CellValue,
        source_col: ColumnMeta,
        tx: &mut Tx,
    ) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let Some(engine) = self.sessions.get(&session_id).map(|s| s.profile.engine) else {
            return;
        };
        let Some(where_clause) = build_fk_where_clause_for_engine(engine, &fk.target_column, &value, &source_col) else {
            self.set_notice("Cannot follow a NULL foreign key", false);
            return;
        };
        self.close_fk_peek();
        let (existing_tab_id, existing_dirty) = self
            .sessions
            .get(&session_id)
            .and_then(|sess| {
                sess.tabs
                    .iter()
                    .find(|t| t.schema_and_name() == Some((fk.target_schema.as_str(), fk.target_table.as_str())))
                    .map(|t| (Some(t.id), t.as_table().is_some_and(|tt| tt.staged.has_changes() || tt.committing)))
            })
            .unwrap_or((None, false));

        if let (Some(dirty_tid), true) = (existing_tab_id, existing_dirty) {
            self.focus_dirty_table_tab(session_id, dirty_tid, tx);
            self.set_notice(
                "Commit or discard staged changes on target table before filtering by foreign key",
                true,
            );
            return;
        }

        let has_worker = self.worker.is_some();
        let page_req = has_worker.then(|| self.alloc_req_id());
        let struct_req = (has_worker && existing_tab_id.is_none()).then(|| self.alloc_req_id());

        let tab_id = if let Some(tid) = existing_tab_id {
            tid
        } else {
            self.alloc_tab_id()
        };

        let (connect_req_id, request) = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let connect_req_id = session.active_connect_req_id;
            if existing_tab_id.is_none() {
                let kind = session
                    .status
                    .as_connected()
                    .and_then(|c| {
                        c.objects
                            .iter()
                            .find(|o| o.schema == fk.target_schema && o.name == fk.target_table)
                            .map(|o| o.kind)
                    })
                    .unwrap_or(DbObjectKind::Table);
                session.tabs.push(WorkspaceTab::new_table(
                    tab_id,
                    engine,
                    fk.target_schema.clone(),
                    fk.target_table.clone(),
                    kind,
                    page_req,
                    struct_req,
                ));
            }
            if let ConnectionState::Connected(ref mut data) = session.status {
                data.selected_object = Some((fk.target_schema.clone(), fk.target_table.clone()));
            }
            session.active_tab = Some(tab_id);
            let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) else {
                return;
            };
            tab.view_mode = TableViewMode::Data;
            tab.filter_input = TextEdit::new(&where_clause);
            tab.applied_filter = Some(where_clause);
            tab.filter_error = None;
            tab.sort = None;
            tab.offset = 0;
            tab.selected_row = None;
            tab.selected_row_key = None;
            tab.selected_cell = None;
            tab.editing_cell = None;
            tab.fk_peek = None;
            tab.data_table.clear_sort();
            tab.data_table.clear_selection();
            tab.auto_select_first_row = true;
            if let Some(rid) = page_req {
                tab.page_loading = true;
                tab.pending_page = None;
                tab.latest_page_req = Some(rid);
            }
            (connect_req_id, tab.page_request())
        };

        self.sync_window_title(tx);
        if let Some(worker) = &self.worker {
            if let Some(rid) = page_req {
                worker.send(WorkerCmd::FetchTablePage {
                    session_id,
                    tab_id,
                    req_id: rid,
                    connect_req_id,
                    request,
                });
            }
            if let Some(rid) = struct_req {
                worker.send(WorkerCmd::DescribeTable {
                    session_id,
                    tab_id,
                    req_id: rid,
                    connect_req_id,
                    schema: fk.target_schema,
                    table: fk.target_table,
                });
            }
        }
    }

    pub fn open_export_modal_for_active_tab(&mut self, tx: &mut Tx) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        let engine = session.profile.engine;
        let Some(tab) = session.active_tab() else {
            return false;
        };
        let modal = match &tab.content {
            TabContent::Table(t) => {
                let Some(page) = &t.page else {
                    self.set_notice("Load table rows before exporting", false);
                    return true;
                };
                if page.columns.is_empty() {
                    self.set_notice("No columns available to export", false);
                    return true;
                }
                ExportModalState {
                    engine,
                    source_label: format!("{}.{}", t.schema, t.object_name),
                    schema: Some(t.schema.clone()),
                    table_name: TextEdit::new(&t.object_name),
                    columns: page.columns.clone(),
                    rows: page.rows.clone(),
                    format: ExportFormat::Csv,
                }
            }
            TabContent::Query(q) => {
                let Some(res) = q.active_result_state() else {
                    self.set_notice("Run a query with tabular results before exporting", false);
                    return true;
                };
                if !res.statement.is_tabular() {
                    self.set_notice("Active statement has no tabular result to export", false);
                    return true;
                }
                ExportModalState {
                    engine,
                    source_label: q.title.clone(),
                    schema: None,
                    table_name: TextEdit::new("query_result"),
                    columns: res.statement.columns.clone(),
                    rows: res.statement.rows.clone(),
                    format: ExportFormat::Csv,
                }
            }
        };
        self.overlay = OverlayState::Export(Box::new(modal));
        tx.focus_when_shown("export-save-btn", true);
        true
    }

    pub fn set_export_format(&mut self, format: ExportFormat) {
        if let OverlayState::Export(ref mut modal) = self.overlay {
            modal.format = format;
        }
    }

    pub fn copy_export_to_clipboard(&mut self, tx: &mut Tx) {
        let OverlayState::Export(ref modal) = self.overlay else {
            return;
        };
        let content = export_rows_for_engine(
            modal.engine,
            modal.format,
            modal.schema.as_deref(),
            &modal.table_name.text,
            &modal.columns,
            &modal.rows,
        );
        let row_count = modal.rows.len();
        let label = modal.format.label();
        tx.copy(content);
        self.overlay = OverlayState::None;
        self.set_notice(format!("Copied {row_count} rows as {label}"), false);
    }

    pub fn save_export_to_file(&mut self, tx: &mut Tx) {
        let OverlayState::Export(ref modal) = self.overlay else {
            return;
        };
        let content = export_rows_for_engine(
            modal.engine,
            modal.format,
            modal.schema.as_deref(),
            &modal.table_name.text,
            &modal.columns,
            &modal.rows,
        );
        let filename = default_export_filename(&modal.table_name.text, modal.format);
        let row_count = modal.rows.len();
        let options = FileDialogOptions {
            title: format!("Export as {}", modal.format.label()),
            file_name: Some(filename),
            ..Default::default()
        }
        .filter(modal.format.filter_name(), &[modal.format.extension()]);

        tx.save_file(options, move |s: &mut Self, result, _| match result {
            Ok(Some(path)) => match atomic_write_bytes(&path, content.as_bytes()) {
                Ok(()) => {
                    s.overlay = OverlayState::None;
                    s.set_notice(format!("Exported {row_count} rows to {}", path.display()), false);
                }
                Err(err) => {
                    s.set_notice(format!("Failed to write export file: {err}"), true);
                }
            },
            Ok(None) => {}
            Err(err) => {
                s.set_notice(format!("Export dialog failed: {err}"), true);
            }
        });
    }

    pub fn run_active_query(&mut self, all: bool) -> bool {
        let Screen::Workspace(session_id) = self.screen else {
            return false;
        };
        struct ReadyQuery {
            connect_req_id: RequestId,
            tab_id: TabId,
            engine: DbEngine,
            sql_mode: SqlSessionMode,
            policy: SafetyPolicy,
            env: EnvTag,
            statements: Vec<String>,
            executed_sql_text: String,
            row_limit: Option<usize>,
        }
        let outcome = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return false;
            };
            let connect_req_id = session.active_connect_req_id;
            let engine = session.profile.engine;
            let policy = session.profile.safety;
            let env = session.profile.env;
            let sql_mode = session.sql_mode();
            let is_connected = session.status.as_connected().is_some();
            let Some(tab_id) = session.active_tab else {
                return false;
            };
            let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut) else {
                return false;
            };
            if tab.running {
                return true;
            }
            if !is_connected {
                tab.batch_error = Some("Database is not connected. Reconnect before running queries.".to_owned());
                tab.show_explain = false;
                return true;
            }

            let extracted = if all {
                statements_for_all(&tab.editor.text, engine, sql_mode.clone())
            } else {
                statements_for_current(&tab.editor, engine, sql_mode.clone())
            };

            let Some((statements, executed_sql_text)) = extracted else {
                self.set_notice("No SQL statement to execute", false);
                return true;
            };

            ReadyQuery {
                connect_req_id,
                tab_id,
                engine,
                sql_mode,
                policy,
                env,
                statements,
                executed_sql_text,
                row_limit: tab.row_limit,
            }
        };

        let ReadyQuery {
            connect_req_id,
            tab_id,
            engine,
            sql_mode,
            policy,
            env,
            statements,
            executed_sql_text,
            row_limit,
        } = outcome;

        match evaluate_sql_safety_for_engine_with_session_mode(
            engine,
            policy,
            env,
            &statements,
            sql_mode.clone(),
        ) {
            SafetyDecision::Allow => {
                self.dispatch_query_execution(
                    session_id,
                    tab_id,
                    statements,
                    executed_sql_text,
                    row_limit,
                    sql_mode,
                );
            }
            SafetyDecision::Block { reason, .. } => {
                if let Some(session) = self.sessions.get_mut(&session_id)
                    && let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut)
                {
                    tab.show_explain = false;
                    tab.batch_cancelled = false;
                    tab.batch_error = Some(format!("Blocked by {}: {reason}", policy.label()));
                    tab.results.clear();
                }
                self.set_notice(reason, true);
            }
            SafetyDecision::RequireConfirmation {
                policy,
                env,
                reason,
                statements: inspected,
                is_destructive,
            } => {
                self.overlay = OverlayState::SafetyConfirm(Box::new(SafetyConfirmModalState {
                    session_id,
                    connect_req_id,
                    tab_id,
                    policy,
                    env,
                    reason,
                    statements: inspected,
                    is_destructive,
                    destructive_ack: false,
                    pending: PendingProtectedAction::ExecuteSql {
                        statements,
                        executed_sql_text,
                        row_limit,
                        approved_sql_mode: sql_mode,
                    },
                }));
            }
        }
        true
    }

    pub fn run_active_query_explain(&mut self, analyze: bool) -> bool {
        let Screen::Workspace(session_id) = self.screen else {
            return false;
        };
        struct ReadyExplain {
            connect_req_id: RequestId,
            tab_id: TabId,
            engine: DbEngine,
            sql_mode: SqlSessionMode,
            policy: SafetyPolicy,
            env: EnvTag,
            statement: String,
        }
        let ReadyExplain {
            connect_req_id,
            tab_id,
            engine,
            sql_mode,
            policy,
            env,
            statement,
        } = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return false;
            };
            let connect_req_id = session.active_connect_req_id;
            let engine = session.profile.engine;
            let policy = session.profile.safety;
            let env = session.profile.env;
            let sql_mode = session.sql_mode();
            let is_connected = session.status.as_connected().is_some();
            let Some(tab_id) = session.active_tab else {
                return false;
            };
            let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut) else {
                return false;
            };
            if tab.running || tab.explain_loading {
                return true;
            }
            if !is_connected {
                tab.explain_error = Some("Database is not connected. Reconnect before running EXPLAIN.".to_owned());
                tab.show_explain = true;
                return true;
            }
            let Some((stmts, _)) = statements_for_current(&tab.editor, engine, sql_mode.clone()) else {
                self.set_notice("Place cursor inside a SQL statement to explain", false);
                return true;
            };
            if stmts.len() > 1
                || stmts.first().is_some_and(|first| {
                    (engine == DbEngine::Mysql && mysql_slice_has_multiple_statements(first, sql_mode.mysql))
                        || (engine == DbEngine::Postgres
                            && postgres_slice_has_multiple_statements(
                                first,
                                sql_mode.pg_standard_conforming_strings(),
                            ))
                })
            {
                tab.explain_error = Some(
                    "EXPLAIN requires a single SQL statement; select or split a single statement before running EXPLAIN."
                        .to_owned(),
                );
                tab.show_explain = true;
                self.set_notice(
                    "EXPLAIN requires a single SQL statement; select or split one statement",
                    true,
                );
                return true;
            }
            let first = stmts.into_iter().next().unwrap_or_default();
            ReadyExplain {
                connect_req_id,
                tab_id,
                engine,
                sql_mode,
                policy,
                env,
                statement: first,
            }
        };

        let analyze = analyze && engine.supports_explain_analyze();
        if analyze {
            let wrapped = vec![build_explain_sql_for_engine(engine, &statement, true)];
            match evaluate_sql_safety_for_engine_with_session_mode(
                engine,
                policy,
                env,
                &wrapped,
                sql_mode.clone(),
            ) {
                SafetyDecision::Allow => {}
                SafetyDecision::Block { reason, .. } => {
                    if let Some(session) = self.sessions.get_mut(&session_id)
                        && let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut)
                    {
                        tab.explain_error = Some(format!("Blocked by {}: {reason}", policy.label()));
                        tab.show_explain = true;
                    }
                    self.set_notice(reason, true);
                    return true;
                }
                SafetyDecision::RequireConfirmation {
                    policy,
                    env,
                    reason,
                    statements: inspected,
                    is_destructive,
                } => {
                    self.overlay = OverlayState::SafetyConfirm(Box::new(SafetyConfirmModalState {
                        session_id,
                        connect_req_id,
                        tab_id,
                        policy,
                        env,
                        reason,
                        statements: inspected,
                        is_destructive,
                        destructive_ack: false,
                        pending: PendingProtectedAction::ExplainAnalyze {
                            statement,
                            approved_sql_mode: sql_mode,
                        },
                    }));
                    return true;
                }
            }
        }

        self.dispatch_explain_execution(session_id, tab_id, statement, analyze, sql_mode);
        true
    }

    pub fn confirm_safety_modal(&mut self) {
        let OverlayState::SafetyConfirm(ref modal) = self.overlay else {
            return;
        };
        if modal.is_destructive && modal.env == EnvTag::Production && !modal.destructive_ack {
            self.set_notice("Confirm the Production destructive operation checkbox first", true);
            return;
        }
        let OverlayState::SafetyConfirm(modal) = std::mem::take(&mut self.overlay) else {
            return;
        };
        let approved_mode = match &modal.pending {
            PendingProtectedAction::ExecuteSql { approved_sql_mode, .. }
            | PendingProtectedAction::ExplainAnalyze { approved_sql_mode, .. } => {
                approved_sql_mode.clone()
            }
        };
        let Some(session) = self.sessions.get(&modal.session_id) else {
            self.set_notice(
                "Connection closed before confirmation; re-run to execute",
                true,
            );
            return;
        };
        if session.active_connect_req_id != modal.connect_req_id
            || session.profile.safety != modal.policy
            || session.profile.env != modal.env
        {
            self.set_notice(
                "Connection or safety policy changed before confirmation; re-run to inspect under the current connection",
                true,
            );
            return;
        }
        if session.profile.engine == DbEngine::Mysql
            && session.mysql_quote_mode() != approved_mode.mysql
        {
            self.set_notice(
                "MySQL session mode changed before confirmation; re-run to inspect under the current mode",
                true,
            );
            return;
        }
        if session.profile.engine == DbEngine::Postgres
            && approved_mode.postgres.is_some()
            && session.postgres_session_mode() != approved_mode.postgres
        {
            self.set_notice(
                "PostgreSQL string escaping mode changed before confirmation; re-run to inspect under the current mode",
                true,
            );
            return;
        }
        match modal.pending {
            PendingProtectedAction::ExecuteSql {
                statements,
                executed_sql_text,
                row_limit,
                approved_sql_mode,
            } => {
                self.dispatch_query_execution(
                    modal.session_id,
                    modal.tab_id,
                    statements,
                    executed_sql_text,
                    row_limit,
                    approved_sql_mode,
                );
            }
            PendingProtectedAction::ExplainAnalyze {
                statement,
                approved_sql_mode,
            } => {
                self.dispatch_explain_execution(
                    modal.session_id,
                    modal.tab_id,
                    statement,
                    true,
                    approved_sql_mode,
                );
            }
        }
    }

    fn dispatch_query_execution(
        &mut self,
        session_id: SessionId,
        tab_id: TabId,
        statements: Vec<String>,
        executed_sql_text: String,
        row_limit: Option<usize>,
        expected_sql_mode: SqlSessionMode,
    ) {
        let req_id = self.alloc_req_id();
        let global_record_history = self.record_query_history;
        let dispatch = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let engine = session.profile.engine;
            let connection_id = session.profile.id;
            let connection_name = session.profile.name.clone();
            let connect_req_id = session.active_connect_req_id;
            let database = session
                .status
                .as_connected()
                .and_then(|c| c.active_database.clone());
            let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut) else {
                return;
            };
            let record_history = tab.effective_record_history(global_record_history);
            tab.running = true;
            tab.cancelling = false;
            tab.active_req_id = Some(req_id);
            tab.elapsed_ms = 0;
            tab.batch_error = None;
            tab.batch_cancelled = false;
            tab.show_explain = false;
            tab.limit_dropdown_open = false;
            (engine, connect_req_id, connection_id, connection_name, database, record_history)
        };

        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::ExecuteSql {
                session_id,
                tab_id,
                req_id,
                engine: dispatch.0,
                connect_req_id: dispatch.1,
                connection_id: dispatch.2,
                connection_name: dispatch.3,
                database: dispatch.4,
                statements,
                executed_sql_text,
                row_limit,
                record_history: dispatch.5,
                expected_sql_mode,
            });
        } else if let Some(tab) = self
            .sessions
            .get_mut(&session_id)
            .and_then(|s| s.tab_mut(tab_id))
            .and_then(WorkspaceTab::as_query_mut)
        {
            tab.running = false;
            tab.active_req_id = None;
        }
    }

    fn dispatch_explain_execution(
        &mut self,
        session_id: SessionId,
        tab_id: TabId,
        statement: String,
        analyze: bool,
        expected_sql_mode: SqlSessionMode,
    ) {
        let req_id = self.alloc_req_id();
        let connect_req_id = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let cid = session.active_connect_req_id;
            if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut) {
                tab.explain_loading = self.worker.is_some();
                tab.explain_req_id = self.worker.as_ref().map(|_| req_id);
                tab.explain_error = None;
                tab.show_explain = true;
            }
            cid
        };
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::ExplainSql {
                session_id,
                tab_id,
                req_id,
                connect_req_id,
                statement,
                analyze,
                expected_sql_mode,
            });
        }
    }

    pub fn cancel_active_query(&mut self) -> bool {
        let Screen::Workspace(session_id) = self.screen else {
            return false;
        };
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_query_tab_mut) else {
            return false;
        };
        if !tab.running {
            return false;
        }
        if !tab.cancelling {
            tab.cancelling = true;
            if let Some(req_id) = tab.active_req_id
                && let Some(worker) = &self.worker
            {
                worker.send(WorkerCmd::CancelQuery { session_id, req_id });
            }
        }
        true
    }

    pub fn open_save_query_modal(&mut self, tx: &mut Tx) -> bool {
        let Screen::Workspace(session_id) = self.screen else {
            return false;
        };
        let info = {
            let Some(session) = self.sessions.get(&session_id) else {
                return false;
            };
            let connection_id = session.profile.id;
            let Some(tab) = session.active_query_tab() else {
                return false;
            };
            let existing = tab
                .saved_query_id
                .and_then(|qid| session.saved_queries.iter().find(|s| s.id == qid))
                .cloned();
            (
                tab.tab_id,
                tab.title.clone(),
                tab.editor.text.clone(),
                tab.saved_query_id,
                connection_id,
                existing,
            )
        };
        let (tab_id, title, sql, saved_id, session_connection_id, existing) = info;

        if let (Some(id), Some(existing_q)) = (saved_id, existing) {
            if let Some(worker) = &self.worker {
                worker.send(WorkerCmd::SaveQuery {
                    session_id,
                    tab_id: Some(tab_id),
                    session_connection_id,
                    connection_id: existing_q.connection_id,
                    id: Some(id),
                    folder: existing_q.folder,
                    name: existing_q.name.clone(),
                    sql,
                });
            }
            self.set_notice(format!("Saved \"{}\"", existing_q.name), false);
            return true;
        }

        self.overlay = OverlayState::SaveQuery(Box::new(SaveQueryModalState {
            session_id,
            tab_id,
            editing_id: saved_id,
            name: TextEdit::new(&title),
            folder: TextEdit::new(""),
            scope_all_connections: false,
            error: None,
        }));
        tx.focus_when_shown("save-query-name", true);
        true
    }

    pub fn submit_save_query_modal(&mut self, tx: &mut Tx) {
        let OverlayState::SaveQuery(ref mut modal) = self.overlay else {
            return;
        };
        let name = modal.name.text.trim().to_owned();
        if name.is_empty() {
            modal.error = Some("Query name cannot be empty".to_owned());
            return;
        }
        let folder = non_empty_opt(&modal.folder.text);
        let session_id = modal.session_id;
        let tab_id = modal.tab_id;
        let editing_id = modal.editing_id;
        let scope_all = modal.scope_all_connections;

        let Some(session) = self.sessions.get_mut(&session_id) else {
            self.overlay = OverlayState::None;
            return;
        };
        let session_connection_id = session.profile.id;
        let target_connection_id = if scope_all { None } else { Some(session_connection_id) };
        let sql = session
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .and_then(WorkspaceTab::as_query)
            .map(|q| q.editor.text.clone())
            .unwrap_or_default();

        let local_id = editing_id.unwrap_or_default();
        if self.worker.is_none() {
            // Update local in-memory state immediately in headless mode.
            let saved = SavedQuery {
                id: local_id,
                connection_id: target_connection_id,
                folder,
                name: name.clone(),
                sql: sql.clone(),
                updated_at: chrono::Utc::now(),
            };
            session.saved_queries.retain(|q| q.id != local_id);
            session.saved_queries.insert(0, saved);
            if let Some(q) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut) {
                q.saved_query_id = Some(local_id);
                q.title = name.clone();
                q.saved_snapshot = sql;
            }
        } else if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::SaveQuery {
                session_id,
                tab_id: Some(tab_id),
                session_connection_id,
                connection_id: target_connection_id,
                id: editing_id,
                folder,
                name: name.clone(),
                sql,
            });
        }
        self.overlay = OverlayState::None;
        self.sync_window_title(tx);
        self.set_notice(format!("Saved query \"{name}\""), false);
    }

    pub fn open_saved_query(&mut self, query_id: SavedQueryId, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let existing_tab = self.sessions.get(&session_id).and_then(|s| {
            s.tabs
                .iter()
                .find(|t| t.as_query().is_some_and(|q| q.saved_query_id == Some(query_id)))
                .map(|t| t.id)
        });
        if let Some(tab_id) = existing_tab {
            self.activate_tab(tab_id, tx);
            tx.focus_when_shown(format!("sql-editor-{}", tab_id.get()), true);
            return;
        }
        let tab_id = self.alloc_tab_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(saved) = session.saved_queries.iter().find(|q| q.id == query_id).cloned() else {
            return;
        };
        let tab = WorkspaceTab::new_query(tab_id, saved.name, saved.sql, Some(saved.id));
        session.tabs.push(tab);
        session.active_tab = Some(tab_id);
        self.sync_window_title(tx);
        tx.focus_when_shown(format!("sql-editor-{}", tab_id.get()), true);
    }

    pub fn open_history_entry(&mut self, entry_id: i64, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let tab_id = self.alloc_tab_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(entry) = session.query_history.iter().find(|h| h.id == entry_id).cloned() else {
            return;
        };
        let seq = session.query_seq;
        session.query_seq += 1;
        let title = concise_sql_title(&entry.sql, seq);
        let tab = WorkspaceTab::new_query(tab_id, title, entry.sql, None);
        session.tabs.push(tab);
        session.active_tab = Some(tab_id);
        self.sync_window_title(tx);
        tx.focus_when_shown(format!("sql-editor-{}", tab_id.get()), true);
    }

    pub fn delete_saved_query(&mut self, id: SavedQueryId) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let connection_id = session.profile.id;
        session.saved_queries.retain(|q| q.id != id);
        for tab in &mut session.tabs {
            if let Some(q) = tab.as_query_mut()
                && q.saved_query_id == Some(id)
            {
                q.saved_query_id = None;
            }
        }
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::DeleteSavedQuery {
                session_id,
                connection_id,
                id,
            });
        }
    }

    pub fn clear_query_history(&mut self) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let connection_id = session.profile.id;
        session.query_history.clear();
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::ClearQueryHistory {
                session_id,
                connection_id: Some(connection_id),
            });
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
        let connect_req_id = session.active_connect_req_id;
        session.latest_req_id = req_id;
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::ReloadWorkspace {
                session_id,
                req_id,
                connect_req_id,
                active_schema,
            });
        }
    }

    pub fn apply_updated_profile_to_active_session(&mut self, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let conn_id = session.profile.id;
        if let Some(dirty_tab_id) = session.first_dirty_table_tab_id() {
            self.focus_dirty_table_tab(session_id, dirty_tab_id, tx);
            self.set_notice(
                "Commit or discard staged edits before connecting to the updated target.",
                true,
            );
            return;
        }
        if !self.connections.iter().any(|c| c.id == conn_id) {
            return;
        }
        self.start_connection_inner(conn_id, None, None, true, tx);
    }

    pub fn reconnect_active(&mut self, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        if let Some(session) = self.sessions.get(&session_id)
            && self
                .pending_profile_save
                .as_ref()
                .is_some_and(|p| p.profile.id == session.profile.id)
        {
            self.set_notice(
                "Wait for the pending profile save to finish before reconnecting",
                true,
            );
            return;
        }
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let ctx = session.current_reconnect_context();
        let active_db = ctx
            .active_database
            .or_else(|| session.profile.database.clone());
        let preferred_schema = ctx.active_schema;
        let profile = session.profile.clone();
        let pw = session.session_password.clone();
        let prompt_for_out_of_sync_keychain = session.profile_out_of_sync
            && profile.engine != DbEngine::Sqlite
            && profile.password_mode == PasswordMode::Keychain
            && pw.is_none();
        if prompt_for_out_of_sync_keychain
            || (profile.engine != DbEngine::Sqlite
                && profile.password_mode == PasswordMode::AskEveryTime
                && pw.is_none())
        {
            let conn_id = profile.id;
            let name = profile.name.clone();
            let error = prompt_for_out_of_sync_keychain.then(|| {
                "Profile settings changed; enter password to reconnect to the previous target, or click Apply Profile."
                    .to_owned()
            });
            self.overlay = OverlayState::PasswordPrompt(PasswordPromptState {
                session_id: Some(session_id),
                connection_id: conn_id,
                connection_name: name,
                database_override: active_db,
                preferred_schema,
                password: TextEdit::new(""),
                error,
            });
            tx.focus_when_shown("password-prompt-input", true);
            return;
        }
        session.latest_req_id = req_id;
        session.active_connect_req_id = req_id;
        session.pending_db_switch_req = None;
        session.set_status(ConnectionState::Reconnecting);
        self.sync_window_title(tx);
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::Connect {
                session_id,
                req_id,
                profile,
                prompt_password: pw,
                database: active_db,
                preferred_schema,
            });
        }
    }

    pub fn disconnect_active(&mut self, tx: &mut Tx) {
        let Screen::Workspace(session_id) = self.screen else {
            return;
        };
        let req_id = self.alloc_req_id();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.latest_req_id = req_id;
        session.active_connect_req_id = req_id;
        session.pending_db_switch_req = None;
        session.set_status(ConnectionState::Disconnected { reason: None });
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
            if kind == PickerKind::Database && !session.profile.engine.supports_multi_database() {
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
        let mut changed = false;
        let update_events = self
            .update_bridge
            .as_ref()
            .map(UpdateBridge::drain_events)
            .unwrap_or_default();
        for event in update_events {
            self.handle_update_event(event, tx);
            changed = true;
        }

        let events = self
            .worker
            .as_ref()
            .map(WorkerBridge::drain_events)
            .unwrap_or_default();
        if !events.is_empty() {
            for event in events {
                self.handle_worker_event(event, tx);
            }
            changed = true;
        }
        changed
    }

    fn handle_update_event(&mut self, event: UpdateHostEvent, tx: &mut Tx) {
        match event {
            UpdateHostEvent::Changed(snapshot) => {
                if update_version_key(&self.update.snapshot.status)
                    != update_version_key(&snapshot.status)
                {
                    self.update.notes_expanded = false;
                    self.update.dismissed_os_warning = false;
                }
                if self.update.manual_check_pending
                    && let Some((msg, is_err)) = manual_check_notice(&snapshot.status)
                {
                    self.update.manual_check_pending = false;
                    self.set_notice(msg, is_err);
                }
                self.update.snapshot = snapshot;
            }
            UpdateHostEvent::OpenUrl(url) => {
                if tx.open_url(&url).is_err() {
                    self.set_notice(format!("Could not open browser for {url}"), true);
                }
            }
            UpdateHostEvent::Quit => {
                tx.quit();
            }
        }
    }

    fn handle_session_disconnected(
        &mut self,
        session_id: SessionId,
        connect_req_id: RequestId,
        error: tv_core::DbError,
        tx: &mut Tx,
    ) {
        let prompt_info = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            if session.active_connect_req_id != connect_req_id {
                return;
            }
            if matches!(
                session.status,
                ConnectionState::Disconnected { .. } | ConnectionState::Failed(_)
            ) {
                return;
            }
            if error.is_auth_failure() && session.profile.password_mode == PasswordMode::AskEveryTime {
                let ctx = session.current_reconnect_context();
                session.session_password = None;
                session.set_status(ConnectionState::Disconnected {
                    reason: Some(error.to_string()),
                });
                Some((
                    session.profile.id,
                    session.profile.name.clone(),
                    ctx.active_database.or_else(|| session.profile.database.clone()),
                    ctx.active_schema,
                ))
            } else {
                session.set_status(ConnectionState::Disconnected {
                    reason: Some(error.to_string()),
                });
                None
            }
        };

        if let Some((conn_id, name, db, schema)) = prompt_info {
            self.overlay = OverlayState::PasswordPrompt(PasswordPromptState {
                session_id: Some(session_id),
                connection_id: conn_id,
                connection_name: name,
                database_override: db,
                preferred_schema: schema,
                password: TextEdit::new(""),
                error: Some(error.to_string()),
            });
            tx.focus_when_shown("password-prompt-input", true);
        }

        if self.screen == Screen::Workspace(session_id) {
            self.sync_window_title(tx);
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent, tx: &mut Tx) {
        match event {
            WorkerEvent::Connected {
                session_id,
                req_id,
                server_info,
                mysql_quote_mode,
                postgres_session_mode,
                databases,
                active_database,
                schemas,
                active_schema,
                objects,
                logs,
            } => {
                let mut page_reloads: Vec<(TabId, PageRequest, RequestId)> = Vec::new();
                let mut struct_reloads: Vec<(TabId, String, String, RequestId)> = Vec::new();
                let conn_id = {
                    let Some(session) = self.sessions.get_mut(&session_id) else {
                        return;
                    };
                    if session.active_connect_req_id != req_id {
                        return;
                    }
                    session.latest_req_id = req_id;
                    session.pending_db_switch_req = None;
                    append_logs(&mut session.console_entries, logs);

                    let restored_selected = session
                        .reconnect_context
                        .selected_object
                        .clone()
                        .filter(|(s, n)| objects.iter().any(|o| o.schema == *s && o.name == *n));

                    session.set_status(ConnectionState::Connected(Box::new(ConnectedData {
                        server_version: server_info.version,
                        mode_seq: 0,
                        mysql_quote_mode,
                        postgres_session_mode,
                        databases,
                        active_database: active_database.clone(),
                        schemas,
                        active_schema,
                        objects,
                        expanded_columns: HashMap::new(),
                        selected_object: restored_selected,
                    })));
                    session.profile.id
                };

                let mut persisted_conn_change = false;
                if let Some(profile) = self.connections.iter_mut().find(|c| c.id == conn_id) {
                    profile.last_connected_at = Some(chrono::Utc::now());
                    persisted_conn_change = true;
                }
                if persisted_conn_change
                    && self.store_load_error.is_none()
                    && self.pending_profile_save.is_none()
                    && self.pending_profile_delete.is_none()
                    && let Err(err) = self.persist_connections()
                {
                    self.set_notice(
                        format!("Connected, but failed to update connection metadata on disk: {err}"),
                        true,
                    );
                }

                if active_database.is_some() && self.worker.is_some() {
                    let mut req_ids_needed = Vec::new();
                    if let Some(session) = self.sessions.get(&session_id) {
                        let active_tab_id = session.active_tab;
                        for tab in &session.tabs {
                            if let TabContent::Table(t) = &tab.content {
                                let is_active = Some(tab.id) == active_tab_id;
                                let reload_table = t.reload_on_reconnect && !t.staged.has_changes();
                                let need_page =
                                    (reload_table || (is_active && t.page.is_none())) && !t.page_loading;
                                let need_struct =
                                    (is_active || reload_table) && t.structure.is_none() && !t.structure_loading;
                                if need_page || need_struct {
                                    req_ids_needed.push((tab.id, need_page, need_struct));
                                }
                            }
                        }
                    }

                    let mut allocated = Vec::with_capacity(req_ids_needed.len());
                    for (tab_id, need_page, need_struct) in req_ids_needed {
                        let p_req = need_page.then(|| self.alloc_req_id());
                        let s_req = need_struct.then(|| self.alloc_req_id());
                        allocated.push((tab_id, p_req, s_req));
                    }

                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        for (tab_id, p_req, s_req) in allocated {
                            if let Some(t) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut) {
                                if let Some(rid) = p_req {
                                    t.page_loading = true;
                                    t.pending_page = None;
                                    t.page_error = None;
                                    t.reload_on_reconnect = false;
                                    t.latest_page_req = Some(rid);
                                    page_reloads.push((tab_id, t.page_request(), rid));
                                }
                                if let Some(rid) = s_req {
                                    t.structure_loading = true;
                                    t.structure_error = None;
                                    t.latest_structure_req = Some(rid);
                                    struct_reloads.push((
                                        tab_id,
                                        t.schema.clone(),
                                        t.object_name.clone(),
                                        rid,
                                    ));
                                }
                                if t.commit_error.as_deref() == Some("database session is disconnected") {
                                    t.commit_error = None;
                                }
                            }
                        }
                        if let Some(q) = session.active_query_tab_mut() {
                            if q.batch_error.as_deref().is_some_and(|e| {
                                e == "database session is disconnected"
                                    || e == "Disconnected from database"
                                    || e.starts_with("Database is not connected.")
                            }) {
                                q.batch_error = None;
                            }
                            if q.explain_error.as_deref().is_some_and(|e| {
                                e == "database session is disconnected"
                                    || e == "Disconnected from database"
                                    || e.starts_with("Database is not connected.")
                            }) {
                                q.explain_error = None;
                            }
                        }
                    }
                }

                if let Some(worker) = &self.worker {
                    for (tab_id, request, page_req) in page_reloads {
                        worker.send(WorkerCmd::FetchTablePage {
                            session_id,
                            tab_id,
                            req_id: page_req,
                            connect_req_id: req_id,
                            request,
                        });
                    }
                    for (tab_id, schema, table, struct_req) in struct_reloads {
                        worker.send(WorkerCmd::DescribeTable {
                            session_id,
                            tab_id,
                            req_id: struct_req,
                            connect_req_id: req_id,
                            schema,
                            table,
                        });
                    }
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
                let prompt_info = {
                    let Some(session) = self.sessions.get_mut(&session_id) else {
                        return;
                    };
                    if session.active_connect_req_id != req_id {
                        return;
                    }
                    session.pending_db_switch_req = None;
                    if error.is_auth_failure() && session.profile.password_mode == PasswordMode::AskEveryTime {
                        let ctx = session.current_reconnect_context();
                        session.session_password = None;
                        session.set_status(ConnectionState::Disconnected {
                            reason: Some(error.to_string()),
                        });
                        Some((
                            session.profile.id,
                            session.profile.name.clone(),
                            ctx.active_database.or_else(|| session.profile.database.clone()),
                            ctx.active_schema,
                        ))
                    } else {
                        session.set_status(ConnectionState::Failed(error.to_string()));
                        None
                    }
                };
                if let Some((conn_id, name, db, schema)) = prompt_info {
                    self.overlay = OverlayState::PasswordPrompt(PasswordPromptState {
                        session_id: Some(session_id),
                        connection_id: conn_id,
                        connection_name: name,
                        database_override: db,
                        preferred_schema: schema,
                        password: TextEdit::new(""),
                        error: Some(error.to_string()),
                    });
                    tx.focus_when_shown("password-prompt-input", true);
                }
                if self.screen == Screen::Workspace(session_id) {
                    self.sync_window_title(tx);
                }
            }

            WorkerEvent::SessionDisconnected {
                session_id,
                connect_req_id,
                error,
            } => {
                self.handle_session_disconnected(session_id, connect_req_id, error, tx);
            }

            WorkerEvent::SchemasAndObjectsLoaded {
                session_id,
                req_id,
                is_db_switch,
                server_version,
                mysql_quote_mode,
                postgres_session_mode,
                databases,
                active_database,
                schemas,
                active_schema,
                objects,
                logs,
            } => {
                let mut clear_password_prompt = false;
                let in_current_screen = {
                    let Some(session) = self.sessions.get_mut(&session_id) else {
                        return;
                    };
                    let matches_pending = session.pending_db_switch_req == Some(req_id);
                    if is_db_switch {
                        if !matches_pending && req_id.get() <= session.active_connect_req_id.get() {
                            return;
                        }
                        if matches_pending {
                            session.pending_db_switch_req = None;
                        }
                        session.active_connect_req_id = req_id;
                        session.latest_req_id = req_id;
                        clear_password_prompt = true;
                    } else if session.latest_req_id != req_id
                        || !matches!(session.status, ConnectionState::Connected(_))
                    {
                        return;
                    }
                    append_logs(&mut session.console_entries, logs);
                    let db_changed = is_db_switch
                        || session
                            .status
                            .as_connected()
                            .and_then(|c| c.active_database.as_deref())
                            != Some(active_database.as_str());
                    if let ConnectionState::Connected(ref mut data) = session.status {
                        if let Some(version) = server_version {
                            data.server_version = version;
                        }
                        if is_db_switch {
                            data.mode_seq = 0;
                            if session.profile.engine == DbEngine::Mysql {
                                data.mysql_quote_mode = mysql_quote_mode;
                            } else if session.profile.engine == DbEngine::Postgres {
                                data.postgres_session_mode = postgres_session_mode;
                            }
                        }
                        data.databases = databases;
                        data.active_database = Some(active_database);
                        data.schemas = schemas;
                        data.active_schema = active_schema;
                        data.objects = objects;
                        data.expanded_columns.clear();
                        if db_changed {
                            data.selected_object = None;
                        }
                    } else if is_db_switch {
                        session.set_status(ConnectionState::Connected(Box::new(ConnectedData {
                            server_version: server_version.unwrap_or_default(),
                            mode_seq: 0,
                            mysql_quote_mode,
                            postgres_session_mode,
                            databases,
                            active_database: Some(active_database),
                            schemas,
                            active_schema,
                            objects,
                            expanded_columns: HashMap::new(),
                            selected_object: None,
                        })));
                    }
                    if db_changed {
                        session.tabs.retain(|t| matches!(t.content, TabContent::Query(_)));
                        if let Some(active) = session.active_tab
                            && !session.tabs.iter().any(|t| t.id == active)
                        {
                            session.active_tab = session.tabs.last().map(|t| t.id);
                        }
                        session.tab_drag = TabDragState::new();
                    }
                    self.screen == Screen::Workspace(session_id)
                };
                if clear_password_prompt
                    && matches!(
                        &self.overlay,
                        OverlayState::PasswordPrompt(prompt) if prompt.session_id == Some(session_id)
                    )
                {
                    self.overlay = OverlayState::None;
                }
                if in_current_screen {
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
                req_id,
                schema,
                object,
                columns,
                logs,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if let ConnectionState::Connected(ref mut data) = session.status
                    && let std::collections::hash_map::Entry::Occupied(mut entry) =
                        data.expanded_columns.entry((schema, object))
                    && matches!(entry.get(), ColumnLoadState::Loading(pending) if *pending == req_id)
                {
                    append_logs(&mut session.console_entries, logs);
                    match columns {
                        Ok(cols) => {
                            entry.insert(ColumnLoadState::Loaded(cols));
                        }
                        Err(tv_core::DbError::Disconnected) => {
                            entry.remove();
                        }
                        Err(err) => {
                            entry.insert(ColumnLoadState::Error(err.to_string()));
                        }
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
                        let preserve_staged = tab.committing || tab.staged.has_changes();
                        let can_populate_initial_page_around_inserts = tab.page.is_none()
                            && tab.staged.updates.is_empty()
                            && tab.staged.deletes.is_empty()
                            && !tab.committing;
                        if preserve_staged && !can_populate_initial_page_around_inserts {
                            tab.pending_page = Some(page);
                            tab.page_error = None;
                            tab.filter_error = None;
                            tab.reload_on_reconnect = false;
                        } else {
                            tab.apply_page(page, !preserve_staged);
                        }
                    }
                    Err(err) => {
                        tab.pending_page = None;
                        if matches!(err, tv_core::DbError::Disconnected) {
                            tab.reload_on_reconnect = true;
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
                        if matches!(err, tv_core::DbError::Disconnected) {
                            tab.structure_error = None;
                        } else {
                            tab.structure_error = Some(err.to_string());
                        }
                    }
                }
            }

            WorkerEvent::QueryTick {
                session_id,
                tab_id,
                req_id,
                elapsed_ms,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut)
                    && tab.active_req_id == Some(req_id)
                    && tab.running
                {
                    tab.elapsed_ms = elapsed_ms;
                }
            }

            WorkerEvent::SessionModeUpdated {
                session_id,
                connect_req_id,
                mode_seq,
                mysql_quote_mode,
                postgres_session_mode,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                if connect_req_id == session.active_connect_req_id
                    && let ConnectionState::Connected(ref mut data) = session.status
                    && mode_seq > data.mode_seq
                {
                    data.mode_seq = mode_seq;
                    if session.profile.engine == DbEngine::Mysql {
                        data.mysql_quote_mode = mysql_quote_mode;
                    } else if session.profile.engine == DbEngine::Postgres {
                        data.postgres_session_mode = postgres_session_mode;
                    }
                }
            }

            WorkerEvent::QueryCompleted {
                session_id,
                tab_id,
                req_id,
                result,
                logs,
                recorded_history,
            } => {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                append_logs(&mut session.console_entries, logs);
                if let Some(entry) = recorded_history {
                    session.query_history.insert(0, entry);
                    if session.query_history.len() > 200 {
                        session.query_history.truncate(200);
                    }
                }
                if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut)
                    && tab.active_req_id == Some(req_id)
                {
                    tab.running = false;
                    tab.cancelling = false;
                    tab.active_req_id = None;
                    match result {
                        Ok(batch) => {
                            tab.result_gen = tab.result_gen.wrapping_add(1);
                            let BatchOutcomes {
                                results,
                                total_duration_ms,
                                cancelled,
                                error_msg,
                            } = unpack_batch(tab_id, tab.result_gen, batch);
                            tab.elapsed_ms = total_duration_ms;
                            tab.total_duration_ms = Some(total_duration_ms);
                            tab.batch_cancelled = cancelled;
                            tab.batch_error = error_msg;
                            let detected_explain = (results.len() == 1)
                                .then(|| ExplainPlan::try_extract_from_result(&results[0].statement))
                                .flatten();
                            tab.results = results;
                            tab.active_result = 0;
                            if let Some(plan) = detected_explain {
                                tab.explain_plan = Some(plan);
                                tab.explain_error = None;
                                tab.show_explain = true;
                            }
                        }
                        Err(err) => {
                            tab.batch_cancelled = matches!(err, tv_core::DbError::Cancelled);
                            if !matches!(err, tv_core::DbError::Disconnected) {
                                tab.batch_error = Some(err.to_string());
                            }
                        }
                    }
                }
            }

            WorkerEvent::QueryLibraryLoaded {
                session_id,
                saved_queries,
                history,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.saved_queries = saved_queries;
                    session.query_history = history;
                }
            }

            WorkerEvent::SavedQueryMutated {
                session_id,
                tab_id,
                saved,
                saved_queries,
                error,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.saved_queries = saved_queries;
                    if let (Some(saved_q), Some(tid)) = (saved, tab_id)
                        && let Some(tab) = session.tab_mut(tid).and_then(WorkspaceTab::as_query_mut)
                    {
                        tab.saved_query_id = Some(saved_q.id);
                        tab.title = saved_q.name;
                        tab.saved_snapshot = saved_q.sql;
                        if self.screen == Screen::Workspace(session_id) {
                            self.sync_window_title(tx);
                        }
                    }
                }
                if let Some(err) = error {
                    self.set_notice(err, true);
                }
            }

            WorkerEvent::HistoryCleared {
                session_id,
                history,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.query_history = history;
                }
            }

            WorkerEvent::OperationFailed {
                session_id,
                req_id,
                error,
            } => {
                let mark_failed = {
                    let Some(session) = self.sessions.get_mut(&session_id) else {
                        return;
                    };
                    let is_pending_switch = session.pending_db_switch_req == Some(req_id);
                    if !is_pending_switch && session.latest_req_id != req_id {
                        return;
                    }
                    if is_pending_switch {
                        session.pending_db_switch_req = None;
                    }
                    if matches!(session.status, ConnectionState::Connecting) {
                        session.set_status(ConnectionState::Failed(error.to_string()));
                        true
                    } else {
                        false
                    }
                };
                if !matches!(error, tv_core::DbError::Disconnected) {
                    self.set_notice(error.to_string(), true);
                }
                if (mark_failed || !matches!(error, tv_core::DbError::Disconnected))
                    && self.screen == Screen::Workspace(session_id)
                {
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
                result,
            } => {
                if self.active_cred_req_id != Some(req_id) {
                    return;
                }
                if let OverlayState::ConnectionForm(ref mut form) = self.overlay
                    && form.editing_id == Some(connection_id)
                {
                    form.loading_credential = false;
                    match result {
                        Ok(Some(pw)) => {
                            if !form.password_touched {
                                form.password = TextEdit::new(&pw);
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            if !form.password_touched {
                                form.validation_error = Some(format!(
                                    "Failed to read stored password from keychain: {err}"
                                ));
                            }
                        }
                    }
                }
            }

            WorkerEvent::CredentialSaved { req_id, result, .. }
            | WorkerEvent::CredentialDeleted { req_id, result, .. } => {
                self.handle_credential_mutation_event(req_id, result, tx);
            }

            WorkerEvent::StagedChangesCommitted {
                session_id,
                tab_id,
                req_id,
                result,
                logs,
            } => {
                let mut reload_target: Option<(TabId, RequestId, PageRequest)> = None;
                let mut notice: Option<(String, bool)> = None;

                {
                    let Some(session) = self.sessions.get_mut(&session_id) else {
                        return;
                    };
                    let connect_req_id = session.active_connect_req_id;
                    append_logs(&mut session.console_entries, logs);

                    if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut)
                        && tab.commit_req_id == Some(req_id)
                    {
                        let commit_gen = tab.commit_staged_gen.take();
                        tab.committing = false;
                        tab.commit_req_id = None;
                        match result {
                            Ok(outcome) => {
                                let changes = tab.staged.change_count();
                                if commit_gen == Some(tab.staged_gen) {
                                    tab.staged.clear();
                                    tab.mark_staged_mutated();
                                    tab.diff_drawer_open = false;
                                }
                                tab.commit_error = None;
                                tab.editing_cell = None;
                                tab.rebuild_diff_state();
                                reload_target = Some((tab_id, connect_req_id, tab.page_request()));
                                notice = Some((
                                    format!(
                                        "Committed {changes} change(s) ({} rows affected · {} ms)",
                                        outcome.total_rows_affected, outcome.elapsed_ms
                                    ),
                                    false,
                                ));
                            }
                            Err(err) => {
                                let msg = err.to_string();
                                tab.commit_error = Some(msg.clone());
                                tab.diff_drawer_open = true;
                                notice = Some((msg, true));
                            }
                        }
                    }
                }

                if let Some((msg, is_err)) = notice {
                    self.set_notice(msg, is_err);
                }
                if let Some((tid, connect_req_id, request)) = reload_target {
                    let next_req = self.alloc_req_id();
                    if let Some(t) = self
                        .sessions
                        .get_mut(&session_id)
                        .and_then(|s| s.tab_mut(tid))
                        .and_then(WorkspaceTab::as_table_mut)
                    {
                        t.page_loading = true;
                        t.pending_page = None;
                        t.latest_page_req = Some(next_req);
                    }
                    if let Some(worker) = &self.worker {
                        worker.send(WorkerCmd::FetchTablePage {
                            session_id,
                            tab_id: tid,
                            req_id: next_req,
                            connect_req_id,
                            request,
                        });
                    }
                }
            }

            WorkerEvent::ExplainCompleted {
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
                if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut)
                    && tab.explain_req_id == Some(req_id)
                {
                    tab.explain_loading = false;
                    tab.explain_req_id = None;
                    tab.show_explain = true;
                    match result {
                        Ok(plan) => {
                            tab.explain_plan = Some(*plan);
                            tab.explain_error = None;
                        }
                        Err(err) => {
                            if !matches!(err, tv_core::DbError::Disconnected) {
                                tab.explain_error = Some(err.to_string());
                            }
                        }
                    }
                }
            }

            WorkerEvent::FkPeekLoaded {
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
                if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut)
                    && let Some(ref mut peek) = tab.fk_peek
                    && peek.req_id == Some(req_id)
                {
                    peek.loading = false;
                    match result {
                        Ok(row) => {
                            peek.result = Some(row);
                            peek.error = None;
                        }
                        Err(err) => {
                            if matches!(err, tv_core::DbError::Disconnected) {
                                tab.fk_peek = None;
                            } else {
                                peek.error = Some(err.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    /// Loads settings from `path`, pausing query history recording and surfacing an error if
    /// an existing settings file cannot be read or parsed.
    pub fn load_settings_from(&mut self, path: PathBuf) {
        self.settings_path = path.clone();
        self.apply_settings_load(SettingsStore::at(path).load());
    }

    fn apply_settings_load(&mut self, result: Result<AppSettings, StorageError>) {
        match result {
            Ok(settings) => {
                self.appearance = appearance_from_pref(settings.appearance);
                self.record_query_history = settings.record_query_history;
                self.settings_save_error = None;
            }
            Err(err) => {
                let defaults = AppSettings {
                    record_query_history: false,
                    ..AppSettings::default()
                };
                self.appearance = appearance_from_pref(defaults.appearance);
                self.record_query_history = defaults.record_query_history;
                self.settings_save_error = Some(format!(
                    "Failed to load settings from disk ({err}). Query history recording has been paused until settings are saved or fixed."
                ));
                self.set_notice(
                    format!("Failed to load settings: {err}. Query history recording paused."),
                    true,
                );
            }
        }
    }

    pub fn persist_settings(&mut self) {
        if !self.persist_enabled {
            return;
        }
        let store = SettingsStore::at(self.settings_path.clone());
        let settings = AppSettings {
            appearance: pref_from_appearance(self.appearance),
            record_query_history: self.record_query_history,
            ..AppSettings::default()
        };
        match store.save(&settings) {
            Ok(()) => {
                let had_error = self.settings_save_error.take().is_some();
                if had_error
                    && self.status_notice.as_ref().is_some_and(|n| {
                        n.is_error
                            && (n.message.starts_with("Failed to save settings:")
                                || n.message.starts_with("Failed to load settings:"))
                    })
                {
                    self.status_notice = None;
                }
            }
            Err(err) => {
                self.settings_save_error = Some(format!(
                    "Failed to save settings to disk ({err}). Changes apply to this session only and will reset on restart."
                ));
                self.set_notice(format!("Failed to save settings: {err}"), true);
            }
        }
    }

    pub fn set_appearance(&mut self, scheme: ColorScheme, tx: &mut Tx) {
        self.appearance = scheme;
        tx.set_color_scheme(scheme);
        self.persist_settings();
    }

    pub fn set_record_query_history(&mut self, enabled: bool) {
        self.record_query_history = enabled;
        self.persist_settings();
    }

    pub fn toggle_active_query_tab_history(&mut self) {
        let global_enabled = self.record_query_history;
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_query_tab_mut) {
            let next = !tab.effective_record_history(global_enabled);
            tab.record_history_override = Some(next);
        }
    }

    pub fn open_settings(&mut self) {
        self.launcher_context_menu = None;
        self.sort_dropdown_open = false;
        if let Some(session) = self.active_session_mut() {
            session.schema_dropdown_open = false;
        }
        self.overlay = OverlayState::Settings;
    }

    pub fn set_automatic_update_checks(&mut self, enabled: bool) {
        self.update.snapshot.automatic = enabled;
        if let Some(ref bridge) = self.update_bridge {
            bridge.set_automatic_checks(enabled);
        }
    }

    pub fn set_notice(&mut self, message: impl Into<String>, is_error: bool) {
        let was_hovered = self.status_notice.as_ref().is_some_and(|n| n.hovered);
        let duration = if is_error {
            STATUS_NOTICE_ERROR_DURATION_SECS
        } else {
            STATUS_NOTICE_DURATION_SECS
        };
        let timer = if was_hovered {
            StatusNoticeTimer::Paused { remaining: duration }
        } else {
            StatusNoticeTimer::Pending { duration }
        };
        self.status_notice = Some(StatusNotice {
            message: message.into(),
            is_error,
            hovered: was_hovered,
            timer,
        });
    }

    fn handle_credential_mutation_event(
        &mut self,
        req_id: RequestId,
        result: Result<(), String>,
        tx: &mut Tx,
    ) {
        if self
            .pending_profile_save
            .as_ref()
            .is_some_and(|p| p.req_id == req_id)
        {
            let Some(pending) = self.pending_profile_save.take() else {
                return;
            };
            let form_matches = matches!(
                self.overlay,
                OverlayState::ConnectionForm(ref form) if form.saving_req_id == Some(req_id)
            );
            if let OverlayState::ConnectionForm(ref mut form) = self.overlay
                && form.saving_req_id == Some(req_id)
            {
                form.saving_req_id = None;
            }
            match result {
                Ok(()) => {
                    self.pending_credential_deletes.remove(&pending.profile.id);
                    if pending.cancelled {
                        let name = pending.profile.name.clone();
                        let (merged_recent, any_out_of_sync) = self.apply_saved_profile_to_state(
                            &pending.profile,
                            pending.next_store,
                            pending.password_mutated,
                        );
                        if any_out_of_sync {
                            self.set_notice(
                                format!(
                                    "Saved connection \"{name}\". Reconnect the active session to apply updated settings."
                                ),
                                false,
                            );
                        } else {
                            self.set_notice(format!("Saved connection \"{name}\""), false);
                        }
                        self.persist_merged_recent_if_needed(merged_recent);
                    } else {
                        self.finish_profile_save(pending, None, tx);
                    }
                }
                Err(cred_err) => {
                    let mut rollback = self.current_store_data();
                    rollback.folders = pending.previous_folders.clone();
                    match self.save_store_data(&rollback) {
                        Ok(()) => {
                            let msg = format!(
                                "Failed to update keychain ({cred_err}). Profile changes were reverted."
                            );
                            if form_matches
                                && let OverlayState::ConnectionForm(ref mut form) = self.overlay
                            {
                                form.validation_error = Some(msg.clone());
                            }
                            self.set_notice(msg, true);
                        }
                        Err(rb_err) => {
                            if pending.deleting_credential {
                                self.pending_credential_deletes.insert(pending.profile.id);
                            }
                            let _ = self.apply_saved_profile_to_state(
                                &pending.profile,
                                pending.next_store,
                                pending.password_mutated,
                            );
                            if form_matches
                                && let OverlayState::ConnectionForm(ref mut form) = self.overlay
                            {
                                form.editing_id = Some(pending.profile.id);
                                form.validation_error = Some(format!(
                                    "Failed to update keychain ({cred_err}) and failed to revert connections file ({rb_err}). Retry Save or switch password mode."
                                ));
                            }
                            self.set_notice(
                                format!(
                                    "Failed to update keychain ({cred_err}) and failed to revert connections file ({rb_err})"
                                ),
                                true,
                            );
                        }
                    }
                }
            }
            return;
        }

        if self
            .pending_profile_delete
            .as_ref()
            .is_some_and(|p| p.req_id == req_id)
        {
            let Some(pending) = self.pending_profile_delete.take() else {
                return;
            };
            match result {
                Ok(()) => {
                    self.pending_credential_deletes
                        .remove(&pending.connection_id);
                    self.finish_profile_delete(
                        pending.connection_id,
                        &pending.connection_name,
                        pending.next_store,
                        true,
                    );
                }
                Err(cred_err) => {
                    let mut rollback = self.current_store_data();
                    rollback.folders = pending.previous_folders;
                    match self.save_store_data(&rollback) {
                        Ok(()) => {
                            let msg = format!(
                                "Failed to remove password from keychain ({cred_err}). Connection \"{}\" was not deleted.",
                                pending.connection_name
                            );
                            self.delete_confirm_error = Some(msg.clone());
                            self.set_notice(msg, true);
                        }
                        Err(rb_err) => {
                            self.finish_profile_delete(
                                pending.connection_id,
                                &pending.connection_name,
                                pending.next_store,
                                false,
                            );
                            self.set_notice(
                                format!(
                                    "Deleted \"{}\", but failed to remove its keychain password ({cred_err}) and failed to restore connections file ({rb_err}).",
                                    pending.connection_name
                                ),
                                true,
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn dismiss_status_notice(&mut self) {
        self.status_notice = None;
    }

    pub fn pin_status_notice(&mut self, tx: &mut Tx) {
        if let Some(ref mut notice) = self.status_notice {
            notice.timer = StatusNoticeTimer::Pinned;
        }
        tx.live_only();
    }

    pub fn hover_status_notice(&mut self, hovered: bool, tx: &mut Tx) {
        if let Some(ref mut notice) = self.status_notice {
            notice.hovered = hovered;
            let now = tx.clock();
            if hovered {
                notice.timer = match notice.timer {
                    StatusNoticeTimer::Pending { duration } => StatusNoticeTimer::Paused { remaining: duration },
                    StatusNoticeTimer::Running { due } => StatusNoticeTimer::Paused {
                        remaining: (due - now).max(0.0),
                    },
                    StatusNoticeTimer::Paused { remaining } => StatusNoticeTimer::Paused { remaining },
                    StatusNoticeTimer::Pinned => StatusNoticeTimer::Pinned,
                };
            } else if let StatusNoticeTimer::Paused { remaining } = notice.timer {
                let left = remaining.max(STATUS_NOTICE_LEAVE_GRACE_SECS);
                notice.timer = StatusNoticeTimer::Running { due: now + left };
                tx.wake_after(Duration::from_secs_f64(left));
            }
        }
        tx.live_only();
    }

    pub fn tick_status_notice(&mut self, now: f64, tx: &mut Tx) -> bool {
        let Some(ref mut notice) = self.status_notice else {
            return false;
        };
        match notice.timer {
            StatusNoticeTimer::Pending { duration } => {
                notice.timer = StatusNoticeTimer::Running { due: now + duration };
                tx.wake_after(Duration::from_secs_f64(duration));
                false
            }
            StatusNoticeTimer::Running { due } => {
                if now + 1e-3 >= due {
                    self.status_notice = None;
                    true
                } else {
                    tx.wake_after(Duration::from_secs_f64((due - now).max(0.0)));
                    false
                }
            }
            StatusNoticeTimer::Paused { .. } | StatusNoticeTimer::Pinned => false,
        }
    }

    pub fn poll(&mut self, now: f64, tx: &mut Tx) -> bool {
        let mut changed = self.apply_worker_events(tx);
        changed |= self.tick_status_notice(now, tx);
        changed
    }

    pub fn check_for_updates(&mut self) {
        if let Some(ref bridge) = self.update_bridge {
            self.update.manual_check_pending = true;
            bridge.check_now();
            self.set_notice("Checking for updates…", false);
        } else {
            let notice = manual_check_notice(&self.update.snapshot.status).unwrap_or_else(|| {
                (
                    format!(
                        "TableView {} is running outside a packaged macOS app bundle; auto-updates are disabled.",
                        self.update.running_version
                    ),
                    false,
                )
            });
            self.set_notice(notice.0, notice.1);
        }
    }

    pub fn restart_to_update(&mut self) {
        if let Some(ref bridge) = self.update_bridge {
            bridge.restart_to_update();
        }
    }

    pub fn open_update_download(&mut self, tx: &mut Tx) {
        if let Some(ref bridge) = self.update_bridge {
            bridge.open_download();
        } else if let UpdateStatus::Available { download_url, .. } = &self.update.snapshot.status {
            let url = download_url.clone();
            if tx.open_url(&url).is_err() {
                self.set_notice(format!("Could not open browser for {url}"), true);
            }
        }
    }

    pub fn open_update_notes(&mut self, tx: &mut Tx) {
        if let Some(ref bridge) = self.update_bridge {
            bridge.open_notes();
        } else if let UpdateStatus::Ready { notes_url: Some(url), .. }
        | UpdateStatus::Available { notes_url: Some(url), .. } = &self.update.snapshot.status
        {
            let url = url.clone();
            if tx.open_url(&url).is_err() {
                self.set_notice(format!("Could not open browser for {url}"), true);
            }
        }
    }

    pub fn toggle_update_notes(&mut self) {
        self.update.notes_expanded = !self.update.notes_expanded;
    }

    pub fn dismiss_update_os_warning(&mut self) {
        self.update.dismissed_os_warning = true;
    }

    pub fn install_update_on_quit(&mut self) {
        if let Some(ref bridge) = self.update_bridge {
            let _installed = bridge.install_on_quit();
        }
    }

    pub fn has_uncommitted_staged_edits(&self) -> bool {
        self.sessions
            .values()
            .any(|session| session.tabs.iter().filter_map(WorkspaceTab::as_table).any(|tab| tab.staged.has_changes()))
    }

    pub fn sync_window_title(&self, tx: &mut Tx) {
        let title = match self.active_session() {
            None => "TableView".to_owned(),
            Some(session) => match &session.status {
                ConnectionState::Connected(data) => {
                    let active_tab = session.active_tab();
                    match (&data.active_database, active_tab, &data.selected_object) {
                        (Some(db), Some(tab), _) => match &tab.content {
                            TabContent::Table(t) => format!(
                                "{} — {} — {}.{}",
                                session.profile.name, db, t.schema, t.object_name
                            ),
                            TabContent::Query(q) => format!("{} — {} — {}", session.profile.name, db, q.title),
                        },
                        (Some(db), None, Some((schema, obj))) => {
                            format!("{} — {} — {}.{}", session.profile.name, db, schema, obj)
                        }
                        (Some(db), None, None) => format!("{} — {}", session.profile.name, db),
                        (None, Some(tab), _) => format!("{} — {}", session.profile.name, tab.title()),
                        (None, None, _) => format!("{} — TableView", session.profile.name),
                    }
                }
                ConnectionState::Connecting => format!("{} (Connecting…) — TableView", session.profile.name),
                ConnectionState::Reconnecting => format!("{} (Reconnecting…) — TableView", session.profile.name),
                ConnectionState::Disconnected { .. } | ConnectionState::Failed(_) => {
                    format!("{} (Disconnected) — TableView", session.profile.name)
                }
            },
        };
        tx.set_title(title);
    }
}

fn update_version_key(status: &UpdateStatus) -> Option<&str> {
    match status {
        UpdateStatus::Ready { version, .. }
        | UpdateStatus::Available { version, .. }
        | UpdateStatus::Downloading { version, .. }
        | UpdateStatus::Installing { version } => Some(version.as_str()),
        UpdateStatus::Failed(UpdateFailure::RequiresNewerOs { version, .. })
        | UpdateStatus::Failed(UpdateFailure::Relaunch { version })
        | UpdateStatus::Failed(UpdateFailure::RelaunchRecovery { version, .. }) => {
            Some(version.as_str())
        }
        _ => None,
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

struct BatchOutcomes {
    results: Vec<QueryResultTabState>,
    total_duration_ms: u64,
    cancelled: bool,
    error_msg: Option<String>,
}

fn unpack_batch(tab_id: TabId, result_gen: u64, batch: QueryBatchResult) -> BatchOutcomes {
    let results = batch
        .results
        .into_iter()
        .enumerate()
        .map(|(idx, statement)| QueryResultTabState::new(tab_id, result_gen, idx, statement))
        .collect();
    let error_msg = if batch.cancelled {
        Some("Query cancelled".to_owned())
    } else {
        None
    };
    BatchOutcomes {
        results,
        total_duration_ms: batch.total_elapsed_ms,
        cancelled: batch.cancelled,
        error_msg,
    }
}

fn statements_for_all(
    text: &str,
    engine: DbEngine,
    sql_mode: SqlSessionMode,
) -> Option<(Vec<String>, String)> {
    let stmts = split_statements_for_engine_with_session_mode(text, engine, sql_mode);
    if stmts.is_empty() {
        return None;
    }
    let sql_text = trim_sql_whitespace_for_engine(engine, text).to_owned();
    Some((stmts.into_iter().map(|s| s.sql).collect(), sql_text))
}

fn statements_for_current(
    editor: &TextEdit,
    engine: DbEngine,
    sql_mode: SqlSessionMode,
) -> Option<(Vec<String>, String)> {
    if editor.anchor != editor.focus {
        let sel = editor.selected();
        let stmts = split_statements_for_engine_with_session_mode(sel, engine, sql_mode.clone());
        if !stmts.is_empty() {
            let texts = stmts.into_iter().map(|s| s.sql).collect();
            return Some((texts, trim_sql_whitespace_for_engine(engine, sel).to_owned()));
        }
    }
    let stmt = statement_at_cursor_for_engine_with_session_mode(
        &editor.text,
        editor.anchor,
        editor.focus,
        engine,
        sql_mode,
    )?;
    let sql = stmt.sql;
    Some((vec![sql.clone()], sql))
}

fn concise_sql_title(sql: &str, fallback_seq: u64) -> String {
    let first_line = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("");
    if first_line.is_empty() {
        return format!("Query {fallback_seq}");
    }
    let mut out: String = first_line.chars().take(24).collect();
    if first_line.chars().count() > 24 {
        out.push('…');
    }
    out
}

fn connection_matches_search(c: &ConnectionProfile, query_lower: &str) -> bool {
    query_lower.is_empty()
        || c.name.to_ascii_lowercase().contains(query_lower)
        || c.engine.label().to_ascii_lowercase().contains(query_lower)
        || c.host.to_ascii_lowercase().contains(query_lower)
        || c.user.to_ascii_lowercase().contains(query_lower)
        || c.database
            .as_deref()
            .is_some_and(|d| d.to_ascii_lowercase().contains(query_lower))
        || c.file_path
            .as_ref()
            .is_some_and(|p| p.to_string_lossy().to_ascii_lowercase().contains(query_lower))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_mode_updated_ignores_stale_sequence_and_query_completed_preserves_mode() {
        let profile = ConnectionProfile::new_postgres("greendale_pg", "127.0.0.1", "troy");
        let conn_id = profile.id;
        let mut state = AppState::init_in_memory(StoreData {
            connections: vec![profile.clone()],
            ..StoreData::default()
        });
        let sid = SessionId::new(1);
        let connect_req_id = RequestId::new(1);
        let mut sess = SessionState::new(sid, profile, connect_req_id);
        sess.status = ConnectionState::Connected(Box::new(ConnectedData {
            server_version: "PostgreSQL 18.6".to_owned(),
            mode_seq: 0,
            mysql_quote_mode: None,
            postgres_session_mode: Some(PostgresSessionMode::new(true)),
            databases: Vec::new(),
            active_database: Some("greendale".to_owned()),
            schemas: Vec::new(),
            active_schema: Some("public".to_owned()),
            objects: Vec::new(),
            expanded_columns: HashMap::new(),
            selected_object: None,
        }));
        let tid_a = state.alloc_tab_id();
        let tid_b = state.alloc_tab_id();
        sess.tabs.push(WorkspaceTab::new_query(tid_a, "Tab A".to_owned(), String::new(), None));
        sess.tabs.push(WorkspaceTab::new_query(tid_b, "Tab B".to_owned(), String::new(), None));
        state.sessions.insert(sid, sess);
        state.screen = Screen::Workspace(sid);

        let newer_mode = PostgresSessionMode::new(false);
        let older_mode = PostgresSessionMode::new(true);
        let mut tx = rune::Tx::default();

        // Tab B's newer mode update (mode_seq = 2) arrives first.
        state.handle_worker_event(
            WorkerEvent::SessionModeUpdated {
                session_id: sid,
                connect_req_id,
                mode_seq: 2,
                mysql_quote_mode: None,
                postgres_session_mode: Some(newer_mode.clone()),
            },
            &mut tx,
        );
        // Delayed older mode update (mode_seq = 1) must be ignored.
        state.handle_worker_event(
            WorkerEvent::SessionModeUpdated {
                session_id: sid,
                connect_req_id,
                mode_seq: 1,
                mysql_quote_mode: None,
                postgres_session_mode: Some(older_mode),
            },
            &mut tx,
        );
        // Delayed QueryCompleted from Tab A carries no mode fields and preserves newer_mode.
        state.handle_worker_event(
            WorkerEvent::QueryCompleted {
                session_id: sid,
                tab_id: tid_a,
                req_id: RequestId::new(10),
                result: Ok(QueryBatchResult {
                    results: Vec::new(),
                    total_elapsed_ms: 1,
                    cancelled: false,
                }),
                logs: Vec::new(),
                recorded_history: None,
            },
            &mut tx,
        );

        let Some(cur) = state.sessions.get(&sid) else {
            panic!("missing session");
        };
        assert_eq!(cur.postgres_session_mode(), Some(newer_mode));
        assert_eq!(
            cur.status.as_connected().map(|c| c.mode_seq),
            Some(2)
        );
        assert_eq!(conn_id, cur.profile.id);
    }
}
