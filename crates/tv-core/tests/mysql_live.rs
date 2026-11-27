#![allow(clippy::expect_used)]

use std::env;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Runtime;
use tv_core::engine::mysql::MysqlSession;
use tv_core::{
    CellValue, ConnectionProfile, DbEngine, DbError, DbObjectKind, DbSession, ForeignKeyRef, PageRequest,
    PasswordMode, SortSpec, SslMode, StagedCellValue, StagedChanges,
};

struct TempMysqlDatabaseGuard {
    admin_profile: ConnectionProfile,
    password: Option<String>,
    db_name: String,
    rt: Runtime,
}

impl Drop for TempMysqlDatabaseGuard {
    fn drop(&mut self) {
        let admin = self.admin_profile.clone();
        let password = self.password.clone();
        let db_name = self.db_name.clone();
        self.rt.block_on(async move {
            if let Ok((session, _)) = MysqlSession::connect(&admin, password.as_deref(), None).await {
                let drop_sql = format!("DROP DATABASE IF EXISTS {}", DbEngine::Mysql.quote_ident(&db_name));
                drop(session.execute_sql(&[drop_sql], None).await);
            }
        });
    }
}

fn resolve_test_connection() -> Option<(ConnectionProfile, Option<String>)> {
    if let Ok(url) = env::var("MYSQL_TEST_URL")
        && !url.trim().is_empty()
    {
        let (mut profile, secret) = ConnectionProfile::from_mysql_url(&url).expect("valid MYSQL_TEST_URL");
        profile.database = None;
        return Some((profile, secret));
    }

    let addr: SocketAddr = "127.0.0.1:3306".parse().expect("valid socket addr");
    if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err() {
        return None;
    }

    let user = env::var("MYSQL_USER").unwrap_or_else(|_| "root".to_owned());
    let password = env::var("MYSQL_PASSWORD").ok().filter(|s| !s.is_empty());
    let mut profile = ConnectionProfile::new_mysql("greendale_mysql_local", "127.0.0.1", &user);
    profile.port = 3306;
    profile.password_mode = if password.is_some() {
        PasswordMode::Keychain
    } else {
        PasswordMode::None
    };
    profile.ssl.mode = SslMode::Prefer;
    profile.database = None;
    Some((profile, password))
}

