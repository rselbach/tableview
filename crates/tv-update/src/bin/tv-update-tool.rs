//! Release signing and verification CLI for TableView's signed update feed (`update.json`).
//!
//! ```text
//! tv-update-tool sign-feed --version V --build N [--arch A] --minimum-os X
//!     --published-at RFC3339 --notes-url URL [--notes FILE] --zip FILE --zip-url URL
//!     --dmg FILE --dmg-url URL --out FILE
//! tv-update-tool verify-feed FILE [--version V --build N] [--arch A] [--download]
//! ```

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ed25519_dalek::SigningKey;
use tv_update::feed::{self, Artifact, KIND_APP_ZIP, KIND_DMG, Origins, Release};
use tv_update::transport::{Transport, sha256_file};

const MAX_HIGHLIGHTS: usize = 5;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("sign-feed") => sign_feed(&args[1..]),
        Some("verify-feed") => verify_feed(&args[1..]),
        _ => Err("usage: tv-update-tool sign-feed|verify-feed …".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    options: HashMap<String, String>,
    flags: Vec<String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(args: &[String], allowed_options: &[&str], flags: &[&str]) -> Result<Self, String> {
        let mut parsed = Self {
            options: HashMap::new(),
            flags: Vec::new(),
            positional: Vec::new(),
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let Some(name) = arg.strip_prefix("--") else {
                parsed.positional.push(arg.clone());
                continue;
            };
            if flags.contains(&name) {
                parsed.flags.push(name.to_owned());
                continue;
            }
            if !allowed_options.contains(&name) {
                return Err(format!("unknown option --{name}"));
            }
            let value = iter.next().ok_or_else(|| format!("--{name} needs a value"))?;
            parsed.options.insert(name.to_owned(), value.clone());
        }
        Ok(parsed)
    }

    fn get(&self, name: &str) -> Result<&str, String> {
        self.options
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("missing --{name}"))
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }
}

fn pinned_key_from_reader(mut reader: impl Read) -> Result<SigningKey, String> {
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|error| format!("could not read the key from stdin: {error}"))?;
    let key = feed::signing_key(&text)?;
    let pinned = feed::verifying_key(tv_update::PUBLIC_KEY)?;
    if key.verifying_key() != pinned {
        return Err("the signing key does not match tv_update::PUBLIC_KEY".to_owned());
    }
    Ok(key)
}

fn artifact(kind: &str, arch: &str, minimum_os: &str, file: &Path, url: &str) -> Result<Artifact, String> {
    let size = std::fs::metadata(file)
        .map_err(|error| format!("could not read {}: {error}", file.display()))?
        .len();
    let sha256 = sha256_file(file).map_err(|error| format!("could not hash {}: {error}", file.display()))?;
    Ok(Artifact {
        os: tv_update::OS.to_owned(),
        arch: arch.to_owned(),
        kind: kind.to_owned(),
        minimum_os: minimum_os.to_owned(),
        url: url.to_owned(),
        size,
        sha256,
    })
}

/// Extracts up to `MAX_HIGHLIGHTS` single-line plain-text bullet points from Markdown release notes.
fn extract_highlights(markdown: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) else {
            continue;
        };
        let cleaned = rest.replace("**", "").replace('`', "");
        let item = cleaned.trim();
        if item.is_empty() || item.len() > feed::MAX_HIGHLIGHT_LEN {
            continue;
        }
        found.push(item.to_owned());
        if found.len() >= MAX_HIGHLIGHTS {
            break;
        }
    }
    found
}

fn sign_feed(args: &[String]) -> Result<(), String> {
    let args = Args::parse(
        args,
        &[
            "version",
            "build",
            "arch",
            "minimum-os",
            "published-at",
            "notes-url",
            "notes",
            "zip",
            "zip-url",
            "dmg",
            "dmg-url",
            "out",
        ],
        &[],
    )?;
    if !args.positional.is_empty() {
        return Err(format!("unexpected positional argument {}", args.positional[0]));
    }
    let build = args
        .get("build")?
        .parse()
        .map_err(|_| "--build is not a number".to_owned())?;
    let minimum_os = args.get("minimum-os")?;
    let arch = args
        .options
        .get("arch")
        .map(String::as_str)
        .unwrap_or(tv_update::ARCH);
    let highlights = match args.options.get("notes") {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("could not read notes {path}: {error}"))?;
            extract_highlights(&text)
        }
        None => Vec::new(),
    };
    let release = Release {
        schema: feed::SCHEMA,
        app: tv_update::BUNDLE_ID.to_owned(),
        channel: feed::CHANNEL.to_owned(),
        version: args.get("version")?.to_owned(),
        build,
        published_at: args.get("published-at")?.to_owned(),
        notes_url: Some(args.get("notes-url")?.to_owned()),
        highlights,
        artifacts: vec![
            artifact(
                KIND_APP_ZIP,
                arch,
                minimum_os,
                Path::new(args.get("zip")?),
                args.get("zip-url")?,
            )?,
            artifact(
                KIND_DMG,
                arch,
                minimum_os,
                Path::new(args.get("dmg")?),
                args.get("dmg-url")?,
            )?,
        ],
    };
    let key = pinned_key_from_reader(std::io::stdin())?;
    let sealed = feed::seal(&release, &key);
    let opened = open_pinned(&sealed)?;
    if opened != release {
        return Err("the sealed feed does not round-trip".to_owned());
    }
    let out = PathBuf::from(args.get("out")?);
    std::fs::write(&out, &sealed).map_err(|error| format!("could not write {}: {error}", out.display()))?;
    println!("signed {} build {} into {}", release.version, release.build, out.display());
    Ok(())
}

