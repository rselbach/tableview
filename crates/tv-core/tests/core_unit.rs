#![allow(clippy::expect_used)]

use std::path::PathBuf;

use chrono::{Datelike, Utc};
use tempfile::tempdir;
use tv_core::engine::postgres::{quote_ident, synthesize_table_ddl};
use tv_core::{
    AccentColor, AppSettings, AppearancePreference, CellValue, ColumnMeta, ConnectionId, ConnectionProfile,
    ConnectionStore, DbEngine, EnvTag, ExplainError, ExplainPlan, ExportFormat, ForeignKeyRef, IndexInfo,
    MysqlQuoteMode, NewHistoryEntry, PasswordMode, PostgresServerEncoding, PostgresSessionMode, QueryStatementResult,
    QueryStore, RawCellValue,
    SafetyDecision, SafetyPolicy, SavedQuery, SavedQueryId, SettingsStore, SortOrder, SqlSessionMode, SqliteRawValue,
    SslMode, StagedCellValue, StagedChanges, StagedMutation, StagingError, StatementRisk, StoreData, UrlParseError,
    ValidationError,
    atomic_write_bytes, batch_sets_credentials_with_mysql_mode, batch_sets_credentials_with_session_mode,
    build_explain_sql, build_explain_sql_for_engine, build_fk_where_clause, build_fk_where_clause_for_engine,
    cursor_line_col, evaluate_sql_safety, evaluate_sql_safety_for_engine,
    evaluate_sql_safety_for_engine_with_mysql_mode, evaluate_sql_safety_for_engine_with_session_mode,
    evaluate_staged_commit_safety, export_csv, export_json, export_rows, export_rows_for_engine, export_sql_inserts,
    export_sql_inserts_for_engine, format_mysql_string_literal, format_sql, has_executable_sql_for_engine,
    inspect_statement, inspect_statement_for_engine, inspect_statement_for_engine_with_session_mode,
    postgres_slice_has_multiple_statements, split_statements, split_statements_for_engine,
    split_statements_for_engine_with_mysql_mode, split_statements_for_engine_with_session_mode, sql_sets_credentials,
    sql_sets_credentials_with_session_mode, statement_at_cursor, statement_at_cursor_for_engine,
    statement_at_cursor_for_engine_with_mysql_mode, statement_at_cursor_for_engine_with_session_mode,
    validate_row_editable,
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
fn connection_store_preserves_unreadable_or_corrupt_files_and_backs_up_on_reset() {
    let dir = tempdir().expect("create tempdir");
    let store_path = dir.path().join("connections.json");
    let store = ConnectionStore::at(store_path.clone());

    assert!(store.load().expect("missing file returns empty").connections.is_empty());

    std::fs::write(&store_path, "{greendale corrupt json").expect("write corrupt json");
    let err = store.load().expect_err("corrupt json must fail to load");
    assert!(matches!(err, tv_core::StorageError::Parse(_)));

    let backup_1 = store
        .backup_and_reset()
        .expect("backup and reset succeeds")
        .expect("backup file created");
    assert_eq!(
        std::fs::read_to_string(&backup_1).expect("read first backup"),
        "{greendale corrupt json"
    );
    assert!(store.load().expect("reset store loads cleanly").connections.is_empty());

    std::fs::write(&store_path, "{second corrupt").expect("write second corrupt json");
    let backup_2 = store
        .backup_and_reset()
        .expect("second backup and reset succeeds")
        .expect("second backup file created");
    assert_ne!(backup_1, backup_2);
    assert_eq!(
        std::fs::read_to_string(&backup_1).expect("first backup preserved"),
        "{greendale corrupt json"
    );
    assert_eq!(
        std::fs::read_to_string(&backup_2).expect("second backup content"),
        "{second corrupt"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};

        // Test unreadable 0o000 file vs backup_and_reset only if not running as root (uid != 0).
        std::fs::write(&store_path, "secret_unreadable_payload").expect("write file");
        std::fs::set_permissions(&store_path, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
        if std::fs::read_to_string(&store_path).is_err() {
            let io_err = store.load().expect_err("0o000 file must fail load");
            assert!(matches!(io_err, tv_core::StorageError::Io(_)));
            let unreadable_backup = store
                .backup_and_reset()
                .expect("backup unreadable file")
                .expect("backup path");
            std::fs::set_permissions(&unreadable_backup, std::fs::Permissions::from_mode(0o644))
                .expect("restore backup perms");
            assert_eq!(
                std::fs::read_to_string(&unreadable_backup).expect("read unreadable backup"),
                "secret_unreadable_payload"
            );
            assert!(store.load().expect("fresh store loads").connections.is_empty());
        } else {
            std::fs::set_permissions(&store_path, std::fs::Permissions::from_mode(0o644)).expect("restore perms");
        }

        std::fs::remove_file(&store_path).expect("remove regular store");
        symlink(dir.path().join("missing_target.json"), &store_path).expect("create dangling symlink");
        let link_err = store.load().expect_err("dangling symlink must fail load");
        assert!(matches!(link_err, tv_core::StorageError::Io(_)));
        let link_backup = store
            .backup_and_reset()
            .expect("backup dangling symlink")
            .expect("link backup path");
        assert!(
            std::fs::symlink_metadata(&link_backup)
                .expect("backup link meta")
                .file_type()
                .is_symlink()
        );
        assert!(store.load().expect("fresh store after symlink reset").connections.is_empty());
    }
}

#[test]
fn settings_store_roundtrip_and_missing_file_defaults() {
    let dir = tempdir().expect("create tempdir");
    let store = SettingsStore::at(dir.path().join("settings.json"));

    let initial = store.load().expect("missing file returns default settings");
    assert_eq!(initial, AppSettings::default());
    assert_eq!(initial.appearance, AppearancePreference::System);
    assert!(initial.record_query_history);

    for appearance in [
        AppearancePreference::Light,
        AppearancePreference::Dark,
        AppearancePreference::System,
    ] {
        let settings = AppSettings {
            appearance,
            record_query_history: false,
            ..AppSettings::default()
        };
        store.save(&settings).expect("atomic save succeeds");
        let reloaded = store.load().expect("reload succeeds");
        assert_eq!(reloaded, settings);
    }

    let raw = std::fs::read_to_string(store.path()).expect("settings json exists");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid settings json");
    assert_eq!(parsed["appearance"], "system");
    assert_eq!(parsed["record_query_history"], false);

    // Legacy settings.json without `record_query_history` must default to enabled.
    atomic_write_bytes(store.path(), br#"{"version":1,"appearance":"dark"}"#).expect("write legacy settings");
    let legacy = store.load().expect("load legacy settings");
    assert_eq!(legacy.appearance, AppearancePreference::Dark);
    assert!(legacy.record_query_history);
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

fn sample_greendale_columns() -> Vec<ColumnMeta> {
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
            name: "full_name".to_owned(),
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
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "badge".to_owned(),
            data_type: "bytea".to_owned(),
            udt_name: "bytea".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
    ]
}

#[test]
fn safety_guardrails_classify_and_enforce_policies() {
    assert_eq!(
        inspect_statement("SELECT * FROM public.students WHERE gpa >= 3.5").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement("EXPLAIN SELECT * FROM public.students").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement("EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM public.students").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement("EXPLAIN ANALYZE DELETE FROM public.students WHERE student_id = 1").risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement("UPDATE public.students SET gpa = 4.0 WHERE student_id = 3").risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement("UPDATE public.students SET gpa = 0.0").risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement("DELETE FROM public.students").risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement("DROP TABLE public.students").risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement("WITH d AS (DELETE FROM public.students WHERE student_id = 1 RETURNING *) SELECT * FROM d")
            .risk,
        StatementRisk::WriteDml
    );

    // Unparseable SQL fails closed as a write rather than bypassing guardrails.
    let unparsed = inspect_statement("DO $$ BEGIN RAISE NOTICE 'greendale'; END $$;");
    assert!(!unparsed.parsed);
    assert!(unparsed.risk > StatementRisk::ReadOnly);

    let writes = vec!["UPDATE public.students SET gpa = 3.9 WHERE student_id = 1".to_owned()];
    let reads = vec!["SELECT full_name FROM public.students".to_owned()];
    let destructive = vec!["DELETE FROM public.students".to_owned()];

    assert_eq!(evaluate_sql_safety(SafetyPolicy::ReadOnly, EnvTag::Local, &reads), SafetyDecision::Allow);
    assert!(matches!(
        evaluate_sql_safety(SafetyPolicy::ReadOnly, EnvTag::Local, &writes),
        SafetyDecision::Block { .. }
    ));
    assert!(matches!(
        evaluate_sql_safety(SafetyPolicy::StagedOnly, EnvTag::Staging, &writes),
        SafetyDecision::Block { .. }
    ));
    assert!(matches!(
        evaluate_sql_safety(SafetyPolicy::ConfirmWrites, EnvTag::Development, &writes),
        SafetyDecision::RequireConfirmation { is_destructive: false, .. }
    ));
    assert!(matches!(
        evaluate_sql_safety(SafetyPolicy::Unrestricted, EnvTag::Production, &destructive),
        SafetyDecision::RequireConfirmation { is_destructive: true, .. }
    ));
    assert_eq!(
        evaluate_sql_safety(SafetyPolicy::Unrestricted, EnvTag::Local, &destructive),
        SafetyDecision::Allow
    );

    assert!(evaluate_staged_commit_safety(SafetyPolicy::ReadOnly).is_err());
    assert!(evaluate_staged_commit_safety(SafetyPolicy::StagedOnly).is_ok());
}

#[test]
fn staged_changes_build_transaction_diff_and_revert_cleanly() {
    let cols = sample_greendale_columns();
    let row0 = vec![
        CellValue::Int(1),
        CellValue::Text("Troy Barnes".to_owned()),
        CellValue::Text("troy@greendale.edu".to_owned()),
        CellValue::Numeric("3.45".to_owned()),
        CellValue::Json(r#"{"handshake":true}"#.to_owned()),
        CellValue::Bytes(vec![0xde, 0xad]),
    ];
    let row1 = vec![
        CellValue::Int(2),
        CellValue::Text("Abed Nadir".to_owned()),
        CellValue::Null,
        CellValue::Numeric("3.92".to_owned()),
        CellValue::Null,
        CellValue::Null,
    ];

    let mut staged = StagedChanges::new();
    // Edit Troy's name with an embedded apostrophe and revert email back to original.
    staged.stage_cell_edit(0, &row0, 1, StagedCellValue::Literal("Troy 'T-Bone' Barnes".to_owned()));
    staged.stage_cell_edit(0, &row0, 2, StagedCellValue::Null);
    assert_eq!(staged.update_count(), 1);
    staged.stage_cell_edit(0, &row0, 2, StagedCellValue::Literal("troy@greendale.edu".to_owned()));
    assert_eq!(staged.effective_existing_cell(0, 2), None);

    // Insert Pierce as a draft row keeping PK default.
    let draft_id = staged.stage_insert_row(&cols);
    assert!(matches!(staged.inserts[0].values[0], StagedCellValue::DefaultExpr));
    staged.stage_insert_cell_edit(draft_id, 1, StagedCellValue::Literal("Pierce Hawthorne".to_owned()));
    staged.stage_insert_cell_edit(draft_id, 3, StagedCellValue::Literal("2.15".to_owned()));
    staged.stage_insert_cell_edit(draft_id, 5, StagedCellValue::Literal("cafe".to_owned()));

    // Toggle delete on row 1, discarding any pending update on that row.
    staged.stage_cell_edit(1, &row1, 1, StagedCellValue::Literal("Abed Temp".to_owned()));
    assert!(staged.toggle_delete_row(1, &row1));
    assert!(!staged.is_row_updated(1));
    assert!(staged.is_row_deleted(1));

    let tx_sql = staged
        .build_transaction_sql("public", "students", &cols)
        .expect("build tx sql");
    assert!(tx_sql.starts_with("BEGIN;\n"));
    assert!(tx_sql.contains("UPDATE \"public\".\"students\" SET \"full_name\" = 'Troy ''T-Bone'' Barnes' WHERE \"student_id\" = 1 AND \"full_name\"::text COLLATE \"C\" = 'Troy Barnes' COLLATE \"C\" AND \"email\"::text COLLATE \"C\" = 'troy@greendale.edu' COLLATE \"C\" AND \"gpa\"::text COLLATE \"C\" = '3.45' COLLATE \"C\" AND \"profile\"::text COLLATE \"C\" = '{\"handshake\":true}' COLLATE \"C\" AND \"badge\" = '\\xdead'::bytea;"));
    assert!(tx_sql.contains("INSERT INTO \"public\".\"students\" (\"full_name\", \"email\", \"gpa\", \"profile\", \"badge\") VALUES ('Pierce Hawthorne', NULL, 2.15, NULL, '\\xcafe'::bytea);"));
    assert!(tx_sql.contains("DELETE FROM \"public\".\"students\" WHERE \"student_id\" = 2 AND \"full_name\"::text COLLATE \"C\" = 'Abed Nadir' COLLATE \"C\" AND \"email\" IS NULL AND \"gpa\"::text COLLATE \"C\" = '3.92' COLLATE \"C\" AND \"profile\" IS NULL AND \"badge\" IS NULL;"));
    assert!(tx_sql.ends_with("COMMIT;\n"));

    let (before, after) = staged.build_diff_texts("public", "students", &cols);
    assert!(before.contains("Troy Barnes"));
    assert!(after.contains("Troy ''T-Bone'' Barnes"));
    assert!(after.contains("Pierce Hawthorne"));
    assert!(before.contains("Abed Nadir"));

    // Updating existing rows without a PK is refused safely.
    let mut no_pk_cols = cols.clone();
    no_pk_cols[0].is_primary_key = false;
    assert!(matches!(
        staged.build_statements("public", "students", &no_pk_cols),
        Err(StagingError::MissingPrimaryKey { .. })
    ));

    // FK WHERE filter helper skips NULL and escapes strings cleanly.
    assert_eq!(
        build_fk_where_clause("group_id", &CellValue::Int(1), &cols[0]),
        Some("\"group_id\" = 1".to_owned())
    );
    assert_eq!(build_fk_where_clause("group_id", &CellValue::Null, &cols[0]), None);
}

#[test]
fn explain_parser_computes_exclusive_metrics_and_skew() {
    let raw = r#"[
      {
        "Plan": {
          "Node Type": "Hash Join",
          "Join Type": "Inner",
          "Startup Cost": 1.15,
          "Total Cost": 24.50,
          "Plan Rows": 100,
          "Plan Width": 64,
          "Actual Startup Time": 0.420,
          "Actual Total Time": 4.200,
          "Actual Rows": 8,
          "Actual Loops": 1,
          "Hash Cond": "(s.group_id = g.group_id)",
          "Shared Hit Blocks": 14,
          "Shared Read Blocks": 2,
          "Plans": [
            {
              "Node Type": "Seq Scan",
              "Relation Name": "students",
              "Schema": "public",
              "Alias": "s",
              "Startup Cost": 0.00,
              "Total Cost": 18.00,
              "Plan Rows": 1,
              "Plan Width": 40,
              "Actual Startup Time": 0.050,
              "Actual Total Time": 1.500,
              "Actual Rows": 250,
              "Actual Loops": 1,
              "Filter": "(gpa >= 3.0)",
              "Rows Removed by Filter": 2
            },
            {
              "Node Type": "Hash",
              "Startup Cost": 1.05,
              "Total Cost": 1.05,
              "Plan Rows": 4,
              "Plan Width": 24,
              "Actual Startup Time": 0.120,
              "Actual Total Time": 0.200,
              "Actual Rows": 4,
              "Actual Loops": 1,
              "Plans": []
            }
          ]
        },
        "Planning Time": 0.312,
        "Execution Time": 4.350
      }
    ]"#;

    assert_eq!(
        build_explain_sql("SELECT * FROM public.students;", true),
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT * FROM public.students"
    );
    let plan = ExplainPlan::parse_json("SELECT * FROM public.students".to_owned(), true, raw)
        .expect("parse explain json");
    assert!(plan.analyzed);
    assert_eq!(plan.node_count(), 3);
    assert_eq!(plan.total_shared_hits(), 14);
    assert_eq!(plan.total_shared_reads(), 2);

    let root_excl = plan.root.exclusive_time_ms().expect("exclusive ms");
    assert!((root_excl - 2.5).abs() < 0.001, "expected ~2.5ms got {root_excl}");
    assert!(plan.root.children[0].row_estimate_ratio().is_some());
    assert_eq!(
        plan.root.children[0].relation_display().as_deref(),
        Some("public.students")
    );

    // Also auto-detects when executed inline from SQL editor
    let qres = QueryStatementResult {
        sql: "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM public.students".to_owned(),
        columns: vec![ColumnMeta {
            name: "QUERY PLAN".to_owned(),
            data_type: "json".to_owned(),
            udt_name: "json".to_owned(),
            nullable: false,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        }],
        rows: vec![vec![CellValue::Json(raw.to_owned())]],
        rows_affected: None,
        truncated: false,
        total_rows_seen: 1,
        elapsed_ms: 4,
        error: None,
    };
    assert!(ExplainPlan::try_extract_from_result(&qres).is_some());
}

#[test]
fn export_formats_csv_json_and_sql_inserts_with_proper_escaping() {
    let cols = sample_greendale_columns();
    let rows = vec![vec![
        CellValue::Int(1),
        CellValue::Text("Troy \"T-Bone\", Jr.".to_owned()),
        CellValue::Null,
        CellValue::Numeric("3.45".to_owned()),
        CellValue::Json(r#"{"campus":"Greendale"}"#.to_owned()),
        CellValue::Bytes(vec![0xca, 0xfe]),
    ]];

    let csv = export_csv(&cols, &rows);
    assert!(csv.starts_with("student_id,full_name,email,gpa,profile,badge\n"));
    assert!(csv.contains("1,\"Troy \"\"T-Bone\"\", Jr.\",,3.45,\"{\"\"campus\"\":\"\"Greendale\"\"}\",\\xcafe\n"));

    let json = export_json(&cols, &rows);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid exported json");
    assert_eq!(parsed[0]["student_id"], 1);
    assert_eq!(parsed[0]["email"], serde_json::Value::Null);
    assert_eq!(parsed[0]["profile"]["campus"], "Greendale");

    let sql = export_sql_inserts(Some("public"), "students", &cols, &rows);
    assert!(sql.contains("INSERT INTO \"public\".\"students\" (\"student_id\", \"full_name\", \"email\", \"gpa\", \"profile\", \"badge\") VALUES (1, 'Troy \"T-Bone\", Jr.', NULL, 3.45, '{\"campus\":\"Greendale\"}'::jsonb, '\\xcafe'::bytea);"));

    let via_dispatch = export_rows(ExportFormat::SqlInsert, None, "custom_query", &cols, &rows);
    assert!(via_dispatch.starts_with("INSERT INTO \"custom_query\""));
}

#[test]
fn db_engine_properties_and_serde_backward_compatibility() {
    assert_eq!(DbEngine::ALL, [DbEngine::Postgres, DbEngine::Mysql, DbEngine::Sqlite]);
    assert_eq!(DbEngine::default(), DbEngine::Postgres);

    assert_eq!(DbEngine::Postgres.label(), "PostgreSQL");
    assert_eq!(DbEngine::Mysql.label(), "MySQL");
    assert_eq!(DbEngine::Sqlite.label(), "SQLite");

    assert_eq!(DbEngine::Postgres.short_badge(), "PG");
    assert_eq!(DbEngine::Mysql.short_badge(), "MY");
    assert_eq!(DbEngine::Sqlite.short_badge(), "SL");

    assert_eq!(DbEngine::Postgres.default_port(), 5432);
    assert_eq!(DbEngine::Mysql.default_port(), 3306);
    assert_eq!(DbEngine::Sqlite.default_port(), 0);

    assert_eq!(DbEngine::Postgres.default_user(), "postgres");
    assert_eq!(DbEngine::Mysql.default_user(), "root");
    assert_eq!(DbEngine::Sqlite.default_user(), "");

    assert_eq!(DbEngine::from_label("PostgreSQL"), Some(DbEngine::Postgres));
    assert_eq!(DbEngine::from_label("postgres"), Some(DbEngine::Postgres));
    assert_eq!(DbEngine::from_label("PG"), Some(DbEngine::Postgres));
    assert_eq!(DbEngine::from_label("mysql"), Some(DbEngine::Mysql));
    assert_eq!(DbEngine::from_label("my"), Some(DbEngine::Mysql));
    assert_eq!(DbEngine::from_label("SQLite"), Some(DbEngine::Sqlite));
    assert_eq!(DbEngine::from_label("sl"), Some(DbEngine::Sqlite));
    assert_eq!(DbEngine::from_label("greendale"), None);

    assert!(DbEngine::Postgres.uses_schemas());
    assert!(!DbEngine::Mysql.uses_schemas());
    assert!(!DbEngine::Sqlite.uses_schemas());

    assert!(DbEngine::Postgres.supports_multi_database());
    assert!(DbEngine::Mysql.supports_multi_database());
    assert!(!DbEngine::Sqlite.supports_multi_database());

    assert!(DbEngine::Postgres.supports_explain_analyze());
    assert!(DbEngine::Mysql.supports_explain_analyze());
    assert!(!DbEngine::Sqlite.supports_explain_analyze());

    assert_eq!(DbEngine::Postgres.quote_ident("weird\"col"), "\"weird\"\"col\"");
    assert_eq!(DbEngine::Sqlite.quote_ident("weird\"col"), "\"weird\"\"col\"");
    assert_eq!(DbEngine::Mysql.quote_ident("weird`col"), "`weird``col`");

    assert_eq!(
        DbEngine::Postgres.qualify_ident("public", "students"),
        "\"public\".\"students\""
    );
    assert_eq!(
        DbEngine::Mysql.qualify_ident("greendale", "students"),
        "`greendale`.`students`"
    );
    assert_eq!(DbEngine::Mysql.qualify_ident("", "students"), "`students`");
    assert_eq!(DbEngine::Sqlite.qualify_ident("main", "students"), "\"main\".\"students\"");
    assert_eq!(DbEngine::Sqlite.qualify_ident("  ", "students"), "\"students\"");

    // Legacy ConnectionProfile JSON without `engine` or `file_path` defaults to Postgres.
    let legacy_json = r#"{
        "id": "11111111-1111-1111-1111-111111111111",
        "name": "Greendale Legacy PG",
        "host": "db.greendale.edu",
        "port": 5432,
        "user": "dean_pelton",
        "database": "study_room",
        "created_at": "2026-11-19T12:00:00Z"
    }"#;
    let legacy: ConnectionProfile = serde_json::from_str(legacy_json).expect("deserialize legacy profile");
    assert_eq!(legacy.engine, DbEngine::Postgres);
    assert_eq!(legacy.file_path, None);
    assert_eq!(legacy.name, "Greendale Legacy PG");

    // StoreData round-trip preserves mixed engines and omits `file_path` when None.
    let mysql = ConnectionProfile::new_mysql("Greendale MySQL", "127.0.0.1", "troy_barnes");
    let sqlite = ConnectionProfile::new_sqlite("Study Room SQLite", "/tmp/greendale.db");
    let mut store = StoreData::default();
    store.upsert(legacy);
    store.upsert(mysql);
    store.upsert(sqlite);
    let serialized = store.export_json().expect("serialize store");
    let mut reloaded = StoreData::default();
    let count = reloaded.import_json(&serialized).expect("reload store");
    assert_eq!(count, 3);
    assert_eq!(reloaded.connections[0].engine, DbEngine::Postgres);
    assert_eq!(reloaded.connections[1].engine, DbEngine::Mysql);
    assert_eq!(reloaded.connections[2].engine, DbEngine::Sqlite);
    assert_eq!(
        reloaded.connections[2].file_path,
        Some(PathBuf::from("/tmp/greendale.db"))
    );
}

#[test]
fn mysql_and_sqlite_profiles_validate_all_constraints() {
    let mut mysql = ConnectionProfile::new_mysql("Greendale Registrar", "mysql.greendale.edu", "annie_edison");
    assert_eq!(mysql.engine, DbEngine::Mysql);
    assert_eq!(mysql.port, 3306);
    assert_eq!(mysql.file_path, None);
    assert_eq!(mysql.password_mode, PasswordMode::Keychain);
    assert!(mysql.validate().is_ok());
    assert_eq!(mysql.endpoint_summary(), "mysql.greendale.edu");

    mysql.port = 3307;
    mysql.database = Some("study_room_f".to_owned());
    assert_eq!(mysql.endpoint_summary(), "mysql.greendale.edu:3307 : study_room_f");

    mysql.name = "  ".to_owned();
    assert_eq!(mysql.validate(), Err(ValidationError::EmptyField("name")));

    mysql.name = "Greendale Registrar".to_owned();
    mysql.host = "   ".to_owned();
    assert_eq!(mysql.validate(), Err(ValidationError::EmptyField("host")));

    mysql.host = "mysql.greendale.edu".to_owned();
    mysql.port = 0;
    assert_eq!(mysql.validate(), Err(ValidationError::InvalidPort));

    // Unix socket paths are permitted even with port 0.
    mysql.host = "/tmp/mysql.sock".to_owned();
    assert!(mysql.validate().is_ok());
    assert_eq!(mysql.endpoint_summary(), "/tmp/mysql.sock : study_room_f");

    mysql.user = String::new();
    assert_eq!(mysql.validate(), Err(ValidationError::EmptyField("user")));

    mysql.user = "annie_edison".to_owned();
    mysql.ssl.mode = SslMode::VerifyFull;
    mysql.ssl.ca_cert_path = Some(PathBuf::from("/nonexistent/greendale-ca.pem"));
    assert_eq!(
        mysql.validate(),
        Err(ValidationError::MissingFile(
            "CA certificate",
            PathBuf::from("/nonexistent/greendale-ca.pem")
        ))
    );

    let mut sqlite = ConnectionProfile::new_sqlite("Abed Local DB", "/tmp/greendale_campus.sqlite3");
    assert_eq!(sqlite.engine, DbEngine::Sqlite);
    assert_eq!(sqlite.host, "");
    assert_eq!(sqlite.port, 0);
    assert_eq!(sqlite.user, "");
    assert_eq!(sqlite.password_mode, PasswordMode::None);
    assert_eq!(sqlite.ssl.mode, SslMode::Disable);
    assert_eq!(sqlite.env, EnvTag::Local);
    assert_eq!(sqlite.color, AccentColor::Green);
    assert!(sqlite.validate().is_ok());
    assert_eq!(sqlite.endpoint_summary(), "/tmp/greendale_campus.sqlite3");

    let mem_sqlite = ConnectionProfile::new_sqlite("In-Memory Study", ":memory:");
    assert!(mem_sqlite.validate().is_ok());
    assert_eq!(mem_sqlite.endpoint_summary(), ":memory:");

    sqlite.file_path = Some(PathBuf::new());
    assert_eq!(
        sqlite.validate(),
        Err(ValidationError::EmptyField("database file path"))
    );

    sqlite.file_path = None;
    assert_eq!(
        sqlite.validate(),
        Err(ValidationError::EmptyField("database file path"))
    );

    sqlite.file_path = Some(PathBuf::from(":memory:"));
    sqlite.name = String::new();
    assert_eq!(sqlite.validate(), Err(ValidationError::EmptyField("name")));
}

#[test]
fn url_parse_and_export_roundtrip_for_postgres_mysql_and_sqlite() {
    // PostgreSQL via unified from_url / to_url
    let (pg, pg_pw) =
        ConnectionProfile::from_url("postgresql://jeff_winger:lawyer%20up@db.greendale.edu:5432/campus?sslmode=require")
            .expect("parse postgresql url");
    assert_eq!(pg.engine, DbEngine::Postgres);
    assert_eq!(pg_pw.as_deref(), Some("lawyer up"));
    assert_eq!(pg.port, 5432);
    assert_eq!(pg.ssl.mode, SslMode::Require);
    let pg_masked = pg.to_url(true);
    assert!(pg_masked.starts_with("postgresql://jeff_winger:********@db.greendale.edu:5432/campus"));
    assert!(pg_masked.contains("sslmode=require"));
    assert!(!pg.to_url(false).contains("********"));

    // MySQL with credentials, custom ssl-mode, and default port 3306
    let (my_full, my_pw) = ConnectionProfile::from_url(
        "mysql://abed_nadir:cool%20cool%20cool@mysql.greendale.edu/film_club?ssl-mode=verify_identity",
    )
    .expect("parse mysql url");
    assert_eq!(my_full.engine, DbEngine::Mysql);
    assert_eq!(my_full.user, "abed_nadir");
    assert_eq!(my_pw.as_deref(), Some("cool cool cool"));
    assert_eq!(my_full.host, "mysql.greendale.edu");
    assert_eq!(my_full.port, 3306);
    assert_eq!(my_full.database.as_deref(), Some("film_club"));
    assert_eq!(my_full.ssl.mode, SslMode::VerifyFull);
    assert_eq!(my_full.password_mode, PasswordMode::Keychain);

    let my_masked = my_full.to_url(true);
    assert!(!my_masked.contains("cool"));
    assert_eq!(
        my_masked,
        "mysql://abed_nadir:********@mysql.greendale.edu:3306/film_club?ssl-mode=verify_identity"
    );
    let my_unmasked = my_full.to_url(false);
    assert_eq!(
        my_unmasked,
        "mysql://abed_nadir@mysql.greendale.edu:3306/film_club?ssl-mode=verify_identity"
    );

    // MySQL without credentials defaults to root / 3306 / PasswordMode::None
    let (my_anon, my_anon_pw) =
        ConnectionProfile::from_url("mysql2://127.0.0.1/air_conditioning").expect("parse mysql2 url");
    assert_eq!(my_anon.engine, DbEngine::Mysql);
    assert_eq!(my_anon.user, "root");
    assert_eq!(my_anon.port, 3306);
    assert_eq!(my_anon_pw, None);
    assert_eq!(my_anon.password_mode, PasswordMode::None);
    assert!(!my_anon.to_url(true).contains("********"));

    // MySQL Unix socket via query parameter
    let (my_sock, _) =
        ConnectionProfile::from_url("mysql://troy_barnes@localhost/plumbing?socket=%2Ftmp%2Fmysql.sock")
            .expect("parse mysql socket url");
    assert_eq!(my_sock.host, "/tmp/mysql.sock");
    assert!(my_sock.validate().is_ok());
    assert!(my_sock.to_url(false).contains("socket=%2Ftmp%2Fmysql.sock"));

    assert!(matches!(
        ConnectionProfile::from_url("mysql://127.0.0.1/db?ssl-mode=imaginary"),
        Err(UrlParseError::InvalidSslMode(_))
    ));

    // SQLite :memory:
    let (sl_mem, sl_mem_pw) = ConnectionProfile::from_url("sqlite://:memory:").expect("parse sqlite memory");
    assert_eq!(sl_mem.engine, DbEngine::Sqlite);
    assert_eq!(sl_mem.name, "SQLite (memory)");
    assert_eq!(sl_mem.file_path, Some(PathBuf::from(":memory:")));
    assert_eq!(sl_mem_pw, None);
    assert_eq!(sl_mem.to_url(true), "sqlite://:memory:");

    // SQLite percent-encoded file path
    let (sl_encoded, _) =
        ConnectionProfile::from_url("sqlite:///tmp/greendale%20study%20room.db").expect("parse sqlite encoded path");
    assert_eq!(sl_encoded.engine, DbEngine::Sqlite);
    assert_eq!(sl_encoded.name, "greendale study room");
    assert_eq!(
        sl_encoded.file_path,
        Some(PathBuf::from("/tmp/greendale study room.db"))
    );
    assert_eq!(sl_encoded.to_url(false), "sqlite:///tmp/greendale%20study%20room.db");

    // sqlite3:// and file: schemes
    let (sl3, _) = ConnectionProfile::from_url("sqlite3:///var/lib/greendale/registrar.sqlite3")
        .expect("parse sqlite3 url");
    assert_eq!(sl3.engine, DbEngine::Sqlite);
    assert_eq!(sl3.name, "registrar");
    assert_eq!(
        sl3.file_path,
        Some(PathBuf::from("/var/lib/greendale/registrar.sqlite3"))
    );
    assert_eq!(sl3.to_url(true), "sqlite:///var/lib/greendale/registrar.sqlite3");

    let (sl_file, _) = ConnectionProfile::from_url("file:/tmp/troy_and_abed.db?mode=ro").expect("parse file url");
    assert_eq!(sl_file.engine, DbEngine::Sqlite);
    assert_eq!(sl_file.file_path, Some(PathBuf::from("/tmp/troy_and_abed.db")));

    assert!(matches!(
        ConnectionProfile::from_url("sqlite://"),
        Err(UrlParseError::InvalidUrl(_))
    ));
    assert!(matches!(
        ConnectionProfile::from_url("ftp://greendale.edu/db"),
        Err(UrlParseError::UnsupportedScheme(_))
    ));
}

fn sample_multi_engine_columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta {
            name: "student_id".to_owned(),
            data_type: "int".to_owned(),
            udt_name: "int".to_owned(),
            nullable: false,
            default_expr: Some("auto_increment".to_owned()),
            is_primary_key: true,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "full_name".to_owned(),
            data_type: "varchar(255)".to_owned(),
            udt_name: "varchar".to_owned(),
            nullable: false,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "enrolled".to_owned(),
            data_type: "tinyint(1)".to_owned(),
            udt_name: "tinyint(1)".to_owned(),
            nullable: false,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "gpa".to_owned(),
            data_type: "decimal(4,2)".to_owned(),
            udt_name: "decimal".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "badge".to_owned(),
            data_type: "blob".to_owned(),
            udt_name: "blob".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
        ColumnMeta {
            name: "metadata".to_owned(),
            data_type: "json".to_owned(),
            udt_name: "json".to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        },
    ]
}

#[test]
fn staged_changes_build_statements_and_transactions_for_mysql_and_sqlite() {
    let cols = sample_multi_engine_columns();
    let row0 = vec![
        CellValue::Int(1),
        CellValue::Text("Britta Perry".to_owned()),
        CellValue::Bool(true),
        CellValue::Numeric("2.85".to_owned()),
        CellValue::Bytes(vec![0xab, 0xcd]),
        CellValue::Json(r#"{"major":"psych"}"#.to_owned()),
    ];
    let row1 = vec![
        CellValue::Int(2),
        CellValue::Text("Pierce Hawthorne".to_owned()),
        CellValue::Bool(false),
        CellValue::Null,
        CellValue::Null,
        CellValue::Null,
    ];

    let mut staged = StagedChanges::new();
    staged.stage_cell_edit(0, &row0, 1, StagedCellValue::Literal(r"Britta's \protest".to_owned()));
    staged.stage_cell_edit(0, &row0, 2, StagedCellValue::Literal("false".to_owned()));
    staged.stage_cell_edit(0, &row0, 4, StagedCellValue::Literal("0xcafe".to_owned()));
    staged.stage_cell_edit(0, &row0, 5, StagedCellValue::Literal(r#"{"note":"a'b\c"}"#.to_owned()));

    let draft_id = staged.stage_insert_row(&cols);
    staged.stage_insert_cell_edit(draft_id, 1, StagedCellValue::Literal("Shirley Bennett".to_owned()));
    staged.stage_insert_cell_edit(draft_id, 2, StagedCellValue::Literal("true".to_owned()));
    staged.stage_insert_cell_edit(draft_id, 3, StagedCellValue::Literal("3.88".to_owned()));
    staged.stage_insert_cell_edit(draft_id, 4, StagedCellValue::Literal("beef".to_owned()));
    staged.stage_insert_cell_edit(draft_id, 5, StagedCellValue::Literal(r#"{"club":"baking"}"#.to_owned()));

    staged.toggle_delete_row(1, &row1);

    // MySQL dialect
    let my_stmts = staged
        .build_statements_for_engine(DbEngine::Mysql, "greendale", "students", &cols)
        .expect("mysql statements");
    assert_eq!(my_stmts.len(), 3);
    assert_eq!(
        my_stmts[0],
        "UPDATE `greendale`.`students` SET `full_name` = CONCAT('Britta''s ', CHAR(92 USING utf8mb4), 'protest'), `enrolled` = FALSE, `badge` = X'cafe', `metadata` = CONCAT('{\"note\":\"a''b', CHAR(92 USING utf8mb4), 'c\"}') WHERE `student_id` = 1 AND CAST(CONVERT(`full_name` USING utf8mb4) AS BINARY) = CAST(CONVERT('Britta Perry' USING utf8mb4) AS BINARY) AND `enrolled` = TRUE AND `gpa` = 2.85 AND `badge` = X'abcd' AND `metadata` = CAST('{\"major\":\"psych\"}' AS JSON);"
    );
    assert_eq!(
        my_stmts[1],
        "INSERT INTO `greendale`.`students` (`full_name`, `enrolled`, `gpa`, `badge`, `metadata`) VALUES ('Shirley Bennett', TRUE, 3.88, X'beef', '{\"club\":\"baking\"}');"
    );
    assert_eq!(
        my_stmts[2],
        "DELETE FROM `greendale`.`students` WHERE `student_id` = 2 AND CAST(CONVERT(`full_name` USING utf8mb4) AS BINARY) = CAST(CONVERT('Pierce Hawthorne' USING utf8mb4) AS BINARY) AND `enrolled` = FALSE AND `gpa` IS NULL AND `badge` IS NULL AND `metadata` IS NULL;"
    );

    let my_tx = staged
        .build_transaction_sql_for_engine(DbEngine::Mysql, "greendale", "students", &cols)
        .expect("mysql tx sql");
    assert!(my_tx.starts_with("START TRANSACTION;\n\n"));
    assert!(!my_tx.contains('\\'));
    assert!(my_tx.ends_with("\n\nCOMMIT;\n"));

    // SQLite dialect (double-quoted identifiers, 1/0 bools, X'...' bytes, no `::` casts)
    let sl_stmts = staged
        .build_statements_for_engine(DbEngine::Sqlite, "", "students", &cols)
        .expect("sqlite statements");
    assert_eq!(sl_stmts.len(), 3);
    assert_eq!(
        sl_stmts[0],
        "UPDATE \"students\" SET \"full_name\" = 'Britta''s \\protest', \"enrolled\" = 0, \"badge\" = X'cafe', \"metadata\" = '{\"note\":\"a''b\\c\"}' WHERE \"student_id\" = 1 AND \"full_name\" COLLATE BINARY = 'Britta Perry' AND \"enrolled\" = 1 AND \"gpa\" = 2.85 AND \"badge\" = X'abcd' AND \"metadata\" COLLATE BINARY = '{\"major\":\"psych\"}';"
    );
    assert_eq!(
        sl_stmts[1],
        "INSERT INTO \"students\" (\"full_name\", \"enrolled\", \"gpa\", \"badge\", \"metadata\") VALUES ('Shirley Bennett', 1, 3.88, X'beef', '{\"club\":\"baking\"}');"
    );
    assert_eq!(
        sl_stmts[2],
        "DELETE FROM \"students\" WHERE \"student_id\" = 2 AND \"full_name\" COLLATE BINARY = 'Pierce Hawthorne' AND \"enrolled\" = 0 AND \"gpa\" IS NULL AND \"badge\" IS NULL AND \"metadata\" IS NULL;"
    );
    for stmt in &sl_stmts {
        assert!(!stmt.contains("::"), "sqlite SQL must not contain :: casts: {stmt}");
    }

    let sl_tx = staged
        .build_transaction_sql_for_engine(DbEngine::Sqlite, "", "students", &cols)
        .expect("sqlite tx sql");
    assert!(sl_tx.starts_with("BEGIN;\n\n"));
    assert!(sl_tx.ends_with("\n\nCOMMIT;\n"));

    // All-default empty insert syntax differs between MySQL and SQLite
    let default_cols = vec![cols[0].clone()];
    let mut default_only = StagedChanges::new();
    default_only.stage_insert_row(&default_cols);
    assert_eq!(
        default_only
            .build_statements_for_engine(DbEngine::Mysql, "greendale", "students", &default_cols)
            .expect("mysql empty insert"),
        vec!["INSERT INTO `greendale`.`students` () VALUES ();"]
    );
    assert_eq!(
        default_only
            .build_statements_for_engine(DbEngine::Sqlite, "", "students", &default_cols)
            .expect("sqlite empty insert"),
        vec!["INSERT INTO \"students\" DEFAULT VALUES;"]
    );

    // FK WHERE helper respects dialect quoting and literal formatting
    assert_eq!(
        build_fk_where_clause_for_engine(
            DbEngine::Mysql,
            "full_name",
            &CellValue::Text(r"Troy's \Fort".to_owned()),
            &cols[1],
        ),
        Some("`full_name` = CONCAT('Troy''s ', CHAR(92 USING utf8mb4), 'Fort')".to_owned())
    );
    assert_eq!(
        build_fk_where_clause_for_engine(DbEngine::Sqlite, "enrolled", &CellValue::Bool(true), &cols[2]),
        Some("\"enrolled\" = 1".to_owned())
    );
}

#[test]
fn export_rows_and_sql_inserts_support_mysql_and_sqlite() {
    let cols = sample_multi_engine_columns();
    let rows = vec![vec![
        CellValue::Int(1),
        CellValue::Text(r"Troy 'T-Bone' \Barnes".to_owned()),
        CellValue::Bool(true),
        CellValue::Numeric("3.45".to_owned()),
        CellValue::Bytes(vec![0xca, 0xfe]),
        CellValue::Json(r#"{"campus":"Greendale"}"#.to_owned()),
    ]];

    let mysql_inserts = export_sql_inserts_for_engine(DbEngine::Mysql, Some("greendale"), "students", &cols, &rows);
    assert_eq!(
        mysql_inserts,
        "INSERT INTO `greendale`.`students` (`student_id`, `full_name`, `enrolled`, `gpa`, `badge`, `metadata`) VALUES (1, CONCAT('Troy ''T-Bone'' ', CHAR(92 USING utf8mb4), 'Barnes'), TRUE, 3.45, X'cafe', '{\"campus\":\"Greendale\"}');\n"
    );
    assert!(!mysql_inserts.contains('\\'));
    assert_eq!(
        export_rows_for_engine(
            DbEngine::Mysql,
            ExportFormat::SqlInsert,
            Some("greendale"),
            "students",
            &cols,
            &rows
        ),
        mysql_inserts
    );

    let sqlite_inserts = export_sql_inserts_for_engine(DbEngine::Sqlite, None, "students", &cols, &rows);
    assert_eq!(
        sqlite_inserts,
        "INSERT INTO \"students\" (\"student_id\", \"full_name\", \"enrolled\", \"gpa\", \"badge\", \"metadata\") VALUES (1, 'Troy ''T-Bone'' \\Barnes', 1, 3.45, X'cafe', '{\"campus\":\"Greendale\"}');\n"
    );
    assert!(!sqlite_inserts.contains("::"));
    assert_eq!(
        export_rows_for_engine(DbEngine::Sqlite, ExportFormat::SqlInsert, None, "students", &cols, &rows),
        sqlite_inserts
    );

    // CSV & JSON export remain identical across engines
    assert_eq!(
        export_rows_for_engine(DbEngine::Mysql, ExportFormat::Csv, None, "students", &cols, &rows),
        export_csv(&cols, &rows)
    );
    assert_eq!(
        export_rows_for_engine(DbEngine::Sqlite, ExportFormat::Json, None, "students", &cols, &rows),
        export_json(&cols, &rows)
    );
}

#[test]
fn staging_validation_rejects_null_pks_non_utf8_sqlite_and_preserves_mysql_backslashes() {
    let cols = sample_multi_engine_columns();
    let null_pk_row = vec![
        CellValue::Null,
        CellValue::Text("Chang".to_owned()),
        CellValue::Bool(true),
        CellValue::Null,
        CellValue::Null,
        CellValue::Null,
    ];
    let null_pk_raw = vec![
        RawCellValue::Sqlite(SqliteRawValue::Null),
        RawCellValue::Sqlite(SqliteRawValue::Text("Chang".to_owned())),
        RawCellValue::Sqlite(SqliteRawValue::Integer(1)),
        RawCellValue::Sqlite(SqliteRawValue::Null),
        RawCellValue::Sqlite(SqliteRawValue::Null),
        RawCellValue::Sqlite(SqliteRawValue::Null),
    ];
    assert!(validate_row_editable(DbEngine::Sqlite, &cols, &null_pk_row, Some(&null_pk_raw)).is_err());

    let mut null_staged = StagedChanges::new();
    null_staged.stage_cell_edit_with_raw(
        0,
        &null_pk_row,
        Some(&null_pk_raw),
        1,
        StagedCellValue::Literal("Kevin".to_owned()),
    );
    assert!(matches!(
        null_staged.build_commit_batch(DbEngine::Sqlite, "main", "students", &cols),
        Err(StagingError::NullPrimaryKey { .. })
    ));

    let valid_row = vec![
        CellValue::Int(7),
        CellValue::Text("Chang".to_owned()),
        CellValue::Bool(true),
        CellValue::Null,
        CellValue::Null,
        CellValue::Null,
    ];
    let non_utf8_raw = vec![
        RawCellValue::Sqlite(SqliteRawValue::Integer(7)),
        RawCellValue::Sqlite(SqliteRawValue::NonUtf8Text(vec![0xff, 0xfe])),
        RawCellValue::Sqlite(SqliteRawValue::Integer(1)),
        RawCellValue::Sqlite(SqliteRawValue::Null),
        RawCellValue::Sqlite(SqliteRawValue::Null),
        RawCellValue::Sqlite(SqliteRawValue::Null),
    ];
    assert!(validate_row_editable(DbEngine::Sqlite, &cols, &valid_row, Some(&non_utf8_raw)).is_err());
    let mut non_utf8_staged = StagedChanges::new();
    non_utf8_staged.toggle_delete_row_with_raw(0, &valid_row, Some(&non_utf8_raw));
    assert!(matches!(
        non_utf8_staged.build_commit_batch(DbEngine::Sqlite, "main", "students", &cols),
        Err(StagingError::UnbindableSqliteText { .. })
    ));

    // MySQL backslash expressions never contain raw `\` across 1, 2, and >=3 consecutive slashes.
    assert_eq!(format_mysql_string_literal(r"\"), "CHAR(92 USING utf8mb4)");
    assert_eq!(format_mysql_string_literal(r"\\"), "CHAR(92, 92 USING utf8mb4)");
    assert_eq!(
        format_mysql_string_literal(r"\\\"),
        "REPEAT(CHAR(92 USING utf8mb4), 3)"
    );
    let mut bs_staged = StagedChanges::new();
    let bs_row = vec![
        CellValue::Int(9),
        CellValue::Text(r"C:\greendale\\a'b".to_owned()),
        CellValue::Bool(true),
        CellValue::Null,
        CellValue::Null,
        CellValue::Null,
    ];
    bs_staged.stage_cell_edit(0, &bs_row, 1, StagedCellValue::Literal(r"D:\study\\\room".to_owned()));
    let batch = bs_staged
        .build_commit_batch(DbEngine::Mysql, "greendale", "students", &cols)
        .expect("build mysql batch");
    assert_eq!(batch.mutations.len(), 1);
    assert!(matches!(&batch.mutations[0], StagedMutation::Update { .. }));
    let sql = batch.render_sql(DbEngine::Mysql).expect("render mysql sql");
    assert!(!sql[0].contains('\\'), "mysql rendered SQL must not depend on NO_BACKSLASH_ESCAPES: {}", sql[0]);
    assert!(sql[0].contains(
        "CAST(CONVERT(`full_name` USING utf8mb4) AS BINARY) = CAST(CONVERT(CONCAT('C:', CHAR(92 USING utf8mb4), 'greendale', CHAR(92, 92 USING utf8mb4), 'a''b') USING utf8mb4) AS BINARY)"
    ));
}

#[test]
fn sql_splitter_and_cursor_respect_mysql_sqlite_and_pg_jsonb_operators() {
    // MySQL: backtick identifiers, \' escapes inside single quotes, and # line comments
    let mysql_sql = concat!(
        "SELECT `study;room`, `weird``;col` FROM `greendale`;\n",
        "SELECT 'Troy\\'s ; blanket fort' FROM students;\n",
        "# Dean Pelton; reminder\n",
        "SELECT 'Abed' FROM students;"
    );
    let my_stmts = split_statements_for_engine(mysql_sql, DbEngine::Mysql);
    assert_eq!(my_stmts.len(), 3);
    assert_eq!(my_stmts[0].sql, "SELECT `study;room`, `weird``;col` FROM `greendale`");
    assert_eq!(my_stmts[1].sql, "SELECT 'Troy\\'s ; blanket fort' FROM students");
    assert!(my_stmts[2].sql.ends_with("SELECT 'Abed' FROM students"));

    let fort_pos = mysql_sql.find("blanket fort").expect("find blanket fort");
    let at_fort = statement_at_cursor_for_engine(mysql_sql, fort_pos, fort_pos, DbEngine::Mysql)
        .expect("statement inside escaped quote");
    assert_eq!(at_fort.sql, "SELECT 'Troy\\'s ; blanket fort' FROM students");

    let abed_pos = mysql_sql.find("'Abed'").expect("find Abed");
    let at_abed = statement_at_cursor_for_engine(mysql_sql, abed_pos, abed_pos, DbEngine::Mysql)
        .expect("statement after hash comment");
    assert!(at_abed.sql.ends_with("SELECT 'Abed' FROM students"));
    assert!(!has_executable_sql_for_engine("# just a mysql comment", DbEngine::Mysql));

    // SQLite: backtick and double-quoted identifiers plus doubled single quotes
    let sqlite_sql = "SELECT `my``;col`, \"other\"\";col\" FROM `study;table`;\nSELECT 'Annie''s ; pen';";
    let sl_stmts = split_statements_for_engine(sqlite_sql, DbEngine::Sqlite);
    assert_eq!(sl_stmts.len(), 2);
    assert_eq!(sl_stmts[0].sql, "SELECT `my``;col`, \"other\"\";col\" FROM `study;table`");
    assert_eq!(sl_stmts[1].sql, "SELECT 'Annie''s ; pen'");

    let pen_pos = sqlite_sql.find("pen").expect("find pen");
    let at_pen = statement_at_cursor_for_engine(sqlite_sql, pen_pos, pen_pos, DbEngine::Sqlite)
        .expect("sqlite statement at cursor");
    assert_eq!(at_pen.sql, "SELECT 'Annie''s ; pen'");

    // PostgreSQL: `#>` and `#>>` JSONB operators MUST NOT be treated as line comments
    let pg_sql = "SELECT profile #> '{campus,room}' FROM students;\nSELECT profile #>> '{campus,dean}' FROM students;";
    let pg_stmts = split_statements_for_engine(pg_sql, DbEngine::Postgres);
    assert_eq!(pg_stmts.len(), 2);
    assert_eq!(pg_stmts[0].sql, "SELECT profile #> '{campus,room}' FROM students");
    assert_eq!(pg_stmts[1].sql, "SELECT profile #>> '{campus,dean}' FROM students");
    assert!(has_executable_sql_for_engine("#> '{campus}'", DbEngine::Postgres));

    let jsonb_pos = pg_sql.find("#>").expect("find jsonb operator");
    let at_jsonb = statement_at_cursor_for_engine(pg_sql, jsonb_pos, jsonb_pos, DbEngine::Postgres)
        .expect("pg jsonb statement");
    assert_eq!(at_jsonb.sql, "SELECT profile #> '{campus,room}' FROM students");
}

#[test]
fn safety_guardrails_handle_mysql_and_sqlite_specific_statements() {
    // MySQL-specific classification
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "SHOW TABLES").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "DESCRIBE students").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "REPLACE INTO students (student_id, full_name) VALUES (1, 'Troy')")
            .risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "DELETE FROM students").risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "DELETE FROM students WHERE student_id = 1").risk,
        StatementRisk::WriteDml
    );

    // SQLite-specific classification
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Sqlite, "PRAGMA table_info(students)").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Sqlite, "PRAGMA foreign_keys = OFF").risk,
        StatementRisk::WriteDdl
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Sqlite, "EXPLAIN QUERY PLAN SELECT * FROM students").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Sqlite, "VACUUM").risk,
        StatementRisk::WriteDdl
    );

    // Policy evaluation for MySQL
    let mysql_reads = vec!["SHOW TABLES".to_owned(), "DESCRIBE students".to_owned()];
    let mysql_replace = vec!["REPLACE INTO students VALUES (1, 'Abed')".to_owned()];
    let mysql_delete_all = vec!["DELETE FROM students".to_owned()];

    assert_eq!(
        evaluate_sql_safety_for_engine(DbEngine::Mysql, SafetyPolicy::ReadOnly, EnvTag::Local, &mysql_reads),
        SafetyDecision::Allow
    );
    assert!(matches!(
        evaluate_sql_safety_for_engine(DbEngine::Mysql, SafetyPolicy::ReadOnly, EnvTag::Local, &mysql_replace),
        SafetyDecision::Block { .. }
    ));
    assert!(matches!(
        evaluate_sql_safety_for_engine(
            DbEngine::Mysql,
            SafetyPolicy::ConfirmWrites,
            EnvTag::Development,
            &mysql_delete_all
        ),
        SafetyDecision::RequireConfirmation {
            is_destructive: true,
            ..
        }
    ));

    // Policy evaluation for SQLite
    let sqlite_reads = vec![
        "PRAGMA table_info(students)".to_owned(),
        "EXPLAIN QUERY PLAN SELECT * FROM students".to_owned(),
    ];
    let sqlite_ddl = vec!["PRAGMA foreign_keys = OFF".to_owned(), "VACUUM".to_owned()];

    assert_eq!(
        evaluate_sql_safety_for_engine(DbEngine::Sqlite, SafetyPolicy::ReadOnly, EnvTag::Local, &sqlite_reads),
        SafetyDecision::Allow
    );
    assert!(matches!(
        evaluate_sql_safety_for_engine(DbEngine::Sqlite, SafetyPolicy::ReadOnly, EnvTag::Local, &sqlite_ddl),
        SafetyDecision::Block { .. }
    ));
    assert!(matches!(
        evaluate_sql_safety_for_engine(
            DbEngine::Sqlite,
            SafetyPolicy::ConfirmWrites,
            EnvTag::Local,
            &sqlite_ddl
        ),
        SafetyDecision::RequireConfirmation {
            is_destructive: false,
            ..
        }
    ));

    // MySQL/MariaDB executable comments and BOM-prefixed statements must not bypass write guardrails.
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "/*M! SET @x = 1 */").risk,
        StatementRisk::WriteDdl
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "/*!50000 DROP TABLE students */").risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "/*M!100401 DELETE FROM students */").risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "SELECT '/*M! DROP TABLE students */'").risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "\u{feff}DELETE FROM students").risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "SELECT \"Troy\\\"Abed\" /*M! INTO @greendale */").risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "SELECT 3--1 /*M! INTO @greendale */").risk,
        StatementRisk::WriteDml
    );
}

