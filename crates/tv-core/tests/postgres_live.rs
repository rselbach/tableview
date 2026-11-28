#![allow(clippy::expect_used)]

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection};
use tokio::runtime::Runtime;
use uuid::Uuid;
use tv_core::engine::postgres::PostgresSession;
use tv_core::{
    CellValue, ConnectionProfile, DbError, DbObjectKind, DbSession, ForeignKeyRef, PageRequest, PasswordMode,
    SortSpec, SslMode, StagedCellValue, StagedChanges,
};

fn resolve_test_connection() -> (ConnectionProfile, Option<String>) {
    if let Ok(url) = env::var("POSTGRES_TEST_URL")
        && !url.trim().is_empty()
    {
        let (mut profile, secret) = ConnectionProfile::from_postgres_url(&url).expect("valid POSTGRES_TEST_URL");
        if profile.database.as_deref().is_none_or(|d| d.trim().is_empty()) {
            profile.database = Some("postgres".to_owned());
        }
        return (profile, secret);
    }

    let host = env::var("PGHOST")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = env::var("PGPORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(5432);
    let user = env::var("PGUSER")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "postgres".to_owned());
    let password = env::var("PGPASSWORD").ok().filter(|s| !s.is_empty());

    let mut admin = ConnectionProfile::new_postgres("greendale_local", host, user);
    admin.port = port;
    admin.password_mode = if password.is_some() {
        PasswordMode::Keychain
    } else {
        PasswordMode::None
    };
    admin.ssl.mode = SslMode::Disable;
    admin.database = Some("postgres".to_owned());
    (admin, password)
}

struct TempDatabaseGuard {
    admin_profile: ConnectionProfile,
    password: Option<String>,
    admin_db: String,
    db_name: String,
    rt: Runtime,
}

impl Drop for TempDatabaseGuard {
    fn drop(&mut self) {
        let admin_profile = self.admin_profile.clone();
        let password = self.password.clone();
        let admin_db = self.admin_db.clone();
        let db_name = self.db_name.clone();
        self.rt.block_on(async move {
            if let Ok((admin_session, _)) =
                PostgresSession::connect(&admin_profile, password.as_deref(), Some(&admin_db)).await
            {
                let cleanup_stmts = vec![
                    format!(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
                    ),
                    format!("DROP DATABASE IF EXISTS \"{db_name}\""),
                ];
                drop(admin_session.execute_sql(&cleanup_stmts, None).await);
            }
        });
    }
}

