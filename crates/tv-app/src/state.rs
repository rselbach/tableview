use std::collections::HashMap;
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
    StatementInspection, StoreData, TabId, TablePage, TableStructure, build_explain_sql_for_engine,
    build_fk_where_clause_for_engine, default_export_filename, evaluate_sql_safety_for_engine,
    evaluate_staged_commit_safety, export_rows_for_engine,
};
use tv_core::sql::{
    cursor_line_col, format_sql, split_statements_for_engine, statement_at_cursor_for_engine,
};

use tv_update::{UpdateFailure, UpdateStatus};

use crate::update::{UpdateBridge, UpdateHostEvent, UpdateUiState, manual_check_notice};
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
    Loading,
    Loaded(Vec<ColumnSummary>),
    Error(String),
}

/// Catalog state populated once a session connects to a database.
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
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub database_override: Option<String>,
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
    },
    ExplainAnalyze {
        statement: String,
    },
}

/// Modal state for confirming mutating or destructive SQL on protected connections.
#[derive(Clone, Debug)]
pub struct SafetyConfirmModalState {
    pub session_id: SessionId,
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
    pub page_error: Option<String>,
    pub structure: Option<TableStructure>,
    pub structure_error: Option<String>,
    pub data_table: TableState,
    pub structure_table: TableState,
    pub indexes_table: TableState,
    pub selected_row: Option<usize>,
    pub selected_row_key: Option<RowKey>,
    pub selected_cell: Option<(RowKey, usize)>,
    pub editing_cell: Option<ActiveCellEdit>,
    pub row_menu: Option<(RowKey, PointerPress)>,
    pub staged: StagedChanges,
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
            page_error: None,
            structure: None,
            structure_error: None,
            data_table: TableState::new().key(format!("tv-data-{tid}")),
            structure_table: TableState::new().key(format!("tv-struct-{tid}")),
            indexes_table: TableState::new().key(format!("tv-idx-{tid}")),
            selected_row: None,
            selected_row_key: None,
            selected_cell: None,
            editing_cell: None,
            row_menu: None,
            staged: StagedChanges::new(),
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
    pub table: TableState,
    pub selected_row: Option<usize>,
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
    pub split_ratio: f32,
    pub running: bool,
    pub cancelling: bool,
    pub active_req_id: Option<RequestId>,
    pub elapsed_ms: u64,
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
            split_ratio: 0.42,
            running: false,
            cancelling: false,
            active_req_id: None,
            elapsed_ms: 0,
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

    pub fn active_result_state(&self) -> Option<&QueryResultTabState> {
        self.results.get(self.active_result)
    }