#[test]
fn explain_parsers_handle_mysql_json_analyze_tree_sqlite_plan_and_result_extraction() {
    assert_eq!(
        build_explain_sql_for_engine(DbEngine::Mysql, "SELECT * FROM students;", false),
        "EXPLAIN FORMAT=JSON SELECT * FROM students"
    );
    assert_eq!(
        build_explain_sql_for_engine(DbEngine::Mysql, "SELECT * FROM students;", true),
        "EXPLAIN ANALYZE SELECT * FROM students"
    );
    assert_eq!(
        build_explain_sql_for_engine(DbEngine::Sqlite, "SELECT * FROM students;", false),
        "EXPLAIN QUERY PLAN SELECT * FROM students"
    );
    assert_eq!(
        build_explain_sql_for_engine(DbEngine::Sqlite, "SELECT * FROM students;", true),
        "EXPLAIN QUERY PLAN SELECT * FROM students"
    );

    // 1. MySQL EXPLAIN FORMAT=JSON
    let mysql_json = r#"{
      "query_block": {
        "select_id": 1,
        "cost_info": { "query_cost": "12.40" },
        "ordering_operation": {
          "using_filesort": true,
          "nested_loop": [
            {
              "table": {
                "table_name": "students",
                "access_type": "ALL",
                "rows_examined_per_scan": 120,
                "cost_info": { "prefix_cost": "4.20" },
                "attached_condition": "(students.gpa >= 3.0)"
              }
            },
            {
              "table": {
                "table_name": "study_groups",
                "access_type": "eq_ref",
                "key": "PRIMARY",
                "ref": ["greendale.students.group_id"],
                "rows_examined_per_scan": 1,
                "cost_info": { "prefix_cost": "12.40" }
              }
            }
          ]
        }
      }
    }"#;
    let my_plan = ExplainPlan::parse_mysql_json("SELECT * FROM students".to_owned(), mysql_json)
        .expect("parse mysql json plan");
    assert!(!my_plan.analyzed);
    assert_eq!(my_plan.root.node_type, "Sort / Ordering (filesort)");
    assert!((my_plan.total_cost() - 12.40).abs() < 0.001);
    assert_eq!(my_plan.node_count(), 4);
    let nested = &my_plan.root.children[0];
    assert_eq!(nested.node_type, "Nested Loop");
    assert_eq!(nested.children[0].node_type, "Table Scan (ALL)");
    assert_eq!(nested.children[0].relation_name.as_deref(), Some("students"));
    assert_eq!(nested.children[0].filter.as_deref(), Some("(students.gpa >= 3.0)"));
    assert_eq!(nested.children[1].node_type, "Unique Index Lookup (eq_ref)");
    assert_eq!(nested.children[1].index_name.as_deref(), Some("PRIMARY"));

    assert!(matches!(
        ExplainPlan::parse_mysql_json("SELECT 1".to_owned(), "{bad json"),
        Err(ExplainError::InvalidJson(_))
    ));
    assert!(matches!(
        ExplainPlan::parse_mysql_json("SELECT 1".to_owned(), r#"{"unrelated": 1}"#),
        Err(ExplainError::MissingPlanRoot)
    ));

    // 2. MySQL EXPLAIN ANALYZE tree
    let analyze_tree = "\
-> Nested loop inner join  (cost=4.80 rows=8) (actual time=0.110..0.420 rows=8 loops=1)
    -> Table scan on s  (cost=1.20 rows=8) (actual time=0.040..0.090 rows=8 loops=1)
    -> Filter: (g.active = 1)  (cost=0.45 rows=1) (actual time=0.015..0.030 rows=1 loops=8)
        -> Single-row index lookup on g using PRIMARY (group_id=s.group_id)  (cost=0.35 rows=1) (actual time=0.010..0.020 rows=1 loops=8)
    -> Table scan on archived_notes  (cost=0.25 rows=1) (never executed)";
    let analyzed = ExplainPlan::parse_mysql_analyze_tree("SELECT ...".to_owned(), analyze_tree)
        .expect("parse mysql analyze tree");
    assert!(analyzed.analyzed);
    assert_eq!(analyzed.node_count(), 5);
    assert!((analyzed.total_runtime_ms().expect("runtime") - 0.420).abs() < 0.001);
    assert_eq!(analyzed.root.children[1].node_type, "Filter");
    assert_eq!(analyzed.root.children[1].filter.as_deref(), Some("(g.active = 1)"));
    assert_eq!(analyzed.root.children[1].children[0].index_name.as_deref(), Some("PRIMARY"));
    assert_eq!(analyzed.root.children[2].actual_rows, Some(0));
    assert_eq!(analyzed.root.children[2].actual_loops, Some(0.0));
    assert!(analyzed.root.exclusive_time_ms().expect("excl") > 0.0);

    assert!(matches!(
        ExplainPlan::parse_mysql_analyze_tree("SELECT 1".to_owned(), "no tree arrows here"),
        Err(ExplainError::MissingPlanRoot)
    ));

    // 3. SQLite EXPLAIN QUERY PLAN (SCAN, SEARCH ... USING INDEX ..., hierarchy, empty error)
    let sqlite_rows = vec![
        (2, 0, "SCAN students".to_owned()),
        (5, 2, "SEARCH study_groups USING INDEX idx_group_name (name=?)".to_owned()),
        (8, 0, "USE TEMP B-TREE FOR ORDER BY".to_owned()),
    ];
    let sl_plan = ExplainPlan::parse_sqlite_query_plan(
        "SELECT * FROM students JOIN study_groups USING (group_id) ORDER BY students.full_name".to_owned(),
        &sqlite_rows,
    )
    .expect("parse sqlite plan");
    assert!(!sl_plan.analyzed);
    assert_eq!(sl_plan.root.node_type, "QUERY PLAN");
    assert_eq!(sl_plan.root.children.len(), 2);
    assert_eq!(sl_plan.root.children[0].node_type, "SCAN");
    assert_eq!(sl_plan.root.children[0].relation_name.as_deref(), Some("students"));
    assert_eq!(sl_plan.root.children[0].index_name, None);
    assert_eq!(sl_plan.root.children[0].children[0].node_type, "SEARCH");
    assert_eq!(
        sl_plan.root.children[0].children[0].relation_name.as_deref(),
        Some("study_groups")
    );
    assert_eq!(
        sl_plan.root.children[0].children[0].index_name.as_deref(),
        Some("idx_group_name")
    );
    assert_eq!(
        sl_plan.root.children[0].children[0].index_cond.as_deref(),
        Some("(name=?)")
    );
    assert_eq!(sl_plan.root.children[1].node_type, "USE TEMP B-TREE");

    assert!(matches!(
        ExplainPlan::parse_sqlite_query_plan("SELECT 1".to_owned(), &[]),
        Err(ExplainError::EmptyPlan)
    ));

    // 4. ExplainPlan::try_extract_from_result for MySQL JSON, MySQL Analyze, SQLite, and non-explain
    let text_col = |name: &str| ColumnMeta {
        name: name.to_owned(),
        data_type: "text".to_owned(),
        udt_name: "text".to_owned(),
        nullable: true,
        default_expr: None,
        is_primary_key: false,
        is_unique: false,
        comment: None,
        foreign_key: None,
    };

    let mysql_json_res = QueryStatementResult {
        sql: "EXPLAIN FORMAT=JSON SELECT * FROM students".to_owned(),
        columns: vec![text_col("EXPLAIN")],
        rows: vec![vec![CellValue::Text(mysql_json.to_owned())]],
        rows_affected: None,
        truncated: false,
        total_rows_seen: 1,
        elapsed_ms: 2,
        error: None,
    };
    let extracted_my_json = ExplainPlan::try_extract_from_result(&mysql_json_res).expect("extract mysql json");
    assert_eq!(extracted_my_json.root.node_type, "Sort / Ordering (filesort)");

    let mysql_analyze_res = QueryStatementResult {
        sql: "EXPLAIN ANALYZE SELECT * FROM students".to_owned(),
        columns: vec![text_col("EXPLAIN")],
        rows: vec![vec![CellValue::Text(analyze_tree.to_owned())]],
        rows_affected: None,
        truncated: false,
        total_rows_seen: 1,
        elapsed_ms: 3,
        error: None,
    };
    let extracted_my_tree = ExplainPlan::try_extract_from_result(&mysql_analyze_res).expect("extract mysql analyze");
    assert!(extracted_my_tree.analyzed);
    assert_eq!(extracted_my_tree.node_count(), 5);

    let sqlite_res = QueryStatementResult {
        sql: "EXPLAIN QUERY PLAN SELECT * FROM students".to_owned(),
        columns: vec![
            text_col("id"),
            text_col("parent"),
            text_col("notused"),
            text_col("detail"),
        ],
        rows: vec![vec![
            CellValue::Int(2),
            CellValue::Int(0),
            CellValue::Int(0),
            CellValue::Text("SEARCH students USING INDEX idx_gpa (gpa>?)".to_owned()),
        ]],
        rows_affected: None,
        truncated: false,
        total_rows_seen: 1,
        elapsed_ms: 1,
        error: None,
    };
    let extracted_sl = ExplainPlan::try_extract_from_result(&sqlite_res).expect("extract sqlite eqp");
    assert_eq!(extracted_sl.root.node_type, "SEARCH");
    assert_eq!(extracted_sl.root.relation_name.as_deref(), Some("students"));
    assert_eq!(extracted_sl.root.index_name.as_deref(), Some("idx_gpa"));

    let normal_res = QueryStatementResult {
        sql: "SELECT full_name FROM students".to_owned(),
        columns: vec![text_col("full_name")],
        rows: vec![vec![CellValue::Text("Troy Barnes".to_owned())]],
        rows_affected: None,
        truncated: false,
        total_rows_seen: 1,
        elapsed_ms: 1,
        error: None,
    };
    assert!(ExplainPlan::try_extract_from_result(&normal_res).is_none());
}