#[test]
#[ignore = "requires live PostgreSQL server (set POSTGRES_TEST_URL or PGHOST/PGPORT/PGUSER/PGPASSWORD)"]
fn live_postgres_introspection_data_grid_and_structure_roundtrip() {
    let rt = Runtime::new().expect("create tokio runtime");
    let (admin, password) = resolve_test_connection();
    let admin_db = admin.database.clone().unwrap_or_else(|| "postgres".to_owned());
    let db_name = format!("tableview_pg_test_{}", Uuid::new_v4().simple());

    rt.block_on(async {
        let (admin_session, _) =
            PostgresSession::connect(&admin, password.as_deref(), Some(&admin_db))
                .await
                .expect("connect to PostgreSQL admin database");
        let setup_stmts = vec![format!("CREATE DATABASE \"{db_name}\"")];
        let (batch, _) = admin_session
            .execute_sql(&setup_stmts, None)
            .await
            .expect("setup temporary test db");
        assert_eq!(batch.results.len(), setup_stmts.len());
        for res in &batch.results {
            assert!(res.error.is_none(), "admin setup statement failed: {:?}", res.error);
        }
    });

    let _guard = TempDatabaseGuard {
        admin_profile: admin.clone(),
        password: password.clone(),
        admin_db: admin_db.clone(),
        db_name: db_name.clone(),
        rt: Runtime::new().expect("cleanup runtime"),
    };

    rt.block_on(async {
        let (seed_session, _) =
            PostgresSession::connect(&admin, password.as_deref(), Some(&db_name))
                .await
                .expect("connect to temp db");
        let seed_stmts = vec![
            "CREATE SCHEMA study_room".to_owned(),
            "CREATE TYPE public.greendale_major AS ENUM ('Spanish', 'Film', 'Law', 'AirConditioning')".to_owned(),
            "CREATE TABLE public.study_groups (\n    group_id INTEGER PRIMARY KEY,\n    room_name TEXT NOT NULL\n)".to_owned(),
            "INSERT INTO public.study_groups VALUES (1, 'Study Room F')".to_owned(),
            "CREATE TABLE public.students (\n    student_id INTEGER PRIMARY KEY,\n    id UUID NOT NULL,\n    full_name TEXT NOT NULL,\n    email TEXT,\n    enrolled BOOLEAN NOT NULL DEFAULT true,\n    gpa NUMERIC(4,2),\n    major public.greendale_major NOT NULL,\n    tags TEXT[] NOT NULL DEFAULT '{}',\n    profile JSONB,\n    badge BYTEA,\n    group_id INTEGER REFERENCES public.study_groups(group_id),\n    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()\n)".to_owned(),
            "CREATE UNIQUE INDEX students_id_uidx ON public.students (id)".to_owned(),
            "CREATE INDEX students_profile_gin ON public.students USING gin (profile)".to_owned(),
            "COMMENT ON TABLE public.students IS 'Greendale Community College active roster'".to_owned(),
            "COMMENT ON COLUMN public.students.full_name IS 'Preferred display name'".to_owned(),
            "INSERT INTO public.students\n    (student_id, id, full_name, email, enrolled, gpa, major, tags, profile, badge, group_id, created_at)\nVALUES\n    (1, '210b3d6b-da52-4259-ac05-b58d3d0a6d5b', 'Troy Barnes', 'troy@greendale.edu', true, 3.45, 'AirConditioning', ARRAY['football','plumbing'], '{\"best_friend\":\"Abed\",\"handshake\":true}'::jsonb, '\\xdeadbeef'::bytea, 1, '2026-11-19T18:09:11.432Z'),\n    (2, '8c5b8676-0203-44ba-9198-90125b690e81', 'Abed Nadir', NULL, true, 3.92, 'Film', ARRAY['meta','inspector'], '{\"bedtime\":\"strict\"}'::jsonb, '\\x00ff'::bytea, 1, '2026-11-19T18:24:56.558Z'),\n    (3, '11111111-2222-3333-4444-555555555555', 'Annie Edison', '', true, 4.00, 'Law', ARRAY['notes'], '{\"pens\":12}'::jsonb, NULL, 1, '2026-11-19T19:00:00.000Z'),\n    (4, '99999999-8888-7777-6666-555555555555', 'Jeff Winger', 'jeff@greendale.edu', false, 2.75, 'Law', ARRAY['speech'], NULL, '\\x'::bytea, 1, '2026-11-20T09:15:00.000Z')".to_owned(),
            "CREATE VIEW public.active_students_view AS\n    SELECT student_id, full_name FROM public.students WHERE enrolled = true".to_owned(),
        ];
        let (batch, _) = seed_session
            .execute_sql(&seed_stmts, None)
            .await
            .expect("seed Community test schema");
        assert_eq!(batch.results.len(), seed_stmts.len());
        for res in &batch.results {
            assert!(res.error.is_none(), "seed statement failed: {:?}", res.error);
        }
        drop(seed_session);

        let tested = PostgresSession::test_connection(&admin, password.as_deref())
            .await
            .expect("test_connection succeeds");
        assert!(tested.version.starts_with("PostgreSQL "), "got {}", tested.version);

        let (session, init_logs) = PostgresSession::connect(&admin, password.as_deref(), Some(&db_name))
            .await
            .expect("connect to temporary database");
        assert_eq!(session.server_info().current_database, db_name);
        assert_eq!(init_logs.len(), 1);

        let (dbs, _) = session.list_databases().await.expect("list_databases");
        assert!(dbs.iter().any(|d| d.name == db_name));

        let (schemas, _) = session.list_schemas().await.expect("list_schemas");
        assert_eq!(schemas.first().map(|s| s.name.as_str()), Some("public"));
        assert!(schemas.iter().any(|s| s.name == "study_room" && !s.is_system));
        assert!(schemas.iter().any(|s| s.name == "pg_catalog" && s.is_system));

        let (objects, _) = session.list_objects("public").await.expect("list_objects");
        let names: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
        assert_eq!(
            names,
            vec![
                ("active_students_view", DbObjectKind::View),
                ("students", DbObjectKind::Table),
                ("study_groups", DbObjectKind::Table),
            ]
        );

        let (cols, _) = session
            .list_columns("public", "students")
            .await
            .expect("list_columns");
        assert_eq!(cols.len(), 12);
        assert_eq!(cols[0].name, "student_id");
        assert!(cols[0].is_primary_key);

        // Fetch sorted page 1 (limit 2, offset 0)
        let (page1, page1_logs) = session
            .fetch_table_page(&PageRequest {
                schema: "public".to_owned(),
                table: "students".to_owned(),
                filter_where: None,
                sort: Some(SortSpec {
                    column: "student_id".to_owned(),
                    ascending: true,
                }),
                limit: 2,
                offset: 0,
            })
            .await
            .expect("fetch page 1");
        assert!(!page1_logs.is_empty());
        assert_eq!(page1.rows.len(), 2);
        assert!(page1.has_more);
        assert!(!page1.total_is_exact);

        assert_eq!(page1.rows[0][0], CellValue::Int(1));
        assert!(matches!(page1.rows[0][1], CellValue::Uuid(_)));
        assert_eq!(page1.rows[0][2], CellValue::Text("Troy Barnes".to_owned()));
        assert_eq!(page1.rows[0][3], CellValue::Text("troy@greendale.edu".to_owned()));
        assert_eq!(page1.rows[0][4], CellValue::Bool(true));
        assert_eq!(page1.rows[0][5], CellValue::Numeric("3.45".to_owned()));
        assert_eq!(page1.rows[0][6], CellValue::Text("AirConditioning".to_owned()));
        assert_eq!(page1.rows[0][7], CellValue::Array("{football,plumbing}".to_owned()));
        assert!(matches!(page1.rows[0][8], CellValue::Json(_)));
        assert_eq!(page1.rows[0][9], CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        assert!(matches!(page1.rows[0][11], CellValue::Timestamp { utc: Some(_), .. }));

        // Row 2 has SQL NULL email
        assert_eq!(page1.rows[1][3], CellValue::Null);

        // Fetch page 2 (limit 2, offset 2) -> exact total 4
        let (page2, _) = session
            .fetch_table_page(&PageRequest {
                schema: "public".to_owned(),
                table: "students".to_owned(),
                filter_where: None,
                sort: Some(SortSpec {
                    column: "student_id".to_owned(),
                    ascending: true,
                }),
                limit: 2,
                offset: 2,
            })
            .await
            .expect("fetch page 2");
        assert_eq!(page2.rows.len(), 2);
        assert!(!page2.has_more);
        assert!(page2.total_is_exact);
        assert_eq!(page2.total_rows_estimate, Some(4));
        // Annie has empty string "" email vs Abed's SQL NULL
        assert_eq!(page2.rows[0][3], CellValue::Text(String::new()));
        // Jeff has NULL profile and empty bytea
        assert_eq!(page2.rows[1][8], CellValue::Null);
        assert_eq!(page2.rows[1][9], CellValue::Bytes(Vec::new()));

        // WHERE filter + DESC sort
        let (filtered, _) = session
            .fetch_table_page(&PageRequest {
                schema: "public".to_owned(),
                table: "students".to_owned(),
                filter_where: Some("enrolled = true".to_owned()),
                sort: Some(SortSpec {
                    column: "student_id".to_owned(),
                    ascending: false,
                }),
                limit: 50,
                offset: 0,
            })
            .await
            .expect("filtered desc");
        assert_eq!(filtered.rows.len(), 3);
        assert_eq!(filtered.rows[0][0], CellValue::Int(3));

        // Unknown sort column is rejected at boundary
        let bad_sort = session
            .fetch_table_page(&PageRequest {
                schema: "public".to_owned(),
                table: "students".to_owned(),
                filter_where: None,
                sort: Some(SortSpec {
                    column: "not_a_real_column".to_owned(),
                    ascending: true,
                }),
                limit: 10,
                offset: 0,
            })
            .await;
        assert!(matches!(bad_sort, Err(DbError::InvalidInput(_))));

        // Syntax-broken WHERE filter returns DbError::QueryFailed
        let bad_where = session
            .fetch_table_page(&PageRequest {
                schema: "public".to_owned(),
                table: "students".to_owned(),
                filter_where: Some("student_id == 'nope'".to_owned()),
                sort: None,
                limit: 10,
                offset: 0,
            })
            .await;
        assert!(matches!(bad_where, Err(DbError::QueryFailed { .. })));

        // Structure & Indexes introspection
        let (structure, _) = session
            .describe_table("public", "students")
            .await
            .expect("describe_table students");
        assert_eq!(structure.columns.len(), 12);
        assert!(structure.columns[0].is_primary_key);
        assert!(structure.columns[1].is_unique);
        assert_eq!(structure.columns[2].comment.as_deref(), Some("Preferred display name"));
        let fk = structure.columns[10].foreign_key.as_ref().expect("group_id FK");
        assert_eq!(fk.target_table, "study_groups");
        assert_eq!(fk.target_column, "group_id");

        assert_eq!(structure.indexes.len(), 3);
        assert!(structure.indexes.iter().any(|i| i.is_primary && i.columns == ["student_id"]));
        assert!(structure.indexes.iter().any(|i| i.name == "students_profile_gin" && i.method == "gin"));
        assert!(structure.stats.total_bytes.is_some());
        assert_eq!(
            structure.stats.comment.as_deref(),
            Some("Greendale Community College active roster")
        );
        assert!(structure.stats.ddl.contains("CREATE TABLE \"public\".\"students\""));

        // View DDL introspection
        let (view_struct, _) = session
            .describe_table("public", "active_students_view")
            .await
            .expect("describe_table view");
        assert!(view_struct.stats.ddl.contains("CREATE OR REPLACE VIEW \"public\".\"active_students_view\""));

        // Multi-statement SQL execution with typed column decoding
        let stmts = vec![
            "SELECT student_id, id, full_name, email, enrolled, gpa, profile, badge, created_at FROM public.students ORDER BY student_id".to_owned(),
            "SELECT COUNT(*) AS student_total FROM public.students".to_owned(),
        ];
        let (batch, exec_logs) = session
            .execute_sql(&stmts, None)
            .await
            .expect("execute multi-statement batch");
        assert!(!batch.cancelled);
        assert_eq!(exec_logs.len(), 2);
        assert_eq!(batch.results.len(), 2);
        assert_eq!(batch.results[0].rows.len(), 4);
        assert_eq!(batch.results[0].rows[0][0], CellValue::Int(1));
        assert!(matches!(batch.results[0].rows[0][1], CellValue::Uuid(_)));
        assert_eq!(batch.results[0].rows[1][3], CellValue::Null);
        assert_eq!(batch.results[0].rows[2][3], CellValue::Text(String::new()));
        assert_eq!(batch.results[0].rows[0][5], CellValue::Numeric("3.45".to_owned()));
        assert!(matches!(batch.results[0].rows[0][6], CellValue::Json(_)));
        assert_eq!(batch.results[0].rows[0][7], CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        assert!(matches!(batch.results[0].rows[0][8], CellValue::Timestamp { utc: Some(_), .. }));
        assert_eq!(batch.results[1].rows[0][0], CellValue::Int(4));

        // Row limit enforcement preserves total_rows_seen & truncated flag
        let (limited, _) = session
            .execute_sql(&[stmts[0].clone()], Some(2))
            .await
            .expect("execute with row limit");
        assert_eq!(limited.results[0].rows.len(), 2);
        assert!(limited.results[0].truncated);
        assert_eq!(limited.results[0].total_rows_seen, 4);

        // DML statement reports rows_affected
        let (dml, _) = session
            .execute_sql(
                &["UPDATE public.students SET enrolled = true WHERE student_id = 4".to_owned()],
                None,
            )
            .await
            .expect("execute DML update");
        assert!(!dml.results[0].is_tabular());
        assert_eq!(dml.results[0].rows_affected, Some(1));

        // Syntax error stops subsequent statements cleanly
        let (bad_batch, _) = session
            .execute_sql(
                &[
                    "SELECT * FROM table_does_not_exist".to_owned(),
                    "SELECT 42".to_owned(),
                ],
                None,
            )
            .await
            .expect("returns batch with error result");
        assert_eq!(bad_batch.results.len(), 1);
        assert!(bad_batch.results[0].error.is_some());

        // Live out-of-band cancellation via CancelToken
        let shared = std::sync::Arc::new(session);
        let runner = std::sync::Arc::clone(&shared);
        let canceller = std::sync::Arc::clone(&shared);
        let cancel_handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            canceller.cancel_running_query().await
        });
        let cancel_start = std::time::Instant::now();
        let (sleep_batch, _) = runner
            .execute_sql(&["SELECT pg_sleep(5)".to_owned()], None)
            .await
            .expect("cancelled query returns cleanly");
        let cancel_res = cancel_handle.await.expect("join cancel task");
        assert!(cancel_res.is_ok(), "cancel call failed: {cancel_res:?}");
        assert!(sleep_batch.cancelled);
        assert!(cancel_start.elapsed() < std::time::Duration::from_secs(3));

        shared.ping().await.expect("ping succeeds");

        // 1. Live FK Peek lookup
        let (peek, peek_log) = shared
            .fetch_fk_row(
                &ForeignKeyRef {
                    constraint_name: "students_group_id_fkey".to_owned(),
                    target_schema: "public".to_owned(),
                    target_table: "study_groups".to_owned(),
                    target_column: "group_id".to_owned(),
                },
                &CellValue::Int(1),
                &structure.columns[10],
            )
            .await
            .expect("fetch_fk_row succeeds");
        assert!(peek_log.sql.contains("\"public\".\"study_groups\""));
        let peek_row = peek.row.expect("study group 1 exists");
        assert_eq!(peek_row[0], CellValue::Int(1));
        assert_eq!(peek_row[1], CellValue::Text("Study Room F".to_owned()));

        let null_peek = shared
            .fetch_fk_row(&structure.columns[10].foreign_key.clone().expect("fk"), &CellValue::Null, &structure.columns[10])
            .await;
        assert!(matches!(null_peek, Err(DbError::InvalidInput(_))));

        // 2. Live Visual EXPLAIN (FORMAT JSON) & EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
        let join_sql = "SELECT s.full_name, g.room_name FROM public.students s JOIN public.study_groups g ON g.group_id = s.group_id ORDER BY s.student_id";
        let (est_plan, _) = shared
            .explain_sql(join_sql, false)
            .await
            .expect("estimated explain succeeds");
        assert!(!est_plan.analyzed);
        assert!(est_plan.node_count() >= 1);

        let (ana_plan, ana_log) = shared
            .explain_sql(join_sql, true)
            .await
            .expect("analyze explain succeeds");
        assert!(ana_log.sql.starts_with("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)"));
        assert!(ana_plan.analyzed);
        assert!(ana_plan.execution_time_ms.is_some());
        assert!(ana_plan.root.actual_total_time_ms.is_some());

        // 3. Staged transactional commit (UPDATE with apostrophe/jsonb/bytea + INSERT + DELETE)
        // Update Troy (row 0 of page1) using bound commit batch + raw row snapshot
        let mut staged = StagedChanges::new();
        staged.stage_cell_edit_with_raw(
            0,
            &page1.rows[0],
            page1.raw_row(0),
            2,
            StagedCellValue::Literal("Troy 'T-Bone' Barnes".to_owned()),
        );
        staged.stage_cell_edit_with_raw(
            0,
            &page1.rows[0],
            page1.raw_row(0),
            5,
            StagedCellValue::Literal("3.55".to_owned()),
        );
        staged.stage_cell_edit_with_raw(
            0,
            &page1.rows[0],
            page1.raw_row(0),
            8,
            StagedCellValue::Literal(r#"{"ac":"Master"}"#.to_owned()),
        );
        staged.stage_cell_edit_with_raw(
            0,
            &page1.rows[0],
            page1.raw_row(0),
            9,
            StagedCellValue::Literal("\\xcafebabe".to_owned()),
        );

        let draft = staged.stage_insert_row(&page1.columns);
        staged.stage_insert_cell_edit(draft, 0, StagedCellValue::Literal("5".to_owned()));
        staged.stage_insert_cell_edit(draft, 1, StagedCellValue::Literal("22222222-3333-4444-5555-666666666666".to_owned()));
        staged.stage_insert_cell_edit(draft, 2, StagedCellValue::Literal("Pierce Hawthorne".to_owned()));
        staged.stage_insert_cell_edit(draft, 6, StagedCellValue::Literal("Law".to_owned()));
        staged.stage_insert_cell_edit(draft, 10, StagedCellValue::Literal("1".to_owned()));

        // Delete Jeff (row index 1 of page2, student_id = 4)
        staged.toggle_delete_row_with_raw(100, &page2.rows[1], page2.raw_row(1));
        let commit_batch = staged
            .build_commit_batch(tv_core::DbEngine::Postgres, "public", "students", &page1.columns)
            .expect("build staged commit batch");
        let (tx_out, tx_logs) = shared
            .execute_staged_mutations(&commit_batch)
            .await
            .expect("commit staged transaction succeeds");
        assert_eq!(tx_out.statements_executed, 3);
        assert_eq!(tx_logs.len(), 5); // BEGIN + 3 stmts + COMMIT

        let (after_commit, _) = shared
            .fetch_table_page(&PageRequest {
                schema: "public".to_owned(),
                table: "students".to_owned(),
                filter_where: None,
                sort: Some(SortSpec {
                    column: "student_id".to_owned(),
                    ascending: true,
                }),
                limit: 50,
                offset: 0,
            })
            .await
            .expect("refetch after commit");
        assert_eq!(after_commit.rows.len(), 4);
        assert_eq!(after_commit.rows[0][2], CellValue::Text("Troy 'T-Bone' Barnes".to_owned()));
        assert_eq!(after_commit.rows[0][5], CellValue::Numeric("3.55".to_owned()));
        assert_eq!(after_commit.rows[0][9], CellValue::Bytes(vec![0xca, 0xfe, 0xba, 0xbe]));
        assert_eq!(after_commit.rows[3][0], CellValue::Int(5));
        assert_eq!(after_commit.rows[3][2], CellValue::Text("Pierce Hawthorne".to_owned()));

        // 4. Transactional rollback guarantees atomicity when second statement violates PK
        let rollback_stmts = vec![
            "UPDATE \"public\".\"students\" SET \"full_name\" = 'Should Rollback' WHERE \"student_id\" = 1;".to_owned(),
            "INSERT INTO \"public\".\"students\" (\"student_id\", \"id\", \"full_name\", \"major\") VALUES (1, '33333333-3333-4444-5555-666666666666', 'Evil Abed', 'Film');".to_owned(),
        ];
        let rollback_err = shared.execute_transaction(&rollback_stmts).await;
        assert!(matches!(rollback_err, Err(DbError::QueryFailed { .. })));

        let (after_rollback, _) = shared
            .fetch_table_page(&PageRequest {
                schema: "public".to_owned(),
                table: "students".to_owned(),
                filter_where: Some("student_id = 1".to_owned()),
                sort: None,
                limit: 10,
                offset: 0,
            })
            .await
            .expect("refetch after rollback");
        assert_eq!(after_rollback.rows[0][2], CellValue::Text("Troy 'T-Bone' Barnes".to_owned()));
    });
}

#[test]
#[ignore = "requires live PostgreSQL server (set POSTGRES_TEST_URL or PGHOST/PGPORT/PGUSER/PGPASSWORD)"]
fn live_postgres_tls_enforcement_and_prefer_cancel() {
    let rt = Runtime::new().expect("create tokio runtime");
    let (base, password) = resolve_test_connection();
    let target_db = base.database.clone().unwrap_or_else(|| "postgres".to_owned());

    rt.block_on(async {
        let (session, _) = PostgresSession::connect(&base, password.as_deref(), Some(&target_db))
            .await
            .expect("connect using resolve_test_connection");
        let (ssl_batch, _) = session
            .execute_sql(&["SHOW ssl".to_owned()], None)
            .await
            .expect("query server ssl setting");
        assert_eq!(ssl_batch.results.len(), 1);
        assert!(
            ssl_batch.results[0].error.is_none(),
            "SHOW ssl failed: {:?}",
            ssl_batch.results[0].error
        );
        let ssl_enabled = matches!(
            ssl_batch.results[0]
                .rows
                .first()
                .and_then(|row| row.first()),
            Some(CellValue::Text(val)) if val.eq_ignore_ascii_case("on")
        );
        let is_unix_socket = base.host.trim().starts_with('/');
        let tls_expected = ssl_enabled && !is_unix_socket;

        if tls_expected {
            let mut strict = base.clone();
            strict.ssl.mode = SslMode::Require;
            strict.ssl.ca_cert_path = None;
            let _ = PostgresSession::connect(&strict, password.as_deref(), Some(&target_db))
                .await
                .expect("SslMode::Require connects when server has TLS enabled");
        } else {
            for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
                let mut strict = base.clone();
                strict.ssl.mode = mode;
                strict.ssl.ca_cert_path = None;
                let res = PostgresSession::connect(&strict, password.as_deref(), Some(&target_db)).await;
                assert!(res.is_err(), "{mode:?} must reject plaintext server");
            }
        }

        let mut prefer = base.clone();
        prefer.ssl.mode = SslMode::Prefer;
        prefer.ssl.ca_cert_path = None;
        let (prefer_session, _) = PostgresSession::connect(&prefer, password.as_deref(), Some(&target_db))
            .await
            .expect("SslMode::Prefer connects cleanly");
        let shared = Arc::new(prefer_session);
        let runner = Arc::clone(&shared);
        let canceller = Arc::clone(&shared);
        let cancel_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            canceller.cancel_running_query().await
        });
        let cancel_start = Instant::now();
        let (sleep_batch, _) = runner
            .execute_sql(&["SELECT pg_sleep(5)".to_owned()], None)
            .await
            .expect("cancelled prefer query returns cleanly");
        let cancel_res = cancel_handle.await.expect("join cancel task");
        assert!(cancel_res.is_ok(), "cancel call failed: {cancel_res:?}");
        assert!(sleep_batch.cancelled);
        assert!(cancel_start.elapsed() < Duration::from_secs(3));
    });
}

