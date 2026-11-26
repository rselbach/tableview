use std::collections::HashMap;

use rune::ColorScheme;
use tv_core::{
    AccentColor, ColumnSummary, ConnectionProfile, DatabaseInfo, DbObject, DbObjectKind, EnvTag, ExecutedSql,
    PasswordMode, RequestId, SafetyPolicy, SchemaInfo, SessionId, SslMode, StoreData,
};

use crate::state::{
    AppState, ColumnLoadState, ConnectedData, ConnectionFormState, ConnectionState, OverlayState, PickerKind,
    PickerState, Screen, SessionState,
};

/// Named headless scene entry registered on `AppSpec::scenes`.
pub type SceneEntry = (&'static str, fn(&mut AppState));

/// Headless scenes registered on `AppSpec::scenes` for deterministic visual and DOM inspection.
pub const SCENES: &[SceneEntry] = &[
    ("launcher-dark", scene_launcher_dark),
    ("launcher-light", scene_launcher_light),
    ("connection-form", scene_connection_form),
    ("workspace-disconnected", scene_workspace_disconnected),
    ("workspace-no-db", scene_workspace_no_db),
    ("workspace-connected-dark", scene_workspace_connected_dark),
    ("workspace-connected-light", scene_workspace_connected_light),
    ("workspace-db-picker", scene_workspace_db_picker),
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
    setup_connected_workspace(s, true);
}

fn scene_workspace_connected_light(s: &mut AppState) {
    setup_connected_workspace(s, false);
}

fn scene_workspace_db_picker(s: &mut AppState) {
    setup_connected_workspace(s, false);
    s.overlay = OverlayState::Picker(PickerState::new(PickerKind::Database));
}

fn setup_connected_workspace(s: &mut AppState, dark: bool) {
    reset_to_sample_state(s, dark);
    let profile = s.connections[0].clone();
    let sid = SessionId::new(1);
    let mut session = SessionState::new(sid, profile, RequestId::new(1));

    let students_cols = vec![
        ColumnSummary {
            name: "student_id".to_owned(),
            data_type: "integer".to_owned(),
            nullable: false,
            is_primary_key: true,
        },
        ColumnSummary {
            name: "id".to_owned(),
            data_type: "uuid".to_owned(),
            nullable: false,
            is_primary_key: false,
        },
        ColumnSummary {
            name: "full_name".to_owned(),
            data_type: "text".to_owned(),
            nullable: false,
            is_primary_key: false,
        },
        ColumnSummary {
            name: "email".to_owned(),
            data_type: "text".to_owned(),
            nullable: true,
            is_primary_key: false,
        },
        ColumnSummary {
            name: "created_at".to_owned(),
            data_type: "timestamptz".to_owned(),
            nullable: false,
            is_primary_key: false,
        },
    ];

    let mut expanded_columns = HashMap::new();
    expanded_columns.insert(
        ("public".to_owned(), "students".to_owned()),
        ColumnLoadState::Loaded(students_cols),
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
                estimated_rows: Some(16),
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
    session.console_entries = sample_logs();
    s.sessions.insert(sid, session);
    s.screen = Screen::Workspace(sid);
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
            duration_ms: 4,
            error: None,
        },
        ExecutedSql {
            sql: "SELECT c.relname, c.relkind::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'public';".to_owned(),
            duration_ms: 7,
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
