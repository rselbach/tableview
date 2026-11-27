//! Serialized update worker managing feed checks, verified staging, restart-to-update, and install-on-quit.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};
use std::time::{Duration, SystemTime};

use ed25519_dalek::VerifyingKey;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::feed::{self, Artifact, Decision, Install, Origins, OsVersion, Release};
use crate::install::{self, ApplyError, Expected, InstallError, Staged};
use crate::prefs::Prefs;
use crate::status::{UpdateFailure, UpdateSnapshot, UpdateStatus};
use crate::transport::{TransferError, Transport};

/// Schedule controlling background checks and restart feed refresh timeouts.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    pub launch_delay: Duration,
    pub interval: Duration,
    pub restart_refresh: Duration,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            launch_delay: Duration::from_secs(10),
            interval: Duration::from_secs(60 * 60),
            restart_refresh: Duration::from_secs(5),
        }
    }
}

/// Configuration for starting [`Updater`].
#[derive(Debug, Clone)]
pub struct Config {
    pub feed_url: String,
    pub origins: Origins,
    pub public_key: VerifyingKey,
    pub build: u64,
    pub os_version: OsVersion,
    pub state_dir: PathBuf,
    pub background_checks: bool,
    pub automatic_default: bool,
    pub schedule: Schedule,
    pub user_agent: String,
}

/// UI host callbacks invoked by the updater worker.
pub trait Host: Send + Sync {
    fn changed(&self);
    fn open_url(&self, url: &str);
    fn terminate(&self);
}

/// Bundle staging and replacement abstraction (implemented by [`MacInstaller`]).
pub trait Installer: Send + Sync {
    fn replaceable(&self) -> bool;
    fn remove_leftovers(&self);
    fn stage(&self, zip: &Path, release: &Release) -> Result<Staged, InstallError>;
    fn apply(&self, staged: &Staged, relaunch: bool) -> Result<(), ApplyError>;
    fn discard(&self, staged: &Staged);
}

/// Production macOS installer targeting the currently running `TableView.app`.
pub struct MacInstaller {
    bundle: PathBuf,
    build: u64,
    os_version: OsVersion,
    open_args: Vec<OsString>,
}

impl MacInstaller {
    pub fn new(bundle: PathBuf, build: u64, os_version: OsVersion, open_args: Vec<OsString>) -> Self {
        Self {
            bundle,
            build,
            os_version,
            open_args,
        }
    }
}

impl Installer for MacInstaller {
    fn replaceable(&self) -> bool {
        install::replaceable(&self.bundle)
    }

    fn remove_leftovers(&self) {
        install::remove_leftovers(&self.bundle);
    }

    fn stage(&self, zip: &Path, release: &Release) -> Result<Staged, InstallError> {
        let expected = Expected::for_release(release, self.os_version, crate::CODE_REQUIREMENT);
        install::stage(zip, &self.bundle, &expected)
    }

    fn apply(&self, staged: &Staged, relaunch: bool) -> Result<(), ApplyError> {
        install::apply(staged, self.build, std::process::id(), &self.open_args, relaunch)
    }

    fn discard(&self, staged: &Staged) {
        install::discard(staged);
    }
}

enum Command {
    Check,
    Restart(std_mpsc::Sender<bool>),
}

struct StagedEntry {
    staged: Staged,
    notes_url: Option<String>,
    highlights: Vec<String>,
}

impl StagedEntry {
    fn ready_status(&self) -> UpdateStatus {
        UpdateStatus::Ready {
            version: self.staged.version().to_owned(),
            notes_url: self.notes_url.clone(),
            highlights: self.highlights.clone(),
        }
    }
}

struct State {
    status: UpdateStatus,
    prefs: Prefs,
    staged: Option<StagedEntry>,
    download_url: Option<String>,
    notes_url: Option<String>,
    stranded: bool,
    failed_build: Option<u64>,
}

struct Shared {
    state: Mutex<State>,
    prefs_write: Mutex<()>,
    prefs_file: PathBuf,
    host: Arc<dyn Host>,
    finished: AtomicUsize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_status(&self, status: UpdateStatus) {
        {
            let mut state = self.lock();
            if state.status == status {
                return;
            }
            state.status = status;
        }
        self.host.changed();
    }

