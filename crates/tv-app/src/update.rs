//! Bridges `tv_update::Updater` with Rune UI state, events, and shutdown hooks.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use directories::ProjectDirs;
use rune::Waker;
use tv_update::feed::Origins;
use tv_update::{
    Config, Host, MacInstaller, Schedule, UpdateSnapshot, UpdateStatus, Updater,
};

/// UI-facing auto-update state owned by [`crate::state::AppState`].
#[derive(Clone, Debug)]
pub struct UpdateUiState {
    pub snapshot: UpdateSnapshot,
    pub running_version: String,
    pub notes_expanded: bool,
    pub manual_check_pending: bool,
    pub dismissed_os_warning: bool,
}

impl Default for UpdateUiState {
    fn default() -> Self {
        Self {
            snapshot: UpdateSnapshot::default(),
            running_version: env!("CARGO_PKG_VERSION").to_owned(),
            notes_expanded: false,
            manual_check_pending: false,
            dismissed_os_warning: false,
        }
    }
}

/// Events emitted by [`tv_update::Host`] onto the UI thread.
#[derive(Clone, Debug)]
pub enum UpdateHostEvent {
    Changed(UpdateSnapshot),
    OpenUrl(String),
    Quit,
}

struct BridgeHost {
    event_tx: mpsc::Sender<UpdateHostEvent>,
    waker: Arc<Mutex<Option<Waker>>>,
    updater: Arc<Mutex<Option<Updater>>>,
}

impl BridgeHost {
    fn wake(&self) {
        if let Ok(guard) = self.waker.lock()
            && let Some(waker) = guard.as_ref()
        {
            waker();
        }
    }
}

impl Host for BridgeHost {
    fn changed(&self) {
        let snapshot = self
            .updater
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(Updater::snapshot));
        if let Some(snapshot) = snapshot
            && self.event_tx.send(UpdateHostEvent::Changed(snapshot)).is_ok()
        {
            self.wake();
        }
    }

    fn open_url(&self, url: &str) {
        if self
            .event_tx
            .send(UpdateHostEvent::OpenUrl(url.to_owned()))
            .is_ok()
        {
            self.wake();
        }
    }

    fn terminate(&self) {
        if self.event_tx.send(UpdateHostEvent::Quit).is_ok() {
            self.wake();
        }
    }
}

/// Owns the running [`Updater`] instance for packaged macOS builds.
pub struct UpdateBridge {
    updater: Updater,
    event_rx: mpsc::Receiver<UpdateHostEvent>,
    waker: Arc<Mutex<Option<Waker>>>,
    pub running_version: String,
}

impl UpdateBridge {
    /// Starts the updater when running inside a signed `TableView.app` bundle.
    pub fn start(runtime: &tokio::runtime::Handle) -> Option<Self> {
        let bundle = tv_update::install::detect_running_bundle()?;
        let public_key = tv_update::feed::verifying_key(tv_update::PUBLIC_KEY).ok()?;
        let state_dir = update_state_dir()?;
        let isolated = std::env::var_os("TABLEVIEW_DATA_DIR").is_some();

        let (event_tx, event_rx) = mpsc::channel();
        let waker = Arc::new(Mutex::new(None));
        let updater_slot = Arc::new(Mutex::new(None));
        let host = Arc::new(BridgeHost {
            event_tx,
            waker: Arc::clone(&waker),
            updater: Arc::clone(&updater_slot),
        });

        let installer = Arc::new(MacInstaller::new(
            bundle.path,
            bundle.build,
            bundle.os_version,
            Vec::new(),
        ));
        let config = Config {
            feed_url: tv_update::FEED_URL.to_owned(),
            origins: Origins::github(),
            public_key,
            build: bundle.build,
            os_version: bundle.os_version,
            state_dir,
            background_checks: !isolated,
            automatic_default: true,
            schedule: Schedule::default(),
            user_agent: format!("TableView/{} (macos)", bundle.version),
        };

        let updater = Updater::start(config, installer, host, runtime).ok()?;
        if let Ok(mut guard) = updater_slot.lock() {
            *guard = Some(updater.clone());
        }

        Some(Self {
            updater,
            event_rx,
            waker,
            running_version: bundle.version,
        })
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut guard) = self.waker.lock() {
            *guard = Some(waker);
        }
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        self.updater.snapshot()
    }

    pub fn drain_events(&self) -> Vec<UpdateHostEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            events.push(ev);
        }
        events
    }

    pub fn check_now(&self) {
        self.updater.check_now();
    }

    pub fn set_automatic_checks(&self, enabled: bool) {
        self.updater.set_automatic_checks(enabled);
    }

    pub fn restart_to_update(&self) {
        let updater = self.updater.clone();
        drop(
            std::thread::Builder::new()
                .name("tv-update-restart".to_owned())
                .spawn(move || {
                    let _restarted = updater.restart_to_update();
                }),
        );
    }

    pub fn install_on_quit(&self) -> bool {
        self.updater.install_on_quit()
    }

    pub fn open_download(&self) {
        self.updater.open_download();
    }

    pub fn open_notes(&self) {
        self.updater.open_notes();
    }
}

fn update_state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TABLEVIEW_DATA_DIR") {
        return Some(PathBuf::from(dir).join("updates"));
    }
    let dirs = ProjectDirs::from("dev", "tableview", "TableView")?;
    Some(dirs.data_local_dir().join("updates"))
}

/// Returns whether a status transition after a manual check settled into an actionable notice.
pub fn manual_check_notice(status: &UpdateStatus) -> Option<(String, bool)> {
    match status {
        UpdateStatus::UpToDate => Some(("TableView is up to date.".to_owned(), false)),
        UpdateStatus::Ready { version, .. } => {
            Some((format!("TableView {version} is ready to install on restart."), false))
        }
        UpdateStatus::Available { version, .. } => {
            Some((format!("TableView {version} is available to download."), false))
        }
        UpdateStatus::Failed(failure) => Some((UpdateStatus::failure_message(failure), true)),
        _ => None,
    }
}
