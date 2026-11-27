use std::collections::HashMap;

use chrono::{Duration, Utc};
use rune::ColorScheme;
use tv_core::{
    AccentColor, CellValue, ColumnMeta, ColumnSummary, ConnectionProfile, DatabaseInfo, DbObject, DbObjectKind,
    EnvTag, ExecutedSql, ExplainPlan, ExportFormat, FkPeekRow, ForeignKeyRef, IndexInfo, PasswordMode,
    QueryHistoryEntry, QueryStatementResult, RequestId, SafetyPolicy, SavedQuery, SavedQueryId, SchemaInfo,
    SessionId, SortSpec, SslMode, StagedCellValue, StatementRisk, StoreData, TablePage, TableStats, TableStructure,
    inspect_statement,
};

use crate::state::{
    AppState, ColumnLoadState, ConnectedData, ConnectionFormState, ConnectionState, ExportModalState, FkPeekState,
    OverlayState, PendingProtectedAction, PickerKind, PickerState, QueryResultTabState, QueryTabState, RowKey,
    SafetyConfirmModalState, SaveQueryModalState, Screen, SessionState, SidebarTab, TableViewMode, WorkspaceTab,
};
use tv_update::UpdateStatus;

/// Named headless scene entry registered on `AppSpec::scenes`.
pub type SceneEntry = (&'static str, fn(&mut AppState));

/// Headless scenes registered on `AppSpec::scenes` for deterministic visual and DOM inspection.
pub const SCENES: &[SceneEntry] = &[
    ("launcher-dark", scene_launcher_dark),
    ("launcher-light", scene_launcher_light),
    ("launcher-settings-dark", scene_launcher_settings_dark),
    ("launcher-settings-light", scene_launcher_settings_light),
    ("connection-form", scene_connection_form),
    ("workspace-disconnected", scene_workspace_disconnected),
    ("workspace-no-db", scene_workspace_no_db),
    ("workspace-connected-dark", scene_workspace_connected_dark),
    ("workspace-connected-light", scene_workspace_connected_light),
    ("workspace-db-picker", scene_workspace_db_picker),
    ("workspace-data-grid-dark", scene_workspace_data_grid_dark),
    ("workspace-data-grid-light", scene_workspace_data_grid_light),
    ("workspace-structure", scene_workspace_structure),
    ("workspace-indexes", scene_workspace_indexes),
    ("workspace-filter-error", scene_workspace_filter_error),
    ("workspace-sql-editor-dark", scene_workspace_sql_editor_dark),
    ("workspace-sql-editor-light", scene_workspace_sql_editor_light),
    ("workspace-sql-saved-queries", scene_workspace_sql_saved_queries),
    ("workspace-save-query-modal", scene_workspace_save_query_modal),
    ("workspace-staged-edits-dark", scene_workspace_staged_edits_dark),
    ("workspace-staged-diff-drawer", scene_workspace_staged_diff_drawer),
    ("workspace-fk-peek", scene_workspace_fk_peek),
    ("workspace-safety-confirm", scene_workspace_safety_confirm),
    ("workspace-explain-plan", scene_workspace_explain_plan),
    ("workspace-export-modal", scene_workspace_export_modal),
    ("launcher-update-ready", scene_launcher_update_ready),
    ("workspace-update-ready", scene_workspace_update_ready),
    ("workspace-update-downloading", scene_workspace_update_downloading),
];

/// Builds deterministic Greendale Community College sample profiles for headless scenes and tests.
pub fn sample_store_data() -> StoreData {
    let mut local = ConnectionProfile::new_postgres("greendale_local", "127.0.0.1", "troy_barnes");
    local.env = EnvTag::Local;
    local.color = AccentColor::Green;
    local.password_mode = PasswordMode::None;
    local.ssl.mode = SslMode::Disable;
    local.database = Some("greendale_idp_dev".to_owned());

    let mut dev = ConnectionProfile::new_postgres("study_room_dev", "study-room-f.greendale.internal", "abed_nadir");
    dev.env = EnvTag::Development;
    dev.color = AccentColor::Blue;
    dev.safety = SafetyPolicy::ConfirmWrites;
    dev.database = Some("study_groups_dev".to_owned());

    let mut staging = ConnectionProfile::new_postgres("campus_staging", "staging-db.greendale.edu", "annie_edison");
    staging.env = EnvTag::Staging;
    staging.color = AccentColor::Amber;
    staging.safety = SafetyPolicy::StagedOnly;
    staging.ssl.mode = SslMode::VerifyCa;
    staging.database = Some("campus_billing_staging".to_owned());

    let mut prod = ConnectionProfile::new_postgres("greendale_prod", "prod-writer.greendale.edu", "dean_pelton");
    prod.env = EnvTag::Production;
    prod.color = AccentColor::Red;
    prod.safety = SafetyPolicy::ReadOnly;
    prod.ssl.mode = SslMode::VerifyFull;
    prod.database = Some("greendale_prod".to_owned());

    StoreData {
        version: tv_core::storage::STORE_SCHEMA_VERSION,
        folders: vec!["Greendale".to_owned()],
        connections: vec![local, dev, staging, prod],
    }
}

/// Resets `AppState` to an isolated in-memory sample store (never touching disk or live PG).
pub fn reset_to_sample_state(s: &mut AppState, dark: bool) {
    let mut fresh = AppState::init_in_memory(sample_store_data());
    fresh.appearance = if dark { ColorScheme::Dark } else { ColorScheme::Light };
    *s = fresh;
}

fn scene_launcher_dark(s: &mut AppState) {
    reset_to_sample_state(s, true);
}

fn scene_launcher_light(s: &mut AppState) {
    reset_to_sample_state(s, false);
}

fn scene_launcher_settings_dark(s: &mut AppState) {
    reset_to_sample_state(s, true);
    s.update.snapshot.status = UpdateStatus::UpToDate;
    s.update.snapshot.automatic = true;
    s.overlay = OverlayState::Settings;
}

fn scene_launcher_settings_light(s: &mut AppState) {
    reset_to_sample_state(s, false);
    s.update.snapshot.status = UpdateStatus::UpToDate;
    s.update.snapshot.automatic = true;
    s.overlay = OverlayState::Settings;
}

fn scene_connection_form(s: &mut AppState) {
    reset_to_sample_state(s, true);
    let mut form = ConnectionFormState::new_blank();
    form.name = rune::TextEdit::new("greendale_study_room");
    form.host = rune::TextEdit::new("127.0.0.1");
    form.port = rune::TextEdit::new("5432");
    form.user = rune::TextEdit::new("jeff_winger");
    form.database = rune::TextEdit::new("study_groups_dev");
    form.env = EnvTag::Development;
    form.color = AccentColor::Blue;
    form.safety = SafetyPolicy::ConfirmWrites;
    s.overlay = OverlayState::ConnectionForm(Box::new(form));
}

fn scene_workspace_disconnected(s: &mut AppState) {
    reset_to_sample_state(s, false);
    let profile = s.connections[0].clone();
    let sid = SessionId::new(1);
    let mut session = SessionState::new(sid, profile, RequestId::new(1));
    session.status = ConnectionState::Disconnected {
        reason: Some("Connection closed by local server".to_owned()),
    };
    s.sessions.insert(sid, session);
    s.screen = Screen::Workspace(sid);
}

fn scene_workspace_no_db(s: &mut AppState) {
    reset_to_sample_state(s, true);
    let mut profile = s.connections[0].clone();
    profile.database = None;
    let sid = SessionId::new(1);
    let mut session = SessionState::new(sid, profile, RequestId::new(1));
    session.status = ConnectionState::Connected(Box::new(ConnectedData {
        server_version: "PostgreSQL 18.4".to_owned(),
        databases: sample_databases(),
        active_database: None,
        schemas: Vec::new(),
        active_schema: None,
        objects: Vec::new(),
        expanded_columns: HashMap::new(),
        selected_object: None,
    }));
    session.console_entries = sample_logs();
    s.sessions.insert(sid, session);
    s.screen = Screen::Workspace(sid);
}

fn scene_workspace_connected_dark(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, Some(0));
}