#[test]
fn atomic_write_bytes_and_concurrent_store_saves_leave_no_temp_files() {
    let dir = tempdir().expect("create tempdir");
    let nested_file = dir.path().join("exports/study_room/greendale.json");
    atomic_write_bytes(&nested_file, b"{\"campus\":\"Greendale\"}").expect("initial atomic write");
    assert_eq!(
        std::fs::read_to_string(&nested_file).expect("read written file"),
        "{\"campus\":\"Greendale\"}"
    );

    atomic_write_bytes(&nested_file, b"{\"dean\":\"Pelton\"}").expect("overwrite atomic write");
    assert_eq!(
        std::fs::read_to_string(&nested_file).expect("read overwritten file"),
        "{\"dean\":\"Pelton\"}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode_file = dir.path().join("exports/study_room/shared_export.csv");
        atomic_write_bytes(&mode_file, b"name\nTroy\n").expect("create mode export file");
        std::fs::set_permissions(&mode_file, std::fs::Permissions::from_mode(0o664))
            .expect("chmod 0664");
        atomic_write_bytes(&mode_file, b"name\nAbed\n").expect("overwrite mode export file");
        let after_mode = std::fs::metadata(&mode_file)
            .expect("stat overwritten export")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(after_mode, 0o664);
    }

    let err_dir = dir.path().join("existing_dir");
    std::fs::create_dir_all(&err_dir).expect("create blocking directory");
    let err = atomic_write_bytes(&err_dir, b"blocked");
    assert!(err.is_err(), "persisting onto existing directory must fail");

    for folder in [nested_file.parent().expect("parent"), dir.path()] {
        for entry in std::fs::read_dir(folder).expect("read dir") {
            let name = entry.expect("dir entry").file_name().to_string_lossy().into_owned();
            assert!(!name.starts_with(".tableview-"), "unexpected leftover temp file: {name}");
        }
    }

    // Concurrent multi-threaded saves to the exact same path must never fail or corrupt JSON.
    let store_path = std::sync::Arc::new(dir.path().join("concurrent/connections.json"));
    let mut handles = Vec::new();
    for worker_idx in 0..8usize {
        let path = std::sync::Arc::clone(&store_path);
        handles.push(std::thread::spawn(move || {
            let store = ConnectionStore::at((*path).clone());
            for iter in 0..15usize {
                let mut data = StoreData::default();
                data.connections.push(ConnectionProfile::new_postgres(
                    format!("study_group_{worker_idx}_{iter}"),
                    "127.0.0.1",
                    "troy",
                ));
                store.save(&data).expect("concurrent store save succeeds");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("join writer thread");
    }

    let final_store = ConnectionStore::at((*store_path).clone());
    let loaded = final_store.load().expect("concurrently written store parses cleanly");
    assert_eq!(loaded.connections.len(), 1);
    for entry in std::fs::read_dir(store_path.parent().expect("store parent")).expect("read store dir") {
        let name = entry.expect("entry").file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(".tableview-"), "leftover temp file after concurrency: {name}");
    }
}

#[test]
fn export_json_disambiguates_duplicate_columns_without_collisions() {
    let make_col = |name: &str| ColumnMeta {
        name: name.to_owned(),
        data_type: "text".to_owned(),
        udt_name: "text".to_owned(),
        nullable: true,
        default_expr: None,
        is_primary_key: false,
        is_unique: false,
        comment: None,
        foreign_key: None,
    };

    // Case 1: ["name", "name_2", "name"] -> ["name", "name_2", "name_3"]
    let cols1 = vec![make_col("name"), make_col("name_2"), make_col("name")];
    let rows1 = vec![vec![
        CellValue::Text("Troy Barnes".to_owned()),
        CellValue::Text("Abed Nadir".to_owned()),
        CellValue::Text("Annie Edison".to_owned()),
    ]];
    let parsed1: serde_json::Value = serde_json::from_str(&export_json(&cols1, &rows1)).expect("json1");
    let obj1 = parsed1[0].as_object().expect("obj1");
    assert_eq!(obj1.len(), 3);
    assert_eq!(obj1["name"], "Troy Barnes");
    assert_eq!(obj1["name_2"], "Abed Nadir");
    assert_eq!(obj1["name_3"], "Annie Edison");

    // Case 2: ["name", "name", "name_1"] -> ["name", "name_2", "name_1"]
    let cols2 = vec![make_col("name"), make_col("name"), make_col("name_1")];
    let rows2 = vec![vec![
        CellValue::Text("Jeff Winger".to_owned()),
        CellValue::Text("Britta Perry".to_owned()),
        CellValue::Text("Dean Pelton".to_owned()),
    ]];
    let parsed2: serde_json::Value = serde_json::from_str(&export_json(&cols2, &rows2)).expect("json2");
    let obj2 = parsed2[0].as_object().expect("obj2");
    assert_eq!(obj2.len(), 3);
    assert_eq!(obj2["name"], "Jeff Winger");
    assert_eq!(obj2["name_2"], "Britta Perry");
    assert_eq!(obj2["name_1"], "Dean Pelton");

    // Case 3: ["?column?", "?column?", "?column?"] -> ["?column?", "?column?_1", "?column?_2"]
    let cols3 = vec![make_col("?column?"), make_col("?column?"), make_col("?column?")];
    let rows3 = vec![vec![CellValue::Int(1), CellValue::Int(2), CellValue::Int(3)]];
    let parsed3: serde_json::Value = serde_json::from_str(&export_json(&cols3, &rows3)).expect("json3");
    let obj3 = parsed3[0].as_object().expect("obj3");
    assert_eq!(obj3.len(), 3);
    assert_eq!(obj3["?column?"], 1);
    assert_eq!(obj3["?column?_1"], 2);
    assert_eq!(obj3["?column?_2"], 3);

    // Case 4: ["x", "x_1", "x", "x_2", "x"] -> 5 distinct keys with exact preservation
    let cols4 = vec![
        make_col("x"),
        make_col("x_1"),
        make_col("x"),
        make_col("x_2"),
        make_col("x"),
    ];
    let rows4 = vec![vec![
        CellValue::Int(10),
        CellValue::Int(20),
        CellValue::Int(30),
        CellValue::Int(40),
        CellValue::Int(50),
    ]];
    let parsed4: serde_json::Value = serde_json::from_str(&export_json(&cols4, &rows4)).expect("json4");
    let obj4 = parsed4[0].as_object().expect("obj4");
    assert_eq!(obj4.len(), 5);
    assert_eq!(obj4["x"], 10);
    assert_eq!(obj4["x_1"], 20);
    assert_eq!(obj4["x_3"], 30);
    assert_eq!(obj4["x_2"], 40);
    assert_eq!(obj4["x_4"], 50);
}

#[test]
fn sql_splitter_handles_dialect_edge_cases_across_postgres_mysql_and_sqlite() {
    // 1. PostgreSQL E'...' / e'...' escape strings (including multi-line continuations and UTF-8)
    let pg_escape = concat!(
        "SELECT E'Troy\\'; fort', e'Abed''s\\'; fort', E'Britta\\é; fort';\n",
        "SELECT 'plain\\'; SELECT 2;"
    );
    let pg_escape_stmts = split_statements_for_engine(pg_escape, DbEngine::Postgres);
    assert_eq!(pg_escape_stmts.len(), 3);
    assert!(pg_escape_stmts[0].sql.contains("E'Britta\\é; fort'"));
    assert_eq!(pg_escape_stmts[1].sql, "SELECT 'plain\\'");
    assert_eq!(pg_escape_stmts[2].sql, "SELECT 2");

    // Multi-line E'...' continuation carries escape handling across newline and `--` comments (\n or \r),
    // while same-line literals and block comments break continuation per PostgreSQL scan.l.
    let pg_multiline_escape = concat!(
        "SELECT E'one'\n'two\\';three';\n",
        "SELECT E'one' -- greendale comment\r'two\\';three'\n'four\\';five';\n",
        "SELECT E'one' 'two\\'; SELECT 2;\n",
        "SELECT E'one' /*\n*/ 'two\\'; SELECT 3;\n",
        "-- trailing cr comment\rSELECT 4;"
    );
    let pg_multi_stmts = split_statements_for_engine(pg_multiline_escape, DbEngine::Postgres);
    assert_eq!(pg_multi_stmts.len(), 7);
    assert_eq!(pg_multi_stmts[0].sql, "SELECT E'one'\n'two\\';three'");
    assert_eq!(
        pg_multi_stmts[1].sql,
        "SELECT E'one' -- greendale comment\r'two\\';three'\n'four\\';five'"
    );
    assert_eq!(pg_multi_stmts[2].sql, "SELECT E'one' 'two\\'");
    assert_eq!(pg_multi_stmts[3].sql, "SELECT 2");
    assert_eq!(pg_multi_stmts[4].sql, "SELECT E'one' /*\n*/ 'two\\'");
    assert_eq!(pg_multi_stmts[5].sql, "SELECT 3");
    assert_eq!(pg_multi_stmts[6].sql, "-- trailing cr comment\rSELECT 4");

    // Unicode dollar-quote tags must work just like PostgreSQL.
    let pg_unicode_dollar = "SELECT $ét$Troy; Abed$ét$; SELECT 2;";
    let pg_unicode_stmts = split_statements_for_engine(pg_unicode_dollar, DbEngine::Postgres);
    assert_eq!(pg_unicode_stmts.len(), 2);
    assert_eq!(pg_unicode_stmts[0].sql, "SELECT $ét$Troy; Abed$ét$");
    assert_eq!(pg_unicode_stmts[1].sql, "SELECT 2");

    // `col_e` followed by quote is identifier + normal quote; standalone $1 and a$b$c are not dollar-quotes.
    let pg_ident = "SELECT a$b$c, $1 + $2, col_e'\\'; SELECT 2;";
    let pg_ident_stmts = split_statements_for_engine(pg_ident, DbEngine::Postgres);
    assert_eq!(pg_ident_stmts.len(), 2);
    assert_eq!(pg_ident_stmts[1].sql, "SELECT 2");

    // `SELECT begin atomic ...` and routine `RETURN (SELECT begin atomic ...)` MUST split on `;`.
    for pg_false_atomic in [
        "SELECT begin atomic FROM (VALUES (1)) AS students(begin); SELECT 2;",
        "CREATE FUNCTION greendale_grade() RETURNS int LANGUAGE SQL RETURN (SELECT begin atomic FROM (VALUES (1)) AS students(begin)); SELECT 2;",
    ] {
        let stmts = split_statements_for_engine(pg_false_atomic, DbEngine::Postgres);
        assert_eq!(stmts.len(), 2, "failed split for {pg_false_atomic}");
        assert_eq!(stmts[1].sql, "SELECT 2");
        let tail_pos = pg_false_atomic.rfind("SELECT 2").expect("find tail SELECT");
        let at_tail = statement_at_cursor_for_engine(pg_false_atomic, tail_pos, tail_pos, DbEngine::Postgres)
            .expect("tail statement at cursor");
        assert_eq!(at_tail.sql, "SELECT 2");
    }

    let pg_atomic = concat!(
        "CREATE OR REPLACE FUNCTION greendale_grade(score INT) RETURNS INT LANGUAGE SQL\n",
        "BEGIN ATOMIC\n",
        "  SELECT 1 AS end;\n",
        "  SELECT CASE WHEN score > 90 THEN 4 ELSE 2 END;\n",
        "END;\n",
        "BEGIN; INSERT INTO students VALUES (1); COMMIT;"
    );
    let pg_atomic_stmts = split_statements_for_engine(pg_atomic, DbEngine::Postgres);
    assert_eq!(pg_atomic_stmts.len(), 4);
    assert!(pg_atomic_stmts[0].sql.starts_with("CREATE OR REPLACE FUNCTION greendale_grade"));
    assert!(pg_atomic_stmts[0].sql.ends_with("END"));
    assert_eq!(pg_atomic_stmts[1].sql, "BEGIN");
    assert_eq!(pg_atomic_stmts[2].sql, "INSERT INTO students VALUES (1)");
    assert_eq!(pg_atomic_stmts[3].sql, "COMMIT");
    let as_end_pos = pg_atomic.find("AS end").expect("find AS end");
    let at_routine = statement_at_cursor_for_engine(pg_atomic, as_end_pos, as_end_pos, DbEngine::Postgres)
        .expect("routine at cursor");
    assert_eq!(at_routine.sql, pg_atomic_stmts[0].sql);

    // 2. Block comment nesting differs: PostgreSQL nests, MySQL & SQLite close on the first */.
    let nested_comment = "/* outer /* inner */ SELECT 2;";
    assert!(!has_executable_sql_for_engine(nested_comment, DbEngine::Postgres));
    assert!(has_executable_sql_for_engine(nested_comment, DbEngine::Mysql));
    assert!(has_executable_sql_for_engine(nested_comment, DbEngine::Sqlite));
    assert_eq!(split_statements_for_engine(nested_comment, DbEngine::Postgres).len(), 0);
    let my_after_comment = split_statements_for_engine(nested_comment, DbEngine::Mysql);
    assert_eq!(my_after_comment.len(), 1);
    assert!(my_after_comment[0].sql.ends_with("SELECT 2"));

    // 3. MySQL UTF-8 backslash escape in single quotes and literal backslash in ANSI_QUOTES double quotes.
    let mysql_quotes = "SELECT 'Troy\\é;fort', 'Abed\\';room', \"ab\"\";cd\"; SELECT 2;";
    let my_quotes_stmts = split_statements_for_engine(mysql_quotes, DbEngine::Mysql);
    assert_eq!(my_quotes_stmts.len(), 2);
    assert_eq!(my_quotes_stmts[1].sql, "SELECT 2");

    let mysql_ansi_quotes = "SELECT \"ident\\\"; SELECT 2;";
    let my_ansi_stmts = split_statements_for_engine(mysql_ansi_quotes, DbEngine::Mysql);
    assert_eq!(my_ansi_stmts.len(), 2);
    assert_eq!(my_ansi_stmts[0].sql, "SELECT \"ident\\\"");
    assert_eq!(my_ansi_stmts[1].sql, "SELECT 2");

    // 4. SQLite bracket identifiers and CREATE / EXPLAIN TRIGGER compound bodies (even with `SET end = ...`).
    let sqlite_triggers = concat!(
        "SELECT [weird;col] FROM [study;table];\n",
        "CREATE TRIGGER tr_audit AFTER INSERT ON students BEGIN\n",
        "  UPDATE audit SET end = NEW.student_id;\n",
        "  UPDATE audit SET note = CASE WHEN NEW.gpa > 3 THEN 'honors' ELSE 'pass' END;\n",
        "END;\n",
        "EXPLAIN QUERY PLAN CREATE TEMPORARY TRIGGER IF NOT EXISTS tr_temp AFTER DELETE ON students BEGIN\n",
        "  DELETE FROM log WHERE end = OLD.student_id;\n",
        "END;\n",
        "CREATE TRIGGER tr_when AFTER INSERT ON students WHEN (begin = 1) BEGIN INSERT INTO log VALUES (1); END;\n",
        "BEGIN IMMEDIATE; INSERT INTO log VALUES (42); COMMIT;"
    );
    let sl_stmts = split_statements_for_engine(sqlite_triggers, DbEngine::Sqlite);
    assert_eq!(sl_stmts.len(), 7);
    assert_eq!(sl_stmts[0].sql, "SELECT [weird;col] FROM [study;table]");
    assert!(sl_stmts[1].sql.starts_with("CREATE TRIGGER tr_audit"));
    assert!(sl_stmts[1].sql.ends_with("END"));
    assert!(sl_stmts[2].sql.starts_with("EXPLAIN QUERY PLAN CREATE TEMPORARY TRIGGER"));
    assert!(sl_stmts[2].sql.ends_with("END"));
    assert!(sl_stmts[3].sql.starts_with("CREATE TRIGGER tr_when"));
    assert!(sl_stmts[3].sql.ends_with("END"));
    assert_eq!(sl_stmts[4].sql, "BEGIN IMMEDIATE");
    assert_eq!(sl_stmts[5].sql, "INSERT INTO log VALUES (42)");
    assert_eq!(sl_stmts[6].sql, "COMMIT");

    let set_end_pos = sqlite_triggers.find("SET end = NEW").expect("find SET end");
    let at_tr_audit = statement_at_cursor_for_engine(sqlite_triggers, set_end_pos, set_end_pos, DbEngine::Sqlite)
        .expect("trigger at cursor");
    assert_eq!(at_tr_audit.sql, sl_stmts[1].sql);

    let explain_end_pos = sqlite_triggers.find("WHERE end = OLD").expect("find WHERE end");
    let at_explain_tr = statement_at_cursor_for_engine(
        sqlite_triggers,
        explain_end_pos,
        explain_end_pos,
        DbEngine::Sqlite,
    )
    .expect("explain trigger at cursor");
    assert_eq!(at_explain_tr.sql, sl_stmts[2].sql);

    // SQLite and MySQL line comments end at `\n`, not standalone `\r`.
    let sqlite_cr_comment = "SELECT 1 -- Greendale\r; SELECT 3;\n; SELECT 4;";
    let sl_cr_stmts = split_statements_for_engine(sqlite_cr_comment, DbEngine::Sqlite);
    assert_eq!(sl_cr_stmts.len(), 2);
    assert_eq!(sl_cr_stmts[0].sql, "SELECT 1 -- Greendale\r; SELECT 3;");
    assert_eq!(sl_cr_stmts[1].sql, "SELECT 4");
    let at_sqlite_sel1 = sqlite_cr_comment.find("SELECT 1").expect("find SELECT 1");
    let sl_cur = statement_at_cursor_for_engine(
        sqlite_cr_comment,
        at_sqlite_sel1,
        at_sqlite_sel1,
        DbEngine::Sqlite,
    )
    .expect("sqlite statement at cursor");
    assert_eq!(sl_cur.sql, sl_cr_stmts[0].sql);
    assert!(!has_executable_sql_for_engine("-- Greendale\rSELECT 9;", DbEngine::Sqlite));

    let mysql_cr_comment = "SELECT 1 -- Greendale\r; SELECT 3;\n; SELECT 4;";
    let my_cr_stmts = split_statements_for_engine(mysql_cr_comment, DbEngine::Mysql);
    assert_eq!(my_cr_stmts.len(), 2);
    assert_eq!(my_cr_stmts[0].sql, "SELECT 1 -- Greendale\r; SELECT 3;");
    assert_eq!(my_cr_stmts[1].sql, "SELECT 4");
    assert_eq!(
        split_statements_for_engine("SELECT 1 -- Greendale\r; SELECT 3;\nSELECT 4;", DbEngine::Mysql)
            .len(),
        1
    );
    assert!(!has_executable_sql_for_engine("# Greendale\rSELECT 9;", DbEngine::Mysql));

    // PostgreSQL routine names equal to unreserved keywords (e.g. `return`) must not disable BEGIN ATOMIC.
    let pg_named_return = concat!(
        "CREATE FUNCTION return() RETURNS integer LANGUAGE SQL BEGIN ATOMIC SELECT 1; END;\n",
        "SELECT 2;"
    );
    let pg_ret_stmts = split_statements_for_engine(pg_named_return, DbEngine::Postgres);
    assert_eq!(pg_ret_stmts.len(), 2);
    assert!(pg_ret_stmts[0].sql.starts_with("CREATE FUNCTION return()"));
    assert!(pg_ret_stmts[0].sql.ends_with("END"));
    assert_eq!(pg_ret_stmts[1].sql, "SELECT 2");
    let at_pg_ret_body = pg_named_return.find("SELECT 1").expect("find SELECT 1 in return()");
    let pg_ret_cur = statement_at_cursor_for_engine(
        pg_named_return,
        at_pg_ret_body,
        at_pg_ret_body,
        DbEngine::Postgres,
    )
    .expect("pg return() at cursor");
    assert_eq!(pg_ret_cur.sql, pg_ret_stmts[0].sql);

    // PG return type named `return` + PG parenthesized CREATE RULE action lists must not split early.
    let pg_return_type_and_rule = concat!(
        "CREATE TYPE return AS ENUM ('troy');\n",
        "CREATE FUNCTION greendale_grade() RETURNS return LANGUAGE SQL BEGIN ATOMIC SELECT NULL::return; END;\n",
        "CREATE RULE greendale_rule AS ON INSERT TO students DO ALSO (INSERT INTO audit VALUES (1); INSERT INTO audit VALUES (2));\n",
        "SELECT 2;"
    );
    let pg_tr_stmts = split_statements_for_engine(pg_return_type_and_rule, DbEngine::Postgres);
    assert_eq!(pg_tr_stmts.len(), 4);
    assert!(pg_tr_stmts[1].sql.starts_with("CREATE FUNCTION greendale_grade()"));
    assert!(pg_tr_stmts[1].sql.ends_with("END"));
    assert!(pg_tr_stmts[2].sql.starts_with("CREATE RULE greendale_rule"));
    assert!(pg_tr_stmts[2].sql.ends_with(')'));
    assert_eq!(pg_tr_stmts[3].sql, "SELECT 2");
    let rule_inner_pos = pg_return_type_and_rule
        .find("INSERT INTO audit VALUES (2)")
        .expect("find second rule action");
    let at_rule = statement_at_cursor_for_engine(
        pg_return_type_and_rule,
        rule_inner_pos,
        rule_inner_pos,
        DbEngine::Postgres,
    )
    .expect("rule at cursor");
    assert_eq!(at_rule.sql, pg_tr_stmts[2].sql);

    // MySQL default-mode strings allow \" escapes inside double quotes, while ANSI_QUOTES fallback disambiguates.
    let mysql_default_dq = "SELECT \"Troy\\\";Abed\", \"Jeff\\\";Britta\"; SELECT 2;";
    let my_dq_stmts = split_statements_for_engine(mysql_default_dq, DbEngine::Mysql);
    assert_eq!(my_dq_stmts.len(), 2);
    assert_eq!(my_dq_stmts[0].sql, "SELECT \"Troy\\\";Abed\", \"Jeff\\\";Britta\"");
    assert_eq!(my_dq_stmts[1].sql, "SELECT 2");

    let mysql_ansi_comment = "SELECT 1 AS \"Troy\\\"; -- \"\nSELECT 2;";
    let my_ansi_comment_stmts = split_statements_for_engine(mysql_ansi_comment, DbEngine::Mysql);
    assert_eq!(my_ansi_comment_stmts.len(), 2);
    assert_eq!(my_ansi_comment_stmts[0].sql, "SELECT 1 AS \"Troy\\\"");
    assert_eq!(my_ansi_comment_stmts[1].sql, "-- \"\nSELECT 2");
    let sel2_pos = mysql_ansi_comment.find("SELECT 2").expect("find SELECT 2");
    let at_sel2 = statement_at_cursor_for_engine(mysql_ansi_comment, sel2_pos, sel2_pos, DbEngine::Mysql)
        .expect("SELECT 2 at cursor");
    assert_eq!(at_sel2.sql, my_ansi_comment_stmts[1].sql);

    let mysql_ansi_hash = "SELECT 1 AS \"Troy\\\"; # \"\nSELECT 2;";
    let my_ansi_hash_stmts = split_statements_for_engine(mysql_ansi_hash, DbEngine::Mysql);
    assert_eq!(my_ansi_hash_stmts.len(), 2);
    assert_eq!(my_ansi_hash_stmts[0].sql, "SELECT 1 AS \"Troy\\\"");
    assert_eq!(my_ansi_hash_stmts[1].sql, "# \"\nSELECT 2");
    let hash_sel2_pos = mysql_ansi_hash.find("SELECT 2").expect("find SELECT 2 after #");
    let at_hash_sel2 = statement_at_cursor_for_engine(mysql_ansi_hash, hash_sel2_pos, hash_sel2_pos, DbEngine::Mysql)
        .expect("SELECT 2 after # at cursor");
    assert_eq!(at_hash_sel2.sql, my_ansi_hash_stmts[1].sql);

    // MySQL requires whitespace/control after `--` to start a comment (`SELECT 3--1` is subtraction).
    let mysql_arith = "SELECT 3--1; SELECT 4;";
    let my_arith_stmts = split_statements_for_engine(mysql_arith, DbEngine::Mysql);
    assert_eq!(my_arith_stmts.len(), 2);
    assert_eq!(my_arith_stmts[0].sql, "SELECT 3--1");
    assert_eq!(my_arith_stmts[1].sql, "SELECT 4");
    assert_eq!(split_statements_for_engine(mysql_arith, DbEngine::Postgres).len(), 1);
    assert_eq!(split_statements_for_engine(mysql_arith, DbEngine::Sqlite).len(), 1);

    // PostgreSQL Unicode identifiers before `$tag$` must not trigger dollar quoting.
    let pg_emoji_ident = "SELECT 1 AS greendale\u{1F600}$tag$; SELECT 2;";
    let pg_emoji_stmts = split_statements_for_engine(pg_emoji_ident, DbEngine::Postgres);
    assert_eq!(pg_emoji_stmts.len(), 2);
    assert_eq!(pg_emoji_stmts[0].sql, "SELECT 1 AS greendale\u{1F600}$tag$");
    assert_eq!(pg_emoji_stmts[1].sql, "SELECT 2");
    let emoji_sel2 = pg_emoji_ident.find("SELECT 2").expect("find SELECT 2 after emoji ident");
    let at_emoji_sel2 = statement_at_cursor_for_engine(pg_emoji_ident, emoji_sel2, emoji_sel2, DbEngine::Postgres)
        .expect("SELECT 2 at cursor after emoji ident");
    assert_eq!(at_emoji_sel2.sql, "SELECT 2");
}

#[test]
fn sql_sets_credentials_detects_password_statements_without_false_positives() {
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE ROLE troy_barnes WITH LOGIN PASSWORD 'greendale-secret'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER ROLE abed_nadir PASSWORD $pw$human;being$pw$"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER USER annie_edison PASSWORD 'study-room-f'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR troy SERVER remote_pg OPTIONS (user 'troy', password 'secret')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "SELECT 1; ALTER ROLE troy_barnes PASSWORD 'multi-stmt';"
    ));

    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-secret'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "ALTER USER 'abed'@'%' IDENTIFIED WITH caching_sha2_password BY 'inspector-spacetime'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "SET PASSWORD FOR 'annie'@'localhost' = 'pen-secret'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "GRANT SELECT ON greendale.* TO 'jeff'@'%' IDENTIFIED BY 'winger-secret'"
    ));

    assert!(sql_sets_credentials(
        DbEngine::Sqlite,
        "PRAGMA key = 'greendale-encryption-key'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Sqlite,
        "PRAGMA main.rekey('new-greendale-key')"
    ));

    // Queries referencing password columns, metadata toggles, or literals must NOT trigger false positives.
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "SELECT * FROM students WHERE password = 'not-a-role-command'"
    ));
    assert!(!sql_sets_credentials(DbEngine::Postgres, "ALTER ROLE troy_barnes SUPERUSER"));
    assert!(!sql_sets_credentials(DbEngine::Postgres, "ALTER ROLE troy_barnes PASSWORD NULL"));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "SELECT 'CREATE ROLE troy PASSWORD ''quoted'''"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE GROUP study_group WITH PASSWORD 'greendale-group'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER GROUP study_group PASSWORD 'greendale-group-2'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER ROLE abed_nadir PASSWORD U&'human\\002dbeing'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER ROLE annie_edison PASSWORD N'national-secret'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER USER MAPPING FOR troy SERVER remote_pg OPTIONS (SET \"password\" 'fdw-secret')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER SERVER remote_pg OPTIONS (ADD \"password\" U&'fdw\\002du')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "ALTER USER 'britta'@'localhost' IDENTIFIED BY N'mysql-n-secret'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Sqlite,
        "PRAGMA \"key\" = 'quoted-pragma-key'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Sqlite,
        "PRAGMA [key] = 'greendale-placeholder'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Sqlite,
        "PRAGMA main.'rekey' = 'greendale-placeholder'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Sqlite,
        "PRAGMA `hexkey`('abcd')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER abed_nadir /*! IDENTIFIED BY 'greendale-placeholder' */;"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "SELECT 1 /*! ; ALTER USER 'jeff' IDENTIFIED BY 'secret' */;"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER britta /*M!100401 IDENTIFIED VIA ed25519 USING PASSWORD('secret') */;"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER troy_barnes IDENTIFIED VIA mysql_native_password USING PASSWORD('greendale-placeholder');"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER annie IDENTIFIED VIA unix_socket OR mysql_native_password USING '*HASH';"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER troy_barnes IDENTIFIED WITH mysql_native_password USING PASSWORD('greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER abed_nadir IDENTIFIED VIA mysql_native_password AS PASSWORD('greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER SUBSCRIPTION greendale CONNECTION 'host=localhost password=greendale-placeholder'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE SUBSCRIPTION greendale CONNECTION 'host=localhost password=greendale-placeholder' PUBLICATION pub_all"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER SUBSCRIPTION greendale ENABLE"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (sslpassword 'greendale-placeholder')"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR troy SERVER greendale OPTIONS (user 'troy')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CHANGE REPLICATION SOURCE TO SOURCE_USER = 'troy', SOURCE_PASSWORD = 'greendale-placeholder'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CHANGE MASTER TO MASTER_USER = 'troy', MASTER_PASSWORD = 'greendale-placeholder'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "START REPLICA USER = 'troy' PASSWORD = 'greendale-placeholder'"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Mysql,
        "CHANGE REPLICATION SOURCE TO SOURCE_HOST = 'db.greendale.edu'"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Sqlite,
        "\u{feff}PRAGMA [key] = 'greendale-placeholder'"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER dean IDENTIFIED VIA unix_socket"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "ALTER GROUP study_group ADD USER troy_barnes"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Mysql,
        "ALTER USER 'jeff'@'localhost' PASSWORD EXPIRE"
    ));
    assert!(!sql_sets_credentials(DbEngine::Sqlite, "PRAGMA cipher_version"));
}

#[cfg(unix)]
#[test]
fn atomic_write_bytes_follows_symlinks_and_preserves_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = tempfile::tempdir().expect("tempdir");
    let real_dir = dir.path().join("reports");
    let link_dir = dir.path().join("aliases");
    std::fs::create_dir_all(&real_dir).expect("create reports dir");
    std::fs::create_dir_all(&link_dir).expect("create aliases dir");

    let report = real_dir.join("report.csv");
    std::fs::write(&report, b"old-report").expect("seed report");
    std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o640)).expect("chmod report");

    let alias = link_dir.join("alias.csv");
    symlink("../reports/report.csv", &alias).expect("create relative symlink");

    tv_core::atomic_write_bytes(&alias, b"updated-greendale-report").expect("atomic write through symlink");

    assert!(
        std::fs::symlink_metadata(&alias)
            .expect("alias meta")
            .file_type()
            .is_symlink(),
        "symlink must remain a symlink"
    );
    assert_eq!(
        std::fs::read_to_string(&report).expect("read updated target"),
        "updated-greendale-report"
    );
    assert_eq!(
        std::fs::symlink_metadata(&report)
            .expect("report meta")
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    let dangling_target = dir.path().join("created-target.csv");
    let dangling_link = dir.path().join("dangling.csv");
    symlink("created-target.csv", &dangling_link).expect("create dangling symlink");

    tv_core::atomic_write_bytes(&dangling_link, b"created-via-symlink").expect("write dangling symlink");
    assert!(
        std::fs::symlink_metadata(&dangling_link)
            .expect("dangling link meta")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&dangling_target).expect("read newly created target"),
        "created-via-symlink"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn atomic_write_bytes_preserves_mode_group_and_macos_acl() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::Command;

    struct ClearAclOnDrop(std::path::PathBuf);
    impl Drop for ClearAclOnDrop {
        fn drop(&mut self) {
            drop(exacl::setfacl(&[&self.0], &[], None));
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let sub = dir.path().join("inherited-parent");
    std::fs::create_dir_all(&sub).expect("create subdir");
    let _sub_cleanup = ClearAclOnDrop(sub.clone());

    let parent_acl = [exacl::AclEntry::allow_group(
        "12",
        exacl::Perm::READ,
        Some(exacl::Flag::FILE_INHERIT | exacl::Flag::DIRECTORY_INHERIT),
    )];
    exacl::setfacl(&[&sub], &parent_acl, None).expect("set parent inheritable ACL");

    let target = sub.join("greendale-export.csv");
    std::fs::write(&target, b"initial-greendale-export").expect("seed export inside inherited dir");
    let _target_cleanup = ClearAclOnDrop(target.clone());

    // Remove the parent directory ACL so the inherited ACL entry remains only on the child file.
    exacl::setfacl(&[&sub], &[], None).expect("clear parent dir ACL");

    let mut entries = exacl::getfacl(&target, None).expect("read inherited child ACL");
    assert!(
        entries.iter().any(|entry| entry.flags.contains(exacl::Flag::INHERITED)),
        "expected inherited ACL entry after clearing parent ACL: {entries:?}"
    );
    entries.insert(0, exacl::AclEntry::deny_group("12", exacl::Perm::READ, None));
    exacl::setfacl(&[&target], &entries, None).expect("add explicit deny-read ACL");
    let expected_acls = exacl::getfacl(&target, None).expect("read expected ACLs");

    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).expect("set 0640 mode");

    let initial_gid = std::fs::symlink_metadata(&target).expect("initial meta").gid();
    let id_out = Command::new("/usr/bin/id")
        .arg("-G")
        .output()
        .expect("list supplementary groups");
    assert!(id_out.status.success(), "id -G failed");
    let alt_gid = String::from_utf8_lossy(&id_out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .find(|&g| g != initial_gid);
    let expected_gid = if let Some(gid) = alt_gid {
        let chgrp_out = Command::new("/usr/bin/chgrp")
            .arg(gid.to_string())
            .arg(&target)
            .output()
            .expect("chgrp target");
        assert!(
            chgrp_out.status.success(),
            "chgrp failed: {}",
            String::from_utf8_lossy(&chgrp_out.stderr)
        );
        assert_eq!(std::fs::symlink_metadata(&target).expect("chgrp meta").gid(), gid);
        gid
    } else {
        initial_gid
    };

    tv_core::atomic_write_bytes(&target, b"updated-greendale-export").expect("atomic replace");

    let after_meta = std::fs::symlink_metadata(&target).expect("after meta");
    assert_eq!(after_meta.mode() & 0o777, 0o640);
    assert_eq!(after_meta.gid(), expected_gid);

    let after_acls = exacl::getfacl(&target, None).expect("read preserved ACLs");
    assert_eq!(after_acls, expected_acls);
    assert!(
        after_acls.iter().any(|entry| entry.flags.contains(exacl::Flag::INHERITED)),
        "expected inherited ACL flag preserved after atomic replace: {after_acls:?}"
    );

    let read_err = std::fs::read(&target).expect_err("deny-read ACL should block read");
    assert_eq!(read_err.kind(), std::io::ErrorKind::PermissionDenied);

    exacl::setfacl(&[&target], &[], None).expect("clear ACL");
    assert_eq!(
        std::fs::read_to_string(&target).expect("read replaced contents"),
        "updated-greendale-export"
    );

    let leftover_tmp = std::fs::read_dir(&sub)
        .expect("read subdir")
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains(".tableview-"));
    assert!(!leftover_tmp, "temp file left behind after atomic replace");
}

#[cfg(unix)]
#[test]
fn atomic_write_bytes_rejects_write_protected_targets() {
    use std::os::unix::fs::PermissionsExt;

    struct RestoreWritableOnDrop(std::path::PathBuf);
    impl Drop for RestoreWritableOnDrop {
        fn drop(&mut self) {
            #[cfg(target_os = "macos")]
            drop(exacl::setfacl(&[&self.0], &[], None));
            drop(std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644)));
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let readonly = dir.path().join("greendale.csv");
    std::fs::write(&readonly, b"original-greendale-csv").expect("seed readonly");
    let _guard = RestoreWritableOnDrop(readonly.clone());
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o444)).expect("chmod 0444");

    let err = tv_core::atomic_write_bytes(&readonly, b"should-not-overwrite")
        .expect_err("0444 file must reject overwrite");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read_to_string(&readonly).expect("readonly content unchanged"),
        "original-greendale-csv"
    );

    #[cfg(target_os = "macos")]
    {
        let acl_file = dir.path().join("acl-deny-write.csv");
        std::fs::write(&acl_file, b"original-acl-csv").expect("seed acl deny-write");
        let _acl_guard = RestoreWritableOnDrop(acl_file.clone());
        std::fs::set_permissions(&acl_file, std::fs::Permissions::from_mode(0o640)).expect("chmod 0640");
        exacl::setfacl(
            &[&acl_file],
            &[exacl::AclEntry::deny_group("12", exacl::Perm::WRITE, None)],
            None,
        )
        .expect("set deny-write ACL");

        let acl_err = tv_core::atomic_write_bytes(&acl_file, b"should-not-overwrite-acl")
            .expect_err("deny-write ACL must reject overwrite");
        assert_eq!(acl_err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(&acl_file).expect("acl file content unchanged"),
            "original-acl-csv"
        );
    }

    let leftover_tmp = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains(".tableview-"));
    assert!(!leftover_tmp, "temp file left behind after permission check");
}

