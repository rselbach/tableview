#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tv_core::{
    CellValue, ConnectionProfile, DbEngine, DbError, DbObjectKind, ExplainPlan, PageRequest, SafetyPolicy, SortSpec,
    StagedCellValue, StagedChanges, connect_session, test_connection,
};

fn seed_greendale_db(db_path: &std::path::Path) {
    let conn = Connection::open(db_path).expect("create seed sqlite database");
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;

         CREATE TABLE study_groups (
             group_id INTEGER PRIMARY KEY AUTOINCREMENT,
             room_name TEXT NOT NULL,
             building TEXT NOT NULL
         );
         INSERT INTO study_groups (group_id, room_name, building) VALUES
             (1, 'Study Room F', 'Greendale Library'),
             (2, 'Air Conditioning Annex', 'Vocational Wing');

         CREATE TABLE courses (
             course_code TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             credits INTEGER NOT NULL
         );
         INSERT INTO courses (course_code, title, credits) VALUES
             ('SPAN-101', 'Spanish 101', 3),
             ('FILM-201', 'Meta Filmmaking', 4),
             ('LAW-105', 'Intro to Grifting & Law', 3);

         CREATE TABLE students (
             student_id INTEGER PRIMARY KEY,
             external_uuid UUID NOT NULL UNIQUE,
             full_name TEXT NOT NULL,
             email TEXT,
             enrolled BOOLEAN NOT NULL DEFAULT 1,
             gpa NUMERIC(4,2),
             tuition_balance REAL,
             profile JSON,
             badge BLOB,
             enrolled_on DATE,
             study_time TIME,
             created_at DATETIME NOT NULL,
             group_id INTEGER REFERENCES study_groups
         );
         CREATE INDEX idx_students_name ON students (full_name);
         CREATE INDEX idx_students_group ON students (group_id);

         INSERT INTO students (
             student_id, external_uuid, full_name, email, enrolled, gpa, tuition_balance,
             profile, badge, enrolled_on, study_time, created_at, group_id
         ) VALUES
             (1, '210b3d6b-da52-4259-ac05-b58d3d0a6d5b', 'Troy Barnes', 'troy@greendale.edu', 1, 3.45, 125.50,
              '{\"best_friend\":\"Abed\",\"handshake\":true}', X'deadbeef', '2026-09-01', '14:30:00', '2026-11-19T18:09:11Z', 1),
             (2, '8c5b8676-0203-44ba-9198-90125b690e81', 'Abed Nadir', NULL, 'true', 3.92, 0.0,
              '{\"bedtime\":\"strict\"}', X'00ff', '2026-09-01', '15:00:00', '2026-11-19T18:24:56Z', 1),
             (3, '11111111-2222-3333-4444-555555555555', 'Annie Edison', '', 1, 4.00, 12.25,
              '{\"pens\":12}', NULL, '2026-09-02', '08:15:00', '2026-11-19 19:00:00', 1),
             (4, '99999999-8888-7777-6666-555555555555', 'Jeff Winger', 'jeff@greendale.edu', 0, 2.75, 450.75,
              NULL, X'', '2026-09-03', '11:00:00', '2026-11-20T09:15:00Z', 2);

         CREATE TABLE enrollments (
             enrollment_id INTEGER PRIMARY KEY AUTOINCREMENT,
             student_id INTEGER NOT NULL REFERENCES students(student_id),
             course_code TEXT NOT NULL REFERENCES courses(course_code),
             grade TEXT
         );
         CREATE UNIQUE INDEX idx_enrollments_student_course ON enrollments (student_id, course_code);
         INSERT INTO enrollments (student_id, course_code, grade) VALUES
             (1, 'SPAN-101', 'B+'),
             (2, 'FILM-201', 'A'),
             (3, 'LAW-105', 'A+');

         CREATE VIEW active_students_view AS
             SELECT student_id, full_name, email FROM students WHERE enrolled IN (1, 'true');",
    )
    .expect("seed Greendale SQLite schema");
}