    pub fn active_result_state_mut(&mut self) -> Option<&mut QueryResultTabState> {
        self.results.get_mut(self.active_result)
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
    pub saved_queries: Vec<SavedQuery>,
    pub query_history: Vec<QueryHistoryEntry>,
    pub query_seq: u64,
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
            saved_queries: Vec::new(),
            query_history: Vec::new(),
            query_seq: 1,
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

/// Top-level application state owned by Rune.
pub struct AppState {
    pub store_path: PathBuf,
    pub settings_path: PathBuf,
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
        let (store_path, store_data) = match ConnectionStore::open_default() {
            Ok(store) => {
                let path = store.path().to_path_buf();
                let data = store.load().unwrap_or_default();
                (path, data)
            }
            Err(_) => (PathBuf::from("connections.json"), StoreData::default()),
        };
        let (settings_path, settings) = match SettingsStore::open_default() {
            Ok(store) => {
                let path = store.path().to_path_buf();
                let data = store.load().unwrap_or_default();
                (path, data)
            }
            Err(_) => (PathBuf::from("settings.json"), AppSettings::default()),
        };
        let mut state = Self::from_store_data(store_path, store_data, true);
        state.settings_path = settings_path;
        state.appearance = appearance_from_pref(settings.appearance);
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
            .filter(|c| {
                q.is_empty()
                    || c.name.to_ascii_lowercase().contains(&q)
                    || c.engine.label().to_ascii_lowercase().contains(&q)
                    || c.host.to_ascii_lowercase().contains(&q)
                    || c.user.to_ascii_lowercase().contains(&q)
                    || c.database
                        .as_deref()
                        .is_some_and(|d| d.to_ascii_lowercase().contains(&q))
                    || c.file_path
                        .as_ref()
                        .is_some_and(|p| p.to_string_lossy().to_ascii_lowercase().contains(&q))
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
        let url = profile.to_url(true);
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

        if profile.engine != DbEngine::Sqlite
            && profile.password_mode == PasswordMode::AskEveryTime
            && explicit_password.is_none()
        {
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
        session.tabs.retain(|t| matches!(t.content, TabContent::Query(_)));
        if let Some(active) = session.active_tab
            && !session.tabs.iter().any(|t| t.id == active)
        {
            session.active_tab = session.tabs.last().map(|t| t.id);
        }
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
        let blocked = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return;
            };
            if tab.staged.has_changes() {
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
            if tab.staged.has_changes() {
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
            if tab.staged.has_changes() {
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
            if tab.staged.has_changes() {
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
            if tab.staged.has_changes() {
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
            if tab.staged.has_changes() {
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
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
            return;
        };
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
                if let Some(orig_row) = tab.page.as_ref().and_then(|p| p.rows.get(idx)).cloned() {
                    tab.staged.stage_cell_edit(idx, &orig_row, editing.col_idx, new_val);
                }
            }
        }
        tab.commit_error = None;
        tab.rebuild_diff_state();
    }

    pub fn stage_cell_null(&mut self, row_key: RowKey, col_idx: usize) {
        let Some(policy) = self.active_session().map(|s| s.profile.safety) else {
            return;
        };
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
            return;
        };
        if let Err(reason) = tab.can_edit_table(policy) {
            self.set_notice(reason, true);
            return;
        }
        if matches!(row_key, RowKey::Existing(_)) && !tab.has_primary_key() {
            self.set_notice("Table requires a primary key to edit existing rows safely", true);
            return;
        }
        if tab
            .columns()
            .and_then(|c| c.get(col_idx))
            .is_some_and(|c| !c.nullable)
        {
            self.set_notice("Column is NOT NULL", true);
            return;
        }
        tab.editing_cell = None;
        match row_key {
            RowKey::Draft(id) => tab.staged.stage_insert_cell_edit(id, col_idx, StagedCellValue::Null),
            RowKey::Existing(idx) => {
                if let Some(orig_row) = tab.page.as_ref().and_then(|p| p.rows.get(idx)).cloned() {
                    tab.staged.stage_cell_edit(idx, &orig_row, col_idx, StagedCellValue::Null);
                }
            }
        }
        tab.commit_error = None;
        tab.rebuild_diff_state();
    }

    pub fn insert_staged_row(&mut self, tx: &mut Tx) -> bool {
        let Some(policy) = self.active_session().map(|s| s.profile.safety) else {
            return false;
        };
        let first_editable = {
            let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
                return false;
            };
            if let Err(reason) = tab.can_edit_table(policy) {
                self.set_notice(reason, true);
                return false;
            }
            let Some(cols) = tab.columns().map(|c| c.to_vec()) else {
                self.set_notice("Load table columns before inserting a row", true);
                return false;
            };
            let draft_id = tab.staged.stage_insert_row(&cols);
            let col_idx = cols
                .iter()
                .position(|c| c.default_expr.is_none() && !c.is_primary_key)
                .unwrap_or(0);
            tab.commit_error = None;
            tab.fk_peek = None;
            tab.row_menu = None;
            tab.rebuild_diff_state();
            (RowKey::Draft(draft_id), col_idx)
        };
        self.begin_cell_edit(first_editable.0, first_editable.1, tx)
    }

    pub fn toggle_delete_row(&mut self, row_key: RowKey) {
        let Some(policy) = self.active_session().map(|s| s.profile.safety) else {
            return;
        };
        let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) else {
            return;
        };
        if let Err(reason) = tab.can_edit_table(policy) {
            self.set_notice(reason, true);
            return;
        }
        tab.editing_cell = None;
        tab.row_menu = None;
        match row_key {
            RowKey::Draft(id) => {
                tab.staged.remove_insert_row(id);
                if tab.selected_row_key == Some(row_key) {
                    tab.selected_row_key = None;
                    tab.selected_cell = None;
                    tab.data_table.clear_selection();
                }
            }
            RowKey::Existing(idx) => {
                if !tab.has_primary_key() {
                    self.set_notice("Table requires a primary key to delete existing rows safely", true);
                    return;
                }
                if let Some(orig_row) = tab.page.as_ref().and_then(|p| p.rows.get(idx)).cloned() {
                    tab.staged.toggle_delete_row(idx, &orig_row);
                }
            }
        }
        tab.commit_error = None;
        tab.rebuild_diff_state();
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
        if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.staged.clear();
            tab.editing_cell = None;
            tab.commit_error = None;
            tab.diff_drawer_open = false;
            if matches!(tab.selected_row_key, Some(RowKey::Draft(_))) {
                tab.selected_row_key = None;
                tab.selected_cell = None;
                tab.data_table.clear_selection();
            }
            tab.rebuild_diff_state();
        }
        self.set_notice("Discarded staged changes", false);
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
            let engine = session.profile.engine;
            let policy = session.profile.safety;
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
            tab.committing = true;
            tab.commit_req_id = Some(req_id);
            tab.commit_error = None;
            Some((tab_id, stmts))
        };

        let Some((tab_id, statements)) = dispatch else {
            return;
        };
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::CommitStagedChanges {
                session_id,
                tab_id,
                req_id,
                statements,
            });
        } else if let Some(tab) = self.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.committing = false;
            tab.commit_req_id = None;
            tab.staged.clear();
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
        let existing_tab_id = self.sessions.get(&session_id).and_then(|sess| {
            sess.tabs
                .iter()
                .find(|t| t.schema_and_name() == Some((fk.target_schema.as_str(), fk.target_table.as_str())))
                .map(|t| t.id)
        });

