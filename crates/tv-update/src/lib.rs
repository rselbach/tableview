#![forbid(unsafe_code)]

//! TableView's self-updater: signed JSON feed on GitHub Pages, verified resumable
//! downloads from GitHub Releases, and atomic macOS bundle swap on restart.

pub mod feed;
pub mod install;
mod prefs;
pub mod status;
pub mod transport;
mod updater;

pub use status::{UpdateAction, UpdateFailure, UpdateSnapshot, UpdateStatus};
pub use updater::{Config, Host, Installer, MacInstaller, Schedule, Updater};

/// Signed release feed served on GitHub Pages.
pub const FEED_URL: &str = "https://rselbach.github.io/TableView/update.json";

/// Release signing Ed25519 public key (base64).
pub const PUBLIC_KEY: &str = "2obyIySbPw8xSuXQwzSw2sCbOixUww08jk95se82unc=";

/// TableView's macOS bundle identifier.
pub const BUNDLE_ID: &str = "com.rselbach.tableview";

/// Application bundle directory name inside release ZIPs and `/Applications`.
pub const APP_NAME: &str = "TableView.app";

/// Code signature requirement every staged update bundle must satisfy.
pub const CODE_REQUIREMENT: &str = include_str!("../update-requirement.txt");

/// Current platform in update feed terms.
pub const OS: &str = "macos";

/// Current architecture in update feed terms.
pub const ARCH: &str = match std::env::consts::ARCH.as_bytes() {
    b"aarch64" => "arm64",
    _ => std::env::consts::ARCH,
};