#[test]
fn live_sqlite_full_session_introspection_crud_explain_and_readonly() {
    let dir = TempDir::new().expect("create temp dir");
    let db_path = dir.path().join("greendale.sqlite3");
    seed_greendale_db(&db_path);

    let rt = Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        // Missing file must return ConnectionFailed and MUST NOT create a new empty file.
        let missing_path = dir.path().join("does_not_exist.db");
        let missing_profile = ConnectionProfile::new_sqlite("Missing", &missing_path);
        let missing_err = connect_session(&missing_profile, None, None).await;
        assert!(matches!(missing_err, Err(DbError::ConnectionFailed(_))));
        assert!(!missing_path.exists(), "missing SQLite file must not be auto-created");

        // test_connection & connect_session dispatch
        let profile = ConnectionProfile::new_sqlite("Greendale", &db_path);
        let info = test_connection(&profile, None)
            .await
            .expect("test_connection succeeds");
        assert!(info.version.starts_with("SQLite 3"), "got {}", info.version);
        let ver_str = info.version.strip_prefix("SQLite ").expect("SQLite prefix");
        let mut ver_parts = ver_str.split('.').map(|s| s.parse::<u32>().unwrap_or(0));
        let ver_tuple = (
            ver_parts.next().unwrap_or(0),
            ver_parts.next().unwrap_or(0),
            ver_parts.next().unwrap_or(0),
        );
        assert!(
            ver_tuple >= (3, 54, 0),
            "bundled SQLite must be >= 3.54.0 for RUSTSEC-2026-0459, got {}",
            info.version
        );
        assert_eq!(info.current_database, "greendale.sqlite3");
        assert!(info.current_user.is_empty());

        let (session, init_logs) = connect_session(&profile, None, None)
            .await
            .expect("connect_session succeeds");
        assert_eq!(session.engine(), DbEngine::Sqlite);
        assert_eq!(session.server_info().current_database, "greendale.sqlite3");
        assert_eq!(init_logs.len(), 1);
        session.ping().await.expect("ping succeeds");

        // list_databases
        let (dbs, _) = session.list_databases().await.expect("list_databases");
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].name, "greendale.sqlite3");
        assert!(dbs[0].owner.is_empty());
        assert!(dbs[0].size_bytes.is_some_and(|sz| sz > 0));

        // list_schemas (main initially)
        let (schemas, _) = session.list_schemas().await.expect("list_schemas");
        assert_eq!(schemas.first().map(|s| (s.name.as_str(), s.is_system)), Some(("main", false)));

        // list_objects excludes sqlite_sequence from AUTOINCREMENT
        let (objects, _) = session.list_objects("main").await.expect("list_objects");
        let obj_summary: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
        assert_eq!(
            obj_summary,
            vec![
                ("active_students_view", DbObjectKind::View),
                ("courses", DbObjectKind::Table),
                ("enrollments", DbObjectKind::Table),
                ("students", DbObjectKind::Table),
                ("study_groups", DbObjectKind::Table),
            ]
        );

        // list_columns
        let (cols, _) = session.list_columns("main", "students").await.expect("list_columns");
        assert_eq!(cols.len(), 13);
        assert_eq!(cols[0].name, "student_id");
        assert!(cols[0].is_primary_key);
        assert_eq!(cols[2].name, "full_name");
        assert!(!cols[2].nullable);
        assert_eq!(cols[3].name, "email");
        assert!(cols[3].nullable);

        // fetch_table_page: page 1 (limit 2, offset 0) -> COUNT(*) exact total = 4
        let (page1, page1_logs) = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
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
        assert_eq!(page1_logs.len(), 2); // SELECT + COUNT(*)
        assert_eq!(page1.rows.len(), 2);
        assert!(page1.has_more);
        assert!(page1.total_is_exact);
        assert_eq!(page1.total_rows_estimate, Some(4));

        // Verify typed CellValues on Troy Barnes (row 0) and Abed Nadir (row 1)
        assert_eq!(page1.rows[0][0], CellValue::Int(1));
        assert!(matches!(page1.rows[0][1], CellValue::Uuid(_)));
        assert_eq!(page1.rows[0][2], CellValue::Text("Troy Barnes".to_owned()));
        assert_eq!(page1.rows[0][3], CellValue::Text("troy@greendale.edu".to_owned()));
        assert_eq!(page1.rows[0][4], CellValue::Bool(true));
        assert_eq!(page1.rows[0][5], CellValue::Numeric("3.45".to_owned()));
        assert_eq!(page1.rows[0][6], CellValue::Float(125.50));
        assert!(matches!(page1.rows[0][7], CellValue::Json(_)));
        assert_eq!(page1.rows[0][8], CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(page1.rows[0][9], CellValue::Date("2026-09-01".to_owned()));
        assert_eq!(page1.rows[0][10], CellValue::Time("14:30:00".to_owned()));
        assert!(matches!(page1.rows[0][11], CellValue::Timestamp { utc: Some(_), .. }));

        // Abed has NULL email and text 'true' boolean
        assert_eq!(page1.rows[1][3], CellValue::Null);
        assert_eq!(page1.rows[1][4], CellValue::Bool(true));

        // Page 2 (limit 2, offset 2) -> !has_more, exact total = 4
        let (page2, page2_logs) = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
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
        assert_eq!(page2_logs.len(), 1);
        assert_eq!(page2.rows.len(), 2);
        assert!(!page2.has_more);
        assert!(page2.total_is_exact);
        assert_eq!(page2.total_rows_estimate, Some(4));
        // Annie has empty string email, naive datetime (utc: None)
        assert_eq!(page2.rows[0][3], CellValue::Text(String::new()));
        assert_eq!(
            page2.rows[0][11],
            CellValue::Timestamp {
                raw: "2026-11-19 19:00:00".to_owned(),
                utc: None
            }
        );
        // Jeff has enrolled = 0 -> Bool(false), NULL profile, empty blob
        assert_eq!(page2.rows[1][4], CellValue::Bool(false));
        assert_eq!(page2.rows[1][7], CellValue::Null);
        assert_eq!(page2.rows[1][8], CellValue::Bytes(Vec::new()));

        // Filter + DESC sort
        let (filtered, _) = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
                table: "students".to_owned(),
                filter_where: Some("enrolled = 1 OR enrolled = 'true'".to_owned()),
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

        // Unknown sort column rejected
        let bad_sort = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
                table: "students".to_owned(),
                filter_where: None,
                sort: Some(SortSpec {
                    column: "nonexistent_col".to_owned(),
                    ascending: true,
                }),
                limit: 10,
                offset: 0,
            })
            .await;
        assert!(matches!(bad_sort, Err(DbError::InvalidInput(_))));

        // Syntax-broken WHERE filter rejected
        let bad_where = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
                table: "students".to_owned(),
                filter_where: Some("student_id = (".to_owned()),
                sort: None,
                limit: 10,
                offset: 0,
            })
            .await;
        assert!(matches!(bad_where, Err(DbError::QueryFailed { .. })));

        // Empty table columns handled cleanly
        let (missing_tbl_page, _) = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
                table: "no_such_table".to_owned(),
                filter_where: None,
                sort: None,
                limit: 10,
                offset: 0,
            })
            .await
            .expect("empty columns handled cleanly");
        assert!(missing_tbl_page.columns.is_empty());
        assert!(missing_tbl_page.rows.is_empty());
        assert!(!missing_tbl_page.has_more);

        // describe_table on students (PK, single-col UNIQUE, implicit FK target col, indexes, DDL, row count)
        let (structure, _) = session
            .describe_table("main", "students")
            .await
            .expect("describe_table students");
        assert_eq!(structure.columns.len(), 13);
        assert!(structure.columns[0].is_primary_key);
        assert!(structure.columns[1].is_unique);
        assert!(!structure.columns[2].is_unique);
        let group_fk = structure.columns[12].foreign_key.as_ref().expect("group_id FK");
        assert_eq!(group_fk.target_schema, "main");
        assert_eq!(group_fk.target_table, "study_groups");
        assert_eq!(group_fk.target_column, "group_id");

        assert!(structure.indexes.iter().any(|i| i.name == "idx_students_name" && i.columns == ["full_name"]));
        assert!(structure.indexes.iter().any(|i| i.name == "idx_students_group" && i.columns == ["group_id"]));
        assert_eq!(structure.stats.estimated_rows, Some(4));
        assert!(structure.stats.ddl.contains("CREATE TABLE students"));
        assert!(structure.stats.ddl.contains("CREATE INDEX idx_students_name"));

        // describe_table on enrollments (composite unique index does NOT mark single columns as unique)
        let (enroll_struct, _) = session
            .describe_table("main", "enrollments")
            .await
            .expect("describe_table enrollments");
        assert!(!enroll_struct.columns[1].is_unique);
        assert!(!enroll_struct.columns[2].is_unique);
        let stu_fk = enroll_struct.columns[1].foreign_key.as_ref().expect("student_id FK");
        assert_eq!(stu_fk.target_table, "students");
        assert_eq!(stu_fk.target_column, "student_id");

        // describe_table on courses (TEXT PRIMARY KEY creates sqlite_autoindex with is_primary)
        let (courses_struct, _) = session
            .describe_table("main", "courses")
            .await
            .expect("describe_table courses");
        assert!(courses_struct.columns[0].is_primary_key);
        assert!(
            courses_struct
                .indexes
                .iter()
                .any(|i| i.is_primary && i.columns == ["course_code"])
        );

        // describe_table on view
        let (view_struct, _) = session
            .describe_table("main", "active_students_view")
            .await
            .expect("describe_table view");
        assert_eq!(view_struct.stats.estimated_rows, None);
        assert!(view_struct.stats.ddl.contains("CREATE VIEW active_students_view"));

        // fetch_fk_row (text PK and int PK + NULL guard)
        let (group_peek, peek_log) = session
            .fetch_fk_row(group_fk, &CellValue::Int(2), &structure.columns[12])
            .await
            .expect("fetch_fk_row group 2");
        assert!(peek_log.sql.contains("\"main\".\"study_groups\""));
        let group_row = group_peek.row.expect("group 2 exists");
        assert_eq!(group_row[1], CellValue::Text("Air Conditioning Annex".to_owned()));

        let course_fk = enroll_struct.columns[2].foreign_key.as_ref().expect("course_code FK");
        let (course_peek, _) = session
            .fetch_fk_row(course_fk, &CellValue::Text("FILM-201".to_owned()), &enroll_struct.columns[2])
            .await
            .expect("fetch_fk_row FILM-201");
        let course_row = course_peek.row.expect("FILM-201 exists");
        assert_eq!(course_row[1], CellValue::Text("Meta Filmmaking".to_owned()));

        let null_peek = session
            .fetch_fk_row(group_fk, &CellValue::Null, &structure.columns[12])
            .await;
        assert!(matches!(null_peek, Err(DbError::InvalidInput(_))));

        // explain_sql: SCAN vs SEARCH with index
        let (scan_plan, scan_log) = session
            .explain_sql("SELECT * FROM students WHERE email = 'troy@greendale.edu'", false)
            .await
            .expect("explain SCAN");
        assert!(scan_log.sql.starts_with("EXPLAIN QUERY PLAN"));
        assert!(!scan_plan.analyzed);
        assert_eq!(scan_plan.root.node_type, "SCAN");
        assert_eq!(scan_plan.root.relation_name.as_deref(), Some("students"));

        let (search_plan, _) = session
            .explain_sql("SELECT * FROM students WHERE full_name = 'Troy Barnes'", true)
            .await
            .expect("explain SEARCH");
        assert!(!search_plan.analyzed);
        assert_eq!(search_plan.root.node_type, "SEARCH");
        assert_eq!(search_plan.root.relation_name.as_deref(), Some("students"));
        assert_eq!(search_plan.root.index_name.as_deref(), Some("idx_students_name"));

        // EXPLAIN QUERY PLAN run through execute_sql is also parseable by ExplainPlan::try_extract_from_result
        let (eqp_batch, _) = session
            .execute_sql(
                &["EXPLAIN QUERY PLAN SELECT * FROM students WHERE full_name = 'Troy Barnes'".to_owned()],
                None,
            )
            .await
            .expect("execute_sql EXPLAIN QUERY PLAN");
        let extracted_plan =
            ExplainPlan::try_extract_from_result(&eqp_batch.results[0]).expect("extract sqlite EXPLAIN");
        assert_eq!(extracted_plan.root.node_type, "SEARCH");

        // Empty transaction rejected cleanly
        let empty_tx_err = session.execute_transaction(&[]).await;
        assert!(matches!(empty_tx_err, Err(DbError::InvalidInput(_))));

        // execute_sql: multi-statement batch + row_limit + inferred types + error stop
        let (batch, exec_logs) = session
            .execute_sql(
                &[
                    "SELECT student_id, full_name FROM students ORDER BY student_id".to_owned(),
                    "SELECT COUNT(*) AS student_total FROM students".to_owned(),
                ],
                Some(2),
            )
            .await
            .expect("execute_sql batch");
        assert!(!batch.cancelled);
        assert_eq!(exec_logs.len(), 2);
        assert_eq!(batch.results[0].rows.len(), 2);
        assert!(batch.results[0].truncated);
        assert_eq!(batch.results[0].total_rows_seen, 4);
        assert_eq!(batch.results[1].columns[0].data_type, "integer");
        assert_eq!(batch.results[1].rows[0][0], CellValue::Int(4));

        // execute_sql DML + error stopping batch
        let (dml_batch, _) = session
            .execute_sql(
                &[
                    "UPDATE students SET enrolled = 1 WHERE student_id = 4".to_owned(),
                    "SELECT * FROM missing_table_xyz".to_owned(),
                    "SELECT 99".to_owned(),
                ],
                None,
            )
            .await
            .expect("execute_sql DML and error");
        assert_eq!(dml_batch.results.len(), 2);
        assert_eq!(dml_batch.results[0].rows_affected, Some(1));
        assert!(dml_batch.results[1].error.is_some());

        // Staged edits transaction commit + rollback on error
        let mut staged = StagedChanges::new();
        staged.stage_cell_edit(
            0,
            &page1.rows[0],
            2,
            StagedCellValue::Literal("Troy 'T-Bone' Barnes".to_owned()),
        );
        staged.stage_cell_edit(0, &page1.rows[0], 4, StagedCellValue::Literal("false".to_owned()));
        staged.stage_cell_edit(
            0,
            &page1.rows[0],
            7,
            StagedCellValue::Literal(r#"{"ac":"Master"}"#.to_owned()),
        );
        staged.stage_cell_edit(0, &page1.rows[0], 8, StagedCellValue::Literal("0xcafebabe".to_owned()));

        let draft = staged.stage_insert_row(&page1.columns);
        staged.stage_insert_cell_edit(draft, 0, StagedCellValue::Literal("5".to_owned()));
        staged.stage_insert_cell_edit(
            draft,
            1,
            StagedCellValue::Literal("22222222-3333-4444-5555-666666666666".to_owned()),
        );
        staged.stage_insert_cell_edit(draft, 2, StagedCellValue::Literal("Pierce Hawthorne".to_owned()));
        staged.stage_insert_cell_edit(draft, 4, StagedCellValue::Literal("1".to_owned()));
        staged.stage_insert_cell_edit(draft, 11, StagedCellValue::Literal("2026-11-21T10:00:00Z".to_owned()));
        staged.stage_insert_cell_edit(draft, 12, StagedCellValue::Literal("1".to_owned()));

        // Delete Jeff (student_id = 4); delete enrollment first or test delete on Jeff after clearing FK
        // Wait: Jeff has no enrollments in seed data!
        staged.toggle_delete_row(100, &page2.rows[1]);

        let commit_stmts = staged
            .build_statements_for_engine(DbEngine::Sqlite, "main", "students", &page1.columns)
            .expect("build sqlite staged statements");
        let (tx_out, tx_logs) = session
            .execute_transaction(&commit_stmts)
            .await
            .expect("execute_transaction commit");
        assert_eq!(tx_out.statements_executed, 3);
        assert_eq!(tx_out.total_rows_affected, 3);
        assert_eq!(tx_logs.len(), 5);

        let (after_commit, _) = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
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
        assert_eq!(
            after_commit.rows[0][2],
            CellValue::Text("Troy 'T-Bone' Barnes".to_owned())
        );
        assert_eq!(after_commit.rows[0][4], CellValue::Bool(false));
        assert_eq!(
            after_commit.rows[0][7],
            CellValue::Json(r#"{"ac":"Master"}"#.to_owned())
        );
        assert_eq!(after_commit.rows[0][8], CellValue::Bytes(vec![0xca, 0xfe, 0xba, 0xbe]));
        assert_eq!(after_commit.rows[3][0], CellValue::Int(5));
        assert_eq!(after_commit.rows[3][2], CellValue::Text("Pierce Hawthorne".to_owned()));

        // Transaction rollback on constraint violation (2nd statement violates PK)
        let rollback_err = session
            .execute_transaction(&[
                "UPDATE \"main\".\"students\" SET \"full_name\" = 'Evil Troy' WHERE \"student_id\" = 1;".to_owned(),
                "INSERT INTO \"main\".\"students\" (\"student_id\", \"external_uuid\", \"full_name\", \"enrolled\", \"created_at\") VALUES (1, '33333333-3333-4444-5555-666666666666', 'Evil Abed', 1, '2026-11-21T00:00:00Z');".to_owned(),
            ])
            .await;
        assert!(matches!(rollback_err, Err(DbError::QueryFailed { .. })));

        let (after_rollback, _) = session
            .fetch_table_page(&PageRequest {
                schema: "main".to_owned(),
                table: "students".to_owned(),
                filter_where: Some("student_id = 1".to_owned()),
                sort: None,
                limit: 10,
                offset: 0,
            })
            .await
            .expect("refetch after rollback");
        assert_eq!(
            after_rollback.rows[0][2],
            CellValue::Text("Troy 'T-Bone' Barnes".to_owned())
        );

        // ReadOnly safety policy opens SQLite with SQLITE_OPEN_READ_ONLY
        let mut ro_profile = ConnectionProfile::new_sqlite("Greendale RO", &db_path);
        ro_profile.safety = SafetyPolicy::ReadOnly;
        let (ro_session, _) = connect_session(&ro_profile, None, None)
            .await
            .expect("connect read-only");
        let (ro_read, _) = ro_session
            .execute_sql(&["SELECT COUNT(*) FROM students".to_owned()], None)
            .await
            .expect("read-only SELECT succeeds");
        assert_eq!(ro_read.results[0].rows[0][0], CellValue::Int(4));

        let (ro_write, _) = ro_session
            .execute_sql(
                &["UPDATE students SET full_name = 'Blocked' WHERE student_id = 1".to_owned()],
                None,
            )
            .await
            .expect("read-only write returns batch error");
        assert!(ro_write.results[0].error.is_some());
        let ro_tx = ro_session
            .execute_transaction(&["DELETE FROM students WHERE student_id = 5".to_owned()])
            .await;
        assert!(matches!(ro_tx, Err(DbError::QueryFailed { .. })));

        // Live query cancellation via InterruptHandle
        let shared = Arc::clone(&session);
        let canceller = Arc::clone(&session);
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            canceller.cancel_running_query().await
        });
        let slow_sql = "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 500000000) SELECT SUM(x) FROM cnt";
        let (cancel_batch, _) = shared
            .execute_sql(&[slow_sql.to_owned()], None)
            .await
            .expect("cancelled query returns QueryBatchResult");
        cancel_task.await.expect("join cancel task").expect("cancel ok");
        assert!(cancel_batch.cancelled);
        assert!(cancel_batch.results[0].error.is_some());
    });
}

