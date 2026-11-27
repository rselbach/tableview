#![allow(clippy::expect_used)]

use std::path::PathBuf;

use chrono::{Datelike, Utc};
use tempfile::tempdir;
use tv_core::engine::postgres::{quote_ident, synthesize_table_ddl};
use tv_core::{
    AccentColor, AppSettings, AppearancePreference, CellValue, ColumnMeta, ConnectionId, ConnectionProfile,
    ConnectionStore, DbEngine, EnvTag, ExplainError, ExplainPlan, ExportFormat, ForeignKeyRef, IndexInfo,
    NewHistoryEntry, PasswordMode, QueryStatementResult, QueryStore, SafetyDecision, SafetyPolicy, SavedQuery,
    SavedQueryId, SettingsStore, SortOrder, SslMode, StagedCellValue, StagedChanges, StagingError, StatementRisk,
    StoreData, UrlParseError, ValidationError, build_explain_sql, build_explain_sql_for_engine, build_fk_where_clause,
    build_fk_where_clause_for_engine, cursor_line_col, evaluate_sql_safety, evaluate_sql_safety_for_engine,
    evaluate_staged_commit_safety, export_csv, export_json, export_rows, export_rows_for_engine, export_sql_inserts,
    export_sql_inserts_for_engine, format_sql, has_executable_sql_for_engine, inspect_statement,
    inspect_statement_for_engine, split_statements, split_statements_for_engine, statement_at_cursor,
    statement_at_cursor_for_engine,
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
fn settings_store_roundtrip_and_missing_file_defaults() {
    let dir = tempdir().expect("create tempdir");
    let store = SettingsStore::at(dir.path().join("settings.json"));

    let initial = store.load().expect("missing file returns default settings");
    assert_eq!(initial, AppSettings::default());
    assert_eq!(initial.appearance, AppearancePreference::System);

    for appearance in [
        AppearancePreference::Light,
        AppearancePreference::Dark,
        AppearancePreference::System,
    ] {
        let settings = AppSettings {
            appearance,
            ..AppSettings::default()
        };
        store.save(&settings).expect("atomic save succeeds");
        let reloaded = store.load().expect("reload succeeds");
        assert_eq!(reloaded, settings);
    }

    let raw = std::fs::read_to_string(store.path()).expect("settings json exists");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid settings json");
    assert_eq!(parsed["appearance"], "system");
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
    assert!(tx_sql.contains("UPDATE \"public\".\"students\" SET \"full_name\" = 'Troy ''T-Bone'' Barnes' WHERE \"student_id\" = 1;"));
    assert!(tx_sql.contains("INSERT INTO \"public\".\"students\" (\"full_name\", \"email\", \"gpa\", \"profile\", \"badge\") VALUES ('Pierce Hawthorne', NULL, 2.15, NULL, '\\xcafe'::bytea);"));
    assert!(tx_sql.contains("DELETE FROM \"public\".\"students\" WHERE \"student_id\" = 2;"));
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
        "UPDATE `greendale`.`students` SET `full_name` = 'Britta''s \\\\protest', `enrolled` = FALSE, `badge` = X'cafe', `metadata` = '{\"note\":\"a''b\\\\c\"}' WHERE `student_id` = 1;"
    );
    assert_eq!(
        my_stmts[1],
        "INSERT INTO `greendale`.`students` (`full_name`, `enrolled`, `gpa`, `badge`, `metadata`) VALUES ('Shirley Bennett', TRUE, 3.88, X'beef', '{\"club\":\"baking\"}');"
    );
    assert_eq!(my_stmts[2], "DELETE FROM `greendale`.`students` WHERE `student_id` = 2;");

    let my_tx = staged
        .build_transaction_sql_for_engine(DbEngine::Mysql, "greendale", "students", &cols)
        .expect("mysql tx sql");
    assert!(my_tx.starts_with("START TRANSACTION;\n\n"));
    assert!(my_tx.ends_with("\n\nCOMMIT;\n"));

    // SQLite dialect (double-quoted identifiers, 1/0 bools, X'...' bytes, no `::` casts)
    let sl_stmts = staged
        .build_statements_for_engine(DbEngine::Sqlite, "", "students", &cols)
        .expect("sqlite statements");
    assert_eq!(sl_stmts.len(), 3);
    assert_eq!(
        sl_stmts[0],
        "UPDATE \"students\" SET \"full_name\" = 'Britta''s \\protest', \"enrolled\" = 0, \"badge\" = X'cafe', \"metadata\" = '{\"note\":\"a''b\\c\"}' WHERE \"student_id\" = 1;"
    );
    assert_eq!(
        sl_stmts[1],
        "INSERT INTO \"students\" (\"full_name\", \"enrolled\", \"gpa\", \"badge\", \"metadata\") VALUES ('Shirley Bennett', 1, 3.88, X'beef', '{\"club\":\"baking\"}');"
    );
    assert_eq!(sl_stmts[2], "DELETE FROM \"students\" WHERE \"student_id\" = 2;");
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
        Some("`full_name` = 'Troy''s \\\\Fort'".to_owned())
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
        "INSERT INTO `greendale`.`students` (`student_id`, `full_name`, `enrolled`, `gpa`, `badge`, `metadata`) VALUES (1, 'Troy ''T-Bone'' \\\\Barnes', TRUE, 3.45, X'cafe', '{\"campus\":\"Greendale\"}');\n"
    );
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