#[test]
fn mysql_ansi_quotes_executable_comment_guardrails() {
    use tv_core::safety::{
        SafetyDecision, StatementRisk, evaluate_sql_safety_for_engine, inspect_statement_for_engine,
    };
    use tv_core::{DbEngine, EnvTag, SafetyPolicy};

    let ansi_into_hash = "SELECT 1 AS \"Troy\\\" /*! INTO @greendale */ # \"";
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, ansi_into_hash).risk,
        StatementRisk::WriteDml
    );
    assert!(matches!(
        evaluate_sql_safety_for_engine(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &[ansi_into_hash.to_owned()]
        ),
        SafetyDecision::Block { .. }
    ));

    let ansi_into_unclosed_std = "SELECT 1 AS \"Troy\\\" /*! INTO @greendale */";
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, ansi_into_unclosed_std).risk,
        StatementRisk::WriteDml
    );

    let ansi_mariadb_into_hash = "SELECT 1 AS \"Abed\\\" /*M! INTO @greendale */ # \"";
    assert!(matches!(
        evaluate_sql_safety_for_engine(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &[ansi_mariadb_into_hash.to_owned()]
        ),
        SafetyDecision::Block { .. }
    ));

    // Standard backslash-escaped double-quoted string with trailing quote is still a literal.
    assert_eq!(
        inspect_statement_for_engine(
            DbEngine::Mysql,
            "SELECT \"a\\\" /*M! DROP TABLE students */\""
        )
        .risk,
        StatementRisk::ReadOnly
    );
}

#[test]
fn postgres_unicode_nbsp_and_mid_query_bom_do_not_start_dollar_quotes() {
    use tv_core::DbEngine;
    use tv_core::sql::{split_statements_for_engine, statement_at_cursor_for_engine};

    for sep in ["\u{00A0}", "\u{feff}"] {
        let sql = format!("SELECT 1 AS greendale{sep}$tag$; SELECT 2;");
        let stmts = split_statements_for_engine(&sql, DbEngine::Postgres);
        assert_eq!(stmts.len(), 2, "failed for separator {:?}", sep);
        assert_eq!(stmts[0].sql, format!("SELECT 1 AS greendale{sep}$tag$"));
        assert_eq!(stmts[1].sql, "SELECT 2");

        let at_cur =
            statement_at_cursor_for_engine(&sql, sql.len() - 1, sql.len() - 1, DbEngine::Postgres)
                .expect("statement at SELECT 2");
        assert_eq!(at_cur.sql, "SELECT 2");

        let leading_ident = format!("SELECT 1 AS {sep}$tag$; SELECT 2;");
        let stmts_leading = split_statements_for_engine(&leading_ident, DbEngine::Postgres);
        assert_eq!(stmts_leading.len(), 2, "failed leading ident for {:?}", sep);
        assert_eq!(stmts_leading[1].sql, "SELECT 2");
    }
}

#[test]
fn credential_detection_handles_pg_unicode_idents_and_mysql_ansi_quotes() {
    use tv_core::DbEngine;
    use tv_core::sql::sql_sets_credentials;

    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass\\0077ord\" 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER study_group OPTIONS (U&\"sslpass!0077ord\" UESCAPE '!' 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER study_group OPTIONS (U&\"pass\\+000077ord\" 'greendale-placeholder')"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"ho\\0073t\" 'localhost')"
    ));

    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE USER \"Troy\\\" /*! IDENTIFIED BY 'greendale-placeholder' */ # \""
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Mysql,
        "SELECT \"Troy\\\" /*! IDENTIFIED BY 'greendale-placeholder' */\""
    ));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn atomic_write_bytes_preserves_finder_tags_and_user_xattrs() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("greendale_export.csv");
    std::fs::write(&path, b"name\nJeff Winger\n").expect("seed csv");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod 0640");

    let user_attr = "user.greendale.study_group";
    let user_val = b"Annie Edison";
    rustix::fs::setxattr(&path, user_attr, user_val, rustix::fs::XattrFlags::empty())
        .expect("set user xattr");

    #[cfg(target_os = "macos")]
    let tag_val = b"bplist00\xa1\x01_\x10\x0eGreendale\n6";
    #[cfg(target_os = "macos")]
    rustix::fs::setxattr(
        &path,
        "com.apple.metadata:_kMDItemUserTags",
        tag_val,
        rustix::fs::XattrFlags::empty(),
    )
    .expect("set macOS Finder tags xattr");

    tv_core::atomic_write_bytes(&path, b"name\nTroy Barnes\n").expect("atomic overwrite");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read rewritten"),
        "name\nTroy Barnes\n"
    );
    assert_eq!(
        std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
        0o640
    );

    let size = rustix::fs::getxattr_query(&path, user_attr).expect("query user xattr");
    let mut got_user = vec![0u8; size];
    let got_len = rustix::fs::getxattr(&path, user_attr, &mut *got_user).expect("read user xattr");
    got_user.truncate(got_len);
    assert_eq!(got_user, user_val);

    #[cfg(target_os = "macos")]
    {
        let tag_attr = "com.apple.metadata:_kMDItemUserTags";
        let tag_size = rustix::fs::getxattr_query(&path, tag_attr).expect("query Finder tag xattr");
        let mut got_tag = vec![0u8; tag_size];
        let got_tag_len =
            rustix::fs::getxattr(&path, tag_attr, &mut *got_tag).expect("read Finder tag xattr");
        got_tag.truncate(got_tag_len);
        assert_eq!(got_tag, tag_val);
    }
}

#[test]
fn mysql_guardrails_respect_default_no_backslash_and_outfile_modes() {
    use tv_core::safety::{
        SafetyDecision, StatementRisk, evaluate_sql_safety_for_engine, inspect_statement_for_engine,
    };
    use tv_core::{DbEngine, EnvTag, SafetyPolicy};

    let valid_quoted = "SELECT \"Troy \\\"Abed\\\" /*! study group */\"";
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, valid_quoted).risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        evaluate_sql_safety_for_engine(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &[valid_quoted.to_owned()]
        ),
        SafetyDecision::Allow
    );

    assert_eq!(
        inspect_statement_for_engine(
            DbEngine::Mysql,
            "SELECT \"Troy \\\" /*! study group */ \\\" Abed\""
        )
        .risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, "SELECT 'Troy\\'").risk,
        StatementRisk::ReadOnly
    );

    let no_bs_into = "SELECT 1 AS 'Troy\\' /*! INTO @greendale */ # '";
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, no_bs_into).risk,
        StatementRisk::WriteDml
    );
    assert!(matches!(
        evaluate_sql_safety_for_engine(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &[no_bs_into.to_owned()]
        ),
        SafetyDecision::Block { .. }
    ));

    let ansi_outfile = "SELECT 1 AS \"Troy\\\" /*! INTO OUTFILE '/tmp/greendale' */ # \"";
    assert_eq!(
        inspect_statement_for_engine(DbEngine::Mysql, ansi_outfile).risk,
        StatementRisk::WriteDml
    );
    assert!(matches!(
        evaluate_sql_safety_for_engine(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &[ansi_outfile.to_owned()]
        ),
        SafetyDecision::Block { .. }
    ));
}

#[test]
fn mysql_splitter_supports_no_backslash_escapes_and_default_mode() {
    use tv_core::DbEngine;
    use tv_core::sql::{split_statements_for_engine, statement_at_cursor_for_engine};

    let no_bs_sql = "SELECT 'Troy\\'; SELECT 'Abed';";
    let stmts = split_statements_for_engine(no_bs_sql, DbEngine::Mysql);
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].sql, "SELECT 'Troy\\'");
    assert_eq!(stmts[1].sql, "SELECT 'Abed'");

    assert_eq!(
        statement_at_cursor_for_engine(no_bs_sql, 0, 0, DbEngine::Mysql)
            .expect("first stmt")
            .sql,
        "SELECT 'Troy\\'"
    );
    assert_eq!(
        statement_at_cursor_for_engine(no_bs_sql, no_bs_sql.len() - 1, no_bs_sql.len() - 1, DbEngine::Mysql)
            .expect("second stmt")
            .sql,
        "SELECT 'Abed'"
    );

    let mixed_sql = "SELECT 'Troy\\' AS \"Abed\\\"; SELECT 2;";
    assert_eq!(
        split_statements_for_engine(mixed_sql, DbEngine::Mysql).len(),
        2
    );

    let default_sql = "SELECT 'Troy\\'s study group'; SELECT 'Abed';";
    let default_stmts = split_statements_for_engine(default_sql, DbEngine::Mysql);
    assert_eq!(default_stmts.len(), 2);
    assert_eq!(default_stmts[0].sql, "SELECT 'Troy\\'s study group'");
    assert_eq!(default_stmts[1].sql, "SELECT 'Abed'");
}

#[test]
fn credential_detection_handles_pg_uescape_sconst_and_mysql_no_backslash_escapes() {
    use tv_core::DbEngine;
    use tv_core::sql::sql_sets_credentials;

    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass!0077ord\" UESCAPE E'!' 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass!0077ord\" UESCAPE E'\\x21' 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass!0077ord\" UESCAPE $$!$$ 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass!0077ord\" UESCAPE $esc$!$esc$ 'greendale-placeholder')"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"ho!0073t\" UESCAPE $$!$$ 'localhost')"
    ));

    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "SELECT 'Troy\\'; CREATE USER abed IDENTIFIED BY 'greendale-placeholder'; # '"
    ));
}

#[cfg(unix)]
#[test]
fn atomic_write_bytes_rejects_fifo_symlink_to_fifo_and_directory() {
    use std::os::unix::fs::FileTypeExt;

    let dir = tempfile::tempdir().expect("create tempdir");
    let fifo_path = dir.path().join("greendale.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");

    let fifo_err = tv_core::atomic_write_bytes(&fifo_path, b"Troy and Abed")
        .expect_err("FIFO destination must fail");
    assert_eq!(fifo_err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        std::fs::symlink_metadata(&fifo_path)
            .expect("fifo metadata")
            .file_type()
            .is_fifo()
    );

    let symlink_path = dir.path().join("link_to_fifo.csv");
    std::os::unix::fs::symlink(&fifo_path, &symlink_path).expect("create symlink to fifo");
    let symlink_err = tv_core::atomic_write_bytes(&symlink_path, b"Annie Edison")
        .expect_err("symlink to FIFO must fail");
    assert_eq!(symlink_err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        std::fs::symlink_metadata(&symlink_path)
            .expect("symlink metadata")
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::symlink_metadata(&fifo_path)
            .expect("fifo metadata preserved")
            .file_type()
            .is_fifo()
    );

    let subdir = dir.path().join("greendale_dir");
    std::fs::create_dir(&subdir).expect("create subdir");
    let dir_err = tv_core::atomic_write_bytes(&subdir, b"Jeff Winger")
        .expect_err("directory destination must fail");
    assert_eq!(dir_err.kind(), std::io::ErrorKind::IsADirectory);

    let leftover_tmp = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains(".tableview-"));
    assert!(!leftover_tmp, "temp file left behind after rejecting non-regular file");
}

#[test]
fn pg_unicode_ident_uescape_decodes_octal_byte_truncation_and_utf8() {
    use tv_core::DbEngine;
    use tv_core::sql::sql_sets_credentials;

    // PostgreSQL truncates 3-digit octal escape values in E'...' to unsigned char (0441 & 0xff == 0x21 == '!').
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass!0077ord\" UESCAPE E'\\441' 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass!0077ord\" UESCAPE E'\\041' 'greendale-placeholder')"
    ));
    // LATIN1 single-byte raw escape sequences and Unicode escape forms are decoded, whereas
    // multi-byte raw escapes (E'\\302\\247') exceed PG's 1-byte UESCAPE limit and are ignored.
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass\u{00A7}0077ord\" UESCAPE E'\\247' 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass\u{00A7}0077ord\" UESCAPE E'\\xa7' 'greendale-placeholder')"
    ));
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass\u{00A7}0077ord\" UESCAPE E'\\u00a7' 'greendale-placeholder')"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"pass\u{00A7}0077ord\" UESCAPE E'\\302\\247' 'greendale-placeholder')"
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        "CREATE USER MAPPING FOR CURRENT_USER SERVER greendale OPTIONS (U&\"ho!0073t\" UESCAPE E'\\441' 'localhost')"
    ));
}

#[test]
fn mysql_session_quote_mode_resolves_ties_and_safety() {
    use tv_core::safety::{
        SafetyDecision, StatementRisk, evaluate_sql_safety_for_engine_with_mysql_mode,
        inspect_statement_for_engine_with_mysql_mode,
    };
    use tv_core::sql::{
        MysqlQuoteMode, split_statements_for_engine_with_mysql_mode,
        sql_sets_credentials_with_mysql_mode, statement_at_cursor_for_engine_with_mysql_mode,
    };
    use tv_core::{DbEngine, EnvTag, SafetyPolicy};

    assert_eq!(
        MysqlQuoteMode::from_sql_mode("STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES"),
        MysqlQuoteMode::NO_BACKSLASH_ESCAPES
    );
    assert_eq!(
        MysqlQuoteMode::from_sql_mode("REAL_AS_FLOAT,ANSI"),
        MysqlQuoteMode::ANSI_QUOTES
    );
    assert_eq!(
        MysqlQuoteMode::from_sql_mode("ANSI_QUOTES,NO_BACKSLASH_ESCAPES"),
        MysqlQuoteMode::ANSI_AND_NO_BACKSLASH
    );

    // Exact Sol R9 tie case under NO_BACKSLASH_ESCAPES: both interpretations parse cleanly without live sql_mode.
    let no_bs_tie = "SELECT 'Troy\\'; SELECT 2; -- '";
    let no_bs_stmts = split_statements_for_engine_with_mysql_mode(
        no_bs_tie,
        DbEngine::Mysql,
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES),
    );
    assert_eq!(no_bs_stmts.len(), 2);
    assert_eq!(no_bs_stmts[0].sql, "SELECT 'Troy\\'");
    assert_eq!(no_bs_stmts[1].sql, "SELECT 2");
    assert_eq!(
        statement_at_cursor_for_engine_with_mysql_mode(
            no_bs_tie,
            16,
            16,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES),
        )
        .expect("cursor on SELECT 2")
        .sql,
        "SELECT 2"
    );

    let default_stmts = split_statements_for_engine_with_mysql_mode(
        no_bs_tie,
        DbEngine::Mysql,
        Some(MysqlQuoteMode::DEFAULT),
    );
    assert_eq!(default_stmts.len(), 1);
    assert_eq!(default_stmts[0].sql, no_bs_tie);

    // Dual tie case under ANSI_QUOTES vs DEFAULT.
    let ansi_tie = "SELECT \"Troy\\\"; SELECT 2; -- \"";
    let ansi_stmts = split_statements_for_engine_with_mysql_mode(
        ansi_tie,
        DbEngine::Mysql,
        Some(MysqlQuoteMode::ANSI_QUOTES),
    );
    assert_eq!(ansi_stmts.len(), 2);
    assert_eq!(ansi_stmts[0].sql, "SELECT \"Troy\\\"");
    assert_eq!(ansi_stmts[1].sql, "SELECT 2");
    assert_eq!(
        split_statements_for_engine_with_mysql_mode(
            ansi_tie,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .len(),
        1
    );

    // Exact mode avoids false-positive blocking of valid reads while catching mode-specific writes.
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT \"Troy \\\"Abed\\\" /*! INTO @greendale */\"",
            Some(MysqlQuoteMode::DEFAULT),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT \"Troy\\\" /*! INTO @greendale */ # \"",
            Some(MysqlQuoteMode::ANSI_QUOTES),
        )
        .risk,
        StatementRisk::WriteDml
    );

    // Inline SET sql_mode resets active_mode to None for later statements in the batch.
    let inline_mode_change = vec![
        "SET SESSION sql_mode = 'ANSI_QUOTES'".to_owned(),
        "SELECT 1 AS \"Troy\\\" /*! INTO @greendale */ # \"".to_owned(),
    ];
    assert!(matches!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &inline_mode_change,
            Some(MysqlQuoteMode::DEFAULT),
        ),
        SafetyDecision::Block { .. }
    ));

    // Inline SET sql_mode + credential statement in NO_BACKSLASH_ESCAPES still blocks history persistence.
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'Troy\\'; CREATE USER abed IDENTIFIED BY 'greendale-placeholder'; # '",
        Some(MysqlQuoteMode::DEFAULT),
    ));
}

#[test]
fn mysql_inline_set_sql_mode_updates_splitting_safety_and_batch_credentials() {
    use tv_core::safety::{SafetyDecision, evaluate_sql_safety_for_engine_with_mysql_mode};
    use tv_core::sql::{
        MysqlQuoteMode, batch_sets_credentials_with_mysql_mode,
        split_statements_for_engine_with_mysql_mode, statement_at_cursor_for_engine_with_mysql_mode,
    };
    use tv_core::{DbEngine, EnvTag, SafetyPolicy};

    let inline_session = "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES';\nSELECT 'Troy\\'; SELECT 2; -- '";
    let session_stmts = split_statements_for_engine_with_mysql_mode(
        inline_session,
        DbEngine::Mysql,
        Some(MysqlQuoteMode::DEFAULT),
    );
    assert_eq!(session_stmts.len(), 3);
    assert_eq!(session_stmts[0].sql, "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'");
    assert_eq!(session_stmts[1].sql, "SELECT 'Troy\\'");
    assert_eq!(session_stmts[2].sql, "SELECT 2");

    // SET GLOBAL sql_mode affects future pooled sessions, not the active session in this batch.
    let inline_global = "SET GLOBAL sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'Troy\\'; SELECT 2; -- '";
    let global_stmts = split_statements_for_engine_with_mysql_mode(
        inline_global,
        DbEngine::Mysql,
        Some(MysqlQuoteMode::DEFAULT),
    );
    assert_eq!(global_stmts.len(), 2);

    // CONCAT(@@SESSION.sql_mode, ...) transitions the remainder cleanly.
    let concat_session =
        "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',NO_BACKSLASH_ESCAPES'); SELECT 'Troy\\'; SELECT 2; -- '";
    assert_eq!(
        split_statements_for_engine_with_mysql_mode(
            concat_session,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .len(),
        3
    );

    // Dynamic RHS (@mode) falls back cleanly for Run All while preserving exact offsets;
    // standalone Run Current resolves boundaries under the live connection's active mode.
    let dyn_session = "SET SESSION sql_mode = @mode; SELECT 'Troy\\'; SELECT 2;";
    let dyn_stmts = split_statements_for_engine_with_mysql_mode(
        dyn_session,
        DbEngine::Mysql,
        Some(MysqlQuoteMode::DEFAULT),
    );
    assert_eq!(dyn_stmts.len(), 3);
    assert_eq!(dyn_stmts[1].sql, "SELECT 'Troy\\'");
    assert_eq!(dyn_stmts[2].sql, "SELECT 2");
    assert_eq!(
        statement_at_cursor_for_engine_with_mysql_mode(
            dyn_session,
            dyn_session.len() - 1,
            dyn_session.len() - 1,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES),
        )
        .expect("cursor on SELECT 2 under live NO_BACKSLASH_ESCAPES")
        .sql,
        "SELECT 2"
    );

    // Run Current must ignore unexecuted preceding SET statements when selecting a standalone statement.
    let skipped_set_editor = "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES';\nSELECT 'Troy\\'; SELECT 2; -- '";
    let caret_in_select = skipped_set_editor.find("SELECT 2").expect("SELECT 2 offset");
    assert_eq!(
        statement_at_cursor_for_engine_with_mysql_mode(
            skipped_set_editor,
            caret_in_select,
            caret_in_select,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .expect("standalone statement under live DEFAULT mode")
        .sql,
        "SELECT 'Troy\\'; SELECT 2; -- '"
    );

    // SET GLOBAL must not weaken Read-Only safety or drop an active ANSI_QUOTES check on later statements.
    let global_then_ansi_write = vec![
        "SET GLOBAL sql_mode = ''".to_owned(),
        "SELECT 1 AS \"Troy\\\" /*! INTO @greendale */ # \"".to_owned(),
    ];
    assert!(matches!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &global_then_ansi_write,
            Some(MysqlQuoteMode::ANSI_QUOTES),
        ),
        SafetyDecision::Block { .. }
    ));

    // P1 #23: An earlier credential statement under ANSI_QUOTES must still be caught even if a later
    // statement resets GLOBAL or SESSION sql_mode to DEFAULT.
    assert!(batch_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &[
            "CREATE USER \"Troy\\\" /*! IDENTIFIED BY 'greendale-placeholder' */ # \"".to_owned(),
            "SET GLOBAL sql_mode = ''".to_owned(),
        ],
        "CREATE USER \"Troy\\\" /*! IDENTIFIED BY 'greendale-placeholder' */ # \"; SET GLOBAL sql_mode = '';",
        Some(MysqlQuoteMode::ANSI_QUOTES),
    ));
    assert!(batch_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &[
            "SET SESSION sql_mode = 'ANSI_QUOTES'".to_owned(),
            "CREATE USER \"Troy\\\" /*! IDENTIFIED BY 'greendale-placeholder' */ # \"".to_owned(),
        ],
        "SET SESSION sql_mode = 'ANSI_QUOTES'; CREATE USER \"Troy\\\" /*! IDENTIFIED BY 'greendale-placeholder' */ # \";",
        Some(MysqlQuoteMode::DEFAULT),
    ));
}

#[test]
fn mysql_group_replication_clone_mariadb_server_and_sqlite_attach_credentials_detected() {
    use tv_core::{MysqlServerVersion, StorageError, sql_sets_credentials_with_mysql_mode};

    let mysql_8_4 = MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.4.0"));

    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "START GROUP_REPLICATION USER='troy_barnes', PASSWORD='greendale-placeholder', DEFAULT_AUTH='mysql_native_password'",
    ));
    assert!(batch_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &["START GROUP_REPLICATION USER='troy_barnes', PASSWORD='greendale-placeholder'".to_owned()],
        "START GROUP_REPLICATION USER='troy_barnes', PASSWORD='greendale-placeholder'",
        Some(MysqlQuoteMode::DEFAULT),
    ));
    // Inactive executable comments before MySQL credentials must not suppress history filtering.
    for hidden_cred in [
        "/*!99999 SELECT 1 */ START GROUP_REPLICATION USER='troy', PASSWORD='greendale-placeholder'",
        "/*M! SELECT 1 */ CLONE INSTANCE FROM 'troy'@'greendale.local':3306 IDENTIFIED BY 'greendale-placeholder'",
        "/*!99999 SELECT 1 */ /*!80000 START GROUP_REPLICATION USER='troy', PASSWORD='greendale-placeholder' */",
    ] {
        assert!(
            sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, hidden_cred, Some(mysql_8_4)),
            "expected credential detection with known MySQL 8.4 version: {hidden_cred}"
        );
        assert!(
            sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, hidden_cred, None),
            "expected credential detection without known version: {hidden_cred}"
        );
    }
    assert!(!sql_sets_credentials(DbEngine::Mysql, "START GROUP_REPLICATION"));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CLONE INSTANCE FROM 'troy'@'greendale.local':3306 IDENTIFIED BY 'greendale-placeholder'",
    ));
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "CREATE OR REPLACE SERVER greendale FOREIGN DATA WRAPPER mysql OPTIONS (HOST '127.0.0.1', PASSWORD 'greendale-placeholder')",
    ));

    // SQLite ATTACH KEY expressions (blob literals, parenthesized exprs, casts, schema exprs) & URI params.
    for sqlite_secret in [
        "ATTACH DATABASE 'greendale.db' AS aux KEY 'greendale-placeholder'",
        "ATTACH DATABASE 'greendale.db' AS aux KEY x'deadbeef'",
        "ATTACH DATABASE 'greendale.db' AS aux KEY ('greendale-placeholder')",
        "ATTACH DATABASE 'greendale.db' AS aux KEY CAST('greendale-placeholder' AS BLOB)",
        "ATTACH DATABASE 'greendale.db' AS ('aux') KEY 'greendale-placeholder'",
        "ATTACH 'greendale.db' AS ('a' || 'ux') KEY x'deadbeef'",
        "ATTACH 'greendale.db' AS lower('AUX') KEY ('greendale-placeholder')",
        "ATTACH DATABASE 'greendale.db' AS key KEY 'greendale-placeholder'",
        "ATTACH 'file:greendale.db?textkey=greendale-placeholder' AS aux",
        "ATTACH DATABASE 'file:greendale.db?mode=rwc&hexkey=deadbeef' AS aux",
        "ATTACH 'file:greendale.db?key=greendale-placeholder' AS aux",
        "ATTACH 'file:greendale.db?%74extkey=greendale-placeholder' AS aux",
        "ATTACH 'file:greendale.db?' || 'textkey=greendale-placeholder' AS aux",
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Sqlite, sqlite_secret),
            "expected SQLite ATTACH credential detection for `{sqlite_secret}`"
        );
    }
    assert!(!sql_sets_credentials(
        DbEngine::Sqlite,
        "ATTACH DATABASE 'greendale.db' AS aux",
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Sqlite,
        "ATTACH DATABASE 'greendale.db' AS ('aux')",
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Sqlite,
        "ATTACH 'file:greendale.db?mode=ro&cache=shared' AS aux",
    ));
    assert!(!sql_sets_credentials(
        DbEngine::Sqlite,
        "ATTACH DATABASE 'greendale.db' AS key",
    ));

    // Ordinary comment markers must NOT exhaust the unknown-version gate budget;
    // exhausting the budget with >8 distinct real version gates must fail closed (suppress history).
    let ordinary_comment_noise = "\
        -- /*!10001 /*!10002 /*!10003 /*!10004 /*!10005 /*!10006 /*!10007 /*!10008 /*!10009\n\
         /*!99999 SELECT 1 */ /*!80000 START GROUP_REPLICATION USER='troy', PASSWORD='greendale-placeholder' */";
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        ordinary_comment_noise,
        None,
    ));

    let exhausted_budget = (1..=10)
        .map(|n| format!("/*!{} SELECT {n} */", 80000 + n))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, &exhausted_budget, None),
        "unknown-version credential check must fail closed when plausible gate budget is exhausted"
    );

    // Dangling settings symlink (direct or ancestor) must return Io error instead of defaults.
    let dir = tempdir().expect("tempdir");
    let link_path = dir.path().join("settings.json");
    std::os::unix::fs::symlink(dir.path().join("missing_target.json"), &link_path)
        .expect("create dangling settings symlink");
    assert!(matches!(
        SettingsStore::at(link_path).load(),
        Err(StorageError::Io(_))
    ));
    let dangling_dir = dir.path().join("missing_dir_link");
    std::os::unix::fs::symlink(dir.path().join("nonexistent_dir"), &dangling_dir)
        .expect("create dangling parent dir symlink");
    assert!(matches!(
        SettingsStore::at(dangling_dir.join("settings.json")).load(),
        Err(StorageError::Io(_))
    ));
}

#[test]
fn mysql_many_dynamic_sql_mode_assignments_do_not_overflow_stack() {
    let mut sql = "SET SESSION sql_mode = DEFAULT;\n".repeat(5000);
    sql.push_str("SELECT 'Troy Barnes';");

    let nested_parens_expr = format!(
        "SET SESSION sql_mode = {}'NO_BACKSLASH_ESCAPES'{}; SELECT 'Troy\\'; SELECT 2; -- '",
        "(".repeat(5000),
        ")".repeat(5000)
    );
    let nested_concat_expr = format!(
        "SET SESSION sql_mode = {}'NO_BACKSLASH_ESCAPES'{}; SELECT 'Troy Barnes';",
        "CONCAT(".repeat(5000),
        ")".repeat(5000)
    );

    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let stmts = split_statements_for_engine_with_mysql_mode(
                &sql,
                DbEngine::Mysql,
                Some(MysqlQuoteMode::DEFAULT),
            );
            assert_eq!(stmts.len(), 5001);
            assert_eq!(stmts.last().expect("trailing select").sql, "SELECT 'Troy Barnes'");
            let cur = statement_at_cursor_for_engine_with_mysql_mode(
                &sql,
                sql.len(),
                sql.len(),
                DbEngine::Mysql,
                Some(MysqlQuoteMode::DEFAULT),
            )
            .expect("statement at end cursor");
            assert_eq!(cur.sql, "SELECT 'Troy Barnes'");

            // 5,000 nested parens must evaluate without overflowing a 2 MiB stack.
            let paren_stmts = split_statements_for_engine_with_mysql_mode(
                &nested_parens_expr,
                DbEngine::Mysql,
                Some(MysqlQuoteMode::DEFAULT),
            );
            assert_eq!(paren_stmts.len(), 3);

            // 5,000 nested CONCAT calls must hit recursion cap cleanly without stack overflow.
            let concat_stmts = split_statements_for_engine_with_mysql_mode(
                &nested_concat_expr,
                DbEngine::Mysql,
                Some(MysqlQuoteMode::DEFAULT),
            );
            assert_eq!(concat_stmts.len(), 2);
        })
        .expect("spawn 2 MiB stack worker thread");
    handle
        .join()
        .expect("5000 dynamic or nested sql_mode expressions must not overflow 2 MiB stack");

    // Non-wrapping parens like `('ANSI_QUOTES') + ('NO_BACKSLASH_ESCAPES')` must not be stripped.
    let split_non_wrapping = "SET SESSION sql_mode = ('ANSI_QUOTES') + ('NO_BACKSLASH_ESCAPES'); SELECT 1;";
    assert_eq!(
        split_statements_for_engine_with_mysql_mode(
            split_non_wrapping,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .len(),
        2
    );
}