const PROXY_TLS_ACCEPT: u8 = 0;
const PROXY_TLS_REJECT_N: u8 = 1;
const PROXY_TLS_ABORT_HANDSHAKE: u8 = 2;
const PG_SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

struct PgTlsProxyState {
    mode: AtomicU8,
    stop: AtomicBool,
    tls_accepted: AtomicUsize,
    tls_rejected: AtomicUsize,
    tls_aborted: AtomicUsize,
    direct_plaintext: AtomicUsize,
}

struct PgTlsProxy {
    port: u16,
    state: Arc<PgTlsProxyState>,
    accept_thread: Option<JoinHandle<()>>,
}

impl PgTlsProxy {
    fn start(upstream_host: String, upstream_port: u16, server_cfg: Arc<ServerConfig>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local TLS proxy");
        listener
            .set_nonblocking(true)
            .expect("set proxy listener nonblocking");
        let port = listener.local_addr().expect("proxy local_addr").port();
        let state = Arc::new(PgTlsProxyState {
            mode: AtomicU8::new(PROXY_TLS_ACCEPT),
            stop: AtomicBool::new(false),
            tls_accepted: AtomicUsize::new(0),
            tls_rejected: AtomicUsize::new(0),
            tls_aborted: AtomicUsize::new(0),
            direct_plaintext: AtomicUsize::new(0),
        });
        let accept_state = Arc::clone(&state);
        let accept_thread = thread::spawn(move || {
            let mut workers: Vec<JoinHandle<()>> = Vec::new();
            while !accept_state.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let host = upstream_host.clone();
                        let cfg = Arc::clone(&server_cfg);
                        let st = Arc::clone(&accept_state);
                        workers.push(thread::spawn(move || handle_proxy_client(client, &host, upstream_port, cfg, st)));
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                drop(worker.join());
            }
        });
        Self {
            port,
            state,
            accept_thread: Some(accept_thread),
        }
    }
}