    fn update_prefs<E>(&self, change: impl FnOnce(&mut Prefs) -> Result<(), E>) -> Result<(), E> {
        let _write = self.prefs_write.lock().unwrap_or_else(PoisonError::into_inner);
        let in_memory = self.lock().prefs.clone();
        let updated = Prefs::update_locked(&self.prefs_file, &in_memory, change)?;
        self.lock().prefs = updated;
        Ok(())
    }
}

/// Handle to the running background updater worker. Cheap to clone.
#[derive(Clone)]
pub struct Updater {
    shared: Arc<Shared>,
    installer: Arc<dyn Installer>,
    commands: mpsc::UnboundedSender<Command>,
}

impl Updater {
    /// Spawns the serialized updater task on `runtime`.
    pub fn start(
        config: Config,
        installer: Arc<dyn Installer>,
        host: Arc<dyn Host>,
        runtime: &Handle,
    ) -> Result<Self, String> {
        let transport = Transport::new(config.origins.clone(), &config.user_agent)?;
        let prefs_file = config.state_dir.join("updater.json");
        let mut prefs = Prefs::load(&prefs_file, config.automatic_default);
        prefs.highest_build_seen = prefs.highest_build_seen.max(config.build);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                status: UpdateStatus::Idle,
                prefs,
                staged: None,
                download_url: None,
                notes_url: None,
                stranded: false,
                failed_build: None,
            }),
            prefs_write: Mutex::new(()),
            prefs_file,
            host,
            finished: AtomicUsize::new(0),
        });
        let (commands, receiver) = mpsc::unbounded_channel();
        let worker = Worker {
            shared: shared.clone(),
            config,
            transport,
            installer: installer.clone(),
        };
        drop(runtime.spawn(worker.run(receiver)));
        Ok(Self {
            shared,
            installer,
            commands,
        })
    }

    /// Returns the current UI snapshot.
    pub fn snapshot(&self) -> UpdateSnapshot {
        let state = self.shared.lock();
        UpdateSnapshot {
            status: state.status.clone(),
            automatic: state.prefs.automatic_checks,
            last_check: state.prefs.last_check_time(),
        }
    }

    /// Triggers an immediate update check.
    pub fn check_now(&self) {
        drop(self.commands.send(Command::Check));
    }

    /// Updates and persists the automatic-check preference.
    pub fn set_automatic_checks(&self, enabled: bool) {
        let saved = self.shared.update_prefs(|prefs| {
            prefs.automatic_checks = enabled;
            Ok::<(), ()>(())
        });
        if saved.is_ok() {
            self.shared.host.changed();
        }
    }

    /// Claims a ready update, refreshes briefly for a newer release, swaps bundles, and relaunches.
    pub fn restart_to_update(&self) -> bool {
        {
            let mut state = self.shared.lock();
            let UpdateStatus::Ready { version, .. } = &state.status else {
                return false;
            };
            if state.staged.is_none() {
                return false;
            }
            state.status = UpdateStatus::Installing {
                version: version.clone(),
            };
        }
        self.shared.host.changed();
        let (reply, answer) = std_mpsc::channel();
        if self.commands.send(Command::Restart(reply)).is_err() {
            return false;
        }
        answer.recv().unwrap_or(false)
    }

    /// Swaps a staged update into place without relaunching during normal app shutdown.
    pub fn install_on_quit(&self) -> bool {
        let entry = {
            let mut state = self.shared.lock();
            if state.stranded || matches!(state.status, UpdateStatus::Installing { .. }) {
                return false;
            }
            let Some(entry) = state.staged.take() else {
                return false;
            };
            state.status = UpdateStatus::Installing {
                version: entry.staged.version().to_owned(),
            };
            entry
        };
        match self.installer.apply(&entry.staged, false) {
            Ok(()) => true,
            Err(ApplyError::NotApplied(_)) => {
                self.installer.discard(&entry.staged);
                self.shared.set_status(UpdateStatus::Failed(UpdateFailure::Install));
                false
            }
            Err(ApplyError::Stranded { .. }) => {
                let mut state = self.shared.lock();
                state.stranded = true;
                state.staged = Some(entry);
                false
            }
        }
    }

    /// Opens the manual download URL when [`UpdateStatus::Available`] is active.
    pub fn open_download(&self) {
        let url = {
            let state = self.shared.lock();
            match state.status {
                UpdateStatus::Available { .. } => state.download_url.clone(),
                _ => None,
            }
        };
        if let Some(url) = url {
            self.shared.host.open_url(&url);
        }
    }

    /// Opens the release notes page when a ready or available update includes one.
    pub fn open_notes(&self) {
        let url = {
            let state = self.shared.lock();
            match &state.status {
                UpdateStatus::Ready { notes_url, .. } | UpdateStatus::Available { notes_url, .. } => {
                    notes_url.clone()
                }
                _ => state.notes_url.clone(),
            }
        };
        if let Some(url) = url {
            self.shared.host.open_url(&url);
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    config: Config,
    transport: Transport,
    installer: Arc<dyn Installer>,
}

impl Worker {
    async fn run(self, mut commands: mpsc::UnboundedReceiver<Command>) {
        let cleaner = self.installer.clone();
        drop(tokio::task::spawn_blocking(move || cleaner.remove_leftovers()).await);

        let schedule = self.config.schedule;
        let mut next = Instant::now() + schedule.launch_delay;
        loop {
            let command = tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => command,
                    None => return,
                },
                () = tokio::time::sleep_until(next), if self.config.background_checks => {
                    next = Instant::now() + schedule.interval;
                    if !self.shared.lock().prefs.automatic_checks {
                        continue;
                    }
                    Command::Check
                }
            };
            match command {
                Command::Check => {
                    self.check().await;
                    next = Instant::now() + schedule.interval;
                }
                Command::Restart(reply) => {
                    let (relaunching, replacement) = self.restart().await;
                    if reply.send(relaunching).is_err() {
                        // Caller already stopped waiting.
                    }
                    if let Some(release) = replacement {
                        let outcome = self.act(&release).await;
                        let (status, dropped) = self.settle(outcome);
                        self.discard_staged(dropped).await;
                        self.shared.set_status(status);
                    }
                }
            }
            self.shared.finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn check(&self) {
        {
            let mut state = self.shared.lock();
            if state.stranded || matches!(state.status, UpdateStatus::Installing { .. }) {
                return;
            }
            state.status = UpdateStatus::Checking;
        }
        self.shared.host.changed();
        let outcome = match self.fetch().await {
            Ok(release) => self.act(&release).await,
            Err(failure) => Err(failure),
        };
        let (status, dropped) = self.settle(outcome);
        self.discard_staged(dropped).await;
        self.shared.set_status(status);
    }

    async fn discard_staged(&self, staged: Option<StagedEntry>) {
        if let Some(entry) = staged {
            let installer = self.installer.clone();
            drop(tokio::task::spawn_blocking(move || installer.discard(&entry.staged)).await);
        }
    }

    fn settle(&self, outcome: Result<UpdateStatus, UpdateFailure>) -> (UpdateStatus, Option<StagedEntry>) {
        let mut state = self.shared.lock();
        let Some(ready_status) = state.staged.as_ref().map(StagedEntry::ready_status) else {
            return (outcome.unwrap_or_else(UpdateStatus::Failed), None);
        };
        match outcome {
            Ok(status @ (UpdateStatus::UpToDate | UpdateStatus::Available { .. })) => {
                (status, state.staged.take())
            }
            Ok(status @ UpdateStatus::Ready { .. }) => (status, None),
            Ok(_) | Err(_) => (ready_status, None),
        }
    }

    async fn fetch(&self) -> Result<Release, UpdateFailure> {
        let bytes = self
            .transport
            .fetch_feed(&self.config.feed_url)
            .await
            .map_err(|_| UpdateFailure::Check)?;
        let release = feed::open(&bytes, &self.config.public_key, &self.config.origins)
            .map_err(|_| UpdateFailure::Untrusted)?;
        let shared = self.shared.clone();
        let build = release.build;
        let min_build = self.config.build;
        tokio::task::spawn_blocking(move || {
            shared.update_prefs(|prefs| {
                let floor = prefs.highest_build_seen.max(min_build);
                if build < floor {
                    return Err(UpdateFailure::Untrusted);
                }
                prefs.highest_build_seen = build;
                prefs.set_last_check(SystemTime::now());
                Ok(())
            })
        })
        .await
        .unwrap_or(Err(UpdateFailure::Check))?;
        Ok(release)
    }

    fn install(&self) -> Install {
        Install {
            build: self.config.build,
            os_version: self.config.os_version,
            replaceable: self.installer.replaceable(),
        }
    }

    async fn act(&self, release: &Release) -> Result<UpdateStatus, UpdateFailure> {
        let install = self.install();
        match feed::decide(release, &install) {
            Decision::UpToDate => {
                let downloads = self.config.state_dir.join("downloads");
                drop(tokio::task::spawn_blocking(move || prune(&downloads, None)).await);
                Ok(UpdateStatus::UpToDate)
            }
            Decision::RequiresNewerOs { minimum_os } => Err(UpdateFailure::RequiresNewerOs {
                version: release.version.clone(),
                minimum_os,
            }),
            Decision::DownloadOnly(artifact) => {
                {
                    let mut state = self.shared.lock();
                    state.download_url = Some(artifact.url.clone());
                    state.notes_url = release.notes_url.clone();
                }
                Ok(UpdateStatus::Available {
                    version: release.version.clone(),
                    download_url: artifact.url,
                    notes_url: release.notes_url.clone(),
                    highlights: release.highlights.clone(),
                })
            }
            Decision::Install(artifact) => {
                let result = self.download_and_stage(release, &artifact).await;
                self.shared.lock().failed_build = result.is_err().then_some(release.build);
                result
            }
        }
    }

    async fn download_and_stage(&self, release: &Release, artifact: &Artifact) -> Result<UpdateStatus, UpdateFailure> {
        let ready = UpdateStatus::Ready {
            version: release.version.clone(),
            notes_url: release.notes_url.clone(),
            highlights: release.highlights.clone(),
        };
        let already = self
            .shared
            .lock()
            .staged
            .as_ref()
            .is_some_and(|entry| entry.staged.build() == release.build);
        if already {
            return Ok(ready);
        }
        let downloads = self.config.state_dir.join("downloads");
        let prune_dir = downloads.clone();
        let keep_sha = artifact.sha256.clone();
        drop(tokio::task::spawn_blocking(move || prune(&prune_dir, Some(&keep_sha))).await);

        let shared = self.shared.clone();
        let version = release.version.clone();
        let size = artifact.size.max(1);
        let shown = AtomicU8::new(u8::MAX);
        let progress = move |written: u64| {
            let percent = u8::try_from(written.saturating_mul(100) / size).unwrap_or(100);
            if shown.swap(percent, Ordering::Relaxed) != percent {
                shared.set_status(UpdateStatus::Downloading {
                    version: version.clone(),
                    percent,
                });
            }
        };
        progress(0);
        let (zip, _download_lock) = self
            .transport
            .download_locked(artifact, &downloads, &progress)
            .await
            .map_err(|error| match error {
                TransferError::Mismatch(_) => UpdateFailure::Verify,
                TransferError::Network(_) | TransferError::Http(_) | TransferError::Io(_) => {
                    UpdateFailure::Download
                }
            })?;
        let installer = self.installer.clone();
        let archive = zip.clone();
        let owned = release.clone();
        let staged = tokio::task::spawn_blocking(move || installer.stage(&archive, &owned))
            .await
            .unwrap_or_else(|error| Err(InstallError::Failed(error.to_string())));
        if staged.is_err() {
            drop(tokio::fs::remove_file(&zip).await);
        }
        let staged = staged.map_err(|error| match error {
            InstallError::Rejected(_) => UpdateFailure::Verify,
            InstallError::Failed(_) => UpdateFailure::Install,
        })?;
        let earlier = {
            let mut state = self.shared.lock();
            state.notes_url = release.notes_url.clone();
            state.staged.replace(StagedEntry {
                staged,
                notes_url: release.notes_url.clone(),
                highlights: release.highlights.clone(),
            })
        };
        self.discard_staged(earlier).await;
        Ok(ready)
    }

    fn replaces(&self, release: &Release, staged: &Staged) -> bool {
        release.build > staged.build()
            && self.shared.lock().failed_build != Some(release.build)
            && matches!(feed::decide(release, &self.install()), Decision::Install(_))
    }

    async fn restart(&self) -> (bool, Option<Release>) {
        let staged = {
            let state = self.shared.lock();
            match (&state.status, &state.staged) {
                (UpdateStatus::Installing { .. }, Some(entry)) => entry.staged.clone(),
                _ => return (false, None),
            }
        };
        if let Ok(Ok(release)) = tokio::time::timeout(self.config.schedule.restart_refresh, self.fetch()).await {
            if matches!(
                feed::decide(&release, &self.install()),
                Decision::UpToDate | Decision::DownloadOnly(_)
            ) {
                let outcome = self.act(&release).await;
                let (status, dropped) = self.settle(outcome);
                self.discard_staged(dropped).await;
                self.shared.set_status(status);
                return (false, None);
            }
            if self.replaces(&release, &staged) {
                self.shared.set_status(UpdateStatus::Checking);
                return (false, Some(release));
            }
        }
        let applying = self.installer.clone();
        let target = staged.clone();
        let applied = tokio::task::spawn_blocking(move || applying.apply(&target, true))
            .await
            .unwrap_or_else(|error| Err(ApplyError::NotApplied(InstallError::Failed(error.to_string()))));
        let failure = match applied {
            Ok(()) => {
                self.shared.host.terminate();
                return (true, None);
            }
            Err(ApplyError::NotApplied(error)) => {
                let dropped = self.shared.lock().staged.take();
                self.discard_staged(dropped).await;
                match error {
                    InstallError::Rejected(_) => UpdateFailure::Verify,
                    InstallError::Failed(_) => UpdateFailure::Install,
                }
            }
            Err(ApplyError::Stranded { .. }) => {
                self.shared.lock().stranded = true;
                UpdateFailure::Relaunch {
                    version: staged.version().to_owned(),
                }
            }
        };
        self.shared.set_status(UpdateStatus::Failed(failure));
        (false, None)
    }
}

fn prune(downloads: &Path, keep: Option<&str>) {
    let Ok(entries) = std::fs::read_dir(downloads) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.ends_with(".lock") {
            continue;
        }
        if keep.is_some_and(|keep| name_str.starts_with(keep)) {
            continue;
        }
        if name_str.ends_with(".download") {
            let lock_path = downloads.join(format!("{name_str}.lock"));
            let Ok(Some(_lock)) = crate::transport::try_lock_download(&lock_path) else {
                continue;
            };
        }
        drop(std::fs::remove_file(entry.path()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::feed::{CHANNEL, KIND_APP_ZIP, KIND_DMG, SCHEMA};
    use crate::transport::tests::{Behavior, Server, artifact_for};

    const ZIP_PATH: &str = "/releases/v0.2.0/TableView.zip";
    const DMG_PATH: &str = "/releases/v0.2.0/TableView.dmg";

    #[derive(Default)]
    struct FakeHost {
        changes: AtomicUsize,
        opened: Mutex<Vec<String>>,
        terminated: AtomicUsize,
    }

    impl Host for FakeHost {
        fn changed(&self) {
            self.changes.fetch_add(1, Ordering::SeqCst);
        }

        fn open_url(&self, url: &str) {
            self.opened.lock().unwrap().push(url.to_owned());
        }

        fn terminate(&self) {
            self.terminated.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeInstaller {
        replaceable: bool,
        stage_error: Mutex<Option<InstallError>>,
        apply_error: Mutex<Option<ApplyError>>,
        log: Mutex<Vec<String>>,
    }

    impl FakeInstaller {
        fn new(replaceable: bool) -> Self {
            Self {
                replaceable,
                stage_error: Mutex::new(None),
                apply_error: Mutex::new(None),
                log: Mutex::new(Vec::new()),
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Installer for FakeInstaller {
        fn replaceable(&self) -> bool {
            self.replaceable
        }

        fn remove_leftovers(&self) {
            self.log.lock().unwrap().push("clean".to_owned());
        }

        fn stage(&self, zip: &Path, release: &Release) -> Result<Staged, InstallError> {
            let bytes = std::fs::read(zip).unwrap();
            self.log
                .lock()
                .unwrap()
                .push(format!("stage {} {}", release.build, bytes.len()));
            if let Some(error) = self.stage_error.lock().unwrap().take() {
                return Err(error);
            }
            Ok(Staged::detached(
                PathBuf::from(format!("/Applications/.tableview-update-{}", release.build)),
                PathBuf::from("/Applications/TableView.app"),
                Expected::for_release(release, OsVersion::parse("14.0").unwrap(), "anything"),
            ))
        }

        fn apply(&self, staged: &Staged, relaunch: bool) -> Result<(), ApplyError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("apply {} {relaunch}", staged.build()));
            match self.apply_error.lock().unwrap().take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn discard(&self, staged: &Staged) {
            self.log.lock().unwrap().push(format!("discard {}", staged.build()));
        }
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[9; 32])
    }

    fn zip_body(build: u64) -> Vec<u8> {
        format!("Troy Barnes and Abed Nadir update {build}")
            .into_bytes()
            .repeat(1000)
    }

    struct Fixture {
        server: Server,
        host: Arc<FakeHost>,
        installer: Arc<FakeInstaller>,
        updater: Updater,
        config: Config,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        async fn new(build: u64, replaceable: bool) -> Self {
            Self::with(build, replaceable, false, tempfile::tempdir().unwrap()).await
        }

        async fn with(build: u64, replaceable: bool, background_checks: bool, dir: tempfile::TempDir) -> Self {
            let schedule = Schedule {
                launch_delay: Duration::from_millis(50),
                interval: Duration::from_secs(3600),
                restart_refresh: Duration::from_secs(5),
            };
            Self::scheduled(build, replaceable, background_checks, dir, schedule).await
        }

        async fn scheduled(
            build: u64,
            replaceable: bool,
            background_checks: bool,
            dir: tempfile::TempDir,
            schedule: Schedule,
        ) -> Self {
            let server = Server::start().await;
            server.put(ZIP_PATH, Vec::new(), Behavior::Serve);
            server.put(DMG_PATH, b"dmg".to_vec(), Behavior::Serve);
            let fixture_host = Arc::new(FakeHost::default());
            let installer = Arc::new(FakeInstaller::new(replaceable));
            let config = Config {
                feed_url: server.url("/update.json"),
                origins: server.origins(),
                public_key: signing_key().verifying_key(),
                build,
                os_version: OsVersion::parse("14.0").unwrap(),
                state_dir: dir.path().to_path_buf(),
                background_checks,
                automatic_default: true,
                schedule,
                user_agent: "TableView/test".to_owned(),
            };
            let updater = Updater::start(config.clone(), installer.clone(), fixture_host.clone(), &Handle::current())
                .unwrap();
            Self {
                server,
                host: fixture_host,
                installer,
                updater,
                config,
                _dir: dir,
            }
        }

        fn publish(&self, build: u64) {
            self.publish_signed(build, &signing_key());
        }

        fn publish_signed(&self, build: u64, key: &SigningKey) {
            self.server.replace_body(ZIP_PATH, zip_body(build));
            let mut zip = artifact_for(&self.server, ZIP_PATH, &zip_body(build));
            zip.kind = KIND_APP_ZIP.to_owned();
            let mut dmg = artifact_for(&self.server, DMG_PATH, b"dmg");
            dmg.kind = KIND_DMG.to_owned();
            let release = Release {
                schema: SCHEMA,
                app: crate::BUNDLE_ID.to_owned(),
                channel: CHANNEL.to_owned(),
                version: format!("0.2.{build}"),
                build,
                published_at: "2026-11-26T12:00:00Z".to_owned(),
                notes_url: Some("https://github.com/rselbach/tableview/releases/tag/v0.2.0".to_owned()),
                highlights: vec!["Faster Greendale grid scrolling".to_owned()],
                artifacts: vec![zip, dmg],
            };
            self.server
                .put("/update.json", feed::seal(&release, key), Behavior::Serve);
        }

        async fn finish(&self, send: impl FnOnce(&Updater)) -> UpdateStatus {
            let before = self.updater.shared.finished.load(Ordering::SeqCst);
            send(&self.updater);
            for _ in 0..500 {
                if self.updater.shared.finished.load(Ordering::SeqCst) > before {
                    return self.updater.snapshot().status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("command never finished");
        }

        async fn check(&self) -> UpdateStatus {
            self.finish(Updater::check_now).await
        }

        async fn restart(&self) -> bool {
            let updater = self.updater.clone();
            tokio::task::spawn_blocking(move || updater.restart_to_update())
                .await
                .unwrap()
        }

        async fn settle(&self, done: impl Fn(&UpdateStatus) -> bool) -> UpdateStatus {
            for _ in 0..500 {
                let status = self.updater.snapshot().status;
                if done(&status) {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("status stuck at {:?}", self.updater.snapshot().status);
        }

        fn logged(&self, prefix: &str) -> Vec<String> {
            self.installer
                .log()
                .into_iter()
                .filter(|line| line.starts_with(prefix))
                .collect()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_newer_release_downloads_stages_and_restarts() {
        let fixture = Fixture::new(41, true).await;
        fixture.publish(42);
        let status = fixture.check().await;
        assert_eq!(
            status,
            UpdateStatus::Ready {
                version: "0.2.42".to_owned(),
                notes_url: Some("https://github.com/rselbach/tableview/releases/tag/v0.2.0".to_owned()),
                highlights: vec!["Faster Greendale grid scrolling".to_owned()],
            }
        );
        assert!(fixture.updater.snapshot().last_check.is_some());

        fixture.updater.open_notes();
        assert_eq!(
            *fixture.host.opened.lock().unwrap(),
            vec!["https://github.com/rselbach/tableview/releases/tag/v0.2.0"]
        );

        assert!(fixture.restart().await);
        assert_eq!(
            fixture.updater.snapshot().status,
            UpdateStatus::Installing {
                version: "0.2.42".to_owned()
            }
        );
        assert_eq!(fixture.host.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.installer.log().last().map(String::as_str), Some("apply 42 true"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_on_quit_swaps_without_relaunching() {
        let fixture = Fixture::new(41, true).await;
        fixture.publish(42);
        assert!(matches!(fixture.check().await, UpdateStatus::Ready { .. }));
        assert!(fixture.updater.install_on_quit());
        assert_eq!(fixture.host.terminated.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.installer.log().last().map(String::as_str), Some("apply 42 false"));
        assert!(!fixture.updater.install_on_quit());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_only_install_offers_download() {
        let fixture = Fixture::new(41, false).await;
        fixture.publish(42);
        let status = fixture.check().await;
        assert_eq!(
            status,
            UpdateStatus::Available {
                version: "0.2.42".to_owned(),
                download_url: fixture.server.url(DMG_PATH),
                notes_url: Some("https://github.com/rselbach/tableview/releases/tag/v0.2.0".to_owned()),
                highlights: vec!["Faster Greendale grid scrolling".to_owned()],
            }
        );
        fixture.updater.open_download();
        assert_eq!(*fixture.host.opened.lock().unwrap(), vec![fixture.server.url(DMG_PATH)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn staged_update_survives_failed_checks_and_replays() {
        let fixture = Fixture::new(41, true).await;
        fixture.publish(42);
        assert!(matches!(fixture.check().await, UpdateStatus::Ready { .. }));

        fixture.server.put("/update.json", Vec::new(), Behavior::Fail);
        assert!(matches!(fixture.check().await, UpdateStatus::Ready { .. }));

        fixture.publish(40);
        assert!(matches!(fixture.check().await, UpdateStatus::Ready { .. }));

        fixture.publish(43);
        assert!(matches!(
            fixture.check().await,
            UpdateStatus::Ready { ref version, .. } if version == "0.2.43"
        ));
        assert_eq!(fixture.logged("discard"), ["discard 42"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_refreshes_for_newer_release_and_handles_stranded_swap() {
        let fixture = Fixture::new(41, true).await;
        fixture.publish(42);
        fixture.check().await;

        fixture.publish(43);
        assert!(!fixture.restart().await);
        let settled = fixture
            .settle(|s| matches!(s, UpdateStatus::Ready { version, .. } if version == "0.2.43"))
            .await;
        assert!(matches!(settled, UpdateStatus::Ready { version, .. } if version == "0.2.43"));

        *fixture.installer.apply_error.lock().unwrap() = Some(ApplyError::Stranded {
            previous: PathBuf::from("/Applications/.tableview-update-43/TableView.app"),
            why: "no launcher".to_owned(),
        });
        assert!(!fixture.restart().await);
        assert_eq!(
            fixture.updater.snapshot().status,
            UpdateStatus::Failed(UpdateFailure::Relaunch {
                version: "0.2.43".to_owned()
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn downgrade_and_bad_signature_are_refused() {
        let fixture = Fixture::new(42, true).await;
        fixture.publish(41);
        assert_eq!(fixture.check().await, UpdateStatus::Failed(UpdateFailure::Untrusted));

        fixture.publish_signed(43, &SigningKey::from_bytes(&[8; 32]));
        assert_eq!(fixture.check().await, UpdateStatus::Failed(UpdateFailure::Untrusted));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relaunch_reuses_cached_archive_and_restart_does_not_wait_on_hanging_download() {
        let fixture = Fixture::new(41, true).await;
        fixture.publish(42);
        assert!(matches!(fixture.check().await, UpdateStatus::Ready { .. }));
        assert_eq!(fixture.server.requests(ZIP_PATH).len(), 1);

        let relaunched = Updater::start(
            fixture.config.clone(),
            fixture.installer.clone(),
            fixture.host.clone(),
            &Handle::current(),
        )
        .unwrap();
        let before = relaunched.shared.finished.load(Ordering::SeqCst);
        relaunched.check_now();
        for _ in 0..500 {
            if relaunched.shared.finished.load(Ordering::SeqCst) > before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fixture.server.requests(ZIP_PATH).len(), 1);

        fixture.publish(43);
        fixture.server.behave(ZIP_PATH, Behavior::Hang);
        let replied = tokio::time::timeout(Duration::from_secs(5), fixture.restart()).await;
        assert_eq!(replied, Ok(false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn background_checks_run_on_launch_and_repeat_on_interval() {
        assert_eq!(Schedule::default().launch_delay, Duration::from_secs(10));
        assert_eq!(Schedule::default().interval, Duration::from_secs(60 * 60));

        let schedule = Schedule {
            launch_delay: Duration::from_millis(50),
            interval: Duration::from_millis(100),
            restart_refresh: Duration::from_secs(5),
        };
        let fixture = Fixture::scheduled(41, true, true, tempfile::tempdir().unwrap(), schedule).await;
        fixture.publish(41);

        for _ in 0..500 {
            if fixture.updater.shared.finished.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.updater.shared.finished.load(Ordering::SeqCst) >= 1);
        assert_eq!(fixture.updater.snapshot().status, UpdateStatus::UpToDate);
        assert_eq!(fixture.server.requests("/update.json").len(), 1);

        fixture.publish(42);
        for _ in 0..500 {
            if fixture.updater.shared.finished.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.updater.shared.finished.load(Ordering::SeqCst) >= 2);
        assert!(matches!(
            fixture.updater.snapshot().status,
            UpdateStatus::Ready { ref version, .. } if version == "0.2.42"
        ));
        assert!(fixture.server.requests("/update.json").len() >= 2);

        fixture.updater.set_automatic_checks(false);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let count = fixture.server.requests("/update.json").len();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(fixture.server.requests("/update.json").len(), count);
    }
}
