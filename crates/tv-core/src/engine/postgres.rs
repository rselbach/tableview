use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, TrustAnchor, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};
use webpki::{EndEntityCert, KeyUsage, anchor_from_trusted_cert};
use tokio::task::JoinHandle;
use tokio_postgres::{Config, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::connection::{ConnectionProfile, PasswordMode, SslConfig, SslMode};
use crate::engine::{
    ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql, SchemaInfo, ServerInfo,
};

static CRYPTO_PROVIDER_INIT: OnceLock<()> = OnceLock::new();
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        drop(rustls::crypto::ring::default_provider().install_default());
    });
}

/// Active PostgreSQL database session backed by `tokio-postgres`.
pub struct PostgresSession {
    client: Arc<tokio_postgres::Client>,
    server_info: ServerInfo,
    disconnected: Arc<AtomicBool>,
    conn_task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for PostgresSession {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.conn_task.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}

impl PostgresSession {
    /// Connects to PostgreSQL using the supplied profile, optional runtime password, and optional database override.
    pub async fn connect(
        profile: &ConnectionProfile,
        password: Option<&str>,
        database_override: Option<&str>,
    ) -> Result<(Self, Vec<ExecutedSql>), DbError> {
        profile
            .validate()
            .map_err(|e| DbError::InvalidInput(e.to_string()))?;

        let target_db = database_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| profile.database.as_deref().map(str::trim).filter(|s| !s.is_empty()));

        let effective_db = target_db.unwrap_or("postgres");
        let (client, conn_task, disconnected) =
            match connect_raw(profile, password, effective_db).await {
                Ok(ok) => ok,
                Err(err) if target_db.is_none() && is_missing_database_error(&err) && profile.user.trim() != "postgres" => {
                    connect_raw(profile, password, profile.user.trim()).await?
                }
                Err(err) => return Err(err),
            };

        let mut session = Self {
            client: Arc::new(client),
            server_info: ServerInfo {
                version: String::new(),
                current_database: effective_db.to_owned(),
                current_user: profile.user.trim().to_owned(),
            },
            disconnected,
            conn_task: Mutex::new(Some(conn_task)),
        };

        let (info, log) = session.fetch_server_info().await?;
        session.server_info = info;
        Ok((session, vec![log]))
    }

    /// Tests connectivity and returns concise server metadata without retaining an open session.
    pub async fn test_connection(
        profile: &ConnectionProfile,
        password: Option<&str>,
    ) -> Result<ServerInfo, DbError> {
        let (session, _) = Self::connect(profile, password, None).await?;
        Ok(session.server_info().clone())
    }

    async fn fetch_server_info(&self) -> Result<(ServerInfo, ExecutedSql), DbError> {
        let sql = "SELECT version(), current_database(), current_user";
        let start = Instant::now();
        let row = self
            .client
            .query_one(sql, &[])
            .await
            .map_err(map_pg_err)?;
        let raw_version: String = row.get(0);
        let current_database: String = row.get(1);
        let current_user: String = row.get(2);
        let log = ExecutedSql {
            sql: sql.to_owned(),
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
        };
        Ok((
            ServerInfo {
                version: concise_pg_version(&raw_version),
                current_database,
                current_user,
            },
            log,
        ))
    }

    fn ensure_alive(&self) -> Result<(), DbError> {
        if self.disconnected.load(Ordering::Acquire) || self.client.is_closed() {
            return Err(DbError::Disconnected);
        }
        Ok(())
    }
}