#[test]
fn live_sqlite_in_memory_and_attached_temp_schemas() {
    let dir = TempDir::new().expect("create temp dir");
    let extra_path = dir.path().join("extra.db");
    {
        let conn = Connection::open(&extra_path).expect("create extra db");
        conn.execute_batch("CREATE TABLE campus_events (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
            .expect("seed extra db");
    }

    let rt = Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let profile = ConnectionProfile::new_sqlite("In-Memory Greendale", ":memory:");
        let (session, _) = connect_session(&profile, None, None)
            .await
            .expect("connect :memory:");
        assert_eq!(session.server_info().current_database, ":memory:");

        let (dbs, _) = session.list_databases().await.expect("list_databases :memory:");
        assert_eq!(dbs[0].name, ":memory:");
        assert_eq!(dbs[0].size_bytes, None);

        let extra_sql = format!(
            "ATTACH DATABASE '{}' AS extra",
            extra_path.to_string_lossy().replace('\'', "''")
        );
        let (setup_batch, _) = session
            .execute_sql(
                &[
                    extra_sql,
                    "CREATE TEMP TABLE scratch_notes (id INTEGER PRIMARY KEY, body TEXT)".to_owned(),
                    "CREATE TABLE main.dean_announcements (id INTEGER PRIMARY KEY, title TEXT NOT NULL)".to_owned(),
                ],
                None,
            )
            .await
            .expect("setup schemas");
        assert!(setup_batch.results.iter().all(|r| r.error.is_none()));

        let (schemas, _) = session.list_schemas().await.expect("list_schemas");
        assert_eq!(schemas[0], tv_core::SchemaInfo { name: "main".to_owned(), is_system: false });
        assert!(schemas.iter().any(|s| s.name == "extra" && !s.is_system));
        assert!(schemas.iter().any(|s| s.name == "temp" && s.is_system));

        let (extra_objs, _) = session.list_objects("extra").await.expect("list_objects extra");
        assert_eq!(extra_objs.len(), 1);
        assert_eq!(extra_objs[0].name, "campus_events");

        let (temp_objs, _) = session.list_objects("temp").await.expect("list_objects temp");
        assert_eq!(temp_objs.len(), 1);
        assert_eq!(temp_objs[0].name, "scratch_notes");
    });
}