#[test]
fn mysql_set_comma_scope_inheritance_and_conditional_comments() {
    use tv_core::MysqlServerVersion;

    // Inherited GLOBAL / PERSIST modifier makes later unqualified sql_mode GLOBAL (not SESSION).
    for prefix in ["GLOBAL", "PERSIST", "PERSIST_ONLY"] {
        let sql = format!(
            "SET {prefix} max_connections = 200, sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'Troy\\'; SELECT 2; -- '"
        );
        let stmts = split_statements_for_engine_with_mysql_mode(
            &sql,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        );
        assert_eq!(
            stmts.len(),
            2,
            "unqualified sql_mode after SET {prefix} must inherit {prefix}, not SESSION"
        );
    }

    // Explicit SESSION or @@SESSION. inside a GLOBAL SET still applies to this session.
    let global_then_session =
        "SET GLOBAL max_connections = 200, SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'Troy\\'; SELECT 2; -- '";
    assert_eq!(
        split_statements_for_engine_with_mysql_mode(
            global_then_session,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .len(),
        3
    );
    let global_then_at_session =
        "SET GLOBAL max_connections = 200, @@SESSION.sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'Troy\\'; SELECT 2; -- '";
    assert_eq!(
        split_statements_for_engine_with_mysql_mode(
            global_then_at_session,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .len(),
        3
    );

    // @@GLOBAL. applies only to that variable; subsequent unqualified sql_mode stays SESSION.
    let at_global_then_unqualified =
        "SET @@GLOBAL.max_connections = 200, sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'Troy\\'; SELECT 2; -- '";
    assert_eq!(
        split_statements_for_engine_with_mysql_mode(
            at_global_then_unqualified,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .len(),
        3
    );

    // Unknown server version keeps splitter conservative while Read-Only & Production ConfirmWrites stay safe.
    for cond_comment in [
        "/*!99999 SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */",
        "/*M! SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */",
    ] {
        let split_probe = format!("{cond_comment}; SELECT 'Troy\\'; SELECT 2; -- '");
        assert_eq!(
            split_statements_for_engine_with_mysql_mode(
                &split_probe,
                DbEngine::Mysql,
                Some(MysqlQuoteMode::DEFAULT),
            )
            .len(),
            2,
            "conditional comment `{cond_comment}` must not alter splitter mode when server version is unknown"
        );

        let dangerous_batch = vec![
            cond_comment.to_owned(),
            "SELECT 'Troy\\'; DROP TABLE students; -- '".to_owned(),
        ];
        assert!(matches!(
            evaluate_sql_safety_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                SafetyPolicy::ReadOnly,
                EnvTag::Local,
                &dangerous_batch,
                Some(MysqlQuoteMode::DEFAULT),
            ),
            SafetyDecision::Block { .. }
        ));

        let cred_batch = vec![
            cond_comment.to_owned(),
            "SELECT 'Troy\\'; CREATE USER 'abed' IDENTIFIED BY 'greendale-placeholder'; -- '".to_owned(),
        ];
        assert!(batch_sets_credentials_with_mysql_mode(
            DbEngine::Mysql,
            &cred_batch,
            &cred_batch.join("; "),
            Some(MysqlQuoteMode::DEFAULT),
        ));
    }

    // P1 #20: Server-version-aware resolution on MySQL 8.4 (executes /*!80000, skips /*!99999).
    let mysql_8_4 = MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.4.0"));
    let version_gated_batch =
        "SET SESSION sql_mode = '' /*!80000 , sql_mode = 'NO_BACKSLASH_ESCAPES' */ /*!99999 , sql_mode = '' */; SELECT 'Troy\\'; DROP TABLE students; -- '";
    let stmts_8_4 =
        split_statements_for_engine_with_mysql_mode(version_gated_batch, DbEngine::Mysql, Some(mysql_8_4));
    assert_eq!(stmts_8_4.len(), 3);
    assert_eq!(stmts_8_4[2].sql, "DROP TABLE students");
    let stmt_strings_8_4: Vec<String> = stmts_8_4.iter().map(|s| s.sql.clone()).collect();
    match evaluate_sql_safety_for_engine_with_mysql_mode(
        DbEngine::Mysql,
        SafetyPolicy::ConfirmWrites,
        EnvTag::Production,
        &stmt_strings_8_4,
        Some(mysql_8_4),
    ) {
        SafetyDecision::RequireConfirmation {
            is_destructive,
            statements,
            ..
        } => {
            assert!(is_destructive, "expected destructive flag on DROP TABLE");
            assert!(
                statements.iter().any(|s| s.sql == "DROP TABLE students"),
                "expected DROP TABLE students in confirmation list, got {statements:?}"
            );
        }
        other => panic!("expected RequireConfirmation on MySQL 8.4 batch, got {other:?}"),
    }

    // Even when live server_version is unknown (Some(DEFAULT) without server_version, or None),
    // the skipped split statement must STILL trigger Production ConfirmWrites via ambiguous SET + multi-policy safety!
    let unknown_ver_stmts: Vec<String> = split_statements_for_engine_with_mysql_mode(
        version_gated_batch,
        DbEngine::Mysql,
        Some(MysqlQuoteMode::DEFAULT),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert!(matches!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::ConfirmWrites,
            EnvTag::Production,
            &unknown_ver_stmts,
            Some(MysqlQuoteMode::DEFAULT),
        ),
        SafetyDecision::RequireConfirmation {
            is_destructive: true,
            ..
        }
    ));

    // MariaDB 11.4 ignores MySQL 50700..=99999 version comments and executes /*M!100201 ... */.
    let mariadb_11_4 =
        MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));
    let mariadb_batch =
        "/*M!100201 SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */ /*!80000 , sql_mode = '' */; SELECT 'Troy\\'; DROP TABLE students; -- '";
    let mdb_stmts: Vec<String> =
        split_statements_for_engine_with_mysql_mode(mariadb_batch, DbEngine::Mysql, Some(mariadb_11_4))
            .into_iter()
            .map(|s| s.sql)
            .collect();
    assert_eq!(mdb_stmts.len(), 3);
    assert!(matches!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::ConfirmWrites,
            EnvTag::Production,
            &mdb_stmts,
            Some(mariadb_11_4),
        ),
        SafetyDecision::RequireConfirmation {
            is_destructive: true,
            ..
        }
    ));

    let uncond_comment =
        "/*! SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */; SELECT 'Troy\\'; SELECT 2; -- '";
    assert_eq!(
        split_statements_for_engine_with_mysql_mode(
            uncond_comment,
            DbEngine::Mysql,
            Some(MysqlQuoteMode::DEFAULT),
        )
        .len(),
        3
    );

    // Skipped version comments are blanked to whitespace rather than left as `/*!...*/` for sqlparser.
    let skipped_invalid_select = tv_core::inspect_statement_for_engine_with_mysql_mode(
        DbEngine::Mysql,
        "SELECT 1 /*!99999 invalid */",
        Some(mysql_8_4),
    );
    assert_eq!(skipped_invalid_select.risk, StatementRisk::ReadOnly);
    assert!(skipped_invalid_select.parsed);

    let skipped_then_drop = tv_core::inspect_statement_for_engine_with_mysql_mode(
        DbEngine::Mysql,
        "/*!99999 invalid */ DROP TABLE students",
        Some(mysql_8_4),
    );
    assert_eq!(skipped_then_drop.risk, StatementRisk::Destructive);
    assert!(skipped_then_drop.parsed);

    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*!99999 DROP TABLE students */",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::ReadOnly
    );

    // Server-specific version-gate lexers (MySQL 8.0 vs MySQL 8.4 vs MariaDB 11.4).
    let mysql_8_0 =
        MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.0.36"));
    // MySQL 8.0 consumes only 5 digits (`08040` <= `80036`), leaving leading `0` or `1`.
    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*!80036 DROP TABLE students */",
            Some(mysql_8_0),
        )
        .risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*!80100 DROP TABLE students */",
            Some(mysql_8_0),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    // In MySQL 8.0, `/*!800001 SELECT 1 */` consumes 5 digits (`80000`), leaving `1 SELECT 1` (not skipped).
    assert!(!tv_core::inspect_statement_for_engine_with_mysql_mode(
        DbEngine::Mysql,
        "/*!800001 SELECT 1 */",
        Some(mysql_8_0),
    )
    .parsed);

    // MySQL 8.4 consumes 6 digits iff followed by whitespace.
    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*!080400 DROP TABLE students */",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*!090000 DROP TABLE students */",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    // Without whitespace after the 6th digit, MySQL 8.4 falls back to 5 digits (`09000` <= `80400`), NOT skipped!
    let my84_no_ws = tv_core::inspect_statement_for_engine_with_mysql_mode(
        DbEngine::Mysql,
        "/*!090000DROP TABLE students */",
        Some(mysql_8_4),
    );
    assert!(!my84_no_ws.parsed);

    // MariaDB consumes 6 digits even without trailing whitespace or when a 7th digit follows.
    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*M!110402DROP TABLE students */",
            Some(mariadb_11_4),
        )
        .risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*M!110500DROP TABLE students */",
            Some(mariadb_11_4),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        tv_core::inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*M!9999991 SELECT 1 */",
            Some(mariadb_11_4),
        )
        .risk,
        StatementRisk::ReadOnly
    );
}

#[test]
fn mysql_round14_plausible_gates_nested_comments_and_production_exhaustion() {
    use tv_core::safety::{SafetyDecision, evaluate_sql_safety_for_engine_with_mysql_mode};
    use tv_core::{
        MysqlQuoteMode, MysqlServerVersion, inspect_statement_for_engine_with_mysql_mode,
        split_statements_for_engine_with_mysql_mode, sql_sets_credentials_with_mysql_mode,
    };

    let mysql_8_4 =
        MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.4.0"));
    let mariadb_11_4 = MysqlQuoteMode::DEFAULT
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    // 1. Quotes or line comments inside skipped gates must not hide later destructive/credential gates.
    for hidden_sql in [
        "/*!99999 ' */ /*!80000 DROP TABLE students */",
        "/*!80000 ' */ /*!100201 DROP TABLE students */",
        "/*!80000 -- */ /*!100201 DROP TABLE students */",
        "/*!80000 ' */ /*!100201DROP TABLE students */",
    ] {
        for mode in [None, Some(MysqlQuoteMode::DEFAULT)] {
            assert_eq!(
                inspect_statement_for_engine_with_mysql_mode(DbEngine::Mysql, hidden_sql, mode).risk,
                StatementRisk::Destructive,
                "expected Destructive for `{hidden_sql}` under {mode:?}"
            );
        }
    }
    for hidden_cred in [
        "/*!99999 ' */ /*!80000 CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder' */",
        "/*!80000 ' */ /*!100201 CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder' */",
    ] {
        for mode in [None, Some(MysqlQuoteMode::DEFAULT)] {
            assert!(
                sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, hidden_cred, mode),
                "expected credential suppression for `{hidden_cred}` under {mode:?}"
            );
        }
    }

    // 2. Five MariaDB-only gates count only under MariaDb lexer (within 8-gate budget);
    // nine real MariaDB-only gates exhaust the budget and fail closed (Production destructive).
    let five_mariadb_benign = format!(
        "SELECT 1 {}",
        (1..=5)
            .map(|n| format!("/*M!{} */", 100_200 + n))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(DbEngine::Mysql, &five_mariadb_benign, None).risk,
        StatementRisk::ReadOnly
    );
    assert!(!sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &five_mariadb_benign,
        None
    ));

    let nine_mariadb = format!(
        "SELECT 1 {}",
        (1..=9)
            .map(|n| format!("/*M!{} */", 100_200 + n))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &nine_mariadb,
        None
    ));
    for policy in [SafetyPolicy::Unrestricted, SafetyPolicy::ConfirmWrites] {
        assert!(
            matches!(
                evaluate_sql_safety_for_engine_with_mysql_mode(
                    DbEngine::Mysql,
                    policy,
                    EnvTag::Production,
                    std::slice::from_ref(&nine_mariadb),
                    Some(MysqlQuoteMode::DEFAULT),
                ),
                SafetyDecision::RequireConfirmation {
                    is_destructive: true,
                    ..
                }
            ),
            "expected Production destructive confirmation under {policy:?} when gate budget is exhausted"
        );
    }
    assert!(matches!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::ReadOnly,
            EnvTag::Local,
            &[nine_mariadb],
            None,
        ),
        SafetyDecision::Block { .. }
    ));

    // 3. Inactive MySQL version comments may contain one nested /* ... */ block (MySQL consume_comment(1)),
    // whereas MariaDB stops at the first */.
    for valid_mysql_nested in [
        "SELECT /*!99999 /* nested */ 1 */ 2",
        "/*!99999 /* nested */ 1 */ SELECT 1",
        "/*!99999 /* a */ /* b */ */ SELECT 1",
        "/*!99999 /*/*/ 1 */ SELECT 1",
    ] {
        for mode in [Some(mysql_8_4), None] {
            let insp = inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                valid_mysql_nested,
                mode,
            );
            assert_eq!(
                insp.risk, StatementRisk::ReadOnly,
                "expected ReadOnly for `{valid_mysql_nested}` under {mode:?}, got {insp:?}"
            );
            assert!(insp.parsed);
        }
    }

    let split_nested = split_statements_for_engine_with_mysql_mode(
        "SELECT /*!99999 /* ; */ 1 */ 2; SELECT 3;",
        DbEngine::Mysql,
        Some(mysql_8_4),
    );
    assert_eq!(split_nested.len(), 2);
    assert_eq!(split_nested[0].sql, "SELECT /*!99999 /* ; */ 1 */ 2");
    assert_eq!(split_nested[1].sql, "SELECT 3");

    let nested_before_cred =
        "/*!99999 /* nested */ ' */ CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder'";
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        nested_before_cred,
        Some(mysql_8_4),
    ));
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        nested_before_cred,
        None,
    ));

    // Both MySQL and MariaDB permit one nested /* ... */ block inside inactive versioned comments
    // (`/*!NNNNN` on both; `/*M!NNNNNN` on MariaDB).
    let mariadb_nested_ok = "SELECT /*M!999999 /* nested */ 1 */ 2";
    for mode in [Some(mariadb_11_4), None] {
        let insp = inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            mariadb_nested_ok,
            mode,
        );
        assert_eq!(insp.risk, StatementRisk::ReadOnly);
        assert!(insp.parsed);
    }
    let mariadb_split = split_statements_for_engine_with_mysql_mode(
        "SELECT /*M!999999 /* ; */ 1 */ 2; SELECT 3;",
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(mariadb_split.len(), 2);
    assert_eq!(mariadb_split[0].sql, "SELECT /*M!999999 /* ; */ 1 */ 2");
    assert_eq!(mariadb_split[1].sql, "SELECT 3");

    let mariadb_nested_before_cred =
        "/*M!999999 /* nested */ ' */ CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder'";
    for mode in [Some(mariadb_11_4), None, Some(MysqlQuoteMode::DEFAULT)] {
        assert!(
            sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, mariadb_nested_before_cred, mode),
            "expected MariaDB inactive /*M! nested block not to hide CREATE USER under {mode:?}"
        );
    }

    // Double-nested `/* /* inner */ */` breaks out after one nested block in both MySQL and MariaDB.
    let double_nested_breakout = "SELECT 1 /*!99999 /* /* inner */ */ ; DROP TABLE students -- */";
    for mode in [
        Some(mysql_8_4),
        Some(mariadb_11_4),
        None,
        Some(MysqlQuoteMode::DEFAULT),
    ] {
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                double_nested_breakout,
                mode
            )
            .risk,
            StatementRisk::Destructive,
            "expected Destructive double-nested breakout under {mode:?}"
        );
    }

    // MySQL treats /*M! as a plain flat /* comment, so /*/ or /* */ exposes `; DROP TABLE students`.
    let mysql_mariadb_breakout_stmts = split_statements_for_engine_with_mysql_mode(
        "SELECT 1 /*M! /* */ ; DROP TABLE students -- */",
        DbEngine::Mysql,
        Some(mysql_8_4),
    );
    assert_eq!(mysql_mariadb_breakout_stmts.len(), 2);
    assert_eq!(mysql_mariadb_breakout_stmts[1].sql, "DROP TABLE students -- */");
}

#[test]
fn mysql_round15_mixed_gates_single_stmt_enforcement_and_mariadb_mssql_mode() {
    use tv_core::{
        MysqlQuoteMode, MysqlServerVersion, inspect_statement_for_engine_with_mysql_mode,
        mysql_slice_has_multiple_statements, split_statements_for_engine_with_mysql_mode,
        sql_sets_credentials_with_mysql_mode, validate_mysql_single_statement_batch,
    };

    let mysql_8_4 =
        MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.4.0"));
    let mariadb_11_4_mssql = MysqlQuoteMode::MSSQL
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));
    let mariadb_11_4 = MysqlQuoteMode::DEFAULT
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    // 1. Mixed active + inactive gate discovery across reachable version ranges.
    for mixed_write in [
        "SELECT /*!80000 '*/' */ /*!99999 -- */ /*!80400 INTO @greendale */",
        "SELECT /*!80200 '*/' */ /*!99999 -- */ /*!80400 INTO @greendale */",
        "SELECT /*M!100200 '*/' */ /*M!119999 -- */ /*M!110400 INTO @greendale */",
    ] {
        for mode in [None, Some(MysqlQuoteMode::DEFAULT)] {
            assert_eq!(
                inspect_statement_for_engine_with_mysql_mode(DbEngine::Mysql, mixed_write, mode).risk,
                StatementRisk::WriteDml,
                "expected mixed-gate WriteDml for `{mixed_write}` under {mode:?}"
            );
        }
    }
    for mixed_cred in [
        "SELECT /*!80200 '*/' ; */ /*!99999 -- */ /*!80400 CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder' */",
        "SELECT /*!80200 '*/' */ /*!99999 -- */ /*!80400 ; CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder' */",
        "SELECT /*M!100200 '*/' ; */ /*M!119999 -- */ /*M!110400 CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder' */",
        "SELECT /*M!100200 '*/' */ /*M!119999 -- */ /*M!110400 ; CREATE USER 'troy' IDENTIFIED BY 'greendale-placeholder' */",
    ] {
        assert!(
            sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, mixed_cred, None),
            "expected mixed-gate credential detection for `{mixed_cred}`"
        );
    }
    let chained_nine = (0..9)
        .map(|n| format!("/*!{} '*/' */", 80_200 + n))
        .collect::<Vec<_>>()
        .join(" ");
    let chained_over_budget = format!("SELECT 1 {chained_nine}");
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(DbEngine::Mysql, &chained_over_budget, None)
            .risk,
        StatementRisk::Destructive
    );

    // 2. Multi-statement unparsed fallback & single-statement enforcement.
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SHOW TABLES /*! ; DROP TABLE students */",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SHOW CREATE USER 'troy' /*! ; RENAME TABLE a TO b */",
            None,
        )
        .risk,
        StatementRisk::WriteDdl
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SHOW CREATE USER 'troy' /*! ; CALL unknown_write() */",
            None,
        )
        .risk,
        StatementRisk::WriteDml
    );
    assert!(mysql_slice_has_multiple_statements(
        "SELECT 1 /*! ; DROP TABLE students */",
        Some(mysql_8_4)
    ));
    assert!(mysql_slice_has_multiple_statements(
        "SHOW TABLES /*! ; DROP TABLE students */",
        None
    ));
    assert!(!mysql_slice_has_multiple_statements("SELECT 1;", Some(mysql_8_4)));
    assert!(!mysql_slice_has_multiple_statements(
        "SELECT 1 /*!99999 ; DROP TABLE students */",
        Some(mariadb_11_4)
    ));
    assert!(validate_mysql_single_statement_batch(
        &[
            "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'".to_owned(),
            "SELECT 'Troy\\' /*! ; DROP TABLE students */".to_owned(),
        ],
        Some(mysql_8_4),
    )
    .is_err());

    // 3. MariaDB MSSQL mode ([bracket] identifiers with ]] escaping).
    let mssql_split = split_statements_for_engine_with_mysql_mode(
        "SET SESSION sql_mode = 'MSSQL'; SELECT [a]]b;c] FROM [t]; SELECT 2;",
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(mssql_split.len(), 3);
    assert_eq!(mssql_split[1].sql, "SELECT [a]]b;c] FROM [t]");
    assert_eq!(mssql_split[2].sql, "SELECT 2");

    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1 AS [a' ] /*! INTO @greendale */ # '",
            Some(mariadb_11_4_mssql),
        )
        .risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1 AS [a' ] /*! INTO @greendale */ # '",
            None,
        )
        .risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1 AS [Troy; /*M! INTO @greendale */]",
            Some(mariadb_11_4_mssql),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    assert!(!mysql_slice_has_multiple_statements(
        "SELECT [a]]b;c] FROM [t];",
        Some(mariadb_11_4_mssql)
    ));
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        "CREATE USER [Troy'] /*! IDENTIFIED BY 'greendale-placeholder' */ # '",
        None
    ));
}

#[test]
fn mysql_round16_dash_unicode_trailing_quote_and_mariadb_wsrep_99997() {
    use tv_core::{
        MysqlQuoteMode, MysqlServerVersion, inspect_statement_for_engine_with_mysql_mode,
        mysql_slice_has_multiple_statements, split_statements_for_engine_with_mysql_mode,
        sql_sets_credentials_with_mysql_mode, validate_mysql_single_statement_batch,
    };

    let mysql_8_4 =
        MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.4.0"));
    let mariadb_11_4 = MysqlQuoteMode::DEFAULT
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    // 1. MySQL `--` comment detection uses ASCII whitespace/control only (non-ASCII UTF-8 bytes like NBSP/NEL do not start a comment).
    for sep in ["\u{00a0}", "\u{0085}"] {
        let multi_drop = format!("SELECT 1--{sep}; DROP TABLE students");
        let stmts_84 =
            split_statements_for_engine_with_mysql_mode(&multi_drop, DbEngine::Mysql, Some(mysql_8_4));
        assert_eq!(stmts_84.len(), 2, "failed split under 8.4 for sep {sep:?}");
        let stmts_none =
            split_statements_for_engine_with_mysql_mode(&multi_drop, DbEngine::Mysql, None);
        assert_eq!(stmts_none.len(), 2, "failed split under None for sep {sep:?}");
        assert!(mysql_slice_has_multiple_statements(&multi_drop, Some(mysql_8_4)));
        assert!(mysql_slice_has_multiple_statements(&multi_drop, None));
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                &multi_drop,
                Some(mysql_8_4)
            )
            .risk,
            StatementRisk::Destructive
        );
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(DbEngine::Mysql, &multi_drop, None).risk,
            StatementRisk::Destructive
        );

        let multi_cred = format!(
            "SELECT 1--{sep}; CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'"
        );
        assert!(sql_sets_credentials_with_mysql_mode(
            DbEngine::Mysql,
            &multi_cred,
            Some(mysql_8_4)
        ));
        assert!(sql_sets_credentials_with_mysql_mode(
            DbEngine::Mysql,
            &multi_cred,
            None
        ));
    }

    for ascii_sep in ["\x0b", "\x7f", " "] {
        let single = format!("SELECT 1--{ascii_sep}; DROP TABLE students");
        let stmts =
            split_statements_for_engine_with_mysql_mode(&single, DbEngine::Mysql, Some(mysql_8_4));
        assert_eq!(stmts.len(), 1);
        assert!(!mysql_slice_has_multiple_statements(&single, Some(mysql_8_4)));
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(DbEngine::Mysql, &single, Some(mysql_8_4))
                .risk,
            StatementRisk::ReadOnly
        );
    }

    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1---\u{00a0}; DROP TABLE students",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1---\u{00a0}; DROP TABLE students",
            None,
        )
        .risk,
        StatementRisk::Destructive
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1--\u{00a0}1 INTO @greendale",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1--\u{00a0}1 INTO @greendale",
            None,
        )
        .risk,
        StatementRisk::WriteDml
    );
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1-- \r DROP TABLE students",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::ReadOnly
    );

    // 2. Trailing unterminated quote must not disable single-statement validation or safety inspection.
    for slice in [
        "SELECT 1; SELECT 2; 'unterminated",
        "SELECT 1 /*! ; DROP TABLE students; 'unterminated */",
        "SELECT 1; 'unterminated",
        "SELECT 1 /*!80400 ; DROP TABLE students; 'unterminated */",
    ] {
        assert!(
            mysql_slice_has_multiple_statements(slice, Some(mysql_8_4)),
            "expected multi-statement for {slice:?} under MySQL 8.4"
        );
        assert!(
            mysql_slice_has_multiple_statements(slice, None),
            "expected multi-statement for {slice:?} under unknown mode"
        );
        assert!(
            validate_mysql_single_statement_batch(&[slice.to_owned()], Some(mysql_8_4)).is_err()
        );
    }

    let trailing_quote_drop = "SELECT 1 /*!80400 ; DROP TABLE students; 'unterminated */";
    for mode in [Some(mysql_8_4), Some(MysqlQuoteMode::DEFAULT), None] {
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                trailing_quote_drop,
                mode,
            )
            .risk,
            StatementRisk::Destructive
        );
    }

    assert!(!mysql_slice_has_multiple_statements(
        "SELECT 'a;b",
        Some(mysql_8_4)
    ));
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 'Troy\\'; SELECT 2'",
            None,
        )
        .risk,
        StatementRisk::ReadOnly
    );

    // 3. MariaDB Galera WSREP 99997 (and 6-digit 099997) local executable comments.
    for mode in [Some(mariadb_11_4), None] {
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                "SELECT 1 /*!99997 INTO @greendale */",
                mode,
            )
            .risk,
            StatementRisk::WriteDml
        );
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                "/*!99997 DROP TABLE students */",
                mode,
            )
            .risk,
            StatementRisk::Destructive
        );
        assert!(mysql_slice_has_multiple_statements(
            "SELECT 1 /*!99997 ; DROP TABLE students */",
            mode
        ));
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                "SELECT 1 /*!99997 ; DROP TABLE students */",
                mode,
            )
            .risk,
            StatementRisk::Destructive
        );
        assert!(mysql_slice_has_multiple_statements(
            "SELECT 1 /*!099997 ; DROP TABLE students */",
            mode
        ));
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                "SELECT 1 /*!099997 ; DROP TABLE students */",
                mode,
            )
            .risk,
            StatementRisk::Destructive
        );
        assert!(sql_sets_credentials_with_mysql_mode(
            DbEngine::Mysql,
            "/*!99997 CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder' */",
            mode
        ));
        assert!(sql_sets_credentials_with_mysql_mode(
            DbEngine::Mysql,
            "/*M!100000 SELECT 1; */ /*!99997 CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder' */",
            mode
        ));
    }

    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "/*!99997 /*M!100500 DROP TABLE students */ */",
            None,
        )
        .risk,
        StatementRisk::Destructive
    );
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "/*!99997 SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */".to_owned(),
                "SELECT 'Troy\\' /*! ; DROP TABLE students */".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_err()
    );
    assert!(batch_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &[
            "/*!99997 SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */".to_owned(),
            "SELECT 'Troy\\'; CREATE USER 'abed'@'localhost' IDENTIFIED BY 'greendale-placeholder'"
                .to_owned(),
        ],
        "/*!99997 SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES' */; SELECT 'Troy\\'; CREATE USER 'abed'@'localhost' IDENTIFIED BY 'greendale-placeholder'",
        Some(mariadb_11_4),
    ));

    // Non-regression: 99997 stays inactive on MySQL 8.4 and benign /*!99997 */ stays ReadOnly on MariaDB.
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1 /*!99997 ; DROP TABLE students */",
            Some(mysql_8_4),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    assert!(!mysql_slice_has_multiple_statements(
        "SELECT 1 /*!99997 ; DROP TABLE students */",
        Some(mysql_8_4)
    ));
    assert_eq!(
        inspect_statement_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            "SELECT 1 /*!99997 */",
            Some(mariadb_11_4),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    assert!(!mysql_slice_has_multiple_statements(
        "SELECT 1 /*!99997 */",
        Some(mariadb_11_4)
    ));
}

