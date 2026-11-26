#![allow(clippy::expect_used)]

use chrono::{Datelike, Utc};
use tempfile::tempdir;
use tv_core::engine::postgres::{quote_ident, synthesize_table_ddl};
use tv_core::{
    AccentColor, CellValue, ColumnMeta, ConnectionId, ConnectionProfile, ConnectionStore, EnvTag, ForeignKeyRef,
    IndexInfo, NewHistoryEntry, PasswordMode, QueryStore, SafetyPolicy, SavedQuery, SavedQueryId, SortOrder,
    SslMode, StoreData, UrlParseError, ValidationError, cursor_line_col, format_sql, split_statements,
    statement_at_cursor,
};

#[test]
fn url_parse_extracts_secret_and_params_without_leaking_on_export() {
    let raw = "postgresql://troy_barnes:human%20being@greendale.internal:5433/study_room_f?sslmode=verify-full";
    let (profile, secret) = ConnectionProfile::from_postgres_url(raw).expect("valid postgres url");
    assert_eq!(profile.user, "troy_barnes");
    assert_eq!(secret.as_deref(), Some("human being"));
    assert_eq!(profile.host, "greendale.internal");
    assert_eq!(profile.port, 5433);
    assert_eq!(profile.database.as_deref(), Some("study_room_f"));
    assert_eq!(profile.ssl.mode, SslMode::VerifyFull);
    assert_eq!(profile.password_mode, PasswordMode::Keychain);

    let exported = profile.to_postgres_url(true);
    assert!(!exported.contains("human"));
    assert!(exported.contains(":********@greendale.internal:5433/study_room_f"));
    assert!(exported.contains("sslmode=verify-full"));

    let exported_plain = profile.to_postgres_url(false);
    assert!(!exported_plain.contains("********"));
}

#[test]
fn url_parse_rejects_bad_scheme_and_sslmode() {
    assert!(matches!(
        ConnectionProfile::from_postgres_url("mysql://127.0.0.1/db"),
        Err(UrlParseError::UnsupportedScheme(_))
    ));
    assert!(matches!(
        ConnectionProfile::from_postgres_url("postgres://127.0.0.1/db?sslmode=bogus"),
        Err(UrlParseError::InvalidSslMode(_))
    ));
}

#[test]
fn validation_checks_required_fields_and_tls_pairs() {
    let mut profile = ConnectionProfile::new_postgres("Greendale", "127.0.0.1", "abed_nadir");
    assert!(profile.validate().is_ok());

    profile.name = "   ".to_owned();
    assert_eq!(profile.validate(), Err(ValidationError::EmptyField("name")));

    profile.name = "Greendale".to_owned();
    profile.port = 0;
    assert_eq!(profile.validate(), Err(ValidationError::InvalidPort));

    profile.host = "/tmp/.s.PGSQL.5432".to_owned();
    assert!(profile.validate().is_ok());

    profile.ssl.client_cert_path = Some("/nonexistent/cert.pem".into());
    assert_eq!(profile.validate(), Err(ValidationError::IncompleteClientCert));
}

#[test]
fn connection_store_roundtrip_duplicate_sort_delete_and_import() {
    let dir = tempdir().expect("create tempdir");
    let store = ConnectionStore::at(dir.path().join("connections.json"));

    let mut prod = ConnectionProfile::new_postgres("greendale_prod", "db.greendale.edu", "dean_pelton");
    prod.env = EnvTag::Production;
    prod.color = AccentColor::Red;
    prod.safety = SafetyPolicy::ReadOnly;

    let mut local = ConnectionProfile::new_postgres("greendale_local", "127.0.0.1", "annie_edison");
    local.env = EnvTag::Local;
    local.color = AccentColor::Green;

    let mut data = store.load().expect("missing file returns empty store");
    assert!(data.connections.is_empty());

    data.upsert(prod.clone());
    data.upsert(local.clone());
    let copy = data.duplicate(local.id).expect("duplicate local");
    assert_eq!(copy.name, "greendale_local copy");
    assert_ne!(copy.id, local.id);

    data.sort(SortOrder::Env);
    assert_eq!(data.connections[0].id, prod.id);

    store.save(&data).expect("atomic save succeeds");
    let reloaded = store.load().expect("reload succeeds");
    assert_eq!(reloaded, data);

    let exported = reloaded.export_json().expect("export json");
    let mut other = StoreData::default();
    let count = other.import_json(&exported).expect("import json");
    assert_eq!(count, 3);
    assert!(other.delete(copy.id));
    assert_eq!(other.connections.len(), 2);
}