impl Drop for PgTlsProxy {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(handle) = self.accept_thread.take() {
            drop(handle.join());
        }
    }
}

fn handle_proxy_client(
    mut client: TcpStream,
    upstream_host: &str,
    upstream_port: u16,
    server_cfg: Arc<ServerConfig>,
    state: Arc<PgTlsProxyState>,
) {
    if client.set_nonblocking(false).is_err() {
        return;
    }
    drop(client.set_read_timeout(Some(Duration::from_secs(5))));
    drop(client.set_write_timeout(Some(Duration::from_secs(5))));

    let mut header = [0u8; 8];
    if client.read_exact(&mut header).is_err() {
        return;
    }
    let Ok(mut upstream) = TcpStream::connect((upstream_host, upstream_port)) else {
        return;
    };

    if header == PG_SSL_REQUEST {
        match state.mode.load(Ordering::Acquire) {
            PROXY_TLS_ACCEPT => {
                if client.write_all(b"S").is_err() {
                    return;
                }
                state.tls_accepted.fetch_add(1, Ordering::AcqRel);
                pump_tls_session(client, upstream, server_cfg, state);
            }
            PROXY_TLS_REJECT_N => {
                if client.write_all(b"N").is_err() {
                    return;
                }
                state.tls_rejected.fetch_add(1, Ordering::AcqRel);
                splice_raw(client, upstream, state);
            }
            _ => {
                if client.write_all(b"S").is_err() {
                    return;
                }
                let mut hello = [0u8; 1];
                drop(client.read(&mut hello));
                state.tls_aborted.fetch_add(1, Ordering::AcqRel);
                drop(client.shutdown(Shutdown::Both));
                drop(upstream.shutdown(Shutdown::Both));
            }
        }
    } else {
        state.direct_plaintext.fetch_add(1, Ordering::AcqRel);
        if upstream.write_all(&header).is_err() {
            return;
        }
        splice_raw(client, upstream, state);
    }
}