fn scene_workspace_connected_light(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Data, None);
}

fn scene_workspace_db_picker(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Data, None);
    s.overlay = OverlayState::Picker(PickerState::new(PickerKind::Database));
}

fn scene_workspace_data_grid_dark(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, Some(0));
}

fn scene_workspace_data_grid_light(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Data, None);
}

fn scene_workspace_structure(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Structure, None);
}

fn scene_workspace_indexes(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Indexes, None);
}

fn scene_workspace_filter_error(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, None);
    if let Some(tab) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
        tab.filter_input = rune::TextEdit::new("gpa >> 'abc'");
        tab.applied_filter = Some("gpa >> 'abc'".to_owned());
        tab.filter_error = Some(
            "operator does not exist: numeric >> unknown — No operator matches the given name and argument types."
                .to_owned(),
        );
    }
}

fn setup_connected_workspace(
    s: &mut AppState,
    dark: bool,
    view_mode: TableViewMode,
    selected_row: Option<usize>,
) {
    reset_to_sample_state(s, dark);
    let profile = s.connections[0].clone();
    let sid = SessionId::new(1);
    let mut session = SessionState::new(sid, profile, RequestId::new(1));

    let students_cols = sample_students_columns();
    let sidebar_cols: Vec<ColumnSummary> = students_cols
        .iter()
        .take(5)
        .map(|c| ColumnSummary {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            nullable: c.nullable,
            is_primary_key: c.is_primary_key,
        })
        .collect();

    let mut expanded_columns = HashMap::new();
    expanded_columns.insert(
        ("public".to_owned(), "students".to_owned()),
        ColumnLoadState::Loaded(sidebar_cols),
    );

    session.status = ConnectionState::Connected(Box::new(ConnectedData {
        server_version: "PostgreSQL 18.4".to_owned(),
        databases: sample_databases(),
        active_database: Some("greendale_idp_dev".to_owned()),
        schemas: vec![
            SchemaInfo {
                name: "public".to_owned(),
                is_system: false,
            },
            SchemaInfo {
                name: "study_room".to_owned(),
                is_system: false,
            },
            SchemaInfo {
                name: "pg_catalog".to_owned(),
                is_system: true,
            },
        ],
        active_schema: Some("public".to_owned()),
        objects: vec![
            DbObject {
                schema: "public".to_owned(),
                name: "active_students_view".to_owned(),
                kind: DbObjectKind::View,
                estimated_rows: None,
                total_bytes: None,
            },
            DbObject {
                schema: "public".to_owned(),
                name: "courses".to_owned(),
                kind: DbObjectKind::Table,
                estimated_rows: Some(42),
                total_bytes: Some(32_768),
            },
            DbObject {
                schema: "public".to_owned(),
                name: "enrollments".to_owned(),
                kind: DbObjectKind::Table,
                estimated_rows: Some(128),
                total_bytes: Some(49_152),
            },
            DbObject {
                schema: "public".to_owned(),
                name: "students".to_owned(),
                kind: DbObjectKind::Table,
                estimated_rows: Some(8),
                total_bytes: Some(98_304),
            },
            DbObject {
                schema: "public".to_owned(),
                name: "study_groups".to_owned(),
                kind: DbObjectKind::Table,
                estimated_rows: Some(7),
                total_bytes: Some(16_384),
            },
        ],
        expanded_columns,
        selected_object: Some(("public".to_owned(), "students".to_owned())),
    }));

    let students_tab_id = s.alloc_tab_id();
    let courses_tab_id = s.alloc_tab_id();
    let mut students_tab = WorkspaceTab::new_table(
        students_tab_id,
        "public".to_owned(),
        "students".to_owned(),
        DbObjectKind::Table,
        None,
        None,
    );
    if let Some(t) = students_tab.as_table_mut() {
        t.view_mode = view_mode;
        t.sort = Some(SortSpec {
            column: "student_id".to_owned(),
            ascending: true,
        });
        t.data_table.set_sort("student_id", rune_kit::SortDir::Ascending);
        t.page = Some(sample_students_page(students_cols.clone()));
        t.structure = Some(sample_students_structure(students_cols));
        t.selected_row = selected_row;
        if let Some(idx) = selected_row {
            t.data_table.select(format!("r-{idx}"));
        }
    }

    let mut courses_tab = WorkspaceTab::new_table(
        courses_tab_id,
        "public".to_owned(),
        "courses".to_owned(),
        DbObjectKind::Table,
        None,
        None,
    );
    if let Some(t) = courses_tab.as_table_mut() {
        t.page = Some(sample_courses_page());
    }

    session.tabs = vec![students_tab, courses_tab];
    session.active_tab = Some(students_tab_id);
    session.console_entries = sample_logs();
    let conn_id = session.profile.id;
    session.saved_queries = sample_saved_queries(conn_id);
    session.query_history = sample_query_history(conn_id);
    s.sessions.insert(sid, session);
    s.screen = Screen::Workspace(sid);
}

