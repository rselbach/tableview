//! Signed update feed envelope, identity verification, and update decision logic.

use std::cmp::Ordering;
use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use reqwest::Url;
use serde::{Deserialize, Serialize};

/// Prefix of every signed feed message for domain separation.
pub const SIGNING_CONTEXT: &[u8] = b"tableview-update-feed-v1\n";

/// Supported feed schema version.
pub const SCHEMA: u32 = 1;

/// Supported release channel.
pub const CHANNEL: &str = "stable";

/// Archive kind installed by the in-app updater (ZIP holding `TableView.app`).
pub const KIND_APP_ZIP: &str = "app-zip";

/// Disk image kind offered for manual downloads.
pub const KIND_DMG: &str = "dmg";

/// Feeds larger than this are refused unread.
pub const MAX_FEED_BYTES: usize = 64 * 1024;

/// Archives larger than this are refused.
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Maximum number of one-line release highlights accepted in a feed.
pub const MAX_HIGHLIGHTS: usize = 8;

/// Maximum byte length of a single release highlight line.
pub const MAX_HIGHLIGHT_LEN: usize = 200;

/// Signed JSON envelope published on GitHub Pages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub payload: String,
    pub signature: String,
}

/// Verified release payload describing a published TableView build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub schema: u32,
    pub app: String,
    pub channel: String,
    pub version: String,
    pub build: u64,
    pub published_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<String>,
    pub artifacts: Vec<Artifact>,
}

/// One downloadable artifact inside a [`Release`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub os: String,
    pub arch: String,
    pub kind: String,
    pub minimum_os: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

impl Release {
    /// Returns the artifact matching `os`, `arch`, and `kind`.
    pub fn artifact(&self, os: &str, arch: &str, kind: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.os == os && artifact.arch == arch && artifact.kind == kind)
    }
}

/// Allowed origins for release artifacts and HTTP redirects.
#[derive(Debug, Clone)]
pub struct Origins {
    pub artifact_prefix: String,
    pub redirect_hosts: Vec<String>,
    pub https_only: bool,
}

impl Origins {
    /// TableView's GitHub Releases and Pages hosts.
    pub fn github() -> Self {
        Self {
            artifact_prefix: "https://github.com/rselbach/tableview/releases/download/".to_owned(),
            redirect_hosts: [
                "github.com",
                "objects.githubusercontent.com",
                "release-assets.githubusercontent.com",
                "rselbach.github.io",
            ]
            .map(str::to_owned)
            .to_vec(),
            https_only: true,
        }
    }

    /// Reports whether `url` is valid as a direct release artifact URL.
    pub fn allows_artifact(&self, url: &str) -> bool {
        let Ok(parsed) = Url::parse(url) else {
            return false;
        };
        self.allows_scheme(&parsed)
            && parsed.as_str() == url
            && url.starts_with(&self.artifact_prefix)
            && url.len() > self.artifact_prefix.len()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && !url[self.artifact_prefix.len()..]
                .split('/')
                .any(|seg| seg.is_empty() || seg == ".." || seg.eq_ignore_ascii_case("%2e%2e"))
    }

    /// Reports whether `url` may be followed as an HTTP redirect target.
    pub fn allows_redirect(&self, url: &Url) -> bool {
        self.allows_scheme(url)
            && (!self.https_only || url.port().is_none())
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .host_str()
                .is_some_and(|host| self.redirect_hosts.iter().any(|allowed| allowed == host))
    }

    fn allows_scheme(&self, url: &Url) -> bool {
        match url.scheme() {
            "https" => true,
            "http" => !self.https_only,
            _ => false,
        }
    }
}

/// Why a feed failed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    Malformed(String),
    BadSignature,
    WrongIdentity(String),
}

impl fmt::Display for FeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed update feed: {why}"),
            Self::BadSignature => f.write_str("the update feed's signature does not verify"),
            Self::WrongIdentity(why) => write!(f, "the update feed is not TableView's: {why}"),
        }
    }
}