fn splice_one_way(mut src: TcpStream, mut dst: TcpStream, state: Arc<PgTlsProxyState>) {
    drop(src.set_read_timeout(Some(Duration::from_millis(150))));
    drop(dst.set_write_timeout(Some(Duration::from_secs(5))));
    let mut buf = [0u8; 8192];
    while !state.stop.load(Ordering::Acquire) {
        match src.read(&mut buf) {
            Ok(0) => {
                drop(dst.shutdown(Shutdown::Write));
                break;
            }
            Ok(n) => {
                if dst.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
            Err(_) => break,
        }
    }
}

fn splice_raw(client: TcpStream, upstream: TcpStream, state: Arc<PgTlsProxyState>) {
    let Ok(client_read) = client.try_clone() else {
        return;
    };
    let Ok(upstream_write) = upstream.try_clone() else {
        return;
    };
    let st_c2u = Arc::clone(&state);
    let t_c2u = thread::spawn(move || splice_one_way(client_read, upstream_write, st_c2u));
    splice_one_way(upstream, client, state);
    drop(t_c2u.join());
}

fn pump_tls_session(
    mut client: TcpStream,
    mut upstream: TcpStream,
    server_cfg: Arc<ServerConfig>,
    state: Arc<PgTlsProxyState>,
) {
    let Ok(mut tls) = ServerConnection::new(server_cfg) else {
        return;
    };
    while tls.is_handshaking() {
        if tls.complete_io(&mut client).is_err() {
            return;
        }
    }
    if client.set_nonblocking(true).is_err() || upstream.set_nonblocking(true).is_err() {
        return;
    }

    let mut to_upstream: Vec<u8> = Vec::new();
    let mut client_eof = false;
    let mut upstream_eof = false;
    let mut tmp = [0u8; 8192];

    while !state.stop.load(Ordering::Acquire) {
        let mut progressed = false;

        while tls.wants_write() {
            match tls.write_tls(&mut client) {
                Ok(0) => break,
                Ok(_) => progressed = true,
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                Err(_) => return,
            }
        }

        if !client_eof {
            match tls.read_tls(&mut client) {
                Ok(0) => {
                    client_eof = true;
                    progressed = true;
                }
                Ok(_) => {
                    progressed = true;
                    if tls.process_new_packets().is_err() {
                        return;
                    }
                }
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(_) => return,
            }
        }

        loop {
            match tls.reader().read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    to_upstream.extend_from_slice(&tmp[..n]);
                    progressed = true;
                }
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                Err(_) => break,
            }
        }

        while !to_upstream.is_empty() {
            match upstream.write(&to_upstream) {
                Ok(0) => return,
                Ok(n) => {
                    to_upstream.drain(..n);
                    progressed = true;
                }
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                Err(_) => return,
            }
        }

        if !upstream_eof {
            match upstream.read(&mut tmp) {
                Ok(0) => {
                    upstream_eof = true;
                    tls.send_close_notify();
                    progressed = true;
                }
                Ok(n) => {
                    if tls.writer().write_all(&tmp[..n]).is_err() {
                        return;
                    }
                    progressed = true;
                }
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(_) => return,
            }
        }

        if (client_eof && to_upstream.is_empty()) || (upstream_eof && !tls.wants_write()) {
            break;
        }
        if !progressed {
            thread::sleep(Duration::from_millis(2));
        }
    }

    drop(client.shutdown(Shutdown::Both));
    drop(upstream.shutdown(Shutdown::Both));
}