fn scene_workspace_sql_editor_dark(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, None);
    let cols = sample_students_columns();
    let full_page = sample_students_page(cols.clone());
    let enrolled_rows: Vec<Vec<CellValue>> = full_page
        .rows
        .into_iter()
        .filter(|r| matches!(r.get(4), Some(CellValue::Bool(true))))
        .collect();
    let enrolled_count = enrolled_rows.len();

    let sql = concat!(
        "-- Greendale Community College active roster\n",
        "SELECT\n",
        "    student_id,\n",
        "    id,\n",
        "    full_name,\n",
        "    email,\n",
        "    enrolled,\n",
        "    gpa,\n",
        "    profile,\n",
        "    created_at,\n",
        "    group_id\n",
        "FROM public.students\n",
        "WHERE enrolled = true\n",
        "ORDER BY student_id;\n\n",
        "SELECT COUNT(*) AS active_students FROM public.students WHERE enrolled = true;",
    );

    let tid = s.alloc_tab_id();
    let mut qtab = QueryTabState::new(tid, "Active Greendale Roster".to_owned(), sql.to_owned(), None);
    if let Some(saved_id) = s
        .active_session()
        .and_then(|sess| sess.saved_queries.first().map(|q| q.id))
    {
        qtab.saved_query_id = Some(saved_id);
        qtab.saved_snapshot = sql.to_owned();
    }
    qtab.total_duration_ms = Some(11);
    let mut t1 = rune_kit::TableState::new().key("tv-qres-3-0");
    t1.select("r-0");
    let res1 = QueryResultTabState {
        statement: QueryStatementResult {
            sql: "SELECT student_id, id, full_name, email, enrolled, gpa, profile, created_at, group_id FROM public.students WHERE enrolled = true ORDER BY student_id".to_owned(),
            columns: cols,
            rows: enrolled_rows,
            rows_affected: None,
            total_rows_seen: enrolled_count,
            truncated: false,
            elapsed_ms: 8,
            error: None,
        },
        table: t1,
        selected_row: Some(0),
    };
    let res2 = QueryResultTabState {
        statement: QueryStatementResult {
            sql: "SELECT COUNT(*) AS active_students FROM public.students WHERE enrolled = true".to_owned(),
            columns: vec![ColumnMeta {
                name: "active_students".to_owned(),
                data_type: "bigint".to_owned(),
                udt_name: "int8".to_owned(),
                nullable: false,
                default_expr: None,
                is_primary_key: false,
                is_unique: false,
                comment: None,
                foreign_key: None,
            }],
            rows: vec![vec![CellValue::Int(enrolled_count as i64)]],
            rows_affected: None,
            total_rows_seen: 1,
            truncated: false,
            elapsed_ms: 3,
            error: None,
        },
        table: rune_kit::TableState::new().key("tv-qres-3-1"),
        selected_row: None,
    };
    qtab.results = vec![res1, res2];

    if let Some(session) = s.active_session_mut() {
        session.tabs.push(WorkspaceTab {
            id: tid,
            content: crate::state::TabContent::Query(Box::new(qtab)),
        });
        session.active_tab = Some(tid);
    }
}

fn scene_workspace_sql_editor_light(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Data, None);
    let sql = "SELECT pg_sleep(5),\n       s.full_name,\n       g.topic\nFROM public.students s\nJOIN public.study_groups g ON g.group_id = s.group_id\nWHERE s.gpa >= 3.0;";
    let tid = s.alloc_tab_id();
    let mut qtab = QueryTabState::new(tid, "Query 1".to_owned(), sql.to_owned(), None);
    qtab.running = true;
    qtab.elapsed_ms = 2400;
    qtab.active_req_id = Some(RequestId::new(99));

    if let Some(session) = s.active_session_mut() {
        session.sidebar_tab = SidebarTab::History;
        session.tabs.push(WorkspaceTab {
            id: tid,
            content: crate::state::TabContent::Query(Box::new(qtab)),
        });
        session.active_tab = Some(tid);
    }
}