fn open_pinned(bytes: &[u8]) -> Result<Release, String> {
    let key = feed::verifying_key(tv_update::PUBLIC_KEY)?;
    feed::open(bytes, &key, &Origins::github()).map_err(|error| error.to_string())
}

fn verify_feed(args: &[String]) -> Result<(), String> {
    let args = Args::parse(args, &["version", "build", "arch"], &["download"])?;
    let [file] = args.positional.as_slice() else {
        return Err(
            "usage: tv-update-tool verify-feed FILE [--version V --build N] [--arch A] [--download]".to_owned(),
        );
    };
    let bytes = std::fs::read(file).map_err(|error| format!("could not read {file}: {error}"))?;
    let release = open_pinned(&bytes)?;
    if let Some(version) = args.options.get("version")
        && *version != release.version
    {
        return Err(format!("the feed is version {}, not {version}", release.version));
    }
    if let Some(build) = args.options.get("build")
        && *build != release.build.to_string()
    {
        return Err(format!("the feed is build {}, not {build}", release.build));
    }
    let arch = args
        .options
        .get("arch")
        .map(String::as_str)
        .unwrap_or(tv_update::ARCH);
    for kind in [KIND_APP_ZIP, KIND_DMG] {
        if release.artifact(tv_update::OS, arch, kind).is_none() {
            return Err(format!("the feed has no {kind} for this platform"));
        }
    }
    if args.flag("download") {
        download_all(&release)?;
    }
    println!("verified {} build {}", release.version, release.build);
    Ok(())
}

fn download_all(release: &Release) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start a runtime: {error}"))?;
    let transport = Transport::new(Origins::github(), "TableView-release-check")?;
    let temp = tempfile::tempdir().map_err(|error| format!("could not create temporary download directory: {error}"))?;
    let dir = temp.path();
    runtime.block_on(async {
        for artifact in &release.artifacts {
            let file = transport
                .download(artifact, dir, &|_| {})
                .await
                .map_err(|error| format!("{}: {error}", artifact.url))?;
            println!("downloaded and verified {}", artifact.url);
            std::fs::remove_file(&file)
                .map_err(|error| format!("could not remove {}: {error}", file.display()))?;
        }
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    #[test]
    fn highlights_extract_cleanly_from_markdown() {
        let md = "\
## What's Changed
- **Faster scrolling** on `students` tables
* Fix TLS client cert loading
- 
- Third item
- Fourth item
- Fifth item
- Sixth item should drop
";
        let got = extract_highlights(md);
        assert_eq!(
            got,
            vec![
                "Faster scrolling on students tables",
                "Fix TLS client cert loading",
                "Third item",
                "Fourth item",
                "Fifth item",
            ]
        );
    }

    #[test]
    fn wrong_key_is_rejected() {
        let stranger = STANDARD.encode([4u8; 32]);
        let err = pinned_key_from_reader(stranger.as_bytes()).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn sealed_feed_with_test_key_fails_verify_feed() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://github.com/rselbach/tableview/releases/download/v0.2.0/TableView-0.2.0-macos-arm64";
        let artifact = |kind: &str| Artifact {
            os: tv_update::OS.to_owned(),
            arch: tv_update::ARCH.to_owned(),
            kind: kind.to_owned(),
            minimum_os: "14.0".to_owned(),
            url: format!("{url}.{kind}"),
            size: 9,
            sha256: "b".repeat(64),
        };
        let release = Release {
            schema: feed::SCHEMA,
            app: tv_update::BUNDLE_ID.to_owned(),
            channel: feed::CHANNEL.to_owned(),
            version: "0.2.0".to_owned(),
            build: 42,
            published_at: "2026-11-26T12:00:00Z".to_owned(),
            notes_url: None,
            highlights: Vec::new(),
            artifacts: vec![artifact(KIND_APP_ZIP), artifact(KIND_DMG)],
        };
        let path = dir.path().join("update.json");
        std::fs::write(&path, feed::seal(&release, &SigningKey::from_bytes(&[4; 32]))).unwrap();
        let err = verify_feed(&[path.to_str().unwrap().to_owned()]).unwrap_err();
        assert!(err.contains("signature"));
    }
}
