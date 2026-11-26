#![allow(clippy::expect_used)]

use chrono::Datelike;
use tempfile::tempdir;
use tv_core::engine::postgres::{quote_ident, synthesize_table_ddl};
use tv_core::{
    AccentColor, CellValue, ColumnMeta, ConnectionProfile, ConnectionStore, EnvTag, ForeignKeyRef, IndexInfo,
    PasswordMode, SafetyPolicy, SortOrder, SslMode, StoreData, UrlParseError, ValidationError,
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