fn scene_workspace_sql_saved_queries(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, None);
    let saved_info = s.active_session().and_then(|sess| {
        sess.saved_queries
            .get(2)
            .map(|sq| (sq.id, sq.name.clone(), sq.sql.clone()))
    });
    let Some((saved_id, saved_name, saved_sql)) = saved_info else {
        return;
    };
    let tid = s.alloc_tab_id();
    let mut qtab = QueryTabState::new(tid, saved_name, saved_sql.clone(), Some(saved_id));
    qtab.total_duration_ms = Some(6);
    qtab.results = vec![QueryResultTabState {
        statement: QueryStatementResult {
            sql: saved_sql,
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: Some(1),
            total_rows_seen: 0,
            truncated: false,
            elapsed_ms: 6,
            error: None,
        },
        table: rune_kit::TableState::new().key("tv-qres-3-0"),
        selected_row: None,
    }];

    if let Some(session) = s.active_session_mut() {
        session.sidebar_tab = SidebarTab::Queries;
        session.tabs.push(WorkspaceTab {
            id: tid,
            content: crate::state::TabContent::Query(Box::new(qtab)),
        });
        session.active_tab = Some(tid);
    }
}

fn scene_workspace_save_query_modal(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, None);
    let tid = s.alloc_tab_id();
    let sql = "SELECT full_name, gpa\nFROM public.students\nWHERE gpa >= 3.5\nORDER BY gpa DESC;";
    let qtab = QueryTabState::new(tid, "Query 1".to_owned(), sql.to_owned(), None);

    let sid = if let Some(session) = s.active_session_mut() {
        session.tabs.push(WorkspaceTab {
            id: tid,
            content: crate::state::TabContent::Query(Box::new(qtab)),
        });
        session.active_tab = Some(tid);
        session.id
    } else {
        SessionId::new(1)
    };

    s.overlay = OverlayState::SaveQuery(Box::new(SaveQueryModalState {
        session_id: sid,
        tab_id: tid,
        editing_id: None,
        name: rune::TextEdit::new("Dean's Honor List"),
        folder: rune::TextEdit::new("Reporting"),
        scope_all_connections: false,
        error: None,
    }));
}

fn populate_sample_staged_changes(t: &mut crate::state::TableTabState) {
    let Some(cols) = t.columns().map(|c| c.to_vec()) else {
        return;
    };
    let Some(rows) = t.page.as_ref().map(|p| p.rows.clone()) else {
        return;
    };
    if let Some(row0) = rows.first() {
        t.staged.stage_cell_edit(
            0,
            row0,
            2,
            StagedCellValue::Literal("Troy 'T-Bone' Barnes".to_owned()),
        );
        t.staged
            .stage_cell_edit(0, row0, 5, StagedCellValue::Literal("3.60".to_owned()));
    }
    if let Some(row3) = rows.get(3) {
        t.staged.toggle_delete_row(3, row3);
    }
    let draft = t.staged.stage_insert_row(&cols);
    t.staged.stage_insert_cell_edit(
        draft,
        2,
        StagedCellValue::Literal("Señor Ben Chang".to_owned()),
    );
    t.staged
        .stage_insert_cell_edit(draft, 3, StagedCellValue::Literal("chang@greendale.edu".to_owned()));
    t.staged
        .stage_insert_cell_edit(draft, 5, StagedCellValue::Literal("2.40".to_owned()));
    t.staged
        .stage_insert_cell_edit(draft, 8, StagedCellValue::Literal("1".to_owned()));
    t.rebuild_diff_state();
}

fn scene_workspace_staged_edits_dark(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, Some(0));
    if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
        populate_sample_staged_changes(t);
        t.selected_row = Some(0);
        t.selected_row_key = Some(RowKey::Existing(0));
        t.selected_cell = Some((RowKey::Existing(0), 2));
        t.data_table.select("r-0");
    }
}

fn scene_workspace_staged_diff_drawer(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Data, Some(0));
    if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
        populate_sample_staged_changes(t);
        t.diff_drawer_open = true;
    }
}

fn scene_workspace_fk_peek(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, Some(0));
    if let Some(sess) = s.active_session_mut() {
        sess.inspector_pane.set_open(false);
    }
    if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
        // Bring group_id into the primary visible viewport for clean screenshot & test visibility.
        t.data_table.hide_column("id");
        t.data_table.hide_column("profile");
        t.data_table.hide_column("created_at");
        let fk = sample_students_columns()[8]
            .foreign_key
            .clone()
            .unwrap_or_else(|| ForeignKeyRef {
                constraint_name: "students_group_id_fkey".to_owned(),
                target_schema: "public".to_owned(),
                target_table: "study_groups".to_owned(),
                target_column: "group_id".to_owned(),
            });
        let source_col = sample_students_columns()[8].clone();
        t.fk_peek = Some(FkPeekState {
            anchor_key: "fk-peek-r-0-8".to_owned(),
            fk: fk.clone(),
            value: CellValue::Int(1),
            source_col,
            req_id: None,
            loading: false,
            result: Some(FkPeekRow {
                fk,
                lookup_display: "group_id = 1".to_owned(),
                columns: vec![
                    ColumnMeta {
                        name: "group_id".to_owned(),
                        data_type: "integer".to_owned(),
                        udt_name: "int4".to_owned(),
                        nullable: false,
                        default_expr: None,
                        is_primary_key: true,
                        is_unique: false,
                        comment: None,
                        foreign_key: None,
                    },
                    ColumnMeta {
                        name: "room_name".to_owned(),
                        data_type: "text".to_owned(),
                        udt_name: "text".to_owned(),
                        nullable: false,
                        default_expr: None,
                        is_primary_key: false,
                        is_unique: false,
                        comment: None,
                        foreign_key: None,
                    },
                    ColumnMeta {
                        name: "topic".to_owned(),
                        data_type: "text".to_owned(),
                        udt_name: "text".to_owned(),
                        nullable: false,
                        default_expr: None,
                        is_primary_key: false,
                        is_unique: false,
                        comment: None,
                        foreign_key: None,
                    },
                    ColumnMeta {
                        name: "leader".to_owned(),
                        data_type: "text".to_owned(),
                        udt_name: "text".to_owned(),
                        nullable: true,
                        default_expr: None,
                        is_primary_key: false,
                        is_unique: false,
                        comment: None,
                        foreign_key: None,
                    },
                ],
                row: Some(vec![
                    CellValue::Int(1),
                    CellValue::Text("Study Room F".to_owned()),
                    CellValue::Text("Spanish 101".to_owned()),
                    CellValue::Text("Jeff Winger".to_owned()),
                ]),
                elapsed_ms: 2,
            }),
            error: None,
        });
    }
}