#[test]
fn live_sqlite_cancellation_is_scoped_to_matching_query() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let rt = Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let profile = ConnectionProfile::new_sqlite("Scoped Cancel", ":memory:");
        let (shared, _) = connect_session(&profile, None, None)
            .await
            .expect("connect :memory:");

        shared
            .execute_sql(
                &["CREATE TABLE greendale_cancel_log (id INTEGER PRIMARY KEY, label TEXT NOT NULL)".to_owned()],
                None,
            )
            .await
            .expect("create log table");

        // 1. Cancelling a waiting Query B (key 102) MUST NOT interrupt running Query A (key 101).
        let long_sql_a = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 8000000) SELECT count(*) FROM c".to_owned();
        let flag_a = Arc::new(AtomicBool::new(false));
        let flag_b = Arc::new(AtomicBool::new(false));
        let sess_a = Arc::clone(&shared);
        let flag_a_task = Arc::clone(&flag_a);
        let task_a = tokio::spawn(async move {
            sess_a
                .execute_sql_cancellable(&[long_sql_a], None, 101, Some(&flag_a_task))
                .await
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        let sess_b = Arc::clone(&shared);
        let flag_b_task = Arc::clone(&flag_b);
        let task_b = tokio::spawn(async move {
            sess_b
                .execute_sql_cancellable(&["SELECT 'Troy Barnes'".to_owned()], None, 102, Some(&flag_b_task))
                .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        flag_b.store(true, Ordering::Release);
        shared
            .cancel_query_by_key(102)
            .await
            .expect("cancel waiting query B");

        let res_b = task_b.await.expect("join query B");
        assert!(
            matches!(res_b, Err(DbError::Cancelled)),
            "waiting query B should cancel cleanly, got {res_b:?}"
        );
        let (batch_a, _) = task_a
            .await
            .expect("join query A")
            .expect("query A must finish unharmed");
        assert!(!batch_a.cancelled, "query A must not be interrupted by cancelling query B");
        assert_eq!(batch_a.results[0].rows[0][0], CellValue::Int(8_000_000));

        // 2. Cancelling active Query C (key 103) interrupts Query C cleanly without breaking subsequent queries.
        let long_sql_c = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 500000000) SELECT count(*) FROM c".to_owned();
        let flag_c = Arc::new(AtomicBool::new(false));
        let sess_c = Arc::clone(&shared);
        let flag_c_task = Arc::clone(&flag_c);
        let task_c = tokio::spawn(async move {
            sess_c
                .execute_sql_cancellable(&[long_sql_c], None, 103, Some(&flag_c_task))
                .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        flag_c.store(true, Ordering::Release);
        shared
            .cancel_query_by_key(103)
            .await
            .expect("cancel active query C");
        let res_c = task_c.await.expect("join query C");
        assert!(
            matches!(res_c, Err(DbError::Cancelled))
                || matches!(res_c, Ok((ref batch, _)) if batch.cancelled),
            "active query C should be cancelled, got {res_c:?}"
        );

        let (batch_d, _) = shared
            .execute_sql_cancellable(&["SELECT 42".to_owned()], None, 104, None)
            .await
            .expect("query D after cancel succeeds");
        assert!(!batch_d.cancelled);
        assert_eq!(batch_d.results[0].rows[0][0], CellValue::Int(42));

        // 3. Setting cancel_flag on queued Query E behind execute_transaction prevents E from running.
        let sess_tx = Arc::clone(&shared);
        let tx_task = tokio::spawn(async move {
            sess_tx
                .execute_transaction(&[
                    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 8000000) SELECT count(*) FROM c".to_owned(),
                    "INSERT INTO greendale_cancel_log (id, label) VALUES (1, 'Annie Edison')".to_owned(),
                ])
                .await
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        let flag_e = Arc::new(AtomicBool::new(false));
        let sess_e = Arc::clone(&shared);
        let flag_e_task = Arc::clone(&flag_e);
        let task_e = tokio::spawn(async move {
            sess_e
                .execute_sql_cancellable(
                    &["INSERT INTO greendale_cancel_log (id, label) VALUES (2, 'Pierce Hawthorne')".to_owned()],
                    None,
                    105,
                    Some(&flag_e_task),
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(15)).await;
        flag_e.store(true, Ordering::Release);

        tx_task
            .await
            .expect("join tx task")
            .expect("tx commits without interruption");
        let res_e = task_e.await.expect("join query E");
        assert!(
            matches!(res_e, Err(DbError::Cancelled)),
            "queued query E with cancel_flag should not execute, got {res_e:?}"
        );

        let (verify_batch, _) = shared
            .execute_sql(
                &["SELECT id, label FROM greendale_cancel_log ORDER BY id".to_owned()],
                None,
            )
            .await
            .expect("verify table contents");
        assert_eq!(verify_batch.results[0].rows.len(), 1);
        assert_eq!(verify_batch.results[0].rows[0][0], CellValue::Int(1));
        assert_eq!(
            verify_batch.results[0].rows[0][1],
            CellValue::Text("Annie Edison".to_owned())
        );

        // 4. Dropping an unkeyed query future while running MUST keep conn_gate held until cleanup,
        // so a subsequently queued & cancelled keyed INSERT still cancels cleanly without executing.
        let unkeyed_sess = Arc::clone(&shared);
        let unkeyed_handle = tokio::spawn(async move {
            let slow = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 6000000) SELECT count(*) FROM c".to_owned();
            unkeyed_sess.execute_sql(&[slow], None).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        unkeyed_handle.abort();
        drop(unkeyed_handle.await);

        let flag_f = Arc::new(AtomicBool::new(false));
        let sess_f = Arc::clone(&shared);
        let flag_f_task = Arc::clone(&flag_f);
        let task_f = tokio::spawn(async move {
            sess_f
                .execute_sql_cancellable(
                    &["INSERT INTO greendale_cancel_log (id, label) VALUES (3, 'Shirley Bennett')".to_owned()],
                    None,
                    106,
                    Some(&flag_f_task),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        flag_f.store(true, Ordering::Release);
        shared
            .cancel_query_by_key(106)
            .await
            .expect("cancel queued INSERT after dropped unkeyed future");

        let res_f = task_f.await.expect("join query F");
        assert!(
            matches!(res_f, Err(DbError::Cancelled)),
            "queued INSERT after dropped unkeyed future should cancel cleanly, got {res_f:?}"
        );

        let (verify_after_drop, _) = shared
            .execute_sql(
                &["SELECT id, label FROM greendale_cancel_log ORDER BY id".to_owned()],
                None,
            )
            .await
            .expect("verify table after dropped future cancel");
        assert_eq!(verify_after_drop.results[0].rows.len(), 1);
    });
}