#[test]
fn mysql_round17_execute_drift_nbsp_trim_set_statement_and_pg_primary_conninfo() {
    use tv_core::{
        MysqlQuoteMode, MysqlServerVersion, evaluate_sql_safety_for_engine_with_mysql_mode,
        inspect_statement_for_engine_with_mysql_mode, mysql_slice_has_multiple_statements,
        split_statements_for_engine_with_mysql_mode, sql_sets_credentials_with_mysql_mode,
        trim_sql_whitespace_for_engine, validate_mysql_single_statement_batch,
    };

    let mysql_8_4 =
        MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.4.0"));
    let mariadb_11_4 = MysqlQuoteMode::DEFAULT
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    // 1. Mid-batch EXECUTE / EXECUTE IMMEDIATE / CALL / compound block mode invalidation.
    let prepare_stmt =
        "PREPARE s FROM CONCAT('SET SESSION sql_', 'mode = ''NO_BACKSLASH_ESCAPES''')".to_owned();
    let drop_slice = "SELECT 'Troy\\'; DROP TABLE students; '".to_owned();
    assert_eq!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::Unrestricted,
            EnvTag::Production,
            &[prepare_stmt.clone(), "EXECUTE s".to_owned(), drop_slice.clone()],
            Some(mysql_8_4),
        ),
        SafetyDecision::RequireConfirmation {
            policy: SafetyPolicy::Unrestricted,
            env: EnvTag::Production,
            reason: "Production safeguard: destructive SQL requires explicit confirmation"
                .to_owned(),
            statements: vec![
                inspect_statement_for_engine_with_mysql_mode(
                    DbEngine::Mysql,
                    &prepare_stmt,
                    Some(mysql_8_4),
                ),
                inspect_statement_for_engine_with_mysql_mode(
                    DbEngine::Mysql,
                    "EXECUTE s",
                    Some(mysql_8_4),
                ),
                inspect_statement_for_engine_with_mysql_mode(
                    DbEngine::Mysql,
                    &drop_slice,
                    Some(
                        MysqlQuoteMode::NO_BACKSLASH_ESCAPES
                            .with_server_version(mysql_8_4.server_version),
                    ),
                ),
            ],
            is_destructive: true,
        }
    );
    assert!(
        validate_mysql_single_statement_batch(
            &[
                prepare_stmt.clone(),
                "EXECUTE s".to_owned(),
                "SELECT 'Troy\\'; DROP TABLE students".to_owned(),
            ],
            Some(mysql_8_4),
        )
        .is_err()
    );
    let exec_cred_batch = "PREPARE s FROM CONCAT('SET SESSION sql_', 'mode = ''NO_BACKSLASH_ESCAPES'''); EXECUTE s; SELECT 'Troy\\'; CREATE USER 'abed'@'localhost' IDENTIFIED BY 'greendale-placeholder'";
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        exec_cred_batch,
        Some(mysql_8_4)
    ));
    assert!(batch_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &[
            prepare_stmt,
            "EXECUTE s".to_owned(),
            "SELECT 'Troy\\'; CREATE USER 'abed'@'localhost' IDENTIFIED BY 'greendale-placeholder'".to_owned(),
        ],
        exec_cred_batch,
        Some(mysql_8_4),
    ));
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "SET STATEMENT max_statement_time=10 FOR EXECUTE s".to_owned(),
                "SELECT 'Troy\\'; DROP TABLE students".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_err()
    );
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "EXECUTE IMMEDIATE 'SET SESSION sql_mode = ''NO_BACKSLASH_ESCAPES'''".to_owned(),
                "SELECT 'Troy\\'; DROP TABLE students".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_err()
    );
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "CALL greendale_set_mode()".to_owned(),
                "SELECT 'Troy\\'; DROP TABLE students".to_owned(),
            ],
            Some(mysql_8_4),
        )
        .is_err()
    );
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "BEGIN NOT ATOMIC EXECUTE IMMEDIATE 'SET sql_mode=''NO_BACKSLASH_ESCAPES'''; END"
                    .to_owned(),
                "SELECT 'Troy'".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_ok()
    );
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "BEGIN NOT ATOMIC EXECUTE IMMEDIATE 'SET sql_mode=''NO_BACKSLASH_ESCAPES'''; END"
                    .to_owned(),
                "SELECT 'Troy\\'; DROP TABLE students".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_err()
    );
    // Non-regression: column alias `execute` stays single-statement under MySQL DEFAULT.
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "SELECT 1 AS execute".to_owned(),
                "SELECT 'Troy\\' AS ok".to_owned(),
            ],
            Some(mysql_8_4),
        )
        .is_ok()
    );
    assert!(!mysql_slice_has_multiple_statements("SELECT 1 AS execute", Some(mysql_8_4)));

    // 2. Leading NBSP whitespace trimming consistency between scanners and executors.
    assert_eq!(
        trim_sql_whitespace_for_engine(DbEngine::Postgres, " \t\n\u{00a0}CREATE USER troy "),
        "\u{00a0}CREATE USER troy"
    );
    assert_eq!(
        trim_sql_whitespace_for_engine(DbEngine::Mysql, " \t\n\u{00a0}CREATE USER troy "),
        "\u{00a0}CREATE USER troy"
    );
    let pg_nbsp_sql = "SELECT 1;\u{00a0}CREATE USER troy WITH PASSWORD 'greendale-placeholder'";
    let pg_nbsp_stmts: Vec<String> =
        split_statements_for_engine_with_mysql_mode(pg_nbsp_sql, DbEngine::Postgres, None)
            .into_iter()
            .map(|s| s.sql)
            .collect();
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Postgres,
        pg_nbsp_sql,
        None
    ));
    assert!(batch_sets_credentials_with_mysql_mode(
        DbEngine::Postgres,
        &pg_nbsp_stmts,
        pg_nbsp_sql,
        None,
    ));
    let my_nbsp_sql =
        "SELECT 1;\u{00a0}CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'";
    let my_nbsp_stmts: Vec<String> =
        split_statements_for_engine_with_mysql_mode(my_nbsp_sql, DbEngine::Mysql, Some(mysql_8_4))
            .into_iter()
            .map(|s| s.sql)
            .collect();
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        my_nbsp_sql,
        Some(mysql_8_4)
    ));
    assert!(batch_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &my_nbsp_stmts,
        my_nbsp_sql,
        Some(mysql_8_4),
    ));

    // 3. MariaDB SET STATEMENT ... FOR unwrapping in safety and credential detection.
    for wrapped_drop in [
        "SET STATEMENT max_statement_time=10 FOR DROP TABLE students",
        "SET STATEMENT lc_messages='for', max_statement_time=10 FOR DELETE FROM students",
        "SET STATEMENT a=(SELECT 1 FROM t FOR UPDATE) FOR TRUNCATE TABLE students",
        "SET STATEMENT a=1 FOR SET STATEMENT b=2 FOR DROP TABLE students",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert_eq!(
                inspect_statement_for_engine_with_mysql_mode(DbEngine::Mysql, wrapped_drop, mode)
                    .risk,
                StatementRisk::Destructive,
                "failed for {wrapped_drop:?} under {mode:?}"
            );
            assert!(matches!(
                evaluate_sql_safety_for_engine_with_mysql_mode(
                    DbEngine::Mysql,
                    SafetyPolicy::Unrestricted,
                    EnvTag::Production,
                    &[wrapped_drop.to_owned()],
                    mode,
                ),
                SafetyDecision::RequireConfirmation {
                    is_destructive: true,
                    ..
                }
            ));
        }
    }
    for wrapped_cred in [
        "SET STATEMENT max_statement_time=10 FOR CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'",
        "SET STATEMENT lc_messages='for' FOR SET PASSWORD FOR 'troy'@'localhost' = PASSWORD('greendale-placeholder')",
        "SET STATEMENT a=1 FOR SET STATEMENT b=2 FOR GRANT ALL ON *.* TO 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'",
    ] {
        assert!(sql_sets_credentials_with_mysql_mode(
            DbEngine::Mysql,
            wrapped_cred,
            Some(mariadb_11_4)
        ));
        assert!(sql_sets_credentials_with_mysql_mode(
            DbEngine::Mysql,
            wrapped_cred,
            None
        ));
    }
    assert!(!sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        "SET STATEMENT max_statement_time=10 FOR SELECT 1",
        Some(mariadb_11_4)
    ));
    // SET STATEMENT sql_mode=... FOR <single stmt> is statement-scoped and must not leak NO_BACKSLASH_ESCAPES.
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "SET STATEMENT max_statement_time=10, sql_mode='NO_BACKSLASH_ESCAPES' FOR SELECT 1"
                    .to_owned(),
                "SELECT 'Troy\\'; SELECT 2'".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_ok()
    );

    // 4. PostgreSQL replication / credential GUCs (including double-quoted and bare values).
    for pg_secret_guc in [
        "ALTER SYSTEM SET primary_conninfo = 'host=greendale.internal user=troy password=humanbeing'",
        "ALTER SYSTEM SET primary_conninfo TO 'postgresql://troy:humanbeing@greendale.internal/study_room'",
        "ALTER SYSTEM SET \"primary_conninfo\" = $$host=greendale password=secret$$",
        "ALTER SYSTEM SET primary_conninfo = \"host=db.greendale.edu password=greendale-placeholder\"",
        "ALTER SYSTEM SET primary_conninfo TO \"host=db.greendale.edu password=greendale-placeholder\"",
        "ALTER SYSTEM SET primary_conninfo = U&\"host=db password=greendale-placeholder\"",
        "SET app.password = greendale_secret",
        "ALTER SYSTEM SET U&\"\\0070rimary_conninfo\" = 'host=greendale password=secret'",
        "ALTER SYSTEM SET ssl_passphrase_command = 'echo greendale-passphrase'",
        "ALTER ROLE troy SET myext.api_password = 'greendale-placeholder'",
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Postgres, pg_secret_guc),
            "expected credential detection for {pg_secret_guc}"
        );
    }
    for pg_safe_guc in [
        "ALTER SYSTEM SET primary_conninfo = DEFAULT",
        "ALTER ROLE troy SET app.password FROM CURRENT",
        "ALTER SYSTEM RESET primary_conninfo",
        "ALTER SYSTEM SET password_encryption = 'scram-sha-256'",
        "ALTER SYSTEM SET shared_buffers = '4GB'",
    ] {
        assert!(
            !sql_sets_credentials(DbEngine::Postgres, pg_safe_guc),
            "did not expect credential flag for {pg_safe_guc}"
        );
    }

    // 5. MySQL/MariaDB prepared, immediate, SET @var, and compound-block credential detection (R18-2).
    // "CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'"
    let hex_create_user = "4352454154452055534552202774726f792740276c6f63616c686f737427204944454e5449464945442042592027677265656e64616c652d706c616365686f6c64657227";
    let hex_lit_stmt = format!("PREPARE s FROM X'{hex_create_user}'");
    let mysql_prepared_secrets = [
        "PREPARE s FROM 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''",
        "PREPARE s FROM \"CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'\"",
        "PREPARE s FROM N'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''",
        "PREPARE s FROM 'CREATE USER\\n''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''",
        "EXECUTE IMMEDIATE 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''",
        "EXECUTE IMMEDIATE CONCAT('CREATE USER ', @u, ' IDENTIFIED BY ''greendale-placeholder''')",
        "EXECUTE IMMEDIATE 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ?' USING 'greendale-placeholder'",
        "SET @q = 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''",
        "BEGIN NOT ATOMIC CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'; END",
        "BEGIN NOT ATOMIC IF 1 THEN SET @q = 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''; END IF; END",
        &hex_lit_stmt,
    ];
    for secret_sql in mysql_prepared_secrets {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, secret_sql, mode),
                "expected prepared/compound credential detection for {secret_sql} under {mode:?}"
            );
        }
    }
    for safe_sql in [
        "PREPARE s FROM 'SELECT * FROM students WHERE password = ''x'''",
        "EXECUTE IMMEDIATE 'SELECT ? FROM students' USING 'greendale-placeholder'",
        "EXECUTE IMMEDIATE 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ?' USING @pw",
        "SET @q = 'SELECT 1'",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                !sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, safe_sql, mode),
                "did not expect credential flag for {safe_sql} under {mode:?}"
            );
        }
    }

    // 6. MariaDB compound statement splitting & safety inspection (R18-4).
    let block_and_select = split_statements_for_engine_with_mysql_mode(
        "BEGIN NOT ATOMIC SELECT 1; END; SELECT 2;",
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(block_and_select.len(), 2);
    assert_eq!(block_and_select[0].sql, "BEGIN NOT ATOMIC SELECT 1; END");
    assert_eq!(block_and_select[1].sql, "SELECT 2");

    let complex_block = "lbl: BEGIN NOT ATOMIC \
        DECLARE v INT DEFAULT 0; \
        IF CASE WHEN 1 THEN 1 ELSE 0 END = 1 THEN \
            SELECT 1; \
        ELSEIF 0 THEN \
            SELECT 2; \
        ELSE \
            BEGIN \
                WHILE v < 1 DO SET v := v + 1; END WHILE; \
            END; \
        END IF; \
        CASE v WHEN 1 THEN REPEAT SET v := v + 1; UNTIL CASE WHEN v > 1 THEN 1 ELSE 0 END END REPEAT; ELSE SELECT 3; END CASE; \
        FOR r IN (SELECT 1 AS n) DO SELECT r.n; END FOR; \
    END lbl; \
    SELECT 99;";
    let complex_stmts = split_statements_for_engine_with_mysql_mode(
        complex_block,
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(complex_stmts.len(), 2);
    assert_eq!(complex_stmts[1].sql, "SELECT 99");

    // Standard transactions and CASE expressions still split normally.
    let bare_tx = split_statements_for_engine_with_mysql_mode(
        "BEGIN; SELECT 1; COMMIT;",
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(bare_tx.len(), 3);
    let begin_work_tx = split_statements_for_engine_with_mysql_mode(
        "BEGIN WORK; SELECT CASE WHEN 1 THEN 2 END; COMMIT;",
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(begin_work_tx.len(), 3);

    for compound_drop in [
        "BEGIN NOT ATOMIC SELECT 1; DROP TABLE students; END",
        "lbl: BEGIN NOT ATOMIC IF 1 THEN TRUNCATE TABLE students; END IF; END lbl",
    ] {
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                compound_drop,
                Some(mariadb_11_4),
            )
            .risk,
            StatementRisk::Destructive
        );
        assert!(matches!(
            evaluate_sql_safety_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                SafetyPolicy::Unrestricted,
                EnvTag::Production,
                &[compound_drop.to_owned()],
                Some(mariadb_11_4),
            ),
            SafetyDecision::RequireConfirmation {
                is_destructive: true,
                ..
            }
        ));
    }

    // 7. MariaDB SET STATEMENT sql_mode=... restores outer mode even if inner statement sets sql_mode (R18-5).
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "SET STATEMENT sql_mode='ANSI_QUOTES' FOR SET SESSION sql_mode='NO_BACKSLASH_ESCAPES'"
                    .to_owned(),
                "SELECT 'Troy\\'; SELECT 2'".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_ok()
    );
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "SET STATEMENT max_statement_time=10 FOR SET STATEMENT sql_mode='ANSI_QUOTES' FOR SET SESSION sql_mode='NO_BACKSLASH_ESCAPES'".to_owned(),
                "SELECT 'Troy\\'; SELECT 2'".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_ok()
    );
    // When outer SET STATEMENT does not assign sql_mode, inner SET SESSION sql_mode still applies.
    assert!(
        validate_mysql_single_statement_batch(
            &[
                "SET STATEMENT max_statement_time=10 FOR SET SESSION sql_mode='NO_BACKSLASH_ESCAPES'"
                    .to_owned(),
                "SELECT 'Troy\\'; DROP TABLE students".to_owned(),
            ],
            Some(mariadb_11_4),
        )
        .is_err()
    );

    // 8. MariaDB bare handler bodies, nested BEGIN NOT ATOMIC, and handler IF blocks (R19-1, R19-4).
    for handler_drop in [
        "BEGIN NOT ATOMIC DECLARE CONTINUE HANDLER FOR SQLEXCEPTION TRUNCATE TABLE students; SELECT 1; END",
        "BEGIN NOT ATOMIC DECLARE EXIT HANDLER FOR SQLSTATE '45000', 1062 DROP TABLE students; SELECT 1; END",
    ] {
        assert_eq!(
            inspect_statement_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                handler_drop,
                Some(mariadb_11_4),
            )
            .risk,
            StatementRisk::Destructive,
            "expected Destructive for {handler_drop}"
        );
        assert!(matches!(
            evaluate_sql_safety_for_engine_with_mysql_mode(
                DbEngine::Mysql,
                SafetyPolicy::Unrestricted,
                EnvTag::Production,
                &[handler_drop.to_owned()],
                Some(mariadb_11_4),
            ),
            SafetyDecision::RequireConfirmation {
                is_destructive: true,
                ..
            }
        ));
    }

    for handler_secret in [
        "BEGIN NOT ATOMIC DECLARE CONTINUE HANDLER FOR SQLEXCEPTION SET @q = 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''; SELECT 1; END",
        "BEGIN NOT ATOMIC DECLARE CONTINUE HANDLER FOR 1062, NOT FOUND CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'; SELECT 1; END",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, handler_secret, mode),
                "expected credential flag for handler {handler_secret} under {mode:?}"
            );
        }
    }

    for valid_nested_block in [
        "BEGIN NOT ATOMIC DECLARE CONTINUE HANDLER FOR SQLEXCEPTION IF 1 THEN SELECT 1; END IF; SELECT 2; END",
        "BEGIN NOT ATOMIC BEGIN NOT ATOMIC IF 1 THEN SELECT 1; END IF; END; SELECT 2; END",
        "BEGIN NOT ATOMIC DECLARE EXIT HANDLER FOR SQLSTATE VALUE '23000', 1062, NOT FOUND lbl: BEGIN SELECT 1; END lbl; SELECT 2; END",
    ] {
        let with_tail = format!("{valid_nested_block}; SELECT 3;");
        let split = split_statements_for_engine_with_mysql_mode(
            &with_tail,
            DbEngine::Mysql,
            Some(mariadb_11_4),
        );
        assert_eq!(split.len(), 2, "expected 2 statements for {valid_nested_block}");
        assert_eq!(split[0].sql, valid_nested_block);
        assert_eq!(split[1].sql, "SELECT 3");
        assert!(
            validate_mysql_single_statement_batch(
                &[valid_nested_block.to_owned(), "SELECT 3".to_owned()],
                Some(mariadb_11_4),
            )
            .is_ok(),
            "expected valid nested block to pass validator: {valid_nested_block}"
        );
    }

    // 9. PostgreSQL multi-dot GUCs, routine SET clauses, and set_config calls (R19-2).
    for pg_multi_or_routine_secret in [
        "SET greendale.app.password = 'greendale-placeholder'",
        "ALTER SYSTEM SET greendale.app.password = \"greendale-placeholder\"",
        "CREATE FUNCTION greendale_sync() RETURNS void LANGUAGE sql SET search_path = public SET app.password = 'greendale-placeholder' AS $$ SELECT 1 $$",
        "CREATE OR REPLACE PROCEDURE greendale_proc() LANGUAGE sql SET greendale.app.api_password = \"greendale-placeholder\" AS $$ SELECT 1 $$",
        "ALTER FUNCTION greendale_sync() SET greendale.app.password = 'greendale-placeholder'",
        "ALTER ROUTINE greendale_sync() SET app.password = 'greendale-placeholder'",
        "SELECT set_config('greendale.app.password', 'greendale-placeholder', false)",
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Postgres, pg_multi_or_routine_secret),
            "expected credential detection for {pg_multi_or_routine_secret}"
        );
    }
    for pg_safe_routine_or_config in [
        "CREATE FUNCTION greendale_ok() RETURNS void LANGUAGE sql SET app.password = DEFAULT AS $$ SELECT 1 $$",
        "CREATE FUNCTION greendale_ok2() RETURNS void LANGUAGE sql SET search_path = public AS $$ SELECT 1 $$",
        "SELECT set_config('search_path', 'public', false)",
    ] {
        assert!(
            !sql_sets_credentials(DbEngine::Postgres, pg_safe_routine_or_config),
            "did not expect credential flag for {pg_safe_routine_or_config}"
        );
    }

    // 10. MariaDB dynamic SQL branching & conservative literal fallback (R19-3).
    for dynamic_secret in [
        "EXECUTE IMMEDIATE IF(@flag, 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''', 'SELECT 1')",
        "EXECUTE IMMEDIATE CASE WHEN @flag THEN 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''' ELSE 'SELECT 1' END",
        "EXECUTE IMMEDIATE COALESCE(@sql, 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''')",
        "EXECUTE IMMEDIATE REPLACE('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''__PW__''', '__PW__', 'greendale-placeholder')",
        "EXECUTE IMMEDIATE greendale_build_sql('greendale-placeholder')",
        "PREPARE s FROM greendale_build_sql('greendale-placeholder')",
        "EXECUTE IMMEDIATE @sql USING 'greendale-placeholder'",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, dynamic_secret, mode),
                "expected dynamic credential flag for {dynamic_secret} under {mode:?}"
            );
        }
    }
    for dynamic_safe in [
        "EXECUTE IMMEDIATE IF(@flag, 'SELECT 1', 'SELECT 2')",
        "PREPARE s FROM @sql",
        "EXECUTE IMMEDIATE @sql",
        "SET @x = greendale_build_sql('hello')",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                !sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, dynamic_safe, mode),
                "did not expect dynamic credential flag for {dynamic_safe} under {mode:?}"
            );
        }
    }
}

#[test]
fn mysql_and_pg_round20_set_config_replace_concat_set_statement_and_version_preservation() {
    use tv_core::{
        MysqlQuoteMode, MysqlServerVersion, SafetyDecision, SafetyPolicy,
        evaluate_sql_safety_for_engine_with_mysql_mode,
        split_statements_for_engine_with_mysql_mode, sql_sets_credentials,
        sql_sets_credentials_with_mysql_mode, validate_mysql_single_statement_batch,
    };

    let mysql_8_4 =
        MysqlQuoteMode::DEFAULT.with_server_version(MysqlServerVersion::from_version_str("8.4.0"));
    let mariadb_11_4 = MysqlQuoteMode::DEFAULT
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    // 1. PostgreSQL set_config with schema-qualified / quoted / unicode names, named args, and decoded string forms (R20-1).
    for pg_set_config_secret in [
        "SELECT pg_catalog.\"set_config\"('greendale.app.password', 'greendale-placeholder', false)",
        "SELECT U&\"\\0073et_config\"($$greendale.app.password$$, 'greendale-placeholder', false)",
        "SELECT set_config(new_value => 'greendale-placeholder', setting_name => 'greendale.app.password', is_local => false)",
        "SELECT set_config(is_local := false, new_value := 'greendale-placeholder', setting_name := E'greendale.app.\\x70assword')",
        "SELECT set_config(U&'greendale.app.\\0070assword', 'greendale-placeholder', false)",
        "SELECT set_config('greendale.app.' || 'password', 'greendale-placeholder', false)",
        "SELECT set_config(setting_name => CAST($$greendale.app.password$$ AS text), new_value => 'greendale-placeholder'::text, is_local => false)",
        "SELECT set_config('greendale.' || unknown_part() || '.password', 'greendale-placeholder', false)",
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Postgres, pg_set_config_secret),
            "expected set_config credential detection for {pg_set_config_secret}"
        );
    }
    for pg_set_config_safe in [
        "SELECT pg_catalog.\"set_config\"('search_path', 'public', false)",
        "SELECT set_config(setting_name => 'greendale.app.password', new_value => NULL, is_local => false)",
        "SELECT set_config('greendale.app.password', NULL::text, false)",
        "SELECT set_config(new_value => CAST(NULL AS text), setting_name => $$greendale.app.password$$, is_local => false)",
        "SELECT set_config('app.password_hint', 'greendale-placeholder', false)",
    ] {
        assert!(
            !sql_sets_credentials(DbEngine::Postgres, pg_set_config_safe),
            "did not expect set_config credential flag for {pg_set_config_safe}"
        );
    }

    // 2. MariaDB case-sensitive REPLACE and CONCAT / CHAR(39) evaluation (R20-2).
    for mariadb_dyn_secret in [
        "EXECUTE IMMEDIATE REPLACE('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''', 'identified by', 'COMMENT')",
        "PREPARE s FROM REPLACE('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''', 'identified by', 'COMMENT')",
        "PREPARE s FROM CONCAT('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ', CHAR(39), 'greendale-placeholder', CHAR(39))",
        "EXECUTE IMMEDIATE CONCAT('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ', CHAR(39), 'greendale-placeholder', CHAR(39))",
        "SET @q = CONCAT('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ', CHAR(39), 'greendale-placeholder', CHAR(39))",
        "PREPARE s FROM CONCAT('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ', CHAR(30 + 9), 'greendale-placeholder', CHAR(30 + 9))",
        "SET @q = CONCAT('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ', @qt, 'greendale-placeholder', @qt)",
        "EXECUTE IMMEDIATE REPLACE('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''', '', 'X')",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, mariadb_dyn_secret, mode),
                "expected credential detection for {mariadb_dyn_secret} under {mode:?}"
            );
        }
    }
    for mariadb_dyn_safe in [
        "SET @q = CONCAT('SELECT * FROM students WHERE name = ', @u)",
        "PREPARE s FROM CONCAT('SELECT ', '1')",
        "PREPARE s FROM CONCAT('SELECT ', CHAR(39), 'greendale-placeholder', CHAR(39))",
        "PREPARE s FROM REPLACE('SELECT * FROM students WHERE name = ''troy''', 'troy', 'abed')",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                !sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, mariadb_dyn_safe, mode),
                "did not expect credential flag for {mariadb_dyn_safe} under {mode:?}"
            );
        }
    }

    // 3. Nested 32-fold REPLACE size cap must fail closed without OOM (R20-3 / R22).
    let mut bomb = "'a'".to_owned();
    let pad = "a".repeat(32);
    for _ in 0..7 {
        bomb = format!("REPLACE({bomb}, 'a', '{pad}')");
    }
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!("SET @x = {bomb}"),
        Some(mariadb_11_4),
    ));
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!("PREPARE s FROM {bomb}"),
        Some(mariadb_11_4),
    ));

    // 4. MariaDB SET STATEMENT ... FOR compound splitting, validation, and safety (R20-4).
    let set_stmt_compound =
        "SET STATEMENT max_statement_time=1 FOR BEGIN NOT ATOMIC SELECT 1; SELECT 2; END";
    let split_one =
        split_statements_for_engine_with_mysql_mode(set_stmt_compound, DbEngine::Mysql, Some(mariadb_11_4));
    assert_eq!(split_one.len(), 1);
    let batch_one: Vec<String> = split_one.iter().map(|s| s.sql.clone()).collect();
    assert!(
        validate_mysql_single_statement_batch(&batch_one, Some(mariadb_11_4)).is_ok(),
        "expected SET STATEMENT ... FOR compound block to pass single-statement validation"
    );

    let set_stmt_walrus_labeled =
        "SET STATEMENT max_statement_time := 1 FOR lbl: BEGIN NOT ATOMIC SELECT 1; SELECT 2; END; SELECT 3;";
    let split_two = split_statements_for_engine_with_mysql_mode(
        set_stmt_walrus_labeled,
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(split_two.len(), 2);
    let batch_two: Vec<String> = split_two.iter().map(|s| s.sql.clone()).collect();
    assert!(validate_mysql_single_statement_batch(&batch_two, Some(mariadb_11_4)).is_ok());

    let set_stmt_destructive =
        "SET STATEMENT max_statement_time=1 FOR BEGIN NOT ATOMIC SELECT 1; TRUNCATE TABLE students; END";
    let split_dest = split_statements_for_engine_with_mysql_mode(
        set_stmt_destructive,
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(split_dest.len(), 1);
    assert!(matches!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::Unrestricted,
            EnvTag::Production,
            &[split_dest[0].sql.clone()],
            Some(mariadb_11_4),
        ),
        SafetyDecision::RequireConfirmation {
            is_destructive: true,
            ..
        }
    ));

    let set_stmt_plain_pair = "SET STATEMENT max_statement_time=1 FOR SELECT 1; SELECT 2;";
    let split_pair = split_statements_for_engine_with_mysql_mode(
        set_stmt_plain_pair,
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(split_pair.len(), 2);

    // 5. Dynamic sql_mode assignment preserves known server_version across validation and safety (R20-5).
    let dynamic_m_comment_split = vec![
        "SET SESSION sql_mode = @new_mode".to_owned(),
        "SELECT 3 /*M! ; SELECT 4 */".to_owned(),
    ];
    assert!(
        validate_mysql_single_statement_batch(&dynamic_m_comment_split, Some(mysql_8_4)).is_ok(),
        "MySQL 8.4 must still ignore /*M! comments even after dynamic sql_mode assignment"
    );
    assert!(
        validate_mysql_single_statement_batch(&dynamic_m_comment_split, Some(mariadb_11_4)).is_err(),
        "MariaDB 11.4 must reject /*M! multi-statement after dynamic sql_mode assignment"
    );

    let dynamic_m_comment_drop = vec![
        "SET SESSION sql_mode = @new_mode".to_owned(),
        "SELECT 3 /*M! ; DROP TABLE students */".to_owned(),
    ];
    assert_eq!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::Unrestricted,
            EnvTag::Production,
            &dynamic_m_comment_drop,
            Some(mysql_8_4),
        ),
        SafetyDecision::Allow
    );
    assert!(matches!(
        evaluate_sql_safety_for_engine_with_mysql_mode(
            DbEngine::Mysql,
            SafetyPolicy::Unrestricted,
            EnvTag::Production,
            &dynamic_m_comment_drop,
            Some(mariadb_11_4),
        ),
        SafetyDecision::RequireConfirmation {
            is_destructive: true,
            ..
        }
    ));
}

#[test]
fn mysql_and_pg_round21_casts_and_eval_budget_bounds() {
    use tv_core::{
        MysqlQuoteMode, MysqlServerVersion, split_statements_for_engine_with_mysql_mode,
        sql_sets_credentials, sql_sets_credentials_with_mysql_mode,
    };

    let mariadb_11_4 = MysqlQuoteMode::DEFAULT
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    // 1. PostgreSQL truncating & schema-qualified casts in set_config.
    let long_prefix = "a".repeat(54);
    let pg_name_cast = format!(
        "SELECT set_config('{long_prefix}.password_extra'::name, 'greendale-placeholder', false)"
    );
    for pg_cast_secret in [
        "SELECT set_config('greendale.app.password_extra'::varchar(22), 'greendale-placeholder', false)",
        "SELECT set_config(CAST('greendale.app.password_extra' AS character varying(22)), 'greendale-placeholder', false)",
        "SELECT set_config('greendale.app.password_extra'::char(22)::text, 'greendale-placeholder', false)",
        "SELECT set_config(CAST('greendale.app.password_extra' AS bpchar(22)), 'greendale-placeholder', false)",
        "SELECT set_config('greendale.app.' || 'password_extra'::pg_catalog.\"varchar\"(8), 'greendale-placeholder', false)",
        "SELECT set_config('greendale.app.password_v'::custom_domain, 'greendale-placeholder', false)",
        "SELECT set_config('search_path'::custom_domain, 'public', false)",
        "SELECT set_config('greendale.app.password'::char(10), 'greendale-placeholder', false)",
        "SELECT set_config('greendale.app.password'::varchar(10), 'greendale-placeholder', false)",
        "SELECT set_config('greendale.app.' || 'password'::char, 'greendale-placeholder', false)",
        "SELECT set_config('greendale.app.password_extra'::text, 'greendale-placeholder', false)",
        "SELECT set_config(CAST('greendale.app.password_extra' AS text), 'greendale-placeholder', false)",
        "SELECT set_config(text 'greendale.app.password_extra', 'greendale-placeholder', false)",
        "SELECT set_config('work_mem'::text, '4MB', false)",
        "SELECT set_config('work_mem'::varchar, '4MB', false)",
        "SELECT set_config('work_mem'::character varying, '4MB', false)",
        "SELECT set_config('work_mem'::\"text\", '4MB', false)",
        "SELECT set_config(CAST('work_mem' AS varchar), '4MB', false)",
        "SELECT set_config(text 'work_mem', '4MB', false)",
        pg_name_cast.as_str(),
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Postgres, pg_cast_secret),
            "expected set_config cast detection for {pg_cast_secret}"
        );
    }
    for pg_cast_safe in [
        "SELECT set_config('work_mem'::pg_catalog.text, '4MB', false)",
        "SELECT set_config(CAST('work_mem' AS pg_catalog.varchar), '4MB', false)",
        "SELECT set_config(pg_catalog.text 'work_mem', '4MB', false)",
        "SELECT set_config(('search_path')::pg_catalog.\"text\", 'public', false)",
    ] {
        assert!(
            !sql_sets_credentials(DbEngine::Postgres, pg_cast_safe),
            "did not expect set_config cast flag for {pg_cast_safe}"
        );
    }

    // 2. MySQL / MariaDB truncating CAST / CONVERT expressions in dynamic SQL.
    for mysql_cast_secret in [
        "EXECUTE IMMEDIATE CONCAT(CAST('SELECT 1; ''ignored' AS CHAR(9)), 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''')",
        "EXECUTE IMMEDIATE CONCAT(CONVERT('SELECT 1; ''ignored', CHAR(9)), 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''')",
    ] {
        for mode in [Some(mariadb_11_4), None] {
            assert!(
                sql_sets_credentials_with_mysql_mode(DbEngine::Mysql, mysql_cast_secret, mode),
                "expected truncating cast credential detection for {mysql_cast_secret}"
            );
        }
    }
    assert!(!sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        "EXECUTE IMMEDIATE CAST('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder''' AS CHAR(10))",
        Some(mariadb_11_4),
    ));

    // 3. Per-statement and total evaluation budgets bound work cleanly without starving later statements.
    let base_8k = "a".repeat(8192);
    let mut sixteen_branch = format!("CONCAT('{base_8k}', '0')");
    for bit in 0..4 {
        let alt = format!("CONCAT('{base_8k}', '{bit}')");
        sixteen_branch = format!("IF(@f{bit}, {sixteen_branch}, {alt})");
    }
    let mut deep_replace = sixteen_branch;
    for _ in 0..30 {
        deep_replace = format!("REPLACE({deep_replace}, 'z', 'y')");
    }

    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!("SET @x = {deep_replace}"),
        Some(mariadb_11_4),
    ));
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!("PREPARE s FROM {deep_replace}"),
        Some(mariadb_11_4),
    ));
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!(
            "SET @x = {deep_replace}; CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'"
        ),
        Some(mariadb_11_4),
    ));
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!(
            "SET @x = {deep_replace}; SET @y = CONCAT('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ', CHAR(39), 'greendale-placeholder', CHAR(39))"
        ),
        Some(mariadb_11_4),
    ));

    // Deep multi-branch SET chaining fails closed cleanly on budget cap.
    let mut fanout_set = "'SELECT 1'".to_owned();
    for lvl in (0..3).rev() {
        let mut branch_expr = fanout_set;
        for bit in 0..4 {
            let escaped = branch_expr.replace('\'', "''");
            let rhs_a = format!("'SET @v{lvl}_{bit} = ''{escaped}'''");
            let rhs_b = format!("'SET @w{lvl}_{bit} = ''{escaped}'''");
            branch_expr = format!("IF(@c{lvl}_{bit}, {rhs_a}, {rhs_b})");
        }
        fanout_set = branch_expr;
    }
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!("SET @root = {fanout_set}"),
        Some(mariadb_11_4),
    ));

    // Wide CONCAT inside SET SESSION sql_mode hits MAX_SQL_MODE_EXPR_STEPS cleanly.
    let args: Vec<&str> = (0..300).map(|_| "'STRICT_TRANS_TABLES'").collect();
    let wide_sql_mode = format!("SET SESSION sql_mode = CONCAT({}); SELECT 1;", args.join(", "));
    let stmts = split_statements_for_engine_with_mysql_mode(
        &wide_sql_mode,
        DbEngine::Mysql,
        Some(mariadb_11_4),
    );
    assert_eq!(stmts.len(), 2);
}

#[test]
fn mysql_and_pg_round22_fail_closed_budget_and_linear_nested_calls() {
    use std::time::Instant;
    use tv_core::{
        MysqlQuoteMode, MysqlServerVersion, sql_sets_credentials,
        sql_sets_credentials_with_mysql_mode,
    };

    let mariadb_11_4 = MysqlQuoteMode::DEFAULT
        .with_server_version(MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    // 1. Earlier harmless dynamic statements cannot starve later credential detection.
    let harmless_replaces = "SET @a = REPLACE(CONCAT('greendale', '_dean'), 'dean', 'troy');\n".repeat(12);
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!(
            "{harmless_replaces}SET @q = 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''"
        ),
        Some(mariadb_11_4),
    ));

    let harmless_ifs = "SET @a = CONCAT(IF(@f, 'x1', 'x2'), 'y');\n".repeat(20);
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!(
            "{harmless_ifs}SET @q = CONCAT('CREATE USER ''troy''@''localhost'' IDENTIFIED BY ', CHAR(39), 'greendale-placeholder', CHAR(39))"
        ),
        Some(mariadb_11_4),
    ));

    let harmless_pg_set_config =
        "SELECT set_config(REPLACE('app.k', 'k', 'v'), '1', false);\n".repeat(20);
    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        &format!(
            "{harmless_pg_set_config}SELECT set_config('greendale.app.pass' || 'word', 'greendale-placeholder', false);"
        ),
    ));

    let mut deep_replace = "CONCAT('greendale', '0')".to_owned();
    for _ in 0..30 {
        deep_replace = format!("REPLACE({deep_replace}, 'z', 'y')");
    }
    assert!(sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &format!(
            "SET @a = {deep_replace}, @b = 'CREATE USER ''troy''@''localhost'' IDENTIFIED BY ''greendale-placeholder'''"
        ),
        Some(mariadb_11_4),
    ));

    // Many benign single-literal statements skip budget cleanly without false positives.
    let benign_mysql = "SET @name = 'Troy Barnes';\n".repeat(500);
    assert!(!sql_sets_credentials_with_mysql_mode(
        DbEngine::Mysql,
        &benign_mysql,
        Some(mariadb_11_4),
    ));

    let benign_pg = "SELECT set_config('search_path', 'public', false);\n".repeat(500);
    assert!(!sql_sets_credentials(DbEngine::Postgres, &benign_pg));

    // 2. Deeply nested PostgreSQL calls scan in linear time.
    let mut nested_in_value = "'public'".to_owned();
    for _ in 0..2000 {
        nested_in_value = format!("set_config('search_path', {nested_in_value}, false)");
    }
    let nested_in_value_sql = format!("SELECT {nested_in_value}");
    let start_val = Instant::now();
    assert!(!sql_sets_credentials(
        DbEngine::Postgres,
        &nested_in_value_sql
    ));
    assert!(
        start_val.elapsed().as_millis() < 500,
        "expected linear time for 2000 nested set_config values, took {:?}",
        start_val.elapsed()
    );

    let mut nested_in_name = "'search_path'".to_owned();
    for _ in 0..2000 {
        nested_in_name = format!("set_config({nested_in_name}, 'public', false)");
    }
    let nested_in_name_sql = format!("SELECT {nested_in_name}");
    let start_name = Instant::now();
    let _ = sql_sets_credentials(DbEngine::Postgres, &nested_in_name_sql);
    assert!(
        start_name.elapsed().as_millis() < 500,
        "expected linear time for 2000 nested set_config names, took {:?}",
        start_name.elapsed()
    );

    let mut nested_lower = "'greendale.app.password'".to_owned();
    for _ in 0..2000 {
        nested_lower = format!("lower({nested_lower})");
    }
    let nested_lower_sql = format!("SELECT set_config({nested_lower}, 'greendale-placeholder', false)");
    let start_lower = Instant::now();
    assert!(sql_sets_credentials(DbEngine::Postgres, &nested_lower_sql));
    assert!(
        start_lower.elapsed().as_millis() < 500,
        "expected linear time for 2000 nested lower() calls, took {:?}",
        start_lower.elapsed()
    );
}

fn mysql_escape_single_quotes(sql: &str) -> String {
    sql.replace('\'', "''")
}

fn mysql_char_expr(sql: &str) -> String {
    let bytes = sql
        .as_bytes()
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("CHAR({bytes})")
}

#[test]
fn mysql_credentials_fail_closed_at_recursion_depth_limit() {
    let core = "CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'";
    let wrap_set = |inner: &str| format!("SET @stmt = '{}'", mysql_escape_single_quotes(inner));

    let mut set_4 = core.to_owned();
    for _ in 0..4 {
        set_4 = wrap_set(&set_4);
    }
    let set_5 = wrap_set(&set_4);
    let set_6 = wrap_set(&set_5);

    assert!(sql_sets_credentials(DbEngine::Mysql, &set_4));
    assert!(sql_sets_credentials(DbEngine::Mysql, &set_5));
    assert!(sql_sets_credentials(DbEngine::Mysql, &set_6));

    // 5x nested PREPARE p FROM CHAR(...) at the outermost layer.
    let mut nested_prepare = core.to_owned();
    for depth in 0..4 {
        nested_prepare = format!(
            "PREPARE p{depth} FROM '{}'",
            mysql_escape_single_quotes(&nested_prepare)
        );
    }
    let nested_prepare_char = format!("PREPARE p5 FROM {}", mysql_char_expr(&nested_prepare));
    assert!(sql_sets_credentials(DbEngine::Mysql, &nested_prepare_char));

    assert!(!sql_sets_credentials(
        DbEngine::Mysql,
        "SET @name = 'Troy Barnes'"
    ));
}

#[test]
fn mysql_dotted_and_compound_user_vars_detected_for_credentials() {
    let core = "CREATE USER 'abed'@'localhost' IDENTIFIED BY 'greendale-placeholder'";
    let escaped = mysql_escape_single_quotes(core);

    for var in [
        "@study.query",
        "@study.group.query",
        "@1.2",
        "@.query",
        "@'study.query'",
        "@`study.query`",
        "@$study.query",
    ] {
        for op in ["=", ":="] {
            let stmt = format!("SET {var} {op} '{escaped}'");
            assert!(
                sql_sets_credentials(DbEngine::Mysql, &stmt),
                "expected credential detection for: {stmt}"
            );
        }
    }

    let batch = [
        format!("SET @study.query = '{escaped}'"),
        "PREPARE s FROM @study.query".to_owned(),
        "EXECUTE s".to_owned(),
    ];
    let joined = batch.join("; ");
    assert!(batch_sets_credentials_with_session_mode(
        DbEngine::Mysql,
        &batch,
        &joined,
        SqlSessionMode::mysql(None),
    ));

    assert!(!sql_sets_credentials(
        DbEngine::Mysql,
        "SET @@session.sql_mode = 'ANSI_QUOTES'"
    ));
}