#[test]
fn live_mysql_introspection_data_grid_and_structure_roundtrip() {
    let Some((admin, password)) = resolve_test_connection() else {
        eprintln!("skipping mysql_live: no MySQL server reachable on 127.0.0.1:3306 and MYSQL_TEST_URL not set");
        return;
    };

    let rt = Runtime::new().expect("create tokio runtime");
    let db_name = format!("tableview_mysql_test_{}", std::process::id());
    let fq_students = DbEngine::Mysql.qualify_ident(&db_name, "students");
    let fq_groups = DbEngine::Mysql.qualify_ident(&db_name, "study_groups");

    rt.block_on(async {
        let (admin_session, _) = MysqlSession::connect(&admin, password.as_deref(), None)
            .await
            .expect("connect to MySQL admin session");

        let create_stmts = vec![
            format!("DROP DATABASE IF EXISTS {}", DbEngine::Mysql.quote_ident(&db_name)),
            format!("CREATE DATABASE {}", DbEngine::Mysql.quote_ident(&db_name)),
        ];
        let (init_res, _) = admin_session
            .execute_sql(&create_stmts, None)
            .await
            .expect("create temp mysql database");
        assert!(init_res.results.iter().all(|r| r.error.is_none()));
    });

    let _guard = TempMysqlDatabaseGuard {
        admin_profile: admin.clone(),
        password: password.clone(),
        db_name: db_name.clone(),
        rt: Runtime::new().expect("cleanup runtime"),
    };

    rt.block_on(async {
        let tested = MysqlSession::test_connection(&admin, password.as_deref())
            .await
            .expect("test_connection succeeds");
        assert!(
            tested.version.starts_with("MySQL ") || tested.version.starts_with("MariaDB "),
            "got {}",
            tested.version
        );

        let (admin_no_db, _) = MysqlSession::connect(&admin, password.as_deref(), None)
            .await
            .expect("connect without default db");
        let (all_schemas, _) = admin_no_db.list_schemas().await.expect("list_schemas without active db");
        assert!(
            all_schemas
                .iter()
                .any(|s| s.name.eq_ignore_ascii_case("information_schema") && s.is_system)
        );
        drop(admin_no_db);

        let (session, init_logs) = MysqlSession::connect(&admin, password.as_deref(), Some(&db_name))
            .await
            .expect("connect to temporary database");
        assert_eq!(session.engine(), DbEngine::Mysql);
        assert_eq!(session.server_info().current_database, db_name);
        assert_eq!(init_logs.len(), 1);

        let seed_stmts = vec![
            format!(
                "CREATE TABLE {fq_groups} (
                    group_id INT PRIMARY KEY,
                    room_name VARCHAR(100) NOT NULL
                )"
            ),
            format!("INSERT INTO {fq_groups} VALUES (1, 'Study Room F')"),
            format!(
                "CREATE TABLE {fq_students} (
                    student_id INT PRIMARY KEY,
                    id CHAR(36) NOT NULL,
                    full_name VARCHAR(120) NOT NULL COMMENT 'Preferred display name',
                    email VARCHAR(120) NULL,
                    enrolled BOOLEAN NOT NULL DEFAULT TRUE,
                    gpa DECIMAL(4,2),
                    major ENUM('Spanish', 'Film', 'Law', 'AirConditioning') NOT NULL,
                    profile JSON,
                    badge VARBINARY(64),
                    group_id INT,
                    created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
                    UNIQUE KEY students_id_uidx (id),
                    KEY students_name_idx (full_name),
                    CONSTRAINT students_group_fk FOREIGN KEY (group_id) REFERENCES {fq_groups}(group_id)
                ) COMMENT='Greendale Community College active roster'"
            ),
            format!(
                "INSERT INTO {fq_students}
                    (student_id, id, full_name, email, enrolled, gpa, major, profile, badge, group_id, created_at)
                 VALUES
                    (1, '210b3d6b-da52-4259-ac05-b58d3d0a6d5b', 'Troy Barnes', 'troy@greendale.edu', TRUE, 3.45, 'AirConditioning', '{{\"best_friend\":\"Abed\",\"handshake\":true}}', X'deadbeef', 1, '2026-11-19 18:09:11.432'),
                    (2, '8c5b8676-0203-44ba-9198-90125b690e81', 'Abed Nadir', NULL, TRUE, 3.92, 'Film', '{{\"bedtime\":\"strict\"}}', X'00ff', 1, '2026-11-19 18:24:56.558'),
                    (3, '11111111-2222-3333-4444-555555555555', 'Annie Edison', '', TRUE, 4.00, 'Law', '{{\"pens\":12}}', NULL, 1, '2026-11-19 19:00:00.000'),
                    (4, '99999999-8888-7777-6666-555555555555', 'Jeff Winger', 'jeff@greendale.edu', FALSE, 2.75, 'Law', NULL, X'', 1, '2026-11-20 09:15:00.000')"
            ),
            format!(
                "CREATE VIEW {}.`active_students_view` AS
                 SELECT student_id, full_name FROM {fq_students} WHERE enrolled = TRUE",
                DbEngine::Mysql.quote_ident(&db_name)
            ),
        ];
        let (seed_res, _) = session.execute_sql(&seed_stmts, None).await.expect("seed Greendale MySQL schema");
        for r in &seed_res.results {
            assert!(r.error.is_none(), "seed error on {}: {:?}", r.sql, r.error);
        }

        let (dbs, _) = session.list_databases().await.expect("list_databases");
        assert!(dbs.iter().any(|d| d.name == db_name));

        let (schemas, _) = session.list_schemas().await.expect("list_schemas");
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, db_name);
        assert!(!schemas[0].is_system);

        let (objects, _) = session.list_objects(&db_name).await.expect("list_objects");
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
            .list_columns(&db_name, "students")
            .await
            .expect("list_columns");
        assert_eq!(cols.len(), 11);
        assert_eq!(cols[0].name, "student_id");
        assert!(cols[0].is_primary_key);

        // Fetch sorted page 1 (limit 2, offset 0)
        let (page1, page1_logs) = session
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
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
        assert_eq!(
            page1.rows[0][1],
            CellValue::Text("210b3d6b-da52-4259-ac05-b58d3d0a6d5b".to_owned())
        );
        assert_eq!(page1.rows[0][2], CellValue::Text("Troy Barnes".to_owned()));
        assert_eq!(page1.rows[0][3], CellValue::Text("troy@greendale.edu".to_owned()));
        assert_eq!(page1.rows[0][4], CellValue::Bool(true));
        assert_eq!(page1.rows[0][5], CellValue::Numeric("3.45".to_owned()));
        assert_eq!(page1.rows[0][6], CellValue::Text("AirConditioning".to_owned()));
        assert!(matches!(page1.rows[0][7], CellValue::Json(_)));
        assert_eq!(page1.rows[0][8], CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        assert!(matches!(page1.rows[0][10], CellValue::Timestamp { .. }));

        // Row 2 has SQL NULL email
        assert_eq!(page1.rows[1][3], CellValue::Null);

        // Fetch page 2 (limit 2, offset 2) -> exact total 4
        let (page2, _) = session
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
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
        assert_eq!(page2.rows[0][3], CellValue::Text(String::new()));
        assert_eq!(page2.rows[1][7], CellValue::Null);
        assert_eq!(page2.rows[1][8], CellValue::Bytes(Vec::new()));

        // NULLS LAST sort check on nullable email column (DESC places Abed's NULL last)
        let (nulls_last, _) = session
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
                table: "students".to_owned(),
                filter_where: None,
                sort: Some(SortSpec {
                    column: "email".to_owned(),
                    ascending: false,
                }),
                limit: 10,
                offset: 0,
            })
            .await
            .expect("nulls last sort");
        assert_eq!(nulls_last.rows.last().expect("last row")[3], CellValue::Null);

        // WHERE filter + DESC sort
        let (filtered, _) = session
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
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

        // Unknown sort column rejected
        let bad_sort = session
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
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

        // Broken WHERE filter returns QueryFailed
        let bad_where = session
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
                table: "students".to_owned(),
                filter_where: Some("student_id === 'nope'".to_owned()),
                sort: None,
                limit: 10,
                offset: 0,
            })
            .await;
        assert!(matches!(bad_where, Err(DbError::QueryFailed { .. })));

        // Structure & Indexes introspection
        let (structure, _) = session
            .describe_table(&db_name, "students")
            .await
            .expect("describe_table students");
        assert_eq!(structure.columns.len(), 11);
        assert!(structure.columns[0].is_primary_key);
        assert!(structure.columns[1].is_unique);
        assert_eq!(structure.columns[2].comment.as_deref(), Some("Preferred display name"));
        let fk = structure.columns[9].foreign_key.as_ref().expect("group_id FK");
        assert_eq!(fk.target_table, "study_groups");
        assert_eq!(fk.target_column, "group_id");

        assert!(structure.indexes.iter().any(|i| i.is_primary && i.columns == ["student_id"]));
        assert!(structure.indexes.iter().any(|i| i.name == "students_name_idx"));
        assert!(structure.stats.total_bytes.is_some());
        assert_eq!(
            structure.stats.comment.as_deref(),
            Some("Greendale Community College active roster")
        );
        assert!(structure.stats.ddl.contains("CREATE TABLE"));

        let (view_struct, _) = session
            .describe_table(&db_name, "active_students_view")
            .await
            .expect("describe_table view");
        assert!(view_struct.stats.ddl.contains("VIEW"));

        // Multi-statement SQL execution
        let stmts = vec![
            format!("SELECT student_id, id, full_name, email, enrolled, gpa, profile, badge, created_at FROM {fq_students} ORDER BY student_id"),
            format!("SELECT COUNT(*) AS student_total FROM {fq_students}"),
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
        assert_eq!(batch.results[0].rows[1][3], CellValue::Null);
        assert_eq!(batch.results[0].rows[2][3], CellValue::Text(String::new()));
        assert_eq!(batch.results[0].rows[0][5], CellValue::Numeric("3.45".to_owned()));
        assert!(matches!(batch.results[0].rows[0][6], CellValue::Json(_)));
        assert_eq!(batch.results[0].rows[0][7], CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(batch.results[1].rows[0][0], CellValue::Int(4));

        // Row limit enforcement
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
                &[format!("UPDATE {fq_students} SET enrolled = TRUE WHERE student_id = 4")],
                None,
            )
            .await
            .expect("execute DML update");
        assert!(!dml.results[0].is_tabular());
        assert_eq!(dml.results[0].rows_affected, Some(1));

        // Syntax / missing table error stops subsequent statements cleanly
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

        // Live out-of-band cancellation via KILL QUERY
        let shared = Arc::new(session);
        let runner = Arc::clone(&shared);
        let canceller = Arc::clone(&shared);
        let cancel_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            canceller.cancel_running_query().await
        });
        let slow_sql = "SELECT BENCHMARK(200000000, SHA2('greendale', 256))";
        let (slow_batch, _) = runner
            .execute_sql(&[slow_sql.to_owned()], None)
            .await
            .expect("cancelled query returns cleanly");
        let cancel_res = cancel_handle.await.expect("join cancel task");
        assert!(cancel_res.is_ok(), "cancel call failed: {cancel_res:?}");
        assert!(slow_batch.cancelled);

        shared.ping().await.expect("ping succeeds");

        // FK Peek lookup
        let (peek, peek_log) = shared
            .fetch_fk_row(
                &ForeignKeyRef {
                    constraint_name: "students_group_fk".to_owned(),
                    target_schema: db_name.clone(),
                    target_table: "study_groups".to_owned(),
                    target_column: "group_id".to_owned(),
                },
                &CellValue::Int(1),
                &structure.columns[9],
            )
            .await
            .expect("fetch_fk_row succeeds");
        assert!(peek_log.sql.contains("`study_groups`"));
        let peek_row = peek.row.expect("study group 1 exists");
        assert_eq!(peek_row[0], CellValue::Int(1));
        assert_eq!(peek_row[1], CellValue::Text("Study Room F".to_owned()));

        let null_peek = shared
            .fetch_fk_row(
                &structure.columns[9].foreign_key.clone().expect("fk"),
                &CellValue::Null,
                &structure.columns[9],
            )
            .await;
        assert!(matches!(null_peek, Err(DbError::InvalidInput(_))));

        // EXPLAIN FORMAT=JSON
        let join_sql = format!(
            "SELECT s.full_name, g.room_name FROM {fq_students} s JOIN {fq_groups} g ON g.group_id = s.group_id ORDER BY s.student_id"
        );
        let (est_plan, _) = shared
            .explain_sql(&join_sql, false)
            .await
            .expect("estimated explain succeeds");
        assert!(!est_plan.analyzed);
        assert!(est_plan.node_count() >= 1);

        // EXPLAIN ANALYZE (supported on MySQL 8.0.18+)
        if let Ok((ana_plan, ana_log)) = shared.explain_sql(&join_sql, true).await {
            assert!(ana_log.sql.starts_with("EXPLAIN ANALYZE"));
            assert!(ana_plan.analyzed);
            assert!(ana_plan.node_count() >= 1);
        }

        // Staged transactional commit
        let mut staged = StagedChanges::new();
        staged.stage_cell_edit(
            0,
            &page1.rows[0],
            2,
            StagedCellValue::Literal("Troy 'T-Bone' Barnes".to_owned()),
        );
        staged.stage_cell_edit(0, &page1.rows[0], 5, StagedCellValue::Literal("3.55".to_owned()));
        staged.stage_cell_edit(0, &page1.rows[0], 7, StagedCellValue::Literal(r#"{"ac":"Master"}"#.to_owned()));
        staged.stage_cell_edit(0, &page1.rows[0], 8, StagedCellValue::Literal("0xcafebabe".to_owned()));

        let draft = staged.stage_insert_row(&page1.columns);
        staged.stage_insert_cell_edit(draft, 0, StagedCellValue::Literal("5".to_owned()));
        staged.stage_insert_cell_edit(
            draft,
            1,
            StagedCellValue::Literal("22222222-3333-4444-5555-666666666666".to_owned()),
        );
        staged.stage_insert_cell_edit(draft, 2, StagedCellValue::Literal("Pierce Hawthorne".to_owned()));
        staged.stage_insert_cell_edit(draft, 6, StagedCellValue::Literal("Law".to_owned()));
        staged.stage_insert_cell_edit(draft, 9, StagedCellValue::Literal("1".to_owned()));

        staged.toggle_delete_row(100, &page2.rows[1]);
        let commit_stmts = staged
            .build_statements_for_engine(DbEngine::Mysql, &db_name, "students", &page1.columns)
            .expect("build staged statements");
        let (tx_out, tx_logs) = shared
            .execute_transaction(&commit_stmts)
            .await
            .expect("commit staged transaction succeeds");
        assert_eq!(tx_out.statements_executed, 3);
        assert_eq!(tx_logs.len(), 5);

        let (after_commit, _) = shared
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
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
        assert_eq!(after_commit.rows[0][8], CellValue::Bytes(vec![0xca, 0xfe, 0xba, 0xbe]));
        assert_eq!(after_commit.rows[3][0], CellValue::Int(5));
        assert_eq!(after_commit.rows[3][2], CellValue::Text("Pierce Hawthorne".to_owned()));

        // Transactional rollback on PK conflict
        let rollback_stmts = vec![
            format!("UPDATE {fq_students} SET `full_name` = 'Should Rollback' WHERE `student_id` = 1;"),
            format!("INSERT INTO {fq_students} (`student_id`, `id`, `full_name`, `major`) VALUES (1, '33333333-3333-4444-5555-666666666666', 'Evil Abed', 'Film');"),
        ];
        let rollback_err = shared.execute_transaction(&rollback_stmts).await;
        assert!(matches!(rollback_err, Err(DbError::QueryFailed { .. })));

        let (after_rollback, _) = shared
            .fetch_table_page(&PageRequest {
                schema: db_name.clone(),
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