        let has_worker = self.worker.is_some();
        let page_req = has_worker.then(|| self.alloc_req_id());
        let struct_req = (has_worker && existing_tab_id.is_none()).then(|| self.alloc_req_id());

        let tab_id = if let Some(tid) = existing_tab_id {
            tid
        } else {
            self.alloc_tab_id()
        };

        let request = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
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
                tab.latest_page_req = Some(rid);
            }
            tab.page_request()
        };

        self.sync_window_title(tx);
        if let Some(worker) = &self.worker {
            if let Some(rid) = page_req {
                worker.send(WorkerCmd::FetchTablePage {
                    session_id,
                    tab_id,
                    req_id: rid,
                    request,
                });
            }
            if let Some(rid) = struct_req {
                worker.send(WorkerCmd::DescribeTable {
                    session_id,
                    tab_id,
                    req_id: rid,
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
            Ok(Some(path)) => match std::fs::write(&path, &content) {
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
            tab_id: TabId,
            engine: DbEngine,
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
            let engine = session.profile.engine;
            let policy = session.profile.safety;
            let env = session.profile.env;
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
                let stmts = split_statements_for_engine(&tab.editor.text, engine);
                if stmts.is_empty() {
                    None
                } else {
                    let sql_text = tab.editor.text.trim().to_owned();
                    Some((stmts.into_iter().map(|s| s.sql).collect(), sql_text))
                }
            } else {
                statements_for_current(&tab.editor, engine)
            };

            let Some((statements, executed_sql_text)) = extracted else {
                self.set_notice("No SQL statement to execute", false);
                return true;
            };

            ReadyQuery {
                tab_id,
                engine,
                policy,
                env,
                statements,
                executed_sql_text,
                row_limit: tab.row_limit,
            }
        };

        let ReadyQuery {
            tab_id,
            engine,
            policy,
            env,
            statements,
            executed_sql_text,
            row_limit,
        } = outcome;

        match evaluate_sql_safety_for_engine(engine, policy, env, &statements) {
            SafetyDecision::Allow => {
                self.dispatch_query_execution(session_id, tab_id, statements, executed_sql_text, row_limit);
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
        let (tab_id, engine, policy, env, statement) = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return false;
            };
            let engine = session.profile.engine;
            let policy = session.profile.safety;
            let env = session.profile.env;
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
            let Some((stmts, _)) = statements_for_current(&tab.editor, engine) else {
                self.set_notice("Place cursor inside a SQL statement to explain", false);
                return true;
            };
            let first = stmts.into_iter().next().unwrap_or_default();
            (tab_id, engine, policy, env, first)
        };

        let analyze = analyze && engine.supports_explain_analyze();
        if analyze {
            let wrapped = vec![build_explain_sql_for_engine(engine, &statement, true)];
            match evaluate_sql_safety_for_engine(engine, policy, env, &wrapped) {
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
                        tab_id,
                        policy,
                        env,
                        reason,
                        statements: inspected,
                        is_destructive,
                        destructive_ack: false,
                        pending: PendingProtectedAction::ExplainAnalyze { statement },
                    }));
                    return true;
                }
            }
        }

        self.dispatch_explain_execution(session_id, tab_id, statement, analyze);
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
        match modal.pending {
            PendingProtectedAction::ExecuteSql {
                statements,
                executed_sql_text,
                row_limit,
            } => {
                self.dispatch_query_execution(
                    modal.session_id,
                    modal.tab_id,
                    statements,
                    executed_sql_text,
                    row_limit,
                );
            }
            PendingProtectedAction::ExplainAnalyze { statement } => {
                self.dispatch_explain_execution(modal.session_id, modal.tab_id, statement, true);
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
    ) {
        let req_id = self.alloc_req_id();
        let dispatch = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let connection_id = session.profile.id;
            let connection_name = session.profile.name.clone();
            let database = session
                .status
                .as_connected()
                .and_then(|c| c.active_database.clone());
            let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut) else {
                return;
            };
            tab.running = true;
            tab.cancelling = false;
            tab.active_req_id = Some(req_id);
            tab.elapsed_ms = 0;
            tab.batch_error = None;
            tab.batch_cancelled = false;
            tab.show_explain = false;
            tab.limit_dropdown_open = false;
            (connection_id, connection_name, database)
        };

        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::ExecuteSql {
                session_id,
                tab_id,
                req_id,
                connection_id: dispatch.0,
                connection_name: dispatch.1,
                database: dispatch.2,
                statements,
                executed_sql_text,
                row_limit,
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
    ) {
        let req_id = self.alloc_req_id();
        if let Some(tab) = self
            .sessions
            .get_mut(&session_id)
            .and_then(|s| s.tab_mut(tab_id))
            .and_then(WorkspaceTab::as_query_mut)
        {
            tab.explain_loading = self.worker.is_some();
            tab.explain_req_id = self.worker.as_ref().map(|_| req_id);
            tab.explain_error = None;
            tab.show_explain = true;
        }
        if let Some(worker) = &self.worker {
            worker.send(WorkerCmd::ExplainSql {
                session_id,
                tab_id,
                req_id,
                statement,
                analyze,
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
            if let Some(worker) = &self.worker {
                worker.send(WorkerCmd::CancelQuery { session_id });
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
        if profile.engine != DbEngine::Sqlite && profile.password_mode == PasswordMode::AskEveryTime && pw.is_none() {
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
                let db_changed = session
                    .status
                    .as_connected()
                    .and_then(|c| c.active_database.as_deref())
                    != Some(active_database.as_str());
                if let ConnectionState::Connected(ref mut data) = session.status {
                    data.databases = databases;
                    data.active_database = Some(active_database);
                    data.schemas = schemas;
                    data.active_schema = active_schema;
                    data.objects = objects;
                    data.expanded_columns.clear();
                    if db_changed {
                        data.selected_object = None;
                    }
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
                        let auto_select = tab.auto_select_first_row;
                        tab.auto_select_first_row = false;
                        tab.page = Some(page);
                        tab.page_error = None;
                        tab.filter_error = None;
                        tab.editing_cell = None;
                        tab.staged.clear();
                        tab.rebuild_diff_state();
                        if auto_select && row_count > 0 {
                            tab.selected_row = Some(0);
                            tab.selected_row_key = Some(RowKey::Existing(0));
                            tab.selected_cell = Some((RowKey::Existing(0), 0));
                            tab.data_table.select("r-0".to_owned());
                        } else if let Some(sel) = tab.selected_row
                            && sel >= row_count
                        {
                            tab.selected_row = None;
                            tab.selected_row_key = None;
                            tab.selected_cell = None;
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
                let disconnected = matches!(result, Err(tv_core::DbError::Disconnected));
                if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_query_mut)
                    && tab.active_req_id == Some(req_id)
                {
                    tab.running = false;
                    tab.cancelling = false;
                    tab.active_req_id = None;
                    match result {
                        Ok(batch) => {
                            let BatchOutcomes {
                                results,
                                total_duration_ms,
                                cancelled,
                                error_msg,
                            } = unpack_batch(tab_id, batch);
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
                            tab.batch_error = Some(err.to_string());
                        }
                    }
                }
                if disconnected {
                    session.status = ConnectionState::Disconnected {
                        reason: Some("Disconnected from database".to_owned()),
                    };
                    if self.screen == Screen::Workspace(session_id) {
                        self.sync_window_title(tx);
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

            WorkerEvent::StagedChangesCommitted {
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
                let mut reload_target: Option<(TabId, PageRequest, RequestId)> = None;
                let mut disconnected = false;

                if let Some(tab) = session.tab_mut(tab_id).and_then(WorkspaceTab::as_table_mut)
                    && tab.commit_req_id == Some(req_id)
                {
                    tab.committing = false;
                    tab.commit_req_id = None;
                    match result {
                        Ok(outcome) => {
                            let changes = tab.staged.change_count();
                            tab.staged.clear();
                            tab.diff_drawer_open = false;
                            tab.commit_error = None;
                            tab.editing_cell = None;
                            tab.rebuild_diff_state();
                            let next_req = self.alloc_req_id();
                            if let Some(t) = self
                                .sessions
                                .get_mut(&session_id)
                                .and_then(|s| s.tab_mut(tab_id))
                                .and_then(WorkspaceTab::as_table_mut)
                            {
                                t.page_loading = true;
                                t.latest_page_req = Some(next_req);
                                reload_target = Some((tab_id, t.page_request(), next_req));
                            }
                            self.set_notice(
                                format!(
                                    "Committed {changes} change(s) ({} rows affected · {} ms)",
                                    outcome.total_rows_affected, outcome.elapsed_ms
                                ),
                                false,
                            );
                        }
                        Err(err) => {
                            if matches!(err, tv_core::DbError::Disconnected) {
                                disconnected = true;
                            }
                            let msg = err.to_string();
                            tab.commit_error = Some(msg.clone());
                            tab.diff_drawer_open = true;
                            self.set_notice(msg, true);
                        }
                    }
                }

                if disconnected && let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.status = ConnectionState::Disconnected {
                        reason: Some("Disconnected from database".to_owned()),
                    };
                    if self.screen == Screen::Workspace(session_id) {
                        self.sync_window_title(tx);
                    }
                }
                if let (Some(worker), Some((tid, request, page_rid))) = (&self.worker, reload_target) {
                    worker.send(WorkerCmd::FetchTablePage {
                        session_id,
                        tab_id: tid,
                        req_id: page_rid,
                        request,
                    });
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
                let disconnected = matches!(result, Err(tv_core::DbError::Disconnected));
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
                            tab.explain_error = Some(err.to_string());
                        }
                    }
                }
                if disconnected {
                    session.status = ConnectionState::Disconnected {
                        reason: Some("Disconnected from database".to_owned()),
                    };
                    if self.screen == Screen::Workspace(session_id) {
                        self.sync_window_title(tx);
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
                            peek.error = Some(err.to_string());
                        }
                    }
                }
            }
        }
    }

    pub fn persist_settings(&self) {
        if !self.persist_enabled {
            return;
        }
        let store = SettingsStore::at(self.settings_path.clone());
        let settings = AppSettings {
            appearance: pref_from_appearance(self.appearance),
            ..AppSettings::default()
        };
        drop(store.save(&settings));
    }

    pub fn set_appearance(&mut self, scheme: ColorScheme, tx: &mut Tx) {
        self.appearance = scheme;
        tx.set_color_scheme(scheme);
        self.persist_settings();
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
                _ => format!("{} (Disconnected) — TableView", session.profile.name),
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
        | UpdateStatus::Failed(UpdateFailure::Relaunch { version }) => Some(version.as_str()),
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

fn unpack_batch(tab_id: TabId, batch: QueryBatchResult) -> BatchOutcomes {
    let tid = tab_id.get();
    let results = batch
        .results
        .into_iter()
        .enumerate()
        .map(|(idx, statement)| QueryResultTabState {
            statement,
            table: TableState::new().key(format!("tv-qres-{tid}-{idx}")),
            selected_row: None,
        })
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

fn statements_for_current(editor: &TextEdit, engine: DbEngine) -> Option<(Vec<String>, String)> {
    if editor.anchor != editor.focus {
        let sel = editor.selected();
        let stmts = split_statements_for_engine(sel, engine);
        if !stmts.is_empty() {
            let texts = stmts.into_iter().map(|s| s.sql).collect();
            return Some((texts, sel.trim().to_owned()));
        }
    }
    let stmt = statement_at_cursor_for_engine(&editor.text, editor.anchor, editor.focus, engine)?;
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
