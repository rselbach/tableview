#![allow(clippy::expect_used)]

use std::env;

use tokio::runtime::Runtime;
use tv_core::engine::postgres::PostgresSession;
use tv_core::{ConnectionProfile, DbObjectKind, DbSession, PasswordMode, SslMode};

fn local_pg_user() -> String {
    env::var("PGUSER")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "postgres".to_owned())
}

struct TempDatabaseGuard {
    admin_profile: ConnectionProfile,
    db_name: String,
    rt: Runtime,
}

impl Drop for TempDatabaseGuard {
    fn drop(&mut self) {
        let admin_profile = self.admin_profile.clone();
        let db_name = self.db_name.clone();
        self.rt.block_on(async move {
            let mut cfg = tokio_postgres::Config::new();
            cfg.host(&admin_profile.host)
                .port(admin_profile.port)
                .user(&admin_profile.user)
                .dbname("postgres");
            if let Ok((client, connection)) = cfg.connect(tokio_postgres::NoTls).await {
                tokio::spawn(async move {
                    drop(connection.await);
                });
                let terminate = format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}' AND pid <> pg_backend_pid()",
                    db_name
                );
                drop(client.batch_execute(&terminate).await);
                let drop_sql = format!("DROP DATABASE IF EXISTS \"{db_name}\"");
                drop(client.batch_execute(&drop_sql).await);
            }
        });
    }
}

#[test]
fn live_postgres_introspection_roundtrip() {
    let rt = Runtime::new().expect("create tokio runtime");
    let user = local_pg_user();
    let mut admin = ConnectionProfile::new_postgres("greendale_local", "127.0.0.1", &user);
    admin.port = 5432;
    admin.password_mode = PasswordMode::None;
    admin.ssl.mode = SslMode::Disable;
    admin.database = Some("postgres".to_owned());

    let db_name = format!("tableview_m1_test_{}", std::process::id());

    rt.block_on(async {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host("127.0.0.1")
            .port(5432)
            .user(&user)
            .dbname("postgres");
        let (client, connection) = cfg
            .connect(tokio_postgres::NoTls)
            .await
            .expect("connect to local postgres on 127.0.0.1:5432");
        tokio::spawn(async move {
            drop(connection.await);
        });

        client
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .await
            .expect("cleanup stale test db");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .await
            .expect("create temporary test db");
    });

    let _guard = TempDatabaseGuard {
        admin_profile: admin.clone(),
        db_name: db_name.clone(),
        rt: Runtime::new().expect("cleanup runtime"),
    };

    rt.block_on(async {
        let mut target_cfg = tokio_postgres::Config::new();
        target_cfg
            .host("127.0.0.1")
            .port(5432)
            .user(&user)
            .dbname(&db_name);
        let (client, connection) = target_cfg
            .connect(tokio_postgres::NoTls)
            .await
            .expect("connect to temp db");
        tokio::spawn(async move {
            drop(connection.await);
        });
        client
            .batch_execute(
                "CREATE SCHEMA study_room;
                 CREATE TABLE public.students (
                     student_id INTEGER PRIMARY KEY,
                     full_name TEXT NOT NULL,
                     email TEXT
                 );
                 CREATE TABLE public.study_groups (
                     group_id SERIAL PRIMARY KEY,
                     room_name TEXT NOT NULL
                 );
                 CREATE VIEW public.active_students_view AS
                     SELECT student_id, full_name FROM public.students;",
            )
            .await
            .expect("seed Community test schema");
        drop(client);

        let tested = PostgresSession::test_connection(&admin, None)
            .await
            .expect("test_connection succeeds");
        assert!(tested.version.starts_with("PostgreSQL "), "got {}", tested.version);

        let (session, init_logs) = PostgresSession::connect(&admin, None, Some(&db_name))
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
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "student_id");
        assert!(cols[0].is_primary_key);
        assert!(!cols[0].nullable);
        assert_eq!(cols[2].name, "email");
        assert!(cols[2].nullable);
        assert!(!cols[2].is_primary_key);

        session.ping().await.expect("ping succeeds");
    });
}