async fn wait_for_active_query(observer: &PostgresSession, tag: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let query = format!(
        "SELECT COUNT(*) FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND state = 'active' AND query LIKE '%{tag}%'"
    );
    while Instant::now() < deadline {
        if let Ok((batch, _)) = observer.execute_sql(std::slice::from_ref(&query), None).await
            && let Some(CellValue::Int(count)) = batch
                .results
                .first()
                .and_then(|r| r.rows.first())
                .and_then(|row| row.first())
            && *count >= 1
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for tagged query `{tag}` to become active in pg_stat_activity");
}

#[test]
#[ignore = "requires live PostgreSQL server (set POSTGRES_TEST_URL or PGHOST/PGPORT/PGUSER/PGPASSWORD)"]
fn live_postgres_prefer_cancel_falls_back_when_tls_disabled_mid_session() {
    let rt = Runtime::new().expect("create tokio runtime");
    let (base, password) = resolve_test_connection();
    if base.host.trim().starts_with('/') {
        return;
    }
    let target_db = base.database.clone().unwrap_or_else(|| "postgres".to_owned());

    let ssl_enabled = rt.block_on(async {
        let (observer, _) = PostgresSession::connect(&base, password.as_deref(), Some(&target_db))
            .await
            .expect("connect observer session directly");
        let (ssl_batch, _) = observer
            .execute_sql(&["SHOW ssl".to_owned()], None)
            .await
            .expect("query server ssl setting");
        assert_eq!(ssl_batch.results.len(), 1);
        assert!(
            ssl_batch.results[0].error.is_none(),
            "SHOW ssl failed: {:?}",
            ssl_batch.results[0].error
        );
        matches!(
            ssl_batch.results[0]
                .rows
                .first()
                .and_then(|row| row.first()),
            Some(CellValue::Text(val)) if val.eq_ignore_ascii_case("on")
        )
    });
    if ssl_enabled {
        return;
    }

    let dir = tempfile::tempdir().expect("create temp dir for TLS proxy cert");
    let key_path = dir.path().join("proxy.key");
    let cert_path = dir.path().join("proxy.crt");
    let cfg_path = dir.path().join("openssl.cnf");
    std::fs::write(
        &cfg_path,
        "[req]\ndistinguished_name = dn\nx509_extensions = v3_ext\nprompt = no\n\n[dn]\nCN = 127.0.0.1\n\n[v3_ext]\nbasicConstraints = CA:FALSE\nkeyUsage = digitalSignature, keyEncipherment\nextendedKeyUsage = serverAuth\nsubjectAltName = IP:127.0.0.1\n",
    )
    .expect("write openssl v3 config");
    let openssl_out = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            key_path.to_str().expect("utf8 key path"),
            "-out",
            cert_path.to_str().expect("utf8 cert path"),
            "-days",
            "1",
            "-config",
            cfg_path.to_str().expect("utf8 config path"),
        ])
        .output()
        .expect("invoke openssl to generate test certificate");
    assert!(
        openssl_out.status.success(),
        "openssl cert generation failed: {}",
        String::from_utf8_lossy(&openssl_out.stderr)
    );

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert_path)
        .expect("open proxy cert PEM")
        .collect::<Result<Vec<_>, _>>()
        .expect("parse proxy cert PEM");
    let key = PrivateKeyDer::from_pem_file(&key_path).expect("parse proxy key PEM");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_cfg = Arc::new(
        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("safe TLS versions")
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("build proxy rustls ServerConfig"),
    );

    let proxy = PgTlsProxy::start(base.host.trim().to_owned(), base.port, server_cfg);
    let mut proxy_profile = base.clone();
    proxy_profile.host = "127.0.0.1".to_owned();
    proxy_profile.port = proxy.port;
    proxy_profile.ssl.ca_cert_path = None;
    proxy_profile.ssl.client_cert_path = None;
    proxy_profile.ssl.client_key_path = None;

    rt.block_on(async {
        let (observer, _) = PostgresSession::connect(&base, password.as_deref(), Some(&target_db))
            .await
            .expect("connect observer session directly");

        let mut require_profile = proxy_profile.clone();
        require_profile.ssl.mode = SslMode::Require;
        let (require_session, _) =
            PostgresSession::connect(&require_profile, password.as_deref(), Some(&target_db))
                .await
                .expect("SslMode::Require connects through TLS proxy");
        drop(require_session);
        assert!(proxy.state.tls_accepted.load(Ordering::Acquire) >= 1);

        let mut prefer_profile = proxy_profile.clone();
        prefer_profile.ssl.mode = SslMode::Prefer;
        let (prefer_session, _) =
            PostgresSession::connect(&prefer_profile, password.as_deref(), Some(&target_db))
                .await
                .expect("SslMode::Prefer connects cleanly over TLS through proxy");
        assert!(proxy.state.tls_accepted.load(Ordering::Acquire) >= 2);

        let shared = Arc::new(prefer_session);

        // Scenario A: server disables TLS ('N' to SSLRequest) while TLS query is active.
        let tag_a = format!("tv_prefer_cancel_n_{}", Uuid::new_v4().simple());
        let sql_a = vec![format!("SELECT pg_sleep(5) /* {tag_a} */")];
        let runner_a = Arc::clone(&shared);
        let handle_a = tokio::spawn(async move { runner_a.execute_sql(&sql_a, None).await });
        wait_for_active_query(&observer, &tag_a).await;

        proxy.state.mode.store(PROXY_TLS_REJECT_N, Ordering::Release);
        let cancel_start_a = Instant::now();
        let cancel_res_a = shared.cancel_running_query().await;
        let (batch_a, _) = handle_a
            .await
            .expect("join query A task")
            .expect("cancelled query A returns cleanly");
        assert!(cancel_res_a.is_ok(), "cancel A failed: {cancel_res_a:?}");
        assert!(batch_a.cancelled, "query A must be marked cancelled");
        assert!(cancel_start_a.elapsed() < Duration::from_secs(3));
        assert!(proxy.state.tls_rejected.load(Ordering::Acquire) >= 1);

        // Scenario B: TLS handshake aborts on cancel, triggering explicit NoTls retry on Prefer session.
        let tag_b = format!("tv_prefer_cancel_abort_{}", Uuid::new_v4().simple());
        let sql_b = vec![format!("SELECT pg_sleep(5) /* {tag_b} */")];
        let runner_b = Arc::clone(&shared);
        let handle_b = tokio::spawn(async move { runner_b.execute_sql(&sql_b, None).await });
        wait_for_active_query(&observer, &tag_b).await;

        proxy
            .state
            .mode
            .store(PROXY_TLS_ABORT_HANDSHAKE, Ordering::Release);
        let cancel_start_b = Instant::now();
        let cancel_res_b = shared.cancel_running_query().await;
        let (batch_b, _) = handle_b
            .await
            .expect("join query B task")
            .expect("cancelled query B returns cleanly");
        assert!(cancel_res_b.is_ok(), "cancel B failed: {cancel_res_b:?}");
        assert!(batch_b.cancelled, "query B must be marked cancelled");
        assert!(cancel_start_b.elapsed() < Duration::from_secs(3));
        assert!(proxy.state.tls_aborted.load(Ordering::Acquire) >= 1);
        assert!(proxy.state.direct_plaintext.load(Ordering::Acquire) >= 1);

        shared
            .ping()
            .await
            .expect("established Prefer TLS session remains healthy after cancels");
        drop(shared);
        drop(observer);
    });
    drop(proxy);
}