#[test]
fn postgres_set_config_handles_brackets_case_coalesce_and_nested_calls() {
    for sql in [
        "SELECT set_config(CASE WHEN ARRAY[1,2] = ARRAY[1,2] THEN 'greendale.password' ELSE 'work_mem' END, 'greendale-placeholder', false)",
        "SELECT set_config('greendale.pass' || 'word', 'greendale-placeholder', false)",
        "SELECT set_config(CASE WHEN ARRAY[ARRAY[1,2], ARRAY[3,4]] = ARRAY[ARRAY[1,2], ARRAY[3,4]] THEN 'greendale.app_password' ELSE 'work_mem' END, 'greendale-placeholder', false)",
        "SELECT set_config(COALESCE(NULL, 'greendale.pass' || 'word'), 'greendale-placeholder', false)",
        "SELECT set_config((ARRAY['greendale.pass' || 'word'])[1], 'greendale-placeholder', false)",
        "SELECT ARRAY[set_config('greendale.password', 'greendale-placeholder', false)]",
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Postgres, sql),
            "expected credential detection for: {sql}"
        );
    }

    assert!(sql_sets_credentials(
        DbEngine::Postgres,
        "SELECT set_config(CASE WHEN ARRAY[1,2] = ARRAY[1,2] THEN 'work_mem' ELSE 'search_path' END, '4MB', false)"
    ));

    // Unmatched brackets must not panic or hang.
    for malformed in [
        "SELECT set_config(], 'x', false)",
        "SELECT set_config([, 'x', false)",
        "SELECT ] [",
        "SELECT set_config([[[, 'greendale.password', false)",
    ] {
        let _ = sql_sets_credentials(DbEngine::Postgres, malformed);
    }
}

#[test]
fn statement_at_cursor_single_line_2000_statements_is_linear() {
    use std::time::Instant;

    let body = (0..2000)
        .map(|i| format!("SELECT {i}"))
        .collect::<Vec<_>>()
        .join("; ");
    let sql = format!("{body}   ");
    let caret = sql.len();

    let start = Instant::now();
    let found = statement_at_cursor_for_engine_with_session_mode(
        &sql,
        caret,
        caret,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    let elapsed = start.elapsed();

    assert_eq!(found.as_ref().map(|s| s.sql.as_str()), Some("SELECT 1999"));
    assert!(
        elapsed.as_millis() < 500,
        "expected linear cursor lookup for 2000 statements, took {elapsed:?}"
    );
}

#[test]
fn batch_credentials_ignores_redundant_whole_buffer_rescan_unless_mismatched() {
    let unit = "SELECT set_config('work_mem'::pg_catalog.text, '4MB', false)";
    let stmts = vec![unit.to_owned(); 600];
    let executed_sql = stmts.join("; ");

    assert!(!sql_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &executed_sql,
        SqlSessionMode::postgres(true),
    ));
    assert!(!batch_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &stmts,
        &executed_sql,
        SqlSessionMode::postgres(true),
    ));

    // Mismatched executed_sql_text still triggers a fresh full-buffer scan.
    assert!(batch_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &["SELECT 1".to_owned()],
        "SELECT set_config('greendale.password', 'greendale-placeholder', false)",
        SqlSessionMode::postgres(true),
    ));

    // Empty statements slice with credential executed_sql_text must still scan.
    assert!(batch_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &[],
        "ALTER SYSTEM SET primary_conninfo = 'password=greendale-placeholder'",
        SqlSessionMode::postgres(true),
    ));
}

#[test]
fn postgres_standard_conforming_strings_affects_split_safety_and_credentials() {
    // Inline transition to scs = off.
    let inline_sql = "SET standard_conforming_strings = off; SELECT 'Troy\\'; Barnes'; SELECT 2;";
    let inline_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        inline_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        inline_stmts,
        vec![
            "SET standard_conforming_strings = off",
            "SELECT 'Troy\\'; Barnes'",
            "SELECT 2",
        ]
    );

    // SET LOCAL reverts at COMMIT and ROLLBACK; RESET and set_config(..., false) transition state.
    let tx_commit_sql = "BEGIN; SET LOCAL standard_conforming_strings = off; SELECT 'Troy\\'; Barnes'; COMMIT; SELECT 'Abed\\' ; SELECT 2;";
    let tx_commit_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        tx_commit_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        tx_commit_stmts,
        vec![
            "BEGIN",
            "SET LOCAL standard_conforming_strings = off",
            "SELECT 'Troy\\'; Barnes'",
            "COMMIT",
            "SELECT 'Abed\\'",
            "SELECT 2",
        ]
    );

    let tx_rollback_sql = "BEGIN; SET LOCAL standard_conforming_strings = off; SELECT 'Troy\\'; Barnes'; ROLLBACK; SELECT 'Abed\\' ; SELECT 2;";
    let tx_rollback_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        tx_rollback_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        tx_rollback_stmts,
        vec![
            "BEGIN",
            "SET LOCAL standard_conforming_strings = off",
            "SELECT 'Troy\\'; Barnes'",
            "ROLLBACK",
            "SELECT 'Abed\\'",
            "SELECT 2",
        ]
    );

    let reset_sql = "SET standard_conforming_strings = off; RESET standard_conforming_strings; SELECT 'Abed\\' ; SELECT 2;";
    let reset_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        reset_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        reset_stmts,
        vec![
            "SET standard_conforming_strings = off",
            "RESET standard_conforming_strings",
            "SELECT 'Abed\\'",
            "SELECT 2",
        ]
    );

    let set_config_sql =
        "SELECT set_config('standard_conforming_strings', 'off', false); SELECT 'Troy\\'; Barnes'; SELECT 2;";
    let set_config_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        set_config_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        set_config_stmts,
        vec![
            "SELECT set_config('standard_conforming_strings', 'off', false)",
            "SELECT 'Troy\\'; Barnes'",
            "SELECT 2",
        ]
    );

    // Live session scs = false for Run All and Run Current, escaped backslash, and U&'...' immunity.
    let live_off = SqlSessionMode::postgres(false);
    let live_sql = "SELECT 'Troy\\'; Barnes'; SELECT 2;";
    let live_stmts: Vec<String> = split_statements_for_engine_with_session_mode(live_sql, DbEngine::Postgres, live_off.clone())
        .into_iter()
        .map(|s| s.sql)
        .collect();
    assert_eq!(live_stmts, vec!["SELECT 'Troy\\'; Barnes'", "SELECT 2"]);

    let cur = statement_at_cursor_for_engine_with_session_mode(live_sql, 10, 10, DbEngine::Postgres, live_off.clone());
    assert_eq!(
        cur.as_ref().map(|s| s.sql.as_str()),
        Some("SELECT 'Troy\\'; Barnes'")
    );

    let escaped_backslash: Vec<String> = split_statements_for_engine_with_session_mode(
        "SELECT 'Troy\\\\'; SELECT 2;",
        DbEngine::Postgres,
        live_off.clone(),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(escaped_backslash, vec!["SELECT 'Troy\\\\'", "SELECT 2"]);

    let unicode_escape: Vec<String> = split_statements_for_engine_with_session_mode(
        "SELECT U&'Troy\\' ; SELECT 2;",
        DbEngine::Postgres,
        live_off.clone(),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(unicode_escape, vec!["SELECT U&'Troy\\'", "SELECT 2"]);

    // Safety inspection and batch evaluation with scs = false.
    assert_eq!(
        inspect_statement_for_engine_with_session_mode(
            DbEngine::Postgres,
            "SELECT 'Troy\\'; Barnes'",
            live_off.clone(),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    assert_eq!(
        inspect_statement_for_engine_with_session_mode(
            DbEngine::Postgres,
            "SELECT'Troy\\''",
            live_off.clone(),
        )
        .risk,
        StatementRisk::ReadOnly
    );
    let safety_decision = evaluate_sql_safety_for_engine_with_session_mode(
        DbEngine::Postgres,
        SafetyPolicy::ConfirmWrites,
        EnvTag::Production,
        &["SELECT 'Troy\\\\'".to_owned(), "DROP TABLE students".to_owned()],
        live_off.clone(),
    );
    assert!(matches!(
        safety_decision,
        SafetyDecision::RequireConfirmation {
            is_destructive: true,
            ref statements,
            ..
        } if statements.iter().any(|s| s.risk == StatementRisk::Destructive)
    ));

    // Credential detection under scs = false.
    assert!(sql_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        "SELECT set_config('greendale.pass\\x77ord', 'greendale-placeholder', false)",
        live_off.clone(),
    ));
    assert!(!sql_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        "SELECT 'Troy\\'; CREATE ROLE abed PASSWORD ''greendale-placeholder'';'",
        live_off,
    ));
}

#[test]
fn postgres_conditional_set_config_and_multi_statement_slice_checks() {
    let conditional_sql = "SELECT CASE WHEN false THEN set_config('standard_conforming_strings', 'off', false) END; SELECT 'a\\' AS first; SELECT 'b' AS second -- ";
    let stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        conditional_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        stmts,
        vec![
            "SELECT CASE WHEN false THEN set_config('standard_conforming_strings', 'off', false) END",
            "SELECT 'a\\' AS first",
            "SELECT 'b' AS second --",
        ]
    );

    assert!(postgres_slice_has_multiple_statements(
        "SELECT 'a\\' AS first; SELECT 'b' AS second -- ",
        true
    ));
    assert!(!postgres_slice_has_multiple_statements(
        "SELECT 'a\\'; SELECT 2'",
        false
    ));

    let safety_sql = "SELECT CASE WHEN false THEN set_config('standard_conforming_strings', 'off', false) END; SELECT 'a\\'; DROP TABLE students; -- ";
    let safety_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        safety_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    let safety_decision = evaluate_sql_safety_for_engine_with_session_mode(
        DbEngine::Postgres,
        SafetyPolicy::ConfirmWrites,
        EnvTag::Production,
        &safety_stmts,
        SqlSessionMode::postgres(true),
    );
    assert!(matches!(
        safety_decision,
        SafetyDecision::RequireConfirmation {
            is_destructive: true,
            ref statements,
            ..
        } if statements.iter().any(|s| s.risk == StatementRisk::Destructive)
    ));

    let cred_sql = "SELECT CASE WHEN false THEN set_config('standard_conforming_strings', 'off', false) END; SELECT 'a\\'; ALTER ROLE troy PASSWORD ''greendale-placeholder''; -- ";
    assert!(sql_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        cred_sql,
        SqlSessionMode::postgres(true),
    ));
    let cred_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        cred_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert!(batch_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &cred_stmts,
        cred_sql,
        SqlSessionMode::postgres(true),
    ));
}

#[test]
fn postgres_transaction_savepoint_and_reset_default_tracking() {
    // Cross-run in_transaction = true allows SET LOCAL to activate scs = off immediately.
    let cross_run_mode = SqlSessionMode::postgres_mode(Some(
        PostgresSessionMode::new(true).with_in_transaction(Some(true)),
    ));
    let local_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        "SET LOCAL standard_conforming_strings = off; SELECT 'Troy\\'; Barnes'; SELECT 2;",
        DbEngine::Postgres,
        cross_run_mode,
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        local_stmts,
        vec![
            "SET LOCAL standard_conforming_strings = off",
            "SELECT 'Troy\\'; Barnes'",
            "SELECT 2",
        ]
    );

    // Same-batch BEGIN; SAVEPOINT; SET LOCAL off; ROLLBACK TO SAVEPOINT; restores scs = on.
    let sp_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        "BEGIN; SAVEPOINT sp1; SET LOCAL standard_conforming_strings = off; ROLLBACK TO SAVEPOINT sp1; SELECT 'Abed\\' ; SELECT 2;",
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        sp_stmts,
        vec![
            "BEGIN",
            "SAVEPOINT sp1",
            "SET LOCAL standard_conforming_strings = off",
            "ROLLBACK TO SAVEPOINT sp1",
            "SELECT 'Abed\\'",
            "SELECT 2",
        ]
    );

    // ROLLBACK TO SAVEPOINT targeting an earlier batch savepoint marks mode uncertain and fails closed.
    let prior_sp_mode = SqlSessionMode::postgres_mode(Some(
        PostgresSessionMode::new(true).with_in_transaction(Some(true)),
    ));
    let prior_sp_stmts = [
        "ROLLBACK TO SAVEPOINT sp1".to_owned(),
        "SELECT 'Troy\\'; DROP TABLE students; -- '".to_owned(),
    ];
    let prior_sp_decision = evaluate_sql_safety_for_engine_with_session_mode(
        DbEngine::Postgres,
        SafetyPolicy::ConfirmWrites,
        EnvTag::Production,
        &prior_sp_stmts,
        prior_sp_mode.clone(),
    );
    assert!(matches!(
        prior_sp_decision,
        SafetyDecision::RequireConfirmation { is_destructive: true, .. }
    ));
    let prior_sp_cred = "ROLLBACK TO SAVEPOINT sp1; SELECT 'Troy\\'; ALTER ROLE troy PASSWORD ''greendale-placeholder'';";
    assert!(sql_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        prior_sp_cred,
        prior_sp_mode,
    ));

    // Configured reset_val = Some(false) restores scs = off on RESET.
    let reset_off_mode = SqlSessionMode::postgres_mode(Some(
        PostgresSessionMode::new(false).with_reset_val(Some(false)),
    ));
    let reset_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        "SET standard_conforming_strings = on; RESET standard_conforming_strings; SELECT 'Troy\\'; Barnes'; SELECT 2;",
        DbEngine::Postgres,
        reset_off_mode,
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        reset_stmts,
        vec![
            "SET standard_conforming_strings = on",
            "RESET standard_conforming_strings",
            "SELECT 'Troy\\'; Barnes'",
            "SELECT 2",
        ]
    );

    // Unknown reset_val = None on RESET fails closed to Destructive.
    let reset_unknown_mode = SqlSessionMode::postgres_mode(Some(
        PostgresSessionMode::new(false).with_reset_val(None),
    ));
    let reset_unknown_decision = evaluate_sql_safety_for_engine_with_session_mode(
        DbEngine::Postgres,
        SafetyPolicy::ConfirmWrites,
        EnvTag::Production,
        &[
            "RESET standard_conforming_strings".to_owned(),
            "SELECT 'Troy\\'; DROP TABLE students; -- '".to_owned(),
        ],
        reset_unknown_mode,
    );
    assert!(matches!(
        reset_unknown_decision,
        SafetyDecision::RequireConfirmation { is_destructive: true, .. }
    ));
}

#[test]
fn postgres_set_config_handles_cast_and_multidim_arrays() {
    for secret_expr in [
        "(ARRAY['greendale.pass' || 'word']::text[])[1]",
        "(ARRAY['greendale.pass' || 'word']::pg_catalog.text[])[1]",
        "(ARRAY['greendale.pass' || 'word']::_text)[1]",
        "(CAST(ARRAY['greendale.pass' || 'word'] AS text[]))[1]",
        "(ARRAY[['greendale.pass' || 'word']])[1][1]",
        "(ARRAY[['greendale.pass' || 'word']]::text[][])[1][1]",
        "(ARRAY[ARRAY['greendale.pass' || 'word']])[1][1]",
        "(('{greendale.pass' || 'word}')::text[])[1]",
        "(ARRAY['work_mem'] || ARRAY['greendale.pass' || 'word'])[2]",
        "(ARRAY['greendale.pass' || 'word'])[1]::text",
    ] {
        let sql = format!("SELECT set_config({secret_expr}, 'greendale-placeholder', false)");
        assert!(
            sql_sets_credentials(DbEngine::Postgres, &sql),
            "expected credential detection for: {sql}"
        );
    }

    for computed_expr in [
        "(ARRAY['work_mem']::text[])[1]",
        "(ARRAY[['work_mem']])[1][1]",
        "(ARRAY['greendale.password'::char(10)]::text[])[1]",
    ] {
        let sql = format!("SELECT set_config({computed_expr}, '4MB', false)");
        assert!(
            sql_sets_credentials(DbEngine::Postgres, &sql),
            "expected fail-closed credential detection for: {sql}"
        );
    }
}

#[test]
fn mysql_hex_user_variables_detected_for_credentials() {
    let core = "CREATE USER 'troy'@'localhost' IDENTIFIED BY 'greendale-placeholder'";
    let escaped = mysql_escape_single_quotes(core);

    for var in [
        "@0xabc",
        "@study.0xabc",
        "@.0xabc",
        "@0xabc.0xdef",
        "@0xabcg",
    ] {
        for op in ["=", ":="] {
            let stmt = format!("SET {var} {op} '{escaped}'");
            assert!(
                sql_sets_credentials(DbEngine::Mysql, &stmt),
                "expected credential detection for hex-shaped user var: {stmt}"
            );
        }
    }

    let batch = [
        format!("SET @0xabc = '{escaped}'"),
        "PREPARE s FROM @0xabc".to_owned(),
        "EXECUTE s".to_owned(),
    ];
    let joined = batch.join("; ");
    assert!(batch_sets_credentials_with_session_mode(
        DbEngine::Mysql,
        &batch,
        &joined,
        SqlSessionMode::mysql(None),
    ));

    assert!(!sql_sets_credentials(
        DbEngine::Mysql,
        "SET @0xabc = 'Troy Barnes'"
    ));
}

#[test]
fn postgres_array_literal_escapes_aborted_commit_and_savepoint_snapshots() {
    use tv_core::PostgresTxState;

    // 1. PostgreSQL unquoted array backslash escapes, whitespace around quotes, and typed literals.
    for secret_sql in [
        r"SELECT set_config(('{greendale.pass\word}'::text[])[1], 'greendale-placeholder', false)",
        r#"SELECT set_config(('{ "greendale.password" }'::text[])[1], 'greendale-placeholder', false)"#,
        r"SELECT set_config(('{  greendale.pass\word  }'::text[])[1], 'greendale-placeholder', false)",
        r"SELECT set_config((_text '{greendale.pass\word}')[1], 'greendale-placeholder', false)",
        r#"SELECT set_config((pg_catalog."_text" '{greendale.pass\word}')[1], 'greendale-placeholder', false)"#,
        r"SELECT set_config((text[] '{greendale.pass\word}')[1], 'greendale-placeholder', false)",
        "SELECT set_config(text 'greendale.password', 'greendale-placeholder', false)",
        "SELECT set_config(varchar(64) 'greendale.' || 'password', 'greendale-placeholder', false)",
        r"SELECT set_config(array_to_string('{greendale.pass\word}'::text[], ','), 'greendale-placeholder', false)",
        r"SELECT set_config(array_remove('{greendale.pass\word,x}'::text[], 'x')[1], 'greendale-placeholder', false)",
        r"SELECT set_config((SELECT unnest('{greendale.pass\word}'::text[])), 'greendale-placeholder', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.pass', 'word'], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string('{greendale.pass,word}'::text[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.p', NULL, 'ord'], '', 'assw'), 'Troy Barnes', false)",
        "SELECT set_config((ARRAY['greendale.pass'])[1] || 'word', 'Troy Barnes', false)",
        "SELECT set_config(reverse('drowssap.eladneerg'), 'Troy Barnes', false)",
        "SELECT set_config(format('%s%s', 'greendale.pass', 'word'), 'Troy Barnes', false)",
        "SELECT set_config(translate('greendale.pxssword', 'x', 'a'), 'Troy Barnes', false)",
        "SELECT set_config(string_to_array('greendale.password', '')[1], 'Troy Barnes', false)",
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Postgres, secret_sql),
            "expected credential detection for array/typed GUC SQL: {secret_sql}"
        );
    }

    for computed_sql in [
        "SELECT set_config(('{greendale.timezone, NULL}'::text[])[1], 'UTC', false)",
        r"SELECT set_config(('{greendale.pass\ }'::text[])[1], 'UTC', false)",
        r#"SELECT set_config(('{ " greendale.password " }'::text[])[1], 'UTC', false)"#,
        "SELECT set_config(array_to_string(ARRAY['work', 'mem'], '_'), '4MB', false)",
        "SELECT set_config(string_to_array('work_mem,search_path', ',')[1], '4MB', false)",
    ] {
        assert!(
            sql_sets_credentials(DbEngine::Postgres, computed_sql),
            "computed set_config expression must fail closed: {computed_sql}"
        );
    }

    // 2. Aborted transaction rolls back session scs on COMMIT to pre-tx state.
    let aborted_mode = SqlSessionMode::postgres_mode(Some(PostgresSessionMode {
        standard_conforming_strings: false,
        reset_standard_conforming_strings: Some(true),
        tx_state: Some(PostgresTxState::Aborted),
        session_standard_conforming_strings: Some(false),
        pre_tx_standard_conforming_strings: Some(true),
        savepoints: Vec::new(),
        has_untracked_savepoints: false,
        ..PostgresSessionMode::new(false)
    }));
    let aborted_commit_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        "SET standard_conforming_strings = off; COMMIT; SELECT 'Troy\\'; SELECT 2; -- '",
        DbEngine::Postgres,
        aborted_mode.clone(),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        aborted_commit_stmts,
        vec![
            "SET standard_conforming_strings = off",
            "COMMIT",
            "SELECT 'Troy\\'",
            "SELECT 2",
        ]
    );
    let aborted_safety = evaluate_sql_safety_for_engine_with_session_mode(
        DbEngine::Postgres,
        SafetyPolicy::ConfirmWrites,
        EnvTag::Production,
        &[
            "COMMIT".to_owned(),
            "SELECT 'Troy\\'; DROP TABLE students; -- '".to_owned(),
        ],
        aborted_mode.clone(),
    );
    assert!(matches!(
        aborted_safety,
        SafetyDecision::RequireConfirmation { is_destructive: true, .. }
    ));
    assert!(sql_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        "COMMIT; SELECT 'Troy\\'; ALTER ROLE troy PASSWORD ''greendale-placeholder'';",
        aborted_mode.clone(),
    ));

    // ROLLBACK TO SAVEPOINT recovers an aborted transaction back to the savepoint snapshot.
    let aborted_with_sp = SqlSessionMode::postgres_mode(Some(
        PostgresSessionMode {
            standard_conforming_strings: false,
            reset_standard_conforming_strings: Some(true),
            tx_state: Some(PostgresTxState::Aborted),
            session_standard_conforming_strings: Some(false),
            pre_tx_standard_conforming_strings: Some(true),
            savepoints: Vec::new(),
            has_untracked_savepoints: false,
            ..PostgresSessionMode::new(false)
        }
        .with_savepoint("greendale", Some(false), Some(false)),
    ));
    let recover_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        "ROLLBACK TO SAVEPOINT greendale; SELECT 'Troy\\'; SELECT 2; -- '",
        DbEngine::Postgres,
        aborted_with_sp,
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        recover_stmts,
        vec![
            "ROLLBACK TO SAVEPOINT greendale",
            "SELECT 'Troy\\'; SELECT 2; -- '",
        ]
    );

    // 3. Cross-run named savepoint snapshot accurately restores active scs across ROLLBACK TO,
    //    and identifiers > 63 bytes truncate at UTF-8 character boundaries matching PostgreSQL.
    let saved_sp_mode = SqlSessionMode::postgres_mode(Some(
        PostgresSessionMode::new(true)
            .with_tx_state(Some(PostgresTxState::InTransaction))
            .with_savepoint("greendale", Some(false), Some(true)),
    ));
    let sp_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        "ROLLBACK TO greendale; SELECT 'Troy\\'; SELECT 2; -- '",
        DbEngine::Postgres,
        saved_sp_mode,
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        sp_stmts,
        vec![
            "ROLLBACK TO greendale",
            "SELECT 'Troy\\'; SELECT 2; -- '",
        ]
    );

    let sp_prefix = "g".repeat(60);
    let sp_first = format!("{sp_prefix}abc_first");
    let sp_second = format!("{sp_prefix}abc_second");
    let collided_sql = format!(
        "BEGIN; SAVEPOINT {sp_first}; SET LOCAL standard_conforming_strings = off; SAVEPOINT {sp_second}; SET LOCAL standard_conforming_strings = on; ROLLBACK TO SAVEPOINT {sp_first}; SELECT 'Troy\\'; SELECT 2; -- '"
    );
    let collided_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        &collided_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        collided_stmts,
        vec![
            "BEGIN",
            &format!("SAVEPOINT {sp_first}"),
            "SET LOCAL standard_conforming_strings = off",
            &format!("SAVEPOINT {sp_second}"),
            "SET LOCAL standard_conforming_strings = on",
            &format!("ROLLBACK TO SAVEPOINT {sp_first}"),
            "SELECT 'Troy\\'; SELECT 2; -- '",
        ]
    );

    let utf8_prefix = "a".repeat(62);
    let utf8_sp1 = format!("\"{utf8_prefix}é1\"");
    let utf8_sp2 = format!("\"{utf8_prefix}é2\"");
    let utf8_sql = format!(
        "BEGIN; SAVEPOINT {utf8_sp1}; SET LOCAL standard_conforming_strings = off; SAVEPOINT {utf8_sp2}; SET LOCAL standard_conforming_strings = on; ROLLBACK TO SAVEPOINT {utf8_sp1}; SELECT 'Troy\\'; SELECT 2; -- '"
    );
    let utf8_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        &utf8_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres_mode(Some(
            PostgresSessionMode::new(true).with_server_encoding(Some(PostgresServerEncoding::Utf8)),
        )),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        utf8_stmts,
        vec![
            "BEGIN",
            &format!("SAVEPOINT {utf8_sp1}"),
            "SET LOCAL standard_conforming_strings = off",
            &format!("SAVEPOINT {utf8_sp2}"),
            "SET LOCAL standard_conforming_strings = on",
            &format!("ROLLBACK TO SAVEPOINT {utf8_sp1}"),
            "SELECT 'Troy\\'; SELECT 2; -- '",
        ]
    );

    // Non-UTF8 LATIN1 (SingleByte) savepoint identifiers keep 32x 'é' + 'a'/'b' (33 bytes) distinct,
    // whereas UTF-8 truncates both to 62 bytes (31x 'é') and collides them.
    let lat_prefix = "é".repeat(32);
    let lat_sp1 = format!("\"{lat_prefix}a\"");
    let lat_sp2 = format!("\"{lat_prefix}b\"");
    let lat_sql = format!(
        "BEGIN; SAVEPOINT {lat_sp1}; SET LOCAL standard_conforming_strings = off; SAVEPOINT {lat_sp2}; SET LOCAL standard_conforming_strings = on; ROLLBACK TO SAVEPOINT {lat_sp1}; SELECT 'Troy\\'; SELECT 2; -- '"
    );
    let lat_single_byte_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        &lat_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres_mode(Some(
            PostgresSessionMode::new(true)
                .with_server_encoding(Some(PostgresServerEncoding::from_pg_name("LATIN1"))),
        )),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        lat_single_byte_stmts,
        vec![
            "BEGIN",
            &format!("SAVEPOINT {lat_sp1}"),
            "SET LOCAL standard_conforming_strings = off",
            &format!("SAVEPOINT {lat_sp2}"),
            "SET LOCAL standard_conforming_strings = on",
            &format!("ROLLBACK TO SAVEPOINT {lat_sp1}"),
            "SELECT 'Troy\\'",
            "SELECT 2",
        ]
    );
    let lat_utf8_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        &lat_sql,
        DbEngine::Postgres,
        SqlSessionMode::postgres_mode(Some(
            PostgresSessionMode::new(true).with_server_encoding(Some(PostgresServerEncoding::Utf8)),
        )),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        lat_utf8_stmts,
        vec![
            "BEGIN",
            &format!("SAVEPOINT {lat_sp1}"),
            "SET LOCAL standard_conforming_strings = off",
            &format!("SAVEPOINT {lat_sp2}"),
            "SET LOCAL standard_conforming_strings = on",
            &format!("ROLLBACK TO SAVEPOINT {lat_sp1}"),
            "SELECT 'Troy\\'; SELECT 2; -- '",
        ]
    );

    // LATIN1 unquoted 63*'a' + 'é' truncates to exact 63*'a', shadowing an earlier 63*'a' savepoint.
    let sp_a63 = "a".repeat(63);
    let sp_a63_e = format!("{sp_a63}é");
    let latin1_mode = SqlSessionMode::postgres_mode(Some(
        PostgresSessionMode::new(true)
            .with_server_encoding(Some(PostgresServerEncoding::from_pg_name("LATIN1"))),
    ));
    let shadow_sql = format!(
        "BEGIN; SAVEPOINT {sp_a63}; SET LOCAL standard_conforming_strings = off; SAVEPOINT {sp_a63_e}; SET LOCAL standard_conforming_strings = on; ROLLBACK TO SAVEPOINT {sp_a63}; SELECT 'Troy\\'; SELECT 2; -- '"
    );
    let shadow_stmts: Vec<String> = split_statements_for_engine_with_session_mode(
        &shadow_sql,
        DbEngine::Postgres,
        latin1_mode.clone(),
    )
    .into_iter()
    .map(|s| s.sql)
    .collect();
    assert_eq!(
        shadow_stmts,
        vec![
            "BEGIN",
            &format!("SAVEPOINT {sp_a63}"),
            "SET LOCAL standard_conforming_strings = off",
            &format!("SAVEPOINT {sp_a63_e}"),
            "SET LOCAL standard_conforming_strings = on",
            &format!("ROLLBACK TO SAVEPOINT {sp_a63}"),
            "SELECT 'Troy\\'; SELECT 2; -- '",
        ]
    );

    // Untracked unquoted non-ASCII savepoint after tracked ASCII sp1 clears sp1 so rollback fails closed.
    let untracked_batch = vec![
        "BEGIN".to_owned(),
        "SET LOCAL standard_conforming_strings = off".to_owned(),
        "SAVEPOINT sp1".to_owned(),
        "SET LOCAL standard_conforming_strings = on".to_owned(),
        "SAVEPOINT aÉ".to_owned(),
        "ROLLBACK TO SAVEPOINT sp1".to_owned(),
        r"SELECT '\', set_config('greendale.password', 'Troy Barnes', false) -- '".to_owned(),
    ];
    let combined_untracked = untracked_batch.join("; ");
    assert!(
        batch_sets_credentials_with_session_mode(
            DbEngine::Postgres,
            &untracked_batch,
            &combined_untracked,
            latin1_mode.clone(),
        ),
        "expected fail-closed credential check after rollback across untracked non-ASCII savepoint"
    );
    assert!(!batch_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &["SELECT 1".to_owned()],
        "",
        latin1_mode,
    ));

    // Truncating, padded, or non-text/custom array casts must fail closed or apply exact semantics.
    let non_ascii_name = format!(
        "SELECT set_config('{}.password'::name, 'Troy Barnes', false)",
        "é".repeat(31)
    );
    for secret_sql in [
        "SELECT set_config(array_to_string(ARRAY['greendale.passwordextra']::varchar(18)[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.passwordextra']::character varying(18)[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.passwordextra']::pg_catalog.varchar(18)[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.passwordextra']::_varchar(18), ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(CAST(ARRAY['greendale.passwordextra'] AS varchar(18)[]), ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.passwordextra']::bpchar(18)[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['safe']::varchar(18)[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['safe']::char[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['safe']::my_domain[], ''), 'Troy Barnes', false)",
        "SELECT set_config((ARRAY['greendale.passwordextra']::varchar(18)[])[1], 'Troy Barnes', false)",
        "SELECT set_config('greendale.' || array_to_string(ARRAY['passwordextra']::varchar(18)[], ''), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string('{greendale.passwordextra}'::varchar(18)[], ''), 'Troy Barnes', false)",
        "SELECT set_config('greendale.password   '::bpchar, 'Troy Barnes', false)",
        "SELECT set_config('greendale.passwordextra'::\"TEXT\", 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.passwordextra']::\"TEXT\"[], ''), 'Troy Barnes', false)",
        "SELECT set_config(replace(array_to_string(ARRAY['greendale.db'::char(13), 'password'], ''), ' ', '_'), 'Troy Barnes', false)",
        "SELECT set_config(replace(concat('greendale.db'::char(13), 'password'), ' ', '_'), 'Troy Barnes', false)",
        "SELECT set_config(replace(concat_ws('', 'greendale.db'::bpchar(13), 'password'), ' ', '_'), 'Troy Barnes', false)",
        "SELECT set_config('greendale.db '::char(13), 'Troy Barnes', false)",
        "SELECT set_config('greendale.' || ''::char || 'password', 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['a'::varchar(18), 'greendale.passwordextra'], ','), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY[NULL::varchar(18), 'greendale.passwordextra'], ''), 'Troy Barnes', false)",
        "SELECT set_config(replace(array_to_string(ARRAY['1234567890123'::char(13), 'greendale.db', 'password'], ''), ' ', '_'), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['a'::varchar(18), 'greendale.passwordextra']::text[], ','), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY[ARRAY['a'::varchar(18)], ARRAY['greendale.passwordextra']], ','), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['a'::varchar(18)] || ARRAY['greendale.passwordextra'], ','), 'Troy Barnes', false)",
        "SELECT set_config((ARRAY['a'::varchar(18), 'greendale.passwordextra'])[2], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY[NULL::varchar(18), 'greendale.passwordextra'])[2], 'Troy Barnes', false)",
        "SELECT set_config(COALESCE(NULL::varchar(18), 'greendale.passwordextra'), 'Troy Barnes', false)",
        "SELECT set_config(CASE WHEN false THEN 'a'::varchar(18) ELSE 'greendale.passwordextra' END, 'Troy Barnes', false)",
        "SELECT set_config(CASE WHEN false THEN NULL::varchar(18) ELSE 'greendale.passwordextra' END, 'Troy Barnes', false)",
        "SELECT set_config(NULLIF('greendale.passwordextra', 'a'::varchar(18)), 'Troy Barnes', false)",
        "SELECT set_config((ARRAY_APPEND(ARRAY['a'::varchar(18)], 'greendale.passwordextra'))[2], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY_PREPEND('a'::varchar(18), ARRAY['greendale.passwordextra']))[2], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY_CAT(ARRAY['a'::varchar(18)], ARRAY['greendale.passwordextra']))[2], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY[ARRAY['a'::varchar(18)], ARRAY['greendale.passwordextra']])[2][1], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY['a'::varchar(18)] || ARRAY['greendale.passwordextra'])[2], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY['work_mem'] || '{greendale.password}')[2], 'Troy Barnes', false)",
        "SELECT set_config((array_cat(ARRAY['work_mem'], '{greendale.password}'))[2], 'Troy Barnes', false)",
        "SELECT set_config((CASE WHEN true THEN '{greendale.password}' ELSE ARRAY['work_mem'] END)[1], 'Troy Barnes', false)",
        "SELECT set_config((COALESCE(NULL, '{greendale.password}', ARRAY['work_mem']))[1], 'Troy Barnes', false)",
        "SELECT set_config((NULLIF('{greendale.password}', ARRAY['work_mem']))[1], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY_APPEND('{greendale.password}', 'work_mem'))[1], 'Troy Barnes', false)",
        "SELECT set_config((ARRAY_PREPEND('work_mem', '{greendale.password}'))[2], 'Troy Barnes', false)",
        r"SELECT set_config((ARRAY['work_mem'] || '[1:1]={greendale.pass\word}')[2], 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['greendale.passwordextra'::varchar(18)], ''), 'Troy Barnes', false)",
        "SELECT set_config('greendale.pass' || (ARRAY['word'])[1], 'Troy Barnes', false)",
        "SELECT set_config(('greendale.pass' || 'word' || ARRAY['work_mem'])[1], 'Troy Barnes', false)",
        "SELECT set_config((('greendale.pass' || 'word') || ARRAY['work_mem'])[1], 'Troy Barnes', false)",
        "SELECT set_config(('greendale.pass' || (ARRAY['word'])[1])::text, 'Troy Barnes', false)",
        "SELECT set_config(('greendale.pass' || (ARRAY['word'])[1])[1], 'Troy Barnes', false)",
        "SELECT set_config(('{greendale.password}' || ARRAY['work_mem'])[1], 'Troy Barnes', false)",
        "SELECT set_config(array_to_string('{greendale.pass}' || ARRAY['word'], ''), 'Troy Barnes', false)",
        "SELECT set_config('greendale.pass' || 'word'::varchar(4), 'Troy Barnes', false)",
        "SELECT set_config(array_to_string(ARRAY['work', 'mem']::text[], '_'), '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY['work', 'mem']::varchar[], '_'), '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY['work', 'mem']::character varying[], '_'), '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY['work', 'mem']::pg_catalog._text, '_'), '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY['work', 'mem']::pg_catalog.varchar[], '_'), '4MB', false)",
        "SELECT set_config(COALESCE(NULL::text, 'work_mem'), '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY[NULL::text, 'work', 'mem'], '_'), '4MB', false)",
        "SELECT set_config((ARRAY['a'::text, 'work_mem'])[2], '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY['work_mem'::varchar(8)], ''), '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY['work_' || 'mem'], ''), '4MB', false)",
        "SELECT set_config(array_to_string(ARRAY['work_' || 'mem', 'x'], '_'), '4MB', false)",
        "SELECT set_config((ARRAY['work_mem'] || '{search_path}')[2], 'public', false)",
        "SELECT set_config(array_to_string(ARRAY['work', 'mem']::text ARRAY, '_'), '4MB', false)",
        "SELECT set_config('work_' || 'mem'::varchar(8), '4MB', false)",
        "SELECT set_config(('work_' || ARRAY['mem'])[1], '4MB', false)",
        "SELECT set_config(('{search_path}' || ARRAY['work_mem'])[1], 'public', false)",
        "SELECT set_config(array_to_string(ARRAY['work_mem'] || ARRAY['search_path'], '_'), '4MB', false)",
        "SELECT set_config('greendale.password', NULL::text || 'Troy Barnes', false)",
        "SELECT set_config('greendale.password', NULL::custom_domain, false)",
        "SELECT pg_catalog.set_config(N'greendale.app.password ', 'Troy Barnes', false)",
        "SELECT set_config(n'greendale.app.password ', 'Troy Barnes', false)",
        "SELECT set_config(N'work_mem', '4MB', false)",
        "SELECT set_config(B'0101', '4MB', false)",
        "SELECT set_config('greendale.app.password_extra'::text, 'Troy Barnes', false)",
        "SELECT set_config(CAST('greendale.app.password_extra' AS text), 'Troy Barnes', false)",
        "SELECT set_config(text 'greendale.app.password_extra', 'Troy Barnes', false)",
        "SELECT set_config('work_mem'::text, '4MB', false)",
        "SELECT set_config('work_mem'::varchar, '4MB', false)",
        "SELECT set_config('work_mem'::character varying, '4MB', false)",
        "SELECT set_config('work_mem'::\"text\", '4MB', false)",
        "SELECT set_config(CAST('work_mem' AS text), '4MB', false)",
        "SELECT set_config(text 'work_mem', '4MB', false)",
        non_ascii_name.as_str(),
    ] {
        assert!(
            sql_sets_credentials_with_session_mode(DbEngine::Postgres, secret_sql, SqlSessionMode::postgres(true)),
            "expected credential exclusion for array cast expression: {secret_sql}"
        );
    }
    for scs in [true, false] {
        assert!(sql_sets_credentials_with_session_mode(
            DbEngine::Postgres,
            "SELECT pg_catalog.set_config(N'greendale.app.password ', 'Troy Barnes', false)",
            SqlSessionMode::postgres(scs),
        ));
    }
    assert!(sql_sets_credentials(
        DbEngine::Mysql,
        "ALTER USER 'troy'@'localhost' IDENTIFIED BY N'Troy Barnes'"
    ));
    for safe_sql in [
        "SELECT set_config('work_mem', '4MB', false)",
        "SELECT set_config('work_mem'::pg_catalog.text, '4MB', false)",
        "SELECT set_config('work_mem'::pg_catalog.varchar, '4MB', false)",
        "SELECT set_config('work_mem'::pg_catalog.\"text\", '4MB', false)",
        "SELECT set_config(CAST('work_mem' AS pg_catalog.text), '4MB', false)",
        "SELECT set_config(pg_catalog.text 'work_mem', '4MB', false)",
        "SELECT set_config(('search_path'), 'public', false)",
        "SELECT set_config($$search_path$$, 'public', false)",
        "SELECT set_config(E'search_path', 'public', false)",
        "SELECT set_config(U&'search_path', 'public', false)",
        "SELECT set_config(setting_name => 'greendale.app.password', new_value => NULL, is_local => false)",
        "SELECT set_config('greendale.app.password', NULL::text, false)",
        "SELECT set_config(new_value => CAST(NULL AS text), setting_name => 'greendale.app.password', is_local => false)",
    ] {
        assert!(
            !sql_sets_credentials_with_session_mode(DbEngine::Postgres, safe_sql, SqlSessionMode::postgres(true)),
            "did not expect false positive for non-truncating literal cast: {safe_sql}"
        );
    }
}