impl std::error::Error for FeedError {}

/// Parses a base64 Ed25519 verifying public key.
pub fn verifying_key(base64: &str) -> Result<VerifyingKey, String> {
    let bytes = STANDARD
        .decode(base64.trim())
        .map_err(|error| format!("public key is not base64: {error}"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "public key is not 32 bytes".to_owned())?;
    VerifyingKey::from_bytes(&bytes).map_err(|error| format!("invalid public key: {error}"))
}

/// Parses a base64 32-byte Ed25519 seed (`SPARKLE_EDDSA_PRIVATE_KEY` format).
pub fn signing_key(base64: &str) -> Result<SigningKey, String> {
    let bytes = STANDARD
        .decode(base64.trim())
        .map_err(|_| "the signing key is not base64".to_owned())?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "the signing key is not a 32-byte Ed25519 seed".to_owned())?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Signs `release` with `key` and encodes the JSON envelope.
pub fn seal(release: &Release, key: &SigningKey) -> Vec<u8> {
    let payload = serde_json::to_vec(release).unwrap_or_default();
    let signature = key.sign(&signed_message(&payload));
    let envelope = Envelope {
        payload: STANDARD.encode(&payload),
        signature: STANDARD.encode(signature.to_bytes()),
    };
    let mut bytes = serde_json::to_vec_pretty(&envelope).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// Verifies `feed` against `key`, TableView's identity, and `origins` before parsing payload fields.
pub fn open(feed: &[u8], key: &VerifyingKey, origins: &Origins) -> Result<Release, FeedError> {
    let malformed = |why: &str| FeedError::Malformed(why.to_owned());
    if feed.len() > MAX_FEED_BYTES {
        return Err(malformed("too large"));
    }
    let envelope: Envelope = serde_json::from_slice(feed).map_err(|_| malformed("not an envelope"))?;
    let payload = STANDARD
        .decode(envelope.payload.as_bytes())
        .map_err(|_| malformed("payload is not base64"))?;
    let signature = STANDARD
        .decode(envelope.signature.as_bytes())
        .ok()
        .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
        .ok_or_else(|| malformed("signature is not 64 base64 bytes"))?;
    key.verify_strict(&signed_message(&payload), &Signature::from_bytes(&signature))
        .map_err(|_| FeedError::BadSignature)?;
    let release: Release = serde_json::from_slice(&payload).map_err(|error| malformed(&error.to_string()))?;
    check(&release, origins)?;
    Ok(release)
}

fn signed_message(payload: &[u8]) -> Vec<u8> {
    [SIGNING_CONTEXT, payload].concat()
}

fn check(release: &Release, origins: &Origins) -> Result<(), FeedError> {
    let identity = |why: String| Err(FeedError::WrongIdentity(why));
    let malformed = |why: String| Err(FeedError::Malformed(why));
    if release.schema != SCHEMA {
        return identity(format!("schema {}", release.schema));
    }
    if release.app != crate::BUNDLE_ID {
        return identity(format!("app {}", release.app));
    }
    if release.channel != CHANNEL {
        return identity(format!("channel {}", release.channel));
    }
    if !is_version(&release.version) {
        return malformed(format!("version {:?}", release.version));
    }
    if release.build == 0 {
        return malformed("build 0".to_owned());
    }
    if let Some(notes) = &release.notes_url
        && !valid_notes_url(notes)
    {
        return malformed(format!("notes URL {notes}"));
    }
    if release.highlights.len() > MAX_HIGHLIGHTS {
        return malformed(format!("{} highlights", release.highlights.len()));
    }
    for highlight in &release.highlights {
        if highlight.is_empty()
            || highlight.len() > MAX_HIGHLIGHT_LEN
            || highlight.contains(['\r', '\n'])
        {
            return malformed(format!("highlight {highlight:?}"));
        }
    }
    for artifact in &release.artifacts {
        if !is_sha256(&artifact.sha256) {
            return malformed(format!("SHA-256 of {}", artifact.url));
        }
        if artifact.size == 0 || artifact.size > MAX_ARTIFACT_BYTES {
            return malformed(format!("size of {}", artifact.url));
        }
        if OsVersion::parse(&artifact.minimum_os).is_none() {
            return malformed(format!("minimum OS {:?}", artifact.minimum_os));
        }
        if !origins.allows_artifact(&artifact.url) {
            return malformed(format!("artifact URL {}", artifact.url));
        }
    }
    Ok(())
}

fn valid_notes_url(notes: &str) -> bool {
    let Ok(parsed) = Url::parse(notes) else {
        return false;
    };
    let rest = parsed.path().strip_prefix("/rselbach/tableview/");
    parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.host_str() == Some("github.com")
        && rest.is_some_and(|tail| !tail.is_empty())
        && !notes
            .split(['/', '?', '#'])
            .any(|seg| seg == ".." || seg.eq_ignore_ascii_case("%2e%2e"))
}

fn is_version(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text.as_bytes()[0].is_ascii_digit()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

pub(crate) fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Dotted OS version such as `"14.0"` or `"15.3.1"`; missing components default to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsVersion([u32; 3]);

impl OsVersion {
    /// Parses `"14"`, `"14.0"`, or `"14.0.1"`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = [0; 3];
        for (index, part) in text.trim().split('.').enumerate() {
            let slot = parts.get_mut(index)?;
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            *slot = part.parse().ok()?;
        }
        Some(Self(parts))
    }
}

impl PartialOrd for OsVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OsVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

/// Description of the running TableView installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub build: u64,
    pub os_version: OsVersion,
    pub replaceable: bool,
}

/// Action decided for a verified [`Release`] against the running [`Install`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    UpToDate,
    RequiresNewerOs { minimum_os: String },
    Install(Artifact),
    DownloadOnly(Artifact),
}