fn scene_workspace_safety_confirm(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, None);
    let tid = s.alloc_tab_id();
    let dangerous_sql = "DELETE FROM public.students;\nUPDATE public.students SET gpa = 0.0;";
    let qtab = QueryTabState::new(tid, "Cleanup Script".to_owned(), dangerous_sql.to_owned(), None);
    let sid = if let Some(sess) = s.active_session_mut() {
        sess.profile.env = EnvTag::Production;
        sess.profile.color = AccentColor::Red;
        sess.profile.safety = SafetyPolicy::ConfirmWrites;
        sess.profile.name = "greendale_prod".to_owned();
        sess.tabs.push(WorkspaceTab {
            id: tid,
            content: crate::state::TabContent::Query(Box::new(qtab)),
        });
        sess.active_tab = Some(tid);
        sess.id
    } else {
        SessionId::new(1)
    };

    let inspected = vec![
        inspect_statement("DELETE FROM public.students;"),
        inspect_statement("UPDATE public.students SET gpa = 0.0;"),
    ];
    let is_destructive = inspected.iter().any(|st| st.risk == StatementRisk::Destructive);
    s.overlay = OverlayState::SafetyConfirm(Box::new(SafetyConfirmModalState {
        session_id: sid,
        tab_id: tid,
        policy: SafetyPolicy::ConfirmWrites,
        env: EnvTag::Production,
        reason: "Production safeguard: destructive SQL modifies or removes rows without a WHERE filter.".to_owned(),
        statements: inspected,
        is_destructive,
        destructive_ack: false,
        pending: PendingProtectedAction::ExecuteSql {
            statements: vec![
                "DELETE FROM public.students".to_owned(),
                "UPDATE public.students SET gpa = 0.0".to_owned(),
            ],
            executed_sql_text: dangerous_sql.to_owned(),
            row_limit: Some(1000),
        },
    }));
}

fn scene_workspace_explain_plan(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, None);
    let sql = "SELECT s.student_id, s.full_name, g.room_name\nFROM public.students s\nJOIN public.study_groups g ON g.group_id = s.group_id\nWHERE s.gpa >= 3.0\nORDER BY s.gpa DESC;";
    let raw_plan = r#"[
  {
    "Plan": {
      "Node Type": "Sort",
      "Startup Cost": 22.84,
      "Total Cost": 23.10,
      "Plan Rows": 6,
      "Plan Width": 68,
      "Actual Startup Time": 0.910,
      "Actual Total Time": 0.940,
      "Actual Rows": 6,
      "Actual Loops": 1,
      "Sort Key": ["s.gpa DESC"],
      "Shared Hit Blocks": 18,
      "Shared Read Blocks": 2,
      "Plans": [
        {
          "Node Type": "Hash Join",
          "Join Type": "Inner",
          "Startup Cost": 1.15,
          "Total Cost": 21.05,
          "Plan Rows": 6,
          "Plan Width": 68,
          "Actual Startup Time": 0.310,
          "Actual Total Time": 0.780,
          "Actual Rows": 6,
          "Actual Loops": 1,
          "Hash Cond": "(s.group_id = g.group_id)",
          "Plans": [
            {
              "Node Type": "Seq Scan",
              "Relation Name": "students",
              "Schema": "public",
              "Alias": "s",
              "Startup Cost": 0.00,
              "Total Cost": 18.40,
              "Plan Rows": 1,
              "Plan Width": 44,
              "Actual Startup Time": 0.045,
              "Actual Total Time": 0.560,
              "Actual Rows": 42,
              "Actual Loops": 1,
              "Filter": "(gpa >= 3.0)",
              "Rows Removed by Filter": 2
            },
            {
              "Node Type": "Hash",
              "Startup Cost": 1.05,
              "Total Cost": 1.05,
              "Plan Rows": 4,
              "Plan Width": 32,
              "Actual Startup Time": 0.080,
              "Actual Total Time": 0.095,
              "Actual Rows": 4,
              "Actual Loops": 1,
              "Plans": [
                {
                  "Node Type": "Index Scan",
                  "Relation Name": "study_groups",
                  "Schema": "public",
                  "Index Name": "study_groups_pkey",
                  "Startup Cost": 0.14,
                  "Total Cost": 1.05,
                  "Plan Rows": 4,
                  "Plan Width": 32,
                  "Actual Startup Time": 0.020,
                  "Actual Total Time": 0.045,
                  "Actual Rows": 4,
                  "Actual Loops": 1
                }
              ]
            }
          ]
        }
      ]
    },
    "Planning Time": 0.285,
    "Execution Time": 1.042
  }
]"#;

    let tid = s.alloc_tab_id();
    let mut qtab = QueryTabState::new(tid, "Honor Roll Join Plan".to_owned(), sql.to_owned(), None);
    qtab.split_ratio = 0.28;
    qtab.show_explain = true;
    qtab.explain_plan = ExplainPlan::parse_json(sql.to_owned(), true, raw_plan).ok();

    if let Some(sess) = s.active_session_mut() {
        sess.inspector_pane.set_open(false);
        sess.tabs.push(WorkspaceTab {
            id: tid,
            content: crate::state::TabContent::Query(Box::new(qtab)),
        });
        sess.active_tab = Some(tid);
    }
}