#[test]
fn cell_value_parses_pg_types_and_preserves_null_vs_empty_string() {
    assert_eq!(CellValue::parse_from_pg_text("text", None), CellValue::Null);
    assert!(CellValue::parse_from_pg_text("text", None).is_null());

    let empty = CellValue::parse_from_pg_text("text", Some(String::new()));
    assert_eq!(empty, CellValue::Text(String::new()));
    assert!(!empty.is_null());

    assert_eq!(CellValue::parse_from_pg_text("bool", Some("t".to_owned())), CellValue::Bool(true));
    assert_eq!(CellValue::parse_from_pg_text("bool", Some("f".to_owned())), CellValue::Bool(false));
    assert_eq!(CellValue::parse_from_pg_text("int4", Some("42".to_owned())), CellValue::Int(42));
    assert_eq!(
        CellValue::parse_from_pg_text("float8", Some("3.75".to_owned())),
        CellValue::Float(3.75)
    );
    assert_eq!(
        CellValue::parse_from_pg_text("numeric", Some("3.9500".to_owned())),
        CellValue::Numeric("3.9500".to_owned())
    );

    let uid = "210b3d6b-da52-4259-ac05-b58d3d0a6d5b";
    let parsed_uid = CellValue::parse_from_pg_text("uuid", Some(uid.to_owned()));
    assert!(matches!(parsed_uid, CellValue::Uuid(u) if u.to_string() == uid));

    // PostgreSQL wire format includes +00, -05, or +05:30 offsets for timestamptz
    let ts_utc = CellValue::parse_from_pg_text("timestamptz", Some("2026-11-19 18:09:11.432+00".to_owned()));
    assert!(matches!(ts_utc, CellValue::Timestamp { utc: Some(dt), .. } if dt.year() == 2026));

    let ts_short_tz = CellValue::parse_from_pg_text("timestamptz", Some("2026-11-19 13:09:11.432-05".to_owned()));
    assert!(matches!(ts_short_tz, CellValue::Timestamp { utc: Some(_), .. }));

    // Naive timestamp preserves `utc: None` instead of fabricating a timezone
    let ts_naive = CellValue::parse_from_pg_text("timestamp", Some("2026-11-19 18:09:11.432".to_owned()));
    assert!(matches!(ts_naive, CellValue::Timestamp { utc: None, .. }));

    let json = CellValue::parse_from_pg_text("jsonb", Some(r#"{"campus":"Greendale","seat":1}"#.to_owned()));
    let pretty = json.pretty_json().expect("pretty json");
    assert!(pretty.contains('\n'));
    assert!(pretty.contains("\"campus\": \"Greendale\""));

    let bytes = CellValue::parse_from_pg_text("bytea", Some("cafebabe".to_owned()));
    assert_eq!(bytes, CellValue::Bytes(vec![0xca, 0xfe, 0xba, 0xbe]));
    assert_eq!(bytes.copy_text(), "\\xcafebabe");

    let empty_bytes = CellValue::parse_from_pg_text("bytea", Some(String::new()));
    assert_eq!(empty_bytes, CellValue::Bytes(Vec::new()));
    assert_eq!(empty_bytes.display_compact(), "\\x (0 B)");

    let arr = CellValue::parse_from_pg_text("_text", Some("{Spanish,Study}".to_owned()));
    assert_eq!(arr, CellValue::Array("{Spanish,Study}".to_owned()));
}

#[test]
fn quote_ident_and_ddl_synthesis_escape_correctly() {
    assert_eq!(quote_ident("students"), "\"students\"");
    assert_eq!(quote_ident("weird\"col"), "\"weird\"\"col\"");

    let cols = vec![
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
            name: "group_id".to_owned(),
            data_type: "integer".to_owned(),
            udt_name: "int4".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: Some(ForeignKeyRef {
                constraint_name: "students_group_fk".to_owned(),
                target_schema: "public".to_owned(),
                target_table: "study_groups".to_owned(),
                target_column: "group_id".to_owned(),
            }),
        },
    ];
    let indexes = vec![IndexInfo {
        name: "students_group_idx".to_owned(),
        columns: vec!["group_id".to_owned()],
        is_unique: false,
        is_primary: false,
        method: "btree".to_owned(),
        size_bytes: Some(8192),
        definition: "CREATE INDEX students_group_idx ON public.students USING btree (group_id)".to_owned(),
    }];

    let ddl = synthesize_table_ddl("public", "students", &cols, &indexes);
    assert!(ddl.contains("CREATE TABLE \"public\".\"students\""));
    assert!(ddl.contains("PRIMARY KEY (\"student_id\")"));
    assert!(ddl.contains("CONSTRAINT \"students_group_fk\" FOREIGN KEY (\"group_id\") REFERENCES \"public\".\"study_groups\" (\"group_id\")"));
    assert!(ddl.contains("CREATE INDEX students_group_idx ON public.students USING btree (group_id);"));
}

#[test]
fn sql_splitter_handles_quotes_comments_and_dollar_blocks() {
    let sql = r#"
        -- Ignore ; in line comments
        /* Nested /* ; */ still ignored */
        SELECT 'hello ; world', "weird;ident" FROM students;
        CREATE FUNCTION greendale_noop() RETURNS void AS $fn$
        BEGIN
            PERFORM 1;
            PERFORM 2;
        END;
        $fn$ LANGUAGE plpgsql;
        ; -- trailing empty comment statement
    "#;

    let stmts = split_statements(sql);
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].sql.ends_with("FROM students"));
    assert!(stmts[1].sql.starts_with("CREATE FUNCTION greendale_noop()"));
    assert!(stmts[1].sql.ends_with("$fn$ LANGUAGE plpgsql"));
}