#[test]
fn postgres_splitter_preserves_tx_and_savepoints_across_uncertain_set_config() {
    let repro = "BEGIN; SET LOCAL standard_conforming_strings = off; SAVEPOINT greendale; SELECT set_config('work_mem'::text, '4MB', false); ROLLBACK TO greendale; SELECT 'Troy\\'; SELECT 2; -- ';";
    let stmts = split_statements_for_engine_with_session_mode(
        repro,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(stmts.len(), 6, "unexpected split for {repro}: {stmts:#?}");
    assert_eq!(stmts[5].sql, "SELECT 'Troy\\'; SELECT 2; -- '");

    let chained = "BEGIN; SET LOCAL standard_conforming_strings = off; SAVEPOINT greendale; SELECT set_config('work_mem'::text, '4MB', false); SELECT set_config('lock_timeout'::text, '1s', false); ROLLBACK TO greendale; SELECT 'Troy\\'; SELECT 2; -- ';";
    let chained_stmts = split_statements_for_engine_with_session_mode(
        chained,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(
        chained_stmts.len(),
        7,
        "unexpected split for {chained}: {chained_stmts:#?}"
    );
    assert_eq!(chained_stmts[6].sql, "SELECT 'Troy\\'; SELECT 2; -- '");

    let rollback = "SET standard_conforming_strings = off; BEGIN; SET LOCAL standard_conforming_strings = on; SELECT set_config('work_mem'::text, '4MB', false); ROLLBACK; SELECT 'Troy\\'; SELECT 2; -- ';";
    let rollback_stmts = split_statements_for_engine_with_session_mode(
        rollback,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(
        rollback_stmts.len(),
        6,
        "unexpected split for {rollback}: {rollback_stmts:#?}"
    );
    assert_eq!(rollback_stmts[5].sql, "SELECT 'Troy\\'; SELECT 2; -- '");

    let call_reset = "BEGIN; SET LOCAL standard_conforming_strings = off; SAVEPOINT greendale; CALL greendale_proc(); ROLLBACK TO greendale; SELECT 'Troy\\'; SELECT 2; -- ';";
    let call_stmts = split_statements_for_engine_with_session_mode(
        call_reset,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(
        call_stmts.len(),
        7,
        "CALL must conservatively clear savepoints before fallback: {call_stmts:#?}"
    );

    let s4_repro = "BEGIN; SELECT set_config('work_mem'::text, '4MB', false); SAVEPOINT greendale; SELECT set_config('lock_timeout'::text, '1s', false); ROLLBACK TO greendale; SELECT 'Troy\\'; Greendale'; SELECT 2;";
    let s4_stmts = split_statements_for_engine_with_session_mode(
        s4_repro,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(s4_stmts.len(), 7, "unexpected split for {s4_repro}: {s4_stmts:#?}");
    assert_eq!(s4_stmts[5].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s4_stmts[6].sql, "SELECT 2");

    let s4_set_local_undone = "BEGIN; SELECT set_config('work_mem'::text, '4MB', false); SAVEPOINT greendale; SET LOCAL standard_conforming_strings = on; ROLLBACK TO greendale; SELECT 'Troy\\'; Greendale'; SELECT 2;";
    let s4_undone_stmts = split_statements_for_engine_with_session_mode(
        s4_set_local_undone,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s4_undone_stmts.len(),
        7,
        "unexpected split for {s4_set_local_undone}: {s4_undone_stmts:#?}"
    );
    assert_eq!(s4_undone_stmts[5].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s4_undone_stmts[6].sql, "SELECT 2");

    let s5_empty_after_release = "BEGIN; SAVEPOINT greendale; RELEASE SAVEPOINT greendale; ; -- comment only\n; SET LOCAL standard_conforming_strings = off; SELECT 'Troy\\'; Greendale'; SELECT 2;";
    let s5_stmts = split_statements_for_engine_with_session_mode(
        s5_empty_after_release,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(
        s5_stmts.len(),
        6,
        "unexpected split for {s5_empty_after_release}: {s5_stmts:#?}"
    );
    assert_eq!(s5_stmts[4].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s5_stmts[5].sql, "SELECT 2");

    let s6_repro = "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSAVEPOINT greendale;\nSELECT 'Troy\\'; ROLLBACK TO greendale; Barnes';\nSELECT 2;";
    let s6_stmts = split_statements_for_engine_with_session_mode(
        s6_repro,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(s6_stmts.len(), 5, "unexpected split for {s6_repro}: {s6_stmts:#?}");
    assert_eq!(s6_stmts[2].sql, "SAVEPOINT greendale");
    assert_eq!(s6_stmts[3].sql, "SELECT 'Troy\\'; ROLLBACK TO greendale; Barnes'");
    assert_eq!(s6_stmts[4].sql, "SELECT 2");

    let s6_syntax_remainder = "BEGIN; SELECT set_config('work_mem'::text, '4MB', false); SAVEPOINT greendale; SELECT 'Troy\\'; ROLLBACK TO greendale; FROM invalid -- \\''; SELECT 2;";
    let s6_syntax_stmts = split_statements_for_engine_with_session_mode(
        s6_syntax_remainder,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s6_syntax_stmts.len(),
        5,
        "unexpected split for {s6_syntax_remainder}: {s6_syntax_stmts:#?}"
    );
    assert_eq!(
        s6_syntax_stmts[3].sql,
        "SELECT 'Troy\\'; ROLLBACK TO greendale; FROM invalid -- \\''"
    );
    assert_eq!(s6_syntax_stmts[4].sql, "SELECT 2");

    let s6_scs_on_symmetric = "BEGIN; SELECT set_config('work_mem'::text, '4MB', false); SAVEPOINT greendale; SELECT 'Troy\\', 'Abed; ROLLBACK TO greendale; Jeff'; SELECT 2;";
    let s6_on_stmts = split_statements_for_engine_with_session_mode(
        s6_scs_on_symmetric,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(
        s6_on_stmts.len(),
        5,
        "unexpected split for {s6_scs_on_symmetric}: {s6_on_stmts:#?}"
    );
    assert_eq!(
        s6_on_stmts[3].sql,
        "SELECT 'Troy\\', 'Abed; ROLLBACK TO greendale; Jeff'"
    );
    assert_eq!(s6_on_stmts[4].sql, "SELECT 2");

    let s6_both_pause = "BEGIN; SELECT set_config('work_mem'::text, '4MB', false); SAVEPOINT greendale; SELECT 'Troy\\'; ROLLBACK TO greendale; Barnes'; SELECT set_config('lock_timeout'::text, '1s', false); SELECT 2;";
    let s6_both_stmts = split_statements_for_engine_with_session_mode(
        s6_both_pause,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s6_both_stmts.len(),
        6,
        "unexpected split for {s6_both_pause}: {s6_both_stmts:#?}"
    );
    assert_eq!(
        s6_both_stmts[3].sql,
        "SELECT 'Troy\\'; ROLLBACK TO greendale; Barnes'"
    );
    assert_eq!(
        s6_both_stmts[4].sql,
        "SELECT set_config('lock_timeout'::text, '1s', false)"
    );
    assert_eq!(s6_both_stmts[5].sql, "SELECT 2");

    let s6_r39_repro = "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSAVEPOINT greendale;\nSELECT 'Troy\\'; Greendale';\nSELECT set_config('lock_timeout'::text, '1s', false);\nSELECT 'Abed; ROLLBACK TO greendale; Jeff';\nSELECT 2;";
    let s6_r39_stmts = split_statements_for_engine_with_session_mode(
        s6_r39_repro,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s6_r39_stmts.len(),
        7,
        "unexpected split for {s6_r39_repro}: {s6_r39_stmts:#?}"
    );
    assert_eq!(s6_r39_stmts[3].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(
        s6_r39_stmts[4].sql,
        "SELECT set_config('lock_timeout'::text, '1s', false)"
    );
    assert_eq!(s6_r39_stmts[5].sql, "SELECT 'Abed; ROLLBACK TO greendale; Jeff'");
    assert_eq!(s6_r39_stmts[6].sql, "SELECT 2");

    let s6_r39_escaped = "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSAVEPOINT greendale;\nSELECT 'Troy\\'; Greendale';\nSELECT set_config('lock_timeout'::text, '1s', false);\nSELECT 'Abed\\'; ROLLBACK TO greendale; Jeff';\nSELECT 2;";
    let s6_r39_esc_stmts = split_statements_for_engine_with_session_mode(
        s6_r39_escaped,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s6_r39_esc_stmts.len(),
        7,
        "unexpected split for {s6_r39_escaped}: {s6_r39_esc_stmts:#?}"
    );
    assert_eq!(
        s6_r39_esc_stmts[5].sql,
        "SELECT 'Abed\\'; ROLLBACK TO greendale; Jeff'"
    );
    assert_eq!(s6_r39_esc_stmts[6].sql, "SELECT 2");

    let s7_repro = "BEGIN;\nSAVEPOINT greendale;\nSET LOCAL standard_conforming_strings = off;\nSAVEPOINT greendale;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- ';\nSELECT set_config('lock_timeout'::text, '1s', false);\nROLLBACK TO greendale;\nSELECT 'Abed\\'; Jeff';\nSELECT 2;";
    let s7_stmts = split_statements_for_engine_with_session_mode(
        s7_repro,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(s7_stmts.len(), 10, "unexpected split for {s7_repro}: {s7_stmts:#?}");
    assert_eq!(
        s7_stmts[5].sql,
        "SELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- '"
    );
    assert_eq!(s7_stmts[8].sql, "SELECT 'Abed\\'; Jeff'");
    assert_eq!(s7_stmts[9].sql, "SELECT 2");

    let s8_repro = "BEGIN;\nSAVEPOINT greendale;\nSET LOCAL standard_conforming_strings = off;\nSAVEPOINT greendale;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- ';\nSELECT set_config('lock_timeout'::text, '1s', false);\nROLLBACK TO greendale;\nSELECT 'Abed\\'; Jeff';\nSELECT 2;\nSELECT \"";
    let s8_stmts = split_statements_for_engine_with_session_mode(
        s8_repro,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(s8_stmts.len(), 11, "unexpected split for {s8_repro}: {s8_stmts:#?}");
    assert_eq!(
        s8_stmts[5].sql,
        "SELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- '"
    );
    assert_eq!(s8_stmts[8].sql, "SELECT 'Abed\\'; Jeff'");
    assert_eq!(s8_stmts[9].sql, "SELECT 2");
    assert_eq!(s8_stmts[10].sql, "SELECT \"");

    let s8_single_segment = "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; Greendale';\nSELECT 2;\nSELECT \"";
    let s8_single_stmts = split_statements_for_engine_with_session_mode(
        s8_single_segment,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s8_single_stmts.len(),
        5,
        "unexpected split for {s8_single_segment}: {s8_single_stmts:#?}"
    );
    assert_eq!(s8_single_stmts[2].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s8_single_stmts[3].sql, "SELECT 2");
    assert_eq!(s8_single_stmts[4].sql, "SELECT \"");

    let s9_single_quote = "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; Greendale';\nSELECT 2;\nSELECT '";
    let s9_single_stmts = split_statements_for_engine_with_session_mode(
        s9_single_quote,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s9_single_stmts.len(),
        5,
        "unexpected split for {s9_single_quote}: {s9_single_stmts:#?}"
    );
    assert_eq!(s9_single_stmts[2].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s9_single_stmts[3].sql, "SELECT 2");
    assert_eq!(s9_single_stmts[4].sql, "SELECT '");

    let s9_multi_quote = "BEGIN;\nSAVEPOINT greendale;\nSET LOCAL standard_conforming_strings = off;\nSAVEPOINT greendale;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- ';\nSELECT set_config('lock_timeout'::text, '1s', false);\nROLLBACK TO greendale;\nSELECT 'Abed\\'; Jeff';\nSELECT 2;\nSELECT '";
    let s9_multi_stmts = split_statements_for_engine_with_session_mode(
        s9_multi_quote,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(
        s9_multi_stmts.len(),
        11,
        "unexpected split for {s9_multi_quote}: {s9_multi_stmts:#?}"
    );
    assert_eq!(
        s9_multi_stmts[5].sql,
        "SELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- '"
    );
    assert_eq!(s9_multi_stmts[8].sql, "SELECT 'Abed\\'; Jeff'");
    assert_eq!(s9_multi_stmts[9].sql, "SELECT 2");
    assert_eq!(s9_multi_stmts[10].sql, "SELECT '");

    let s10_table_stmt = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; TABLE greendale; SELECT 2;";
    let s10_stmts = split_statements_for_engine_with_session_mode(
        s10_table_stmt,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s10_stmts.len(),
        4,
        "unexpected split for {s10_table_stmt}: {s10_stmts:#?}"
    );
    assert_eq!(s10_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s10_stmts[2].sql, "TABLE greendale");
    assert_eq!(s10_stmts[3].sql, "SELECT 2");

    for unfinished_tail in ["SELECT E'", "SELECT $$", "SELECT 3 /*"] {
        let s11_sql = format!(
            "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; Greendale';\nSELECT 2;\n{unfinished_tail}"
        );
        let s11_stmts = split_statements_for_engine_with_session_mode(
            &s11_sql,
            DbEngine::Postgres,
            SqlSessionMode::postgres(false),
        );
        assert_eq!(
            s11_stmts.len(),
            5,
            "unexpected split for {s11_sql}: {s11_stmts:#?}"
        );
        assert_eq!(s11_stmts[2].sql, "SELECT 'Troy\\'; Greendale'");
        assert_eq!(s11_stmts[3].sql, "SELECT 2");
        assert_eq!(s11_stmts[4].sql, unfinished_tail);
    }

    let s11_comment_only = "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; Greendale';\nSELECT 2;\n/* unfinished";
    let s11_comment_stmts = split_statements_for_engine_with_session_mode(
        s11_comment_only,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s11_comment_stmts.len(),
        4,
        "unexpected split for {s11_comment_only}: {s11_comment_stmts:#?}"
    );
    assert_eq!(s11_comment_stmts[2].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s11_comment_stmts[3].sql, "SELECT 2");

    let s11_comment_with_table = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; TABLE greendale; SELECT 2; /* unfinished";
    let s11_table_stmts = split_statements_for_engine_with_session_mode(
        s11_comment_with_table,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s11_table_stmts.len(),
        4,
        "unexpected split for {s11_comment_with_table}: {s11_table_stmts:#?}"
    );
    assert_eq!(s11_table_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s11_table_stmts[2].sql, "TABLE greendale");
    assert_eq!(s11_table_stmts[3].sql, "SELECT 2");

    let s12_repeated_table = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; TABLE greendale; TABLE greendale; SELECT 2;";
    let s12_stmts = split_statements_for_engine_with_session_mode(
        s12_repeated_table,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s12_stmts.len(),
        5,
        "unexpected split for {s12_repeated_table}: {s12_stmts:#?}"
    );
    assert_eq!(s12_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s12_stmts[2].sql, "TABLE greendale");
    assert_eq!(s12_stmts[3].sql, "TABLE greendale");
    assert_eq!(s12_stmts[4].sql, "SELECT 2");

    let s12_qualified_table = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; TABLE public.greendale ORDER BY id LIMIT 1; TABLE greendale; SELECT 2;";
    let s12_qual_stmts = split_statements_for_engine_with_session_mode(
        s12_qualified_table,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s12_qual_stmts.len(),
        5,
        "unexpected split for {s12_qualified_table}: {s12_qual_stmts:#?}"
    );
    assert_eq!(s12_qual_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(
        s12_qual_stmts[2].sql,
        "TABLE public.greendale ORDER BY id LIMIT 1"
    );
    assert_eq!(s12_qual_stmts[3].sql, "TABLE greendale");
    assert_eq!(s12_qual_stmts[4].sql, "SELECT 2");

    let s12_invalid_trailing = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; TABLE greendale invalid_junk'; SELECT 2;";
    let s12_inv_stmts = split_statements_for_engine_with_session_mode(
        s12_invalid_trailing,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s12_inv_stmts.len(),
        3,
        "unexpected split for {s12_invalid_trailing}: {s12_inv_stmts:#?}"
    );
    assert_eq!(
        s12_inv_stmts[1].sql,
        "SELECT 'Troy\\'; TABLE greendale invalid_junk'"
    );
    assert_eq!(s12_inv_stmts[2].sql, "SELECT 2");

    for unfinished_tail in [
        "SELECT concat('",
        "SELECT concat(E'",
        "SELECT concat($$",
        "SELECT (1 + '",
        "SELECT concat(1, /*",
    ] {
        let s13_sql = format!(
            "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; Greendale';\nSELECT 2;\n{unfinished_tail}"
        );
        let s13_stmts = split_statements_for_engine_with_session_mode(
            &s13_sql,
            DbEngine::Postgres,
            SqlSessionMode::postgres(false),
        );
        assert_eq!(
            s13_stmts.len(),
            5,
            "unexpected split for {s13_sql}: {s13_stmts:#?}"
        );
        assert_eq!(s13_stmts[2].sql, "SELECT 'Troy\\'; Greendale'");
        assert_eq!(s13_stmts[3].sql, "SELECT 2");
        assert_eq!(s13_stmts[4].sql, unfinished_tail);
    }

    let s13_multi_segment = "BEGIN;\nSAVEPOINT greendale;\nSET LOCAL standard_conforming_strings = off;\nSAVEPOINT greendale;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- ';\nSELECT set_config('lock_timeout'::text, '1s', false);\nROLLBACK TO greendale;\nSELECT 'Abed\\'; Jeff';\nSELECT 2;\nSELECT concat('";
    let s13_multi_stmts = split_statements_for_engine_with_session_mode(
        s13_multi_segment,
        DbEngine::Postgres,
        SqlSessionMode::postgres(true),
    );
    assert_eq!(
        s13_multi_stmts.len(),
        11,
        "unexpected split for {s13_multi_segment}: {s13_multi_stmts:#?}"
    );
    assert_eq!(
        s13_multi_stmts[5].sql,
        "SELECT 'Troy\\'; RELEASE SAVEPOINT greendale; -- '"
    );
    assert_eq!(s13_multi_stmts[8].sql, "SELECT 'Abed\\'; Jeff'");
    assert_eq!(s13_multi_stmts[9].sql, "SELECT 2");
    assert_eq!(s13_multi_stmts[10].sql, "SELECT concat('");

    let s14_only_and_star = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; TABLE ONLY greendale; TABLE public.greendale*; SELECT 2;";
    let s14_stmts = split_statements_for_engine_with_session_mode(
        s14_only_and_star,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s14_stmts.len(),
        5,
        "unexpected split for {s14_only_and_star}: {s14_stmts:#?}"
    );
    assert_eq!(s14_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s14_stmts[2].sql, "TABLE ONLY greendale");
    assert_eq!(s14_stmts[3].sql, "TABLE public.greendale*");
    assert_eq!(s14_stmts[4].sql, "SELECT 2");

    let s14_paren_and_ordered = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; TABLE ONLY (public.greendale); TABLE public.greendale* ORDER BY id LIMIT 1; SELECT 2;";
    let s14_ord_stmts = split_statements_for_engine_with_session_mode(
        s14_paren_and_ordered,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s14_ord_stmts.len(),
        5,
        "unexpected split for {s14_paren_and_ordered}: {s14_ord_stmts:#?}"
    );
    assert_eq!(s14_ord_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s14_ord_stmts[2].sql, "TABLE ONLY (public.greendale)");
    assert_eq!(
        s14_ord_stmts[3].sql,
        "TABLE public.greendale* ORDER BY id LIMIT 1"
    );
    assert_eq!(s14_ord_stmts[4].sql, "SELECT 2");

    for invalid_inside_string in [
        "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; TABLE ONLY greendale*'; SELECT 2;",
        "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; TABLE ONLY greendale invalid_junk'; SELECT 2;",
    ] {
        let inv_stmts = split_statements_for_engine_with_session_mode(
            invalid_inside_string,
            DbEngine::Postgres,
            SqlSessionMode::postgres(false),
        );
        assert_eq!(
            inv_stmts.len(),
            3,
            "unexpected split for {invalid_inside_string}: {inv_stmts:#?}"
        );
        assert_eq!(inv_stmts[2].sql, "SELECT 2");
    }

    let s15_balanced_incomplete = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; SELECT '; SELECT concat(";
    let s15_stmts = split_statements_for_engine_with_session_mode(
        s15_balanced_incomplete,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s15_stmts.len(),
        3,
        "unexpected split for {s15_balanced_incomplete}: {s15_stmts:#?}"
    );
    assert_eq!(s15_stmts[1].sql, "SELECT 'Troy\\'; SELECT '");
    assert_eq!(s15_stmts[2].sql, "SELECT concat(");

    let s15_multiline = "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; SELECT ';\nSELECT 2;\nSELECT concat(";
    let s15_multi_stmts = split_statements_for_engine_with_session_mode(
        s15_multiline,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s15_multi_stmts.len(),
        5,
        "unexpected split for {s15_multiline}: {s15_multi_stmts:#?}"
    );
    assert_eq!(s15_multi_stmts[2].sql, "SELECT 'Troy\\'; SELECT '");
    assert_eq!(s15_multi_stmts[3].sql, "SELECT 2");
    assert_eq!(s15_multi_stmts[4].sql, "SELECT concat(");

    let s16_eof_ident = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; SELECT 2; EOF $$";
    let s16_stmts = split_statements_for_engine_with_session_mode(
        s16_eof_ident,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s16_stmts.len(),
        2,
        "unexpected split for {s16_eof_ident}: {s16_stmts:#?}"
    );
    assert_eq!(s16_stmts[1].sql, "SELECT 'Troy\\'; SELECT 2; EOF $$");

    let s16_eof_ident_closed = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; SELECT 2; EOF $$'; SELECT 3;";
    let s16_closed_stmts = split_statements_for_engine_with_session_mode(
        s16_eof_ident_closed,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s16_closed_stmts.len(),
        3,
        "unexpected split for {s16_eof_ident_closed}: {s16_closed_stmts:#?}"
    );
    assert_eq!(
        s16_closed_stmts[1].sql,
        "SELECT 'Troy\\'; SELECT 2; EOF $$'"
    );
    assert_eq!(s16_closed_stmts[2].sql, "SELECT 3");

    for partial_escape_tail in [
        "SELECT E'\\u20",
        "SELECT E'\\u",
        "SELECT E'\\U0001f",
        "SELECT E'\\x",
        "SELECT E'\\x0",
        "SELECT E'\\x8",
        "SELECT E'\\0",
        "SELECT '\\u20",
        "SELECT U&'\\20",
        "SELECT U&'\\+0001",
    ] {
        let s17_sql = format!(
            "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; Greendale';\nSELECT 2;\n{partial_escape_tail}"
        );
        let s17_stmts = split_statements_for_engine_with_session_mode(
            &s17_sql,
            DbEngine::Postgres,
            SqlSessionMode::postgres(false),
        );
        assert_eq!(
            s17_stmts.len(),
            5,
            "unexpected split for {s17_sql}: {s17_stmts:#?}"
        );
        assert_eq!(s17_stmts[2].sql, "SELECT 'Troy\\'; Greendale'");
        assert_eq!(s17_stmts[3].sql, "SELECT 2");
        assert_eq!(s17_stmts[4].sql, partial_escape_tail);
    }

    for s18_reserved_table in [
        "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; TABLE ONLY (select); -- text'; SELECT 2;",
        "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; TABLE select; -- text'; SELECT 2;",
    ] {
        let s18_stmts = split_statements_for_engine_with_session_mode(
            s18_reserved_table,
            DbEngine::Postgres,
            SqlSessionMode::postgres(false),
        );
        assert_eq!(
            s18_stmts.len(),
            3,
            "unexpected split for {s18_reserved_table}: {s18_stmts:#?}"
        );
        assert_eq!(s18_stmts[2].sql, "SELECT 2");
    }

    let s18_quoted_reserved = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; TABLE ONLY (\"select\"); TABLE \"order\"; SELECT 2;";
    let s18_quoted_stmts = split_statements_for_engine_with_session_mode(
        s18_quoted_reserved,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s18_quoted_stmts.len(),
        5,
        "unexpected split for {s18_quoted_reserved}: {s18_quoted_stmts:#?}"
    );
    assert_eq!(s18_quoted_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s18_quoted_stmts[2].sql, "TABLE ONLY (\"select\")");
    assert_eq!(s18_quoted_stmts[3].sql, "TABLE \"order\"");
    assert_eq!(s18_quoted_stmts[4].sql, "SELECT 2");

    let s19_byte_escapes = "SELECT set_config('work_mem'::text, '4MB', false); SELECT 'Troy\\'; Greendale'; SELECT E'\\xC3\\xA9'; SELECT E'\\303\\251'; SELECT 2;";
    let s19_stmts = split_statements_for_engine_with_session_mode(
        s19_byte_escapes,
        DbEngine::Postgres,
        SqlSessionMode::postgres(false),
    );
    assert_eq!(
        s19_stmts.len(),
        5,
        "unexpected split for {s19_byte_escapes}: {s19_stmts:#?}"
    );
    assert_eq!(s19_stmts[1].sql, "SELECT 'Troy\\'; Greendale'");
    assert_eq!(s19_stmts[2].sql, "SELECT E'\\xC3\\xA9'");
    assert_eq!(s19_stmts[3].sql, "SELECT E'\\303\\251'");
    assert_eq!(s19_stmts[4].sql, "SELECT 2");

    for partial_utf8_tail in [
        "SELECT E'\\xC3\\xA9",
        "SELECT E'\\xC3",
        "SELECT E'\\303",
        "SELECT E'\\xE2\\x82",
        "SELECT '\\xC3\\xA9",
    ] {
        let s19_tail_sql = format!(
            "BEGIN;\nSELECT set_config('work_mem'::text, '4MB', false);\nSELECT 'Troy\\'; Greendale';\nSELECT 2;\n{partial_utf8_tail}"
        );
        let s19_tail_stmts = split_statements_for_engine_with_session_mode(
            &s19_tail_sql,
            DbEngine::Postgres,
            SqlSessionMode::postgres(false),
        );
        assert_eq!(
            s19_tail_stmts.len(),
            5,
            "unexpected split for {s19_tail_sql}: {s19_tail_stmts:#?}"
        );
        assert_eq!(s19_tail_stmts[2].sql, "SELECT 'Troy\\'; Greendale'");
        assert_eq!(s19_tail_stmts[3].sql, "SELECT 2");
        assert_eq!(s19_tail_stmts[4].sql, partial_utf8_tail);
    }
}