#[async_trait]
impl DbSession for PostgresSession {
    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError> {
        self.ensure_alive()?;
        let sql = "SELECT d.datname, COALESCE(pg_catalog.pg_get_userbyid(d.datdba), ''), \
                   pg_catalog.pg_database_size(d.datname) \
                   FROM pg_catalog.pg_database d \
                   WHERE d.datistemplate = false \
                   ORDER BY d.datname ASC";
        let start = Instant::now();
        let rows = match self.client.query(sql, &[]).await {
            Ok(rows) => rows,
            Err(err) if is_insufficient_privilege(&err) => {
                let fallback_sql = "SELECT d.datname, COALESCE(pg_catalog.pg_get_userbyid(d.datdba), ''), NULL::bigint \
                                    FROM pg_catalog.pg_database d \
                                    WHERE d.datistemplate = false \
                                    ORDER BY d.datname ASC";
                let rows = self.client.query(fallback_sql, &[]).await.map_err(map_pg_err)?;
                let dbs = rows
                    .into_iter()
                    .map(|row| DatabaseInfo {
                        name: row.get(0),
                        owner: row.get(1),
                        size_bytes: row.get(2),
                    })
                    .collect();
                return Ok((
                    dbs,
                    ExecutedSql {
                        sql: fallback_sql.to_owned(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        error: None,
                    },
                ));
            }
            Err(err) => return Err(map_pg_err(err)),
        };

        let dbs = rows
            .into_iter()
            .map(|row| DatabaseInfo {
                name: row.get(0),
                owner: row.get(1),
                size_bytes: row.get(2),
            })
            .collect();
        Ok((
            dbs,
            ExecutedSql {
                sql: sql.to_owned(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_schemas(&self) -> Result<(Vec<SchemaInfo>, ExecutedSql), DbError> {
        self.ensure_alive()?;
        let sql = "SELECT nspname FROM pg_catalog.pg_namespace \
                   WHERE nspname NOT LIKE 'pg_temp_%' AND nspname NOT LIKE 'pg_toast_temp_%' \
                   ORDER BY (nspname = 'public') DESC, \
                            (nspname LIKE 'pg_%' OR nspname = 'information_schema') ASC, \
                            nspname ASC";
        let start = Instant::now();
        let rows = self.client.query(sql, &[]).await.map_err(map_pg_err)?;
        let schemas = rows
            .into_iter()
            .map(|row| {
                let name: String = row.get(0);
                let is_system = name.starts_with("pg_") || name == "information_schema";
                SchemaInfo { name, is_system }
            })
            .collect();
        Ok((
            schemas,
            ExecutedSql {
                sql: sql.to_owned(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_objects(&self, schema: &str) -> Result<(Vec<DbObject>, ExecutedSql), DbError> {
        self.ensure_alive()?;
        let sql = "SELECT c.relname, c.relkind::text, \
                   CASE WHEN c.reltuples >= 0 THEN c.reltuples::bigint ELSE NULL END, \
                   CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_total_relation_size(c.oid) ELSE NULL END \
                   FROM pg_catalog.pg_class c \
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                   WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                   ORDER BY c.relname ASC";
        let start = Instant::now();
        let rows = self.client.query(sql, &[&schema]).await.map_err(map_pg_err)?;
        let objects = rows
            .into_iter()
            .filter_map(|row| {
                let name: String = row.get(0);
                let relkind: String = row.get(1);
                let kind = match relkind.as_str() {
                    "r" | "p" => DbObjectKind::Table,
                    "v" => DbObjectKind::View,
                    "m" => DbObjectKind::MaterializedView,
                    "f" => DbObjectKind::ForeignTable,
                    _ => return None,
                };
                Some(DbObject {
                    schema: schema.to_owned(),
                    name,
                    kind,
                    estimated_rows: row.get(2),
                    total_bytes: row.get(3),
                })
            })
            .collect();
        Ok((
            objects,
            ExecutedSql {
                sql: format!("{sql} -- $1 = '{}'", schema.replace('\'', "''")),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_columns(&self, schema: &str, object: &str) -> Result<(Vec<ColumnSummary>, ExecutedSql), DbError> {
        self.ensure_alive()?;
        let sql = "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                   NOT a.attnotnull, BOOL_OR(COALESCE(i.indisprimary, false)) \
                   FROM pg_catalog.pg_attribute a \
                   JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                   LEFT JOIN pg_catalog.pg_index i ON i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey) \
                   WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                   GROUP BY a.attnum, a.attname, a.atttypid, a.atttypmod, a.attnotnull \
                   ORDER BY a.attnum ASC";
        let start = Instant::now();
        let rows = self.client.query(sql, &[&schema, &object]).await.map_err(map_pg_err)?;
        let cols = rows
            .into_iter()
            .map(|row| ColumnSummary {
                name: row.get(0),
                data_type: row.get(1),
                nullable: row.get(2),
                is_primary_key: row.get(3),
            })
            .collect();
        Ok((
            cols,
            ExecutedSql {
                sql: format!(
                    "{sql} -- $1 = '{}', $2 = '{}'",
                    schema.replace('\'', "''"),
                    object.replace('\'', "''")
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.ensure_alive()?;
        self.client.simple_query("SELECT 1").await.map_err(map_pg_err)?;
        Ok(())
    }
}

async fn connect_raw(
    profile: &ConnectionProfile,
    password: Option<&str>,
    database: &str,
) -> Result<(tokio_postgres::Client, JoinHandle<()>, Arc<AtomicBool>), DbError> {
    let mut cfg = Config::new();
    let host = profile.host.trim();
    cfg.host(host);
    if !host.starts_with('/') {
        cfg.port(profile.port);
    }
    cfg.user(profile.user.trim());
    if profile.password_mode != PasswordMode::None
        && let Some(pw) = password
        && !pw.is_empty()
    {
        cfg.password(pw);
    }
    cfg.dbname(database);
    cfg.connect_timeout(CONNECT_TIMEOUT);
    cfg.application_name("TableView");

    let use_tls = match profile.ssl.mode {
        SslMode::Disable => false,
        SslMode::Prefer => !host.starts_with('/'),
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => true,
    };

    if !use_tls {
        return connect_no_tls(&cfg).await;
    }

    let tls_cfg = build_rustls_config(&profile.ssl)?;
    let connector = MakeRustlsConnect::new(tls_cfg);
    match tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(connector)).await {
        Ok(Ok((client, connection))) => {
            let (handle, disconnected) = spawn_connection_driver(connection);
            Ok((client, handle, disconnected))
        }
        Ok(Err(err)) => {
            if profile.ssl.mode == SslMode::Prefer && err.as_db_error().is_none() {
                connect_no_tls(&cfg).await
            } else {
                Err(map_pg_err(err))
            }
        }
        Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
    }
}

async fn connect_no_tls(
    cfg: &Config,
) -> Result<(tokio_postgres::Client, JoinHandle<()>, Arc<AtomicBool>), DbError> {
    match tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(NoTls)).await {
        Ok(Ok((client, connection))) => {
            let (handle, disconnected) = spawn_connection_driver(connection);
            Ok((client, handle, disconnected))
        }
        Ok(Err(err)) => Err(map_pg_err(err)),
        Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
    }
}

fn spawn_connection_driver<S, T>(connection: tokio_postgres::Connection<S, T>) -> (JoinHandle<()>, Arc<AtomicBool>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let disconnected = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&disconnected);
    let handle = tokio::spawn(async move {
        drop(connection.await);
        flag.store(true, Ordering::Release);
    });
    (handle, disconnected)
}

fn build_rustls_config(ssl: &SslConfig) -> Result<ClientConfig, DbError> {
    ensure_crypto_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let builder = match ssl.mode {
        SslMode::Disable => return Err(DbError::TlsConfig("TLS disabled".to_owned())),
        SslMode::Prefer | SslMode::Require => ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| DbError::TlsConfig(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifyServerCert { provider })),
        SslMode::VerifyCa => {
            let (roots, anchors) = load_roots_and_anchors(ssl.ca_cert_path.as_deref())?;
            let _ = roots;
            ClientConfig::builder_with_provider(Arc::clone(&provider))
                .with_safe_default_protocol_versions()
                .map_err(|e| DbError::TlsConfig(e.to_string()))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(CaOnlyServerCertVerifier { provider, anchors }))
        }
        SslMode::VerifyFull => {
            let (roots, _) = load_roots_and_anchors(ssl.ca_cert_path.as_deref())?;
            ClientConfig::builder_with_provider(Arc::clone(&provider))
                .with_safe_default_protocol_versions()
                .map_err(|e| DbError::TlsConfig(e.to_string()))?
                .with_root_certificates(roots)
        }
    };

    match (&ssl.client_cert_path, &ssl.client_key_path) {
        (Some(cert_path), Some(key_path)) => {
            let certs = load_certs(cert_path)?;
            let key = load_private_key(key_path)?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| DbError::TlsConfig(e.to_string()))
        }
        _ => Ok(builder.with_no_client_auth()),
    }
}

fn load_roots_and_anchors(ca_path: Option<&Path>) -> Result<(RootCertStore, Vec<TrustAnchor<'static>>), DbError> {
    let mut store = RootCertStore::empty();
    let mut anchors = Vec::new();
    if let Some(path) = ca_path {
        let certs = load_certs(path)?;
        if certs.is_empty() {
            return Err(DbError::TlsConfig(format!(
                "no certificates found in CA file {}",
                path.display()
            )));
        }
        for cert in certs {
            let anchor = anchor_from_trusted_cert(&cert)
                .map_err(|e| DbError::TlsConfig(format!("invalid CA cert {}: {e}", path.display())))?
                .to_owned();
            anchors.push(anchor);
            store
                .add(cert)
                .map_err(|e| DbError::TlsConfig(format!("invalid CA cert {}: {e}", path.display())))?;
        }
        return Ok((store, anchors));
    }

    for anchor in webpki_roots::TLS_SERVER_ROOTS {
        anchors.push(anchor.to_owned());
    }
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        if let Ok(anchor) = anchor_from_trusted_cert(&cert) {
            anchors.push(anchor.to_owned());
        }
        drop(store.add(cert));
    }

    Ok((store, anchors))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, DbError> {
    let file = File::open(path).map_err(|e| DbError::TlsConfig(format!("open {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::TlsConfig(format!("parse PEM certs {}: {e}", path.display())))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, DbError> {
    let file = File::open(path).map_err(|e| DbError::TlsConfig(format!("open {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| DbError::TlsConfig(format!("parse private key {}: {e}", path.display())))?
        .ok_or_else(|| DbError::TlsConfig(format!("no private key found in {}", path.display())))
}

#[derive(Debug)]
struct NoVerifyServerCert {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoVerifyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct CaOnlyServerCertVerifier {
    provider: Arc<CryptoProvider>,
    anchors: Vec<TrustAnchor<'static>>,
}

impl ServerCertVerifier for CaOnlyServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let cert = EndEntityCert::try_from(end_entity)
            .map_err(|_| TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        cert.verify_for_usage(
            self.provider.signature_verification_algorithms.all,
            &self.anchors,
            intermediates,
            now,
            KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|e| TlsError::InvalidCertificate(rustls::CertificateError::Other(rustls::OtherError(Arc::new(e)))))?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn is_missing_database_error(err: &DbError) -> bool {
    matches!(err, DbError::QueryFailed { code: Some(code), .. } if code == "3D000")
}

fn is_insufficient_privilege(err: &tokio_postgres::Error) -> bool {
    err.as_db_error()
        .is_some_and(|db| db.code().code() == "42501")
}

fn map_pg_err(err: tokio_postgres::Error) -> DbError {
    if let Some(db) = err.as_db_error() {
        let code = db.code().code().to_owned();
        if code == "28P01" || code == "28000" {
            return DbError::AuthFailed(db.message().to_owned());
        }
        return DbError::QueryFailed {
            message: db.message().to_owned(),
            code: Some(code),
            detail: db.detail().map(str::to_owned),
            hint: db.hint().map(str::to_owned),
        };
    }
    if err.is_closed() {
        return DbError::Disconnected;
    }
    DbError::ConnectionFailed(err.to_string())
}

/// Extracts a concise version string such as `"PostgreSQL 18.4"` from `SELECT version()`.
pub fn concise_pg_version(raw: &str) -> String {
    let mut parts = raw.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(name), Some(ver)) if name.eq_ignore_ascii_case("postgresql") => {
            format!("PostgreSQL {ver}")
        }
        _ => raw.split(',').next().unwrap_or(raw).trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::concise_pg_version;

    #[test]
    fn parses_concise_postgresql_version() {
        assert_eq!(
            concise_pg_version("PostgreSQL 18.4 (Postgres.app) on aarch64-apple-darwin23.6.0, compiled by Apple clang"),
            "PostgreSQL 18.4"
        );
        assert_eq!(concise_pg_version("PostgreSQL 14.8 on x86_64-pc-linux-gnu"), "PostgreSQL 14.8");
    }
}