/// Evaluates whether `release` is newer than `install` and compatible with the host OS.
pub fn decide(release: &Release, install: &Install) -> Decision {
    if release.build <= install.build {
        return Decision::UpToDate;
    }
    let zip = release.artifact(crate::OS, crate::ARCH, KIND_APP_ZIP);
    let dmg = release.artifact(crate::OS, crate::ARCH, KIND_DMG);
    let chosen = match install.replaceable {
        true => zip,
        false => dmg.or(zip),
    };
    let Some(artifact) = chosen else {
        return Decision::UpToDate;
    };
    let minimum = OsVersion::parse(&artifact.minimum_os).unwrap_or(OsVersion([u32::MAX; 3]));
    if minimum > install.os_version {
        return Decision::RequiresNewerOs {
            minimum_os: artifact.minimum_os.clone(),
        };
    }
    match install.replaceable {
        true => Decision::Install(artifact.clone()),
        false => Decision::DownloadOnly(artifact.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn artifact(kind: &str, minimum_os: &str) -> Artifact {
        Artifact {
            os: "macos".to_owned(),
            arch: crate::ARCH.to_owned(),
            kind: kind.to_owned(),
            minimum_os: minimum_os.to_owned(),
            url: format!(
                "https://github.com/rselbach/tableview/releases/download/v0.2.0/TableView-0.2.0-macos-arm64.{kind}"
            ),
            size: 1024,
            sha256: SHA.to_owned(),
        }
    }

    fn release() -> Release {
        Release {
            schema: SCHEMA,
            app: crate::BUNDLE_ID.to_owned(),
            channel: CHANNEL.to_owned(),
            version: "0.2.0".to_owned(),
            build: 42,
            published_at: "2026-11-26T12:00:00Z".to_owned(),
            notes_url: Some("https://github.com/rselbach/tableview/releases/tag/v0.2.0".to_owned()),
            highlights: vec!["Faster virtualized grid scrolling for Greendale tables".to_owned()],
            artifacts: vec![artifact(KIND_APP_ZIP, "14.0"), artifact(KIND_DMG, "14.0")],
        }
    }

    fn open_with(feed: &[u8], seed: u8) -> Result<Release, FeedError> {
        open(feed, &key(seed).verifying_key(), &Origins::github())
    }

    fn reseal_payload(feed: &[u8], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut envelope: Envelope = serde_json::from_slice(feed).unwrap();
        let mut payload = STANDARD.decode(&envelope.payload).unwrap();
        edit(&mut payload);
        envelope.payload = STANDARD.encode(payload);
        serde_json::to_vec(&envelope).unwrap()
    }

    #[test]
    fn a_sealed_release_opens_with_the_matching_key_only() {
        let feed = seal(&release(), &key(1));
        assert_eq!(open_with(&feed, 1).unwrap(), release());
        assert_eq!(open_with(&feed, 2), Err(FeedError::BadSignature));
    }

    #[test]
    fn tampering_with_the_payload_breaks_the_signature() {
        let feed = seal(&release(), &key(1));
        let tampered = reseal_payload(&feed, |payload| {
            let text = String::from_utf8(payload.clone()).unwrap();
            *payload = text.replace("\"build\":42", "\"build\":43").into_bytes();
        });
        assert_eq!(open_with(&tampered, 1), Err(FeedError::BadSignature));
    }

    #[test]
    fn a_raw_signature_without_context_never_passes() {
        let payload = serde_json::to_vec(&release()).unwrap();
        let raw = key(1).sign(&payload);
        let envelope = Envelope {
            payload: STANDARD.encode(&payload),
            signature: STANDARD.encode(raw.to_bytes()),
        };
        let feed = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(open_with(&feed, 1), Err(FeedError::BadSignature));
    }

    #[test]
    fn identity_and_fields_are_checked_after_the_signature() {
        type Edit = fn(&mut Release);
        let cases: HashMap<&str, (Edit, bool)> = HashMap::from([
            (
                "another app",
                ((|r: &mut Release| r.app = "edu.greendale.tableview".to_owned()) as Edit, true),
            ),
            ("another channel", (|r| r.channel = "beta".to_owned(), true)),
            ("a future schema", (|r| r.schema = 2, true)),
            ("an unsafe version", (|r| r.version = "../0.2.0".to_owned(), false)),
            ("a bad hash", (|r| r.artifacts[0].sha256 = "AB".repeat(32), false)),
            ("an empty artifact", (|r| r.artifacts[0].size = 0, false)),
            ("a bad minimum OS", (|r| r.artifacts[0].minimum_os = "14.x".to_owned(), false)),
            (
                "a download elsewhere",
                (
                    |r| {
                        r.artifacts[0].url =
                            "https://greendale.edu/rselbach/tableview/releases/download/x.zip".to_owned()
                    },
                    false,
                ),
            ),
            (
                "notes elsewhere",
                (|r| r.notes_url = Some("https://greendale.edu/notes".to_owned()), false),
            ),
            ("empty highlight", (|r| r.highlights = vec![String::new()], false)),
            (
                "multiline highlight",
                (|r| r.highlights = vec!["Troy\nBarnes".to_owned()], false),
            ),
            (
                "too many highlights",
                (|r| r.highlights = vec!["Item".to_owned(); MAX_HIGHLIGHTS + 1], false),
            ),
        ]);
        for (name, (edit, identity)) in cases {
            let mut tc = release();
            edit(&mut tc);
            let got = open_with(&seal(&tc, &key(1)), 1).unwrap_err();
            let want = match identity {
                true => matches!(got, FeedError::WrongIdentity(_)),
                false => matches!(got, FeedError::Malformed(_)),
            };
            assert!(want, "{name}: {got:?}");
        }
    }

    #[test]
    fn malformed_envelopes_are_refused() {
        let cases = HashMap::from([
            ("not json", b"Troy and Abed in the morning".to_vec()),
            ("empty object", b"{}".to_vec()),
            ("short signature", br#"{"payload":"e30=","signature":"AAAA"}"#.to_vec()),
            ("too large", vec![b' '; MAX_FEED_BYTES + 1]),
        ]);
        for (name, feed) in cases {
            assert!(matches!(open_with(&feed, 1), Err(FeedError::Malformed(_))), "{name}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let feed = seal(&release(), &key(1));
        let extended = reseal_payload(&feed, |payload| {
            let mut value: serde_json::Value = serde_json::from_slice(payload).unwrap();
            value["rollout"] = serde_json::json!({"percent": 50});
            *payload = serde_json::to_vec(&value).unwrap();
        });
        let envelope: Envelope = serde_json::from_slice(&extended).unwrap();
        let payload = STANDARD.decode(&envelope.payload).unwrap();
        let signature = key(1).sign(&signed_message(&payload));
        let signed = serde_json::to_vec(&Envelope {
            payload: envelope.payload,
            signature: STANDARD.encode(signature.to_bytes()),
        })
        .unwrap();
        assert_eq!(open_with(&signed, 1).unwrap(), release());
    }

    #[test]
    fn artifact_and_notes_urls_stay_on_tableview_github() {
        let origins = Origins::github();
        assert!(origins.allows_artifact(
            "https://github.com/rselbach/tableview/releases/download/v0.2.0/TableView-0.2.0.zip"
        ));
        for bad in [
            "http://github.com/rselbach/tableview/releases/download/v0.2.0/x.zip",
            "https://github.com/rselbach/tableview/releases/download/",
            "https://github.com/rselbach/tableview/releases/download/../../evil/x.zip",
            "https://github.com/rselbach/tableview/releases/download/v2/%2e%2e/x.zip",
            "https://github.com/rselbach/tableview/releases/download/v2/x.zip?token=1",
            "https://github.com/rselbach/tableview-evil/releases/download/v2/x.zip",
        ] {
            assert!(!origins.allows_artifact(bad), "{bad}");
        }
        assert!(valid_notes_url(
            "https://github.com/rselbach/tableview/releases/tag/v0.2.0"
        ));
        assert!(!valid_notes_url(
            "https://github.com/rselbach/tableview/releases/tag/v0.2.0?foo=bar"
        ));
        assert!(!valid_notes_url("https://greendale.edu/notes"));
    }

    #[test]
    fn decisions_respect_build_replaceability_and_minimum_os() {
        let install = |build, os: &str, replaceable| Install {
            build,
            os_version: OsVersion::parse(os).unwrap(),
            replaceable,
        };
        let mut newer_os = release();
        newer_os.artifacts[0].minimum_os = "15.1".to_owned();
        let mut dmg_only = release();
        dmg_only.artifacts.retain(|a| a.kind == KIND_DMG);

        assert_eq!(decide(&release(), &install(42, "14.0", true)), Decision::UpToDate);
        assert_eq!(decide(&release(), &install(43, "14.0", true)), Decision::UpToDate);
        assert_eq!(
            decide(&release(), &install(41, "14.0", true)),
            Decision::Install(artifact(KIND_APP_ZIP, "14.0"))
        );
        assert_eq!(
            decide(&release(), &install(41, "14.0", false)),
            Decision::DownloadOnly(artifact(KIND_DMG, "14.0"))
        );
        assert_eq!(
            decide(&newer_os, &install(41, "15.0", true)),
            Decision::RequiresNewerOs {
                minimum_os: "15.1".to_owned()
            }
        );
        assert_eq!(decide(&dmg_only, &install(41, "14.0", true)), Decision::UpToDate);
    }

    #[test]
    fn the_pinned_key_parses() {
        verifying_key(crate::PUBLIC_KEY).unwrap();
    }
}