fn scene_workspace_export_modal(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Data, Some(0));
    let cols = sample_students_columns();
    let page = sample_students_page(cols.clone());
    s.overlay = OverlayState::Export(Box::new(ExportModalState {
        source_label: "public.students".to_owned(),
        schema: Some("public".to_owned()),
        table_name: rune::TextEdit::new("students"),
        columns: cols,
        rows: page.rows,
        format: ExportFormat::SqlInsert,
    }));
}

fn sample_saved_queries(conn_id: tv_core::ConnectionId) -> Vec<SavedQuery> {
    let now = Utc::now();
    vec![
        SavedQuery {
            id: SavedQueryId::new(),
            connection_id: Some(conn_id),
            folder: Some("Roster".to_owned()),
            name: "Active Greendale Roster".to_owned(),
            sql: "SELECT student_id, full_name, email, gpa FROM public.students WHERE enrolled = true ORDER BY gpa DESC;".to_owned(),
            updated_at: now - Duration::minutes(5),
        },
        SavedQuery {
            id: SavedQueryId::new(),
            connection_id: None,
            folder: Some("Analytics".to_owned()),
            name: "Study Group Enrollment".to_owned(),
            sql: "SELECT g.topic, COUNT(s.student_id) AS members FROM public.study_groups g LEFT JOIN public.students s ON s.group_id = g.group_id GROUP BY g.topic;".to_owned(),
            updated_at: now - Duration::hours(2),
        },
        SavedQuery {
            id: SavedQueryId::new(),
            connection_id: Some(conn_id),
            folder: Some("Admin".to_owned()),
            name: "Fix Dean Pelton Budget".to_owned(),
            sql: "UPDATE public.students SET enrolled = true WHERE full_name = 'Jeff Winger';".to_owned(),
            updated_at: now - Duration::days(1),
        },
    ]
}

fn sample_query_history(conn_id: tv_core::ConnectionId) -> Vec<QueryHistoryEntry> {
    let now = Utc::now();
    vec![
        QueryHistoryEntry {
            id: 1,
            connection_id: Some(conn_id),
            connection_name: "greendale_local".to_owned(),
            database_name: Some("greendale_idp_dev".to_owned()),
            sql: "SELECT student_id, full_name, gpa FROM public.students WHERE enrolled = true ORDER BY student_id;".to_owned(),
            executed_at: now - Duration::minutes(3),
            duration_ms: 8,
            rows_count: Some(6),
            error: None,
        },
        QueryHistoryEntry {
            id: 2,
            connection_id: Some(conn_id),
            connection_name: "greendale_local".to_owned(),
            database_name: Some("greendale_idp_dev".to_owned()),
            sql: "UPDATE public.students SET gpa = 3.95 WHERE full_name = 'Annie Edison';".to_owned(),
            executed_at: now - Duration::minutes(19),
            duration_ms: 4,
            rows_count: Some(1),
            error: None,
        },
        QueryHistoryEntry {
            id: 3,
            connection_id: Some(conn_id),
            connection_name: "greendale_local".to_owned(),
            database_name: Some("greendale_idp_dev".to_owned()),
            sql: "SELECT * FROM public.nonexistent_table;".to_owned(),
            executed_at: now - Duration::hours(1),
            duration_ms: 2,
            rows_count: None,
            error: Some("relation \"public.nonexistent_table\" does not exist".to_owned()),
        },
    ]
}

fn sample_students_columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta {
            name: "student_id".to_owned(),
            data_type: "integer".to_owned(),
            udt_name: "int4".to_owned(),
            nullable: false,
            default_expr: Some("nextval('students_student_id_seq'::regclass)".to_owned()),
            is_primary_key: true,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "id".to_owned(),
            data_type: "uuid".to_owned(),
            udt_name: "uuid".to_owned(),
            nullable: false,
            default_expr: Some("gen_random_uuid()".to_owned()),
            is_primary_key: false,
            is_unique: true,
            comment: Some("External OIDC subject identifier".to_owned()),
            foreign_key: None,
        },
        ColumnMeta {
            name: "full_name".to_owned(),
            data_type: "text".to_owned(),
            udt_name: "text".to_owned(),
            nullable: false,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: Some("Preferred student display name".to_owned()),
            foreign_key: None,
        },
        ColumnMeta {
            name: "email".to_owned(),
            data_type: "text".to_owned(),
            udt_name: "text".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "enrolled".to_owned(),
            data_type: "boolean".to_owned(),
            udt_name: "bool".to_owned(),
            nullable: false,
            default_expr: Some("true".to_owned()),
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "gpa".to_owned(),
            data_type: "numeric(4,2)".to_owned(),
            udt_name: "numeric".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "profile".to_owned(),
            data_type: "jsonb".to_owned(),
            udt_name: "jsonb".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: Some("Study group preferences and extracurricular metadata".to_owned()),
            foreign_key: None,
        },
        ColumnMeta {
            name: "created_at".to_owned(),
            data_type: "timestamp with time zone".to_owned(),
            udt_name: "timestamptz".to_owned(),
            nullable: false,
            default_expr: Some("now()".to_owned()),
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "group_id".to_owned(),
            data_type: "integer".to_owned(),
            udt_name: "int4".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: Some(ForeignKeyRef {
                constraint_name: "students_group_id_fkey".to_owned(),
                target_schema: "public".to_owned(),
                target_table: "study_groups".to_owned(),
                target_column: "group_id".to_owned(),
            }),
        },
    ]
}

