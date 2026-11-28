use std::time::SystemTime;

/// Why an update check, download, verification, or install stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateFailure {
    /// The feed could not be fetched.
    Check,
    /// The feed failed signature, identity, or anti-replay checks.
    Untrusted,
    /// The update archive could not be downloaded.
    Download,
    /// The downloaded archive or unpacked bundle failed verification.
    Verify,
    /// Swapping the staged app into place failed; nothing changed on disk.
    Install,
    /// The new bundle is already in place, but relaunch scheduling failed.
    Relaunch { version: String },
    /// A previous restart-to-update did not launch the target build at this path.
    RelaunchRecovery { version: String, log_path: String },
    /// The latest release requires a newer macOS version.
    RequiresNewerOs { version: String, minimum_os: String },
}

/// Lifecycle state machine driving the sidebar card, collapsed badge, and menu item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    /// Updates are off (development binary outside a signed `.app` bundle).
    #[default]
    Disabled,
    /// No check has completed since launch.
    Idle,
    /// Fetching and verifying the signed release feed.
    Checking,
    /// Running build is already current.
    UpToDate,
    /// Downloading and hashing the release archive.
    Downloading { version: String, percent: u8 },
    /// A verified update is staged next to `TableView.app`, waiting for restart.
    Ready {
        version: String,
        notes_url: Option<String>,
        highlights: Vec<String>,
    },
    /// Swapping the staged bundle in and quitting to relaunch.
    Installing { version: String },
    /// A newer release exists, but this copy cannot replace itself in place.
    Available {
        version: String,
        download_url: String,
        notes_url: Option<String>,
        highlights: Vec<String>,
    },
    /// The latest update attempt failed.
    Failed(UpdateFailure),
}

/// User-facing action mapped from the current [`UpdateStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    Check,
    Restart,
    Download,
}

/// Snapshot of updater status and preferences rendered by the UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateSnapshot {
    pub status: UpdateStatus,
    pub automatic: bool,
    pub last_check: Option<SystemTime>,
}

impl UpdateStatus {
    /// Returns whether the workspace sidebar toggle should display an attention dot when collapsed.
    pub fn needs_sidebar_dot(&self) -> bool {
        matches!(
            self,
            Self::Ready { .. }
                | Self::Downloading { .. }
                | Self::Installing { .. }
                | Self::Available { .. }
                | Self::Failed(UpdateFailure::Relaunch { .. })
                | Self::Failed(UpdateFailure::RelaunchRecovery { .. })
        )
    }

    /// Maps an update failure into a single-sentence user message.
    pub fn failure_message(failure: &UpdateFailure) -> String {
        match failure {
            UpdateFailure::Check => {
                "Couldn't check for updates. Check your internet connection and try again.".to_owned()
            }
            UpdateFailure::Untrusted => {
                "Couldn't verify the update information, so TableView ignored it.".to_owned()
            }
            UpdateFailure::Download => "Couldn't download the update. Try again later.".to_owned(),
            UpdateFailure::Verify => {
                "The downloaded update failed verification, so TableView didn't install it.".to_owned()
            }
            UpdateFailure::Install => {
                "Couldn't install the update. Try again, or download TableView from GitHub Releases.".to_owned()
            }
            UpdateFailure::Relaunch { version } => format!(
                "TableView {version} is installed, but TableView couldn't restart itself. Quit and reopen it to finish."
            ),
            UpdateFailure::RelaunchRecovery { version, log_path } => format!(
                "TableView {version} didn't finish restarting. Check for updates to try again, or inspect {log_path}."
            ),
            UpdateFailure::RequiresNewerOs { version, minimum_os } => {
                format!("TableView {version} requires macOS {minimum_os} or later.")
            }
        }
    }
}
