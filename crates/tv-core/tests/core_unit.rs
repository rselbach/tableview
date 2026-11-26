#![allow(clippy::expect_used)]

use tempfile::tempdir;
use tv_core::{
    AccentColor, ConnectionProfile, ConnectionStore, EnvTag, PasswordMode, SafetyPolicy, SortOrder, SslMode,
    StoreData, UrlParseError, ValidationError,
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