#[test]
fn statement_at_cursor_respects_selection_and_caret_position() {
    let sql = "SELECT 1;\nSELECT full_name FROM students;\nSELECT 3;";
    // Caret inside second statement
    let second_pos = sql.find("full_name").expect("find full_name");
    let stmt = statement_at_cursor(sql, second_pos, second_pos).expect("second statement");
    assert_eq!(stmt.sql, "SELECT full_name FROM students");

    // Non-empty explicit selection wins over statement boundary
    let sel_start = sql.find("full_name").expect("start");
    let sel_end = sel_start + "full_name".len();
    let selected = statement_at_cursor(sql, sel_start, sel_end).expect("selected slice");
    assert_eq!(selected.sql, "full_name");

    // Caret right after semicolon on line 1 selects statement 1
    let after_semi = "SELECT 1;".len();
    let first = statement_at_cursor(sql, after_semi, after_semi).expect("first statement");
    assert_eq!(first.sql, "SELECT 1");

    assert_eq!(cursor_line_col(sql, 0), (1, 1));
    assert_eq!(cursor_line_col(sql, second_pos), (2, 8));
}

#[test]
fn format_sql_uppercases_keywords_and_preserves_literals_and_comments() {
    let raw = "select student_id, full_name from public.students where email = 'troy;and;abed' and gpa >= 3.5 order by student_id desc; -- keep 'lower'";
    let formatted = format_sql(raw);
    assert!(formatted.starts_with("SELECT student_id, full_name\nFROM public.students\nWHERE email = 'troy;and;abed' AND gpa >= 3.5\nORDER BY student_id DESC;"));
    assert!(formatted.contains("'troy;and;abed'"));
    assert!(formatted.contains("-- keep 'lower'"));
}

#[test]
fn query_store_persists_history_and_saved_queries_roundtrip() {
    let store = QueryStore::open_in_memory().expect("open memory query store");
    let cid_a = ConnectionId::new();
    let cid_b = ConnectionId::new();

    let h1 = store
        .record_history(&NewHistoryEntry {
            connection_id: Some(cid_a),
            connection_name: "greendale_local".to_owned(),
            database_name: Some("greendale_idp_dev".to_owned()),
            sql: "SELECT * FROM students ORDER BY gpa DESC".to_owned(),
            duration_ms: 12,
            rows_count: Some(8),
            error: None,
        })
        .expect("record h1");
    assert_eq!(h1.rows_count, Some(8));

    store
        .record_history(&NewHistoryEntry {
            connection_id: Some(cid_b),
            connection_name: "study_room_dev".to_owned(),
            database_name: Some("study_groups_dev".to_owned()),
            sql: "SELECT COUNT(*) FROM study_groups".to_owned(),
            duration_ms: 4,
            rows_count: Some(1),
            error: None,
        })
        .expect("record h2");

    let all_hist = store.list_history(None, None, 50).expect("list all");
    assert_eq!(all_hist.len(), 2);
    assert_eq!(store.list_history(Some(cid_a), None, 50).expect("filter conn").len(), 1);
    assert_eq!(store.list_history(None, Some("gpa"), 50).expect("filter search").len(), 1);

    store.clear_history(Some(cid_a)).expect("clear conn a");
    assert_eq!(store.list_history(None, None, 50).expect("remaining").len(), 1);

    let qid = SavedQueryId::new();
    let saved = SavedQuery {
        id: qid,
        connection_id: Some(cid_a),
        folder: Some("Study Room".to_owned()),
        name: "Honor Roll".to_owned(),
        sql: "SELECT full_name, gpa FROM students WHERE gpa >= 3.8".to_owned(),
        updated_at: Utc::now(),
    };
    store.upsert_saved_query(&saved).expect("insert saved");

    let list_a = store.list_saved_queries(Some(cid_a), None).expect("list saved a");
    assert_eq!(list_a.len(), 1);
    assert_eq!(list_a[0].name, "Honor Roll");

    let list_b = store.list_saved_queries(Some(cid_b), None).expect("list saved b");
    assert!(list_b.is_empty());

    let mut updated = saved.clone();
    updated.sql = "SELECT full_name FROM students".to_owned();
    store.upsert_saved_query(&updated).expect("update saved");
    let loaded = store.get_saved_query(qid).expect("get saved").expect("exists");
    assert_eq!(loaded.sql, "SELECT full_name FROM students");

    store.delete_saved_query(qid).expect("delete saved");
    assert!(store.get_saved_query(qid).expect("get deleted").is_none());
}