fn sample_students_page(columns: Vec<ColumnMeta>) -> TablePage {
    let ts = |s: &str| CellValue::parse_from_pg_text("timestamptz", Some(s.to_owned()));
    let uid = |s: &str| CellValue::parse_from_pg_text("uuid", Some(s.to_owned()));

    let rows = vec![
        vec![
            CellValue::Int(1),
            uid("210b3d6b-da52-4259-ac05-b58d3d0a6d5b"),
            CellValue::Text("Troy Barnes".to_owned()),
            CellValue::Text("troy@greendale.edu".to_owned()),
            CellValue::Bool(true),
            CellValue::Numeric("3.45".to_owned()),
            CellValue::Json(r#"{"campus":"Greendale","major":"Air Conditioning Repair","best_friend":"Abed Nadir","handshake":true}"#.to_owned()),
            ts("2026-11-19 18:09:11.432+00"),
            CellValue::Int(1),
        ],
        vec![
            CellValue::Int(2),
            uid("8c5b8676-0203-44ba-9198-90125b690e81"),
            CellValue::Text("Abed Nadir".to_owned()),
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::Numeric("3.92".to_owned()),
            CellValue::Json(r#"{"show":"Inspector Spacetime","dreamatorium":true,"season":6}"#.to_owned()),
            ts("2026-11-19 18:24:56.558+00"),
            CellValue::Int(1),
        ],
        vec![
            CellValue::Int(3),
            uid("c1b7e813-9f22-4a41-a2b0-4f7c11223344"),
            CellValue::Text("Annie Edison".to_owned()),
            CellValue::Text(String::new()),
            CellValue::Bool(true),
            CellValue::Numeric("4.00".to_owned()),
            CellValue::Json(r#"{"supplies":{"purple_pens":14,"binders":6},"valedictorian_track":true}"#.to_owned()),
            ts("2026-11-19 19:02:10.120+00"),
            CellValue::Int(1),
        ],
        vec![
            CellValue::Int(4),
            uid("72d1b299-4a15-4088-9022-8f61d4e70a11"),
            CellValue::Text("Jeff Winger".to_owned()),
            CellValue::Text("jeff@greendale.edu".to_owned()),
            CellValue::Bool(false),
            CellValue::Numeric("2.85".to_owned()),
            CellValue::Null,
            ts("2026-11-20 09:15:00.000+00"),
            CellValue::Int(1),
        ],
        vec![
            CellValue::Int(5),
            uid("a4d5c6e7-1122-4344-8556-77889900aabb"),
            CellValue::Text("Britta Perry".to_owned()),
            CellValue::Text("britta@greendale.edu".to_owned()),
            CellValue::Bool(true),
            CellValue::Numeric("3.10".to_owned()),
            CellValue::Json(r#"{"major":"Psychology","pronunciation":"baggel"}"#.to_owned()),
            ts("2026-11-21 11:40:22.891+00"),
            CellValue::Int(1),
        ],
        vec![
            CellValue::Int(6),
            uid("e9a01f23-5566-4778-99aa-bcdef0123456"),
            CellValue::Text("Shirley Bennett".to_owned()),
            CellValue::Text("shirley@greendale.edu".to_owned()),
            CellValue::Bool(true),
            CellValue::Numeric("3.88".to_owned()),
            CellValue::Json(r#"{"business":"Shirley's Sandwiches","specialties":["brownies","paninis"]}"#.to_owned()),
            ts("2026-11-22 14:05:18.004+00"),
            CellValue::Int(1),
        ],
        vec![
            CellValue::Int(7),
            uid("f0011223-3344-4556-9778-899aabbccdde"),
            CellValue::Text("Pierce Hawthorne".to_owned()),
            CellValue::Null,
            CellValue::Bool(false),
            CellValue::Null,
            CellValue::Json(r#"{"company":"Hawthorne Wipes","keyboard":"level 5 laser lotus"}"#.to_owned()),
            ts("2026-11-23 16:30:45.000+00"),
            CellValue::Int(1),
        ],
        vec![
            CellValue::Int(8),
            uid("12345678-90ab-4cde-8123-456789abcdef"),
            CellValue::Text("Craig Pelton".to_owned()),
            CellValue::Text("dean@greendale.edu".to_owned()),
            CellValue::Bool(true),
            CellValue::Numeric("3.65".to_owned()),
            CellValue::Json(r#"{"role":"Dean","outfits":27,"mascot":"Greendale Human Being"}"#.to_owned()),
            ts("2026-11-24 08:00:00.000+00"),
            CellValue::Null,
        ],
    ];

    TablePage {
        columns,
        rows,
        offset: 0,
        limit: 300,
        total_rows_estimate: Some(8),
        total_is_exact: true,
        has_more: false,
        elapsed_ms: 4,
    }
}

fn sample_courses_page() -> TablePage {
    TablePage {
        columns: vec![
            ColumnMeta {
                name: "course_code".to_owned(),
                data_type: "text".to_owned(),
                udt_name: "text".to_owned(),
                nullable: false,
                default_expr: None,
                is_primary_key: true,
                is_unique: false,
                comment: None,
                foreign_key: None,
            },
            ColumnMeta {
                name: "title".to_owned(),
                data_type: "text".to_owned(),
                udt_name: "text".to_owned(),
                nullable: false,
                default_expr: None,
                is_primary_key: false,
                is_unique: false,
                comment: None,
                foreign_key: None,
            },
        ],
        rows: vec![
            vec![
                CellValue::Text("SPAN-101".to_owned()),
                CellValue::Text("Spanish 101".to_owned()),
            ],
            vec![
                CellValue::Text("ANTH-201".to_owned()),
                CellValue::Text("Introduction to Anthropology".to_owned()),
            ],
        ],
        offset: 0,
        limit: 300,
        total_rows_estimate: Some(2),
        total_is_exact: true,
        has_more: false,
        elapsed_ms: 2,
    }
}

fn sample_students_structure(columns: Vec<ColumnMeta>) -> TableStructure {
    let indexes = vec![
        IndexInfo {
            name: "students_pkey".to_owned(),
            columns: vec!["student_id".to_owned()],
            is_unique: true,
            is_primary: true,
            method: "btree".to_owned(),
            size_bytes: Some(16_384),
            definition: "CREATE UNIQUE INDEX students_pkey ON public.students USING btree (student_id)".to_owned(),
        },
        IndexInfo {
            name: "students_id_key".to_owned(),
            columns: vec!["id".to_owned()],
            is_unique: true,
            is_primary: false,
            method: "btree".to_owned(),
            size_bytes: Some(32_768),
            definition: "CREATE UNIQUE INDEX students_id_key ON public.students USING btree (id)".to_owned(),
        },
        IndexInfo {
            name: "students_profile_gin".to_owned(),
            columns: vec!["profile".to_owned()],
            is_unique: false,
            is_primary: false,
            method: "gin".to_owned(),
            size_bytes: Some(32_768),
            definition: "CREATE INDEX students_profile_gin ON public.students USING gin (profile)".to_owned(),
        },
    ];
    let ddl = tv_core::engine::postgres::synthesize_table_ddl("public", "students", &columns, &indexes);
    TableStructure {
        columns,
        indexes,
        stats: TableStats {
            total_bytes: Some(98_304),
            data_bytes: Some(16_384),
            index_bytes: Some(81_920),
            estimated_rows: Some(8),
            comment: Some("Greendale Community College active roster".to_owned()),
            ddl,
        },
    }
}

fn sample_databases() -> Vec<DatabaseInfo> {
    vec![
        DatabaseInfo {
            name: "campus_billing_dev".to_owned(),
            owner: "annie_edison".to_owned(),
            size_bytes: Some(14_680_064),
        },
        DatabaseInfo {
            name: "greendale_idp_dev".to_owned(),
            owner: "troy_barnes".to_owned(),
            size_bytes: Some(25_165_824),
        },
        DatabaseInfo {
            name: "greendale_notifications".to_owned(),
            owner: "britta_perry".to_owned(),
            size_bytes: Some(9_437_184),
        },
        DatabaseInfo {
            name: "study_groups_dev".to_owned(),
            owner: "abed_nadir".to_owned(),
            size_bytes: Some(8_388_608),
        },
    ]
}

fn sample_logs() -> Vec<crate::state::ConsoleLogEntry> {
    vec![
        ExecutedSql {
            sql: "SELECT version(), current_database(), current_user;".to_owned(),
            duration_ms: 3,
            error: None,
        },
        ExecutedSql {
            sql: "SELECT \"student_id\"::text, \"id\"::text, \"full_name\"::text, \"email\"::text, \"enrolled\"::text, \"gpa\"::text, \"profile\"::text, \"created_at\"::text, \"group_id\"::text FROM \"public\".\"students\" ORDER BY \"student_id\" ASC NULLS LAST LIMIT 301 OFFSET 0".to_owned(),
            duration_ms: 4,
            error: None,
        },
    ]
    .into_iter()
    .map(|exec| crate::state::ConsoleLogEntry {
        timestamp: "2026-11-26 15:34:30.913".to_owned(),
        sql: exec.sql,
        duration_ms: exec.duration_ms,
        error: exec.error,
    })
    .collect()
}

fn sample_ready_update_status() -> UpdateStatus {
    UpdateStatus::Ready {
        version: "0.2.0".to_owned(),
        notes_url: Some("https://github.com/rselbach/tableview/releases/tag/v0.2.0".to_owned()),
        highlights: vec![
            "Faster schema tree refresh across Greendale databases".to_owned(),
            "Signed auto-updates with atomic bundle swap on restart".to_owned(),
            "Cleaner staged diff gutter colors in light and dark themes".to_owned(),
        ],
    }
}

fn scene_launcher_update_ready(s: &mut AppState) {
    reset_to_sample_state(s, true);
    s.update.snapshot.status = sample_ready_update_status();
}

fn scene_workspace_update_ready(s: &mut AppState) {
    setup_connected_workspace(s, true, TableViewMode::Data, Some(0));
    s.update.snapshot.status = sample_ready_update_status();
}

fn scene_workspace_update_downloading(s: &mut AppState) {
    setup_connected_workspace(s, false, TableViewMode::Data, None);
    s.update.snapshot.status = UpdateStatus::Downloading {
        version: "0.2.0".to_owned(),
        percent: 64,
    };
}
