//! Keeping the installed app current, Zed's `auto_update` crate ported (`docs/updates.md` is the
//! public spec). Every hour, and on Check for Updates…, the [`AutoUpdater`] asks the release feed
//! (`config::update_base_url`) for the latest version; a newer one is downloaded to a folder of
//! its own, checked against the SHA-256 the feed gives, and installed over the running app:
//! rsync'd into the `.app` from the mounted DMG on macOS, the binary renamed into place on Linux,
//! and on Windows the installer is kept beside the exe and run silently when the app restarts or
//! quits. The user is told once the new version is ready and chooses when to restart; nothing
//! ever restarts on its own.
//!
//! The feed and the installer are traits, like `telemetry::Transport`, so the tests run the whole
//! state machine over fakes, and the real installers run against fake packages. A dev build has no feed unless `MAJIK_UPDATE_URL` points it at one, and a binary that
//! isn't an installed app (a `cargo run`) refuses to install over itself.

use crate::config::{self, update_config, Config};
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Subscription, Task, WeakEntity};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Launch is busy (the library opens, thumbnails start), so the first check waits a little.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
/// The download folder's name in the system temp dir; a launch sweeps old ones (a crash mid-update
/// would otherwise leave a DMG behind for good).
const TEMP_DIR_PREFIX: &str = "majik-update";
const STALE_TEMP_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How the Windows installer is run at restart: silently, closing the app it replaces
/// (`CloseApplications=force`), and — `update=true` — relaunching it from its `[Run]` section.
/// `packaging/majik.iss` reads the custom switches; `config::tests` pins the two together.
pub const WINDOWS_INSTALLER_SWITCHES: [&str; 4] = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/update=true"];
/// Added when the app is quitting for good rather than restarting.
pub const WINDOWS_NO_RELAUNCH_SWITCH: &str = "/relaunch=false";

/// What the feed answers for `<base>/<channel>/latest?os=&arch=`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
    /// Hex SHA-256 of the file at `url`; the download is refused when it doesn't match.
    #[serde(default)]
    pub sha256: Option<String>,
}

/// Where releases come from. Blocking: the updater calls it on the background executor.
pub trait ReleaseFeed: Send + Sync {
    fn latest(&self, os: &str, arch: &str) -> Result<ReleaseAsset>;
    /// Write the file at `url` to `to`, reporting the fraction downloaded when the size is known.
    fn download(&self, url: &str, to: &Path, progress: &mut dyn FnMut(Option<f32>)) -> Result<()>;
}

/// What installing left behind: the running app replaced in place, so a plain restart runs the
/// new version, or (Windows) an installer to run once the app has exited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    InPlace,
    StagedInstaller { program: PathBuf, arguments: Vec<OsString> },
}

/// The folder a package is downloaded to: a temp dir that goes with it, or (Windows) a fixed
/// folder beside the exe, because the installer has to outlive the process.
pub struct PackageDir {
    path: PathBuf,
    _temp: Option<tempfile::TempDir>,
}

impl PackageDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Puts a downloaded package where the running app is. Blocking, called on the background executor.
pub trait Installer: Send + Sync {
    fn package_dir(&self) -> Result<PackageDir>;
    fn install(&self, package: &Path, running_app: &Path) -> Result<Installed>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckType {
    Automatic,
    Manual,
}

impl CheckType {
    pub fn is_manual(self) -> bool {
        self == Self::Manual
    }

    fn name(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }
}

/// Where an update failed; what "Update Failed" carries, never the message (a path can be in it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Check,
    Download,
    Verify,
    Install,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Download => "download",
            Self::Verify => "verify",
            Self::Install => "install",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    Downloading {
        version: Version,
        /// `0.0..=1.0`, or `None` until the size is known.
        progress: Option<f32>,
    },
    Installing {
        version: Version,
    },
    /// Installed; a restart runs it.
    Updated {
        version: Version,
    },
    Errored {
        stage: Stage,
        message: SharedString,
    },
}

impl UpdateStatus {
    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

struct Failure {
    stage: Stage,
    error: anyhow::Error,
}

impl Failure {
    fn at(stage: Stage) -> impl FnOnce(anyhow::Error) -> Failure {
        move |error| Failure { stage, error }
    }
}

pub struct AutoUpdater {
    status: UpdateStatus,
    current_version: Version,
    /// `cx.app_path()` at launch: the `.app` on macOS, the exe elsewhere. `None` for a binary that
    /// isn't installed anywhere, which can check but never install.
    running_app: Option<PathBuf>,
    /// `None` on a build with nowhere to ask (a dev build without `MAJIK_UPDATE_URL`).
    feed: Option<Arc<dyn ReleaseFeed>>,
    installer: Arc<dyn Installer>,
    check: CheckType,
    pending: Option<Task<()>>,
    polling: Option<Task<()>>,
    last_checked: Option<Instant>,
    /// The version installed this session and waiting for a restart; later checks compare
    /// against it, not the running version, so it is never downloaded twice.
    installed: Option<Version>,
    /// Windows: the installer to run once the app exits, staged by a finished update.
    staged: Option<(PathBuf, Vec<OsString>)>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct GlobalAutoUpdater(Option<Entity<AutoUpdater>>);

impl Global for GlobalAutoUpdater {}

/// The app's updater, once `init` has run.
pub fn updater(cx: &App) -> Option<Entity<AutoUpdater>> {
    cx.try_global::<GlobalAutoUpdater>().and_then(|global| global.0.clone())
}

impl AutoUpdater {
    /// Create the updater and, when the config allows and there is a feed, start polling.
    pub fn init(current_version: Version, feed: Option<Arc<dyn ReleaseFeed>>, installer: Arc<dyn Installer>, running_app: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let updater = cx.new(|cx| {
            // A download or check in flight when the machine went to sleep is riding a connection
            // that died during suspend and would otherwise stall for good.
            let wake = cx.on_system_wake({
                let this = cx.entity().downgrade();
                move |cx: &mut App| {
                    this.update(cx, |this: &mut Self, cx| this.restart_after_wake(cx)).ok();
                }
            });
            let config = cx.observe_global::<Config>(|this: &mut Self, cx| this.sync_polling(cx));
            // Quitting with an installer staged still applies it, only without the relaunch.
            let quit = cx.on_app_quit(|this: &mut Self, _| {
                if let Some((program, mut arguments)) = this.staged.take() {
                    arguments.push(WINDOWS_NO_RELAUNCH_SWITCH.into());
                    if let Err(e) = spawn_detached(&program, &arguments) {
                        tracing::warn!(target: "majik", "running the staged installer on quit: {e:#}");
                    }
                }
                async {}
            });
            let mut this = Self {
                status: UpdateStatus::Idle,
                current_version,
                running_app,
                feed,
                installer,
                check: CheckType::Automatic,
                pending: None,
                polling: None,
                last_checked: None,
                installed: None,
                staged: None,
                _subscriptions: vec![wake, config, quit],
            };
            this.sync_polling(cx);
            this
        });
        cx.set_global(GlobalAutoUpdater(Some(updater.clone())));
        updater
    }

    pub fn status(&self) -> UpdateStatus {
        self.status.clone()
    }

    /// Whether this build has somewhere to ask.
    pub fn has_feed(&self) -> bool {
        self.feed.is_some()
    }

    pub fn has_checked(&self) -> bool {
        self.last_checked.is_some()
    }

    #[cfg(test)]
    pub fn check_type(&self) -> CheckType {
        self.check
    }

    #[cfg(test)]
    pub fn is_polling(&self) -> bool {
        self.polling.is_some()
    }

    #[cfg(test)]
    pub fn staged_installer(&self) -> Option<(PathBuf, Vec<OsString>)> {
        self.staged.clone()
    }

    /// Start or stop the hourly check to match the config and the build.
    fn sync_polling(&mut self, cx: &mut Context<Self>) {
        let wanted = self.feed.is_some() && cx.global::<Config>().auto_update;
        match (wanted, self.polling.is_some()) {
            (true, false) => {
                self.polling = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(FIRST_CHECK_DELAY).await;
                    loop {
                        if this.update(cx, |this, cx| this.poll(CheckType::Automatic, cx)).is_err() {
                            break;
                        }
                        cx.background_executor().timer(POLL_INTERVAL).await;
                    }
                }));
            }
            (false, true) => {
                self.polling = None;
            }
            _ => {}
        }
    }

    fn restart_after_wake(&mut self, cx: &mut Context<Self>) {
        // Only the network phases can be redone; an install mid-way must not be interrupted.
        if !matches!(self.status, UpdateStatus::Checking | UpdateStatus::Downloading { .. }) {
            return;
        }
        let check = self.check;
        self.pending = None;
        self.status = UpdateStatus::Idle;
        self.poll(check, cx);
    }

    /// Check now. One check runs at a time; a manual one arriving during an automatic one makes
    /// that one report its result the way a manual check would.
    pub fn poll(&mut self, check: CheckType, cx: &mut Context<Self>) {
        if self.feed.is_none() {
            return;
        }
        if self.pending.is_some() {
            if check.is_manual() && self.check == CheckType::Automatic {
                self.check = check;
                cx.notify();
            }
            return;
        }
        self.check = check;
        self.status = UpdateStatus::Checking;
        cx.notify();
        self.pending = Some(cx.spawn(async move |this, cx| {
            let result = Self::run(this.clone(), cx).await;
            this.update(cx, |this, cx| {
                this.pending = None;
                this.last_checked = Some(Instant::now());
                if let Err(Failure { stage, error }) = result {
                    majik_telemetry::event!("Update Failed", stage = stage.name(), check = this.check.name());
                    // Offline is normal for an automatic check; only a manual one, or a package
                    // that was downloaded and couldn't be installed, is worth a word.
                    if this.check.is_manual() || stage == Stage::Install {
                        tracing::warn!(target: "majik", "update ({}): {error:#}", stage.name());
                        this.status = UpdateStatus::Errored { stage, message: format!("{error:#}").into() };
                    } else {
                        tracing::info!(target: "majik", "update check skipped ({}): {error:#}", stage.name());
                        this.status = UpdateStatus::Idle;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    async fn run(this: WeakEntity<Self>, cx: &mut gpui::AsyncApp) -> Result<(), Failure> {
        let (feed, installer, current, installed, running_app) = this
            .read_with(cx, |this, _| (this.feed.clone(), this.installer.clone(), this.current_version.clone(), this.installed.clone(), this.running_app.clone()))
            .map_err(Failure::at(Stage::Check))?;
        let Some(feed) = feed else { return Ok(()) };

        let latest = cx
            .background_spawn({
                let feed = feed.clone();
                async move { feed.latest(std::env::consts::OS, std::env::consts::ARCH) }
            })
            .await
            .map_err(Failure::at(Stage::Check))?;
        let fetched = Version::parse(&latest.version).with_context(|| format!("the feed's version {:?} isn't a version", latest.version)).map_err(Failure::at(Stage::Check))?;
        let Some(version) = newer_version(&current, installed.as_ref(), &fetched) else {
            this.update(cx, |this, cx| {
                this.status = match this.installed.clone() {
                    Some(version) => UpdateStatus::Updated { version },
                    None => UpdateStatus::Idle,
                };
                cx.notify();
            })
            .ok();
            return Ok(());
        };
        tracing::info!(target: "majik", "update: {version} is available, running {current}");

        this.update(cx, |this, cx| {
            this.status = UpdateStatus::Downloading { version: version.clone(), progress: None };
            cx.notify();
        })
        .map_err(Failure::at(Stage::Download))?;
        let package_dir = cx.background_spawn({
            let installer = installer.clone();
            async move { installer.package_dir() }
        })
        .await
        .context("preparing the download folder")
        .map_err(Failure::at(Stage::Download))?;
        let package = package_dir.path().join(package_file_name(running_app.as_deref()));
        let (progress_tx, mut progress_rx) = mpsc::unbounded::<f32>();
        let download = cx.background_spawn({
            let (feed, url, package) = (feed.clone(), latest.url.clone(), package.clone());
            async move {
                feed.download(&url, &package, &mut |fraction| {
                    if let Some(fraction) = fraction {
                        progress_tx.unbounded_send(fraction).ok();
                    }
                })
            }
        });
        while let Some(fraction) = progress_rx.next().await {
            this.update(cx, |this, cx| {
                if let UpdateStatus::Downloading { progress, .. } = &mut this.status {
                    *progress = Some(fraction);
                    cx.notify();
                }
            })
            .ok();
        }
        download.await.with_context(|| format!("downloading {}", latest.url)).map_err(Failure::at(Stage::Download))?;

        if let Some(expected) = latest.sha256.as_deref() {
            let actual = cx.background_spawn({
                let package = package.clone();
                async move { sha256_file(&package) }
            })
            .await
            .map_err(Failure::at(Stage::Verify))?;
            if !actual.eq_ignore_ascii_case(expected.trim()) {
                if let Err(e) = std::fs::remove_file(&package) {
                    tracing::warn!(target: "majik", "removing a download that failed its checksum: {e}");
                }
                return Err(Failure { stage: Stage::Verify, error: anyhow::anyhow!("the download's checksum doesn't match the release's; it was discarded") });
            }
        }

        this.update(cx, |this, cx| {
            this.status = UpdateStatus::Installing { version: version.clone() };
            cx.notify();
        })
        .map_err(Failure::at(Stage::Install))?;
        let running_app = running_app.ok_or_else(|| anyhow::anyhow!("this copy of Majik isn't an installed app, so it can't update itself")).map_err(Failure::at(Stage::Install))?;
        let installed = cx
            .background_spawn({
                let (installer, package) = (installer.clone(), package.clone());
                async move { installer.install(&package, &running_app) }
            })
            .await
            .map_err(Failure::at(Stage::Install))?;
        // The package folder is only needed past this point by a staged installer, whose folder
        // isn't a temp dir; everything else (the DMG, the tarball) goes now.
        drop(package_dir);

        this.update(cx, |this, cx| {
            if let Installed::StagedInstaller { program, arguments } = installed {
                this.staged = Some((program, arguments));
            }
            this.installed = Some(version.clone());
            this.status = UpdateStatus::Updated { version: version.clone() };
            update_config(cx, |config| config.updated_from = Some(current.to_string()));
            notify_ready(&version, cx);
            cx.notify();
        })
        .map_err(Failure::at(Stage::Install))?;
        Ok(())
    }

    /// Run the installed version: relaunch the app, or on Windows hand over to the staged
    /// installer, which replaces the exe once this process has exited and starts the new one.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if !self.status.is_updated() {
            return;
        }
        match self.staged.take() {
            Some((program, arguments)) => match spawn_detached(&program, &arguments) {
                Ok(()) => cx.quit(),
                Err(error) => {
                    self.status = UpdateStatus::Errored { stage: Stage::Install, message: format!("{error:#}").into() };
                    cx.notify();
                }
            },
            None => cx.restart(),
        }
    }
}

/// Whether `fetched` is newer than what is running, or than what is already installed and
/// waiting for a restart. Pre-release and build suffixes are ignored: the feed never sends them.
fn newer_version(current: &Version, staged: Option<&Version>, fetched: &Version) -> Option<Version> {
    fn plain(version: &Version) -> Version {
        Version::new(version.major, version.minor, version.patch)
    }
    let fetched = plain(fetched);
    let baseline = plain(staged.unwrap_or(current));
    (fetched > baseline).then_some(fetched)
}

/// The downloaded package's name in its folder, by platform; on Linux an AppImage updates with an
/// AppImage and a tarball install with a tarball.
pub fn package_file_name(running_app: Option<&Path>) -> &'static str {
    match std::env::consts::OS {
        "macos" => "Majik.dmg",
        "windows" => "MajikSetup.exe",
        _ if is_appimage(running_app) => "majik.AppImage",
        _ => "majik.tar.gz",
    }
}

/// Whether `path` is an AppImage file: what `APPIMAGE` names when the app runs as one.
fn is_appimage(path: Option<&Path>) -> bool {
    path.is_some_and(|path| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("AppImage")))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

fn spawn_detached(program: &Path, arguments: &[OsString]) -> Result<()> {
    Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    Ok(())
}

/// "Majik 0.2.0 is ready" in the Library window, once, when an update has been installed.
fn notify_ready(version: &Version, cx: &mut App) {
    let Some(handle) = cx.try_global::<crate::windows::Windows>().and_then(|windows| windows.library) else { return };
    let message = format!("{} {version} is ready. Restart when you like.", config::app_name());
    handle.update(cx, |_, window, cx| crate::ui::toast(window, message, cx)).ok();
}

/// Check for Updates…: open Settings → About, where the result shows, and check.
pub fn check_for_updates(cx: &mut App) {
    crate::windows::open_settings(crate::views::settings::SettingsTarget { page: crate::views::settings::SettingsPage::About, ..Default::default() }, cx);
    if let Some(updater) = updater(cx) {
        updater.update(cx, |updater, cx| updater.poll(CheckType::Manual, cx));
    }
}

pub fn restart_to_update(cx: &mut App) {
    if let Some(updater) = updater(cx) {
        updater.update(cx, |updater, cx| updater.restart(cx));
    }
}

/// Settings → About → Check automatically.
pub fn set_auto_update(on: bool, cx: &mut App) {
    update_config(cx, |config| config.auto_update = on);
    majik_telemetry::event!("Settings Changed", setting = "auto_update", value = on);
}

/// The first launch after an update says so and records it; `Config.updated_from` was written by
/// the install. A value equal to the running version means the restart ran the old binary
/// (the install didn't take), which is logged and forgotten rather than repeated every launch.
pub fn report_update_applied(cx: &mut App) {
    let Some(from) = cx.global::<Config>().updated_from.clone() else { return };
    update_config(cx, |config| config.updated_from = None);
    let current = env!("CARGO_PKG_VERSION");
    if from == current {
        tracing::warn!(target: "majik", "an update from {from} was installed but {current} is still running");
        return;
    }
    majik_telemetry::event!("Update Applied", from_version = from);
    let Some(handle) = cx.try_global::<crate::windows::Windows>().and_then(|windows| windows.library) else { return };
    let message = format!("Updated to {} {current}.", config::app_name());
    handle.update(cx, |_, window, cx| crate::ui::toast(window, message, cx)).ok();
}

/// Remove what earlier updates left behind: download folders in the temp dir older than a day
/// (a newer one may belong to an update in flight), and on Windows the `updates` folder beside
/// the exe, whose installer has done its work by the time a new version launches.
pub fn sweep_stale_downloads() {
    if std::env::consts::OS == "windows" {
        if let Some(updates) = windows_updates_dir() {
            if updates.exists() {
                if let Err(e) = std::fs::remove_dir_all(&updates) {
                    tracing::warn!(target: "majik", "removing {}: {e}", updates.display());
                }
            }
        }
        return;
    }
    sweep_stale_temp_dirs(&std::env::temp_dir(), STALE_TEMP_DIR_AGE);
}

fn sweep_stale_temp_dirs(temp: &Path, older_than: Duration) {
    let Ok(entries) = std::fs::read_dir(temp) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(TEMP_DIR_PREFIX) {
            continue;
        }
        let stale = entry.metadata().ok().is_some_and(|metadata| metadata.is_dir() && metadata.modified().ok().and_then(|modified| SystemTime::now().duration_since(modified).ok()).is_some_and(|age| age > older_than));
        if stale {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(target: "majik", "removing a stale update folder {}: {e}", entry.path().display());
            }
        }
    }
}

fn windows_updates_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("updates"))
}

/// The feed at `config::update_base_url()`: `GET <base>/<channel>/latest?os=&arch=`, then the
/// asset it names, streamed to disk.
pub struct HttpFeed {
    base_url: String,
    /// `package=` on the check, when the app runs as an AppImage: the feed then names the
    /// AppImage rather than the tarball.
    package: Option<&'static str>,
}

impl HttpFeed {
    pub fn new(base_url: String, package: Option<&'static str>) -> Self {
        Self { base_url, package }
    }

    /// Its own client rather than telemetry's: that one caps a whole request at 30 s, and a DMG
    /// on a slow link takes longer. The check keeps a short cap per request; a download gets a
    /// long one, so a stalled connection still ends rather than holding the updater forever.
    fn client() -> Result<&'static reqwest::blocking::Client> {
        static CLIENT: std::sync::OnceLock<Result<reqwest::blocking::Client, String>> = std::sync::OnceLock::new();
        CLIENT
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .user_agent(concat!("majik/", env!("CARGO_PKG_VERSION")))
                    .connect_timeout(Duration::from_secs(30))
                    .timeout(DOWNLOAD_TIMEOUT)
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("building the update client: {e}"))
    }
}

impl ReleaseFeed for HttpFeed {
    fn latest(&self, os: &str, arch: &str) -> Result<ReleaseAsset> {
        let mut url = format!("{}/{}/latest?os={os}&arch={arch}", self.base_url, config::channel().name());
        if let Some(package) = self.package {
            url.push_str("&package=");
            url.push_str(package);
        }
        let response = Self::client()?.get(&url).timeout(CHECK_TIMEOUT).send().context("asking for the latest release")?;
        let status = response.status();
        let body = response.text().context("reading the release")?;
        anyhow::ensure!(status.is_success(), "the update server answered {status}: {}", body.trim());
        serde_json::from_str(&body).with_context(|| format!("the release answer isn't what was expected: {}", body.trim()))
    }

    fn download(&self, url: &str, to: &Path, progress: &mut dyn FnMut(Option<f32>)) -> Result<()> {
        let mut response = Self::client()?.get(url).send()?;
        anyhow::ensure!(response.status().is_success(), "the download answered {}", response.status());
        let total = response.content_length().filter(|total| *total > 0);
        // Written beside the target and renamed over it, so a half-download never looks whole.
        let part = to.with_file_name(format!("{}.part", to.file_name().map(|name| name.to_string_lossy()).unwrap_or_default()));
        let mut file = std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        let mut buffer = [0u8; 64 * 1024];
        let mut downloaded: u64 = 0;
        let mut last_percent: Option<u8> = None;
        loop {
            let n = response.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n])?;
            downloaded += n as u64;
            if let Some(total) = total {
                let fraction = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
                // Once per whole percent, so the UI isn't redrawn for every chunk.
                let percent = (fraction * 100.0) as u8;
                if last_percent != Some(percent) {
                    last_percent = Some(percent);
                    progress(Some(fraction));
                }
            }
        }
        file.flush()?;
        drop(file);
        std::fs::rename(&part, to).with_context(|| format!("moving the download into place at {}", to.display()))?;
        if total.is_some() && last_percent != Some(100) {
            progress(Some(1.0));
        }
        Ok(())
    }
}

/// The real installers. Each is plain Rust that compiles everywhere and is chosen by the running
/// OS, so the Linux one can be exercised on a Mac; only `hdiutil` is macOS-specific.
pub struct PlatformInstaller;

impl Installer for PlatformInstaller {
    fn package_dir(&self) -> Result<PackageDir> {
        if std::env::consts::OS == "windows" {
            // Beside the exe, as Zed does: the installer runs after this process is gone, when a
            // temp dir owned by it would already have been removed.
            let dir = windows_updates_dir().context("no folder beside the app to stage the installer in")?;
            if dir.exists() {
                std::fs::remove_dir_all(&dir).with_context(|| format!("clearing {}", dir.display()))?;
            }
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            return Ok(PackageDir { path: dir, _temp: None });
        }
        let temp = tempfile::Builder::new().prefix(TEMP_DIR_PREFIX).tempdir().context("creating a folder for the download")?;
        Ok(PackageDir { path: temp.path().to_path_buf(), _temp: Some(temp) })
    }

    fn install(&self, package: &Path, running_app: &Path) -> Result<Installed> {
        match std::env::consts::OS {
            "macos" => install_macos(package, running_app),
            "windows" => Ok(stage_windows(package)),
            _ => install_linux(package, running_app),
        }
    }
}

/// Mount the DMG and rsync the bundle inside it over the running one, then unmount. Replacing a
/// running bundle's files is what every macOS updater does; the new files carry their own
/// signature. rsync is part of macOS.
pub fn install_macos(dmg: &Path, running_app: &Path) -> Result<Installed> {
    anyhow::ensure!(running_app.extension().is_some_and(|extension| extension == "app"), "{} is not an app bundle; only an installed {}.app can update itself", running_app.display(), config::app_name());
    let mount_root = dmg.parent().context("the download has no folder")?.join("mount");
    std::fs::create_dir_all(&mount_root)?;
    run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen"]).arg(dmg).arg("-mountroot").arg(&mount_root), "mounting the disk image")?;
    // The volume is named after the version ("Majik 0.2.0"), so find it rather than guess it.
    let mount_point = std::fs::read_dir(&mount_root)?.flatten().map(|entry| entry.path()).find(|path| path.is_dir()).context("the disk image mounted nowhere")?;
    let result = (|| {
        let bundle = std::fs::read_dir(&mount_point)?.flatten().map(|entry| entry.path()).find(|path| path.is_dir() && path.extension().is_some_and(|extension| extension == "app")).context("no app bundle in the disk image")?;
        run(Command::new("rsync").args(["-a", "--delete", "--exclude", "Icon?"]).arg(with_trailing_slash(&bundle)).arg(with_trailing_slash(running_app)), "copying the new version into place")
    })();
    // Unmount whether or not the copy worked: a mounted image inside the temp folder stops the
    // folder from being removed, and leaks the image.
    if let Err(e) = run(Command::new("hdiutil").args(["detach", "-force"]).arg(&mount_point), "unmounting the disk image") {
        tracing::warn!(target: "majik", "{e:#}");
    }
    result?;
    Ok(Installed::InPlace)
}

/// Rename the new binary over the running one (legal on Linux: the old inode stays until the
/// process exits). An AppImage is replaced as the one file it is, `running_app` being the path
/// `APPIMAGE` named. A tarball is unpacked, its `bin/majik` put over the running binary, and the
/// icons refreshed when that binary lives in a `<prefix>/bin` beside a `<prefix>/share` that
/// the tarball's `install.sh` filled.
pub fn install_linux(package: &Path, running_app: &Path) -> Result<Installed> {
    if is_appimage(Some(package)) {
        replace_binary(package, running_app)?;
        return Ok(Installed::InPlace);
    }
    let extracted = package.parent().context("the download has no folder")?.join("extracted");
    std::fs::create_dir_all(&extracted)?;
    run(Command::new("tar").arg("-xzf").arg(package).arg("-C").arg(&extracted), "unpacking the download")?;
    let root = std::fs::read_dir(&extracted)?.flatten().map(|entry| entry.path()).find(|path| path.is_dir()).context("the download unpacked to nothing")?;
    let new_binary = root.join("bin").join("majik");
    anyhow::ensure!(new_binary.is_file(), "no bin/majik in the download");
    replace_binary(&new_binary, running_app)?;
    let target_dir = running_app.parent().context("the running binary has no folder")?;
    if target_dir.file_name().is_some_and(|name| name == "bin") {
        if let Some(prefix) = target_dir.parent() {
            refresh_icons(&root.join("share").join("icons"), &prefix.join("share").join("icons"));
        }
    }
    Ok(Installed::InPlace)
}

/// Copy `new` beside `target` as `<name>.new`, make it executable, and rename it over `target`.
fn replace_binary(new: &Path, target: &Path) -> Result<()> {
    let target_dir = target.parent().context("the running binary has no folder")?;
    let staged = target_dir.join(format!("{}.new", target.file_name().map(|name| name.to_string_lossy()).unwrap_or_default()));
    let cannot_write = || format!("{} can't replace {}; download the new version by hand", config::app_name(), target.display());
    std::fs::copy(new, &staged).with_context(cannot_write)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&staged, target).with_context(cannot_write)?;
    Ok(())
}

/// Overwrite the icons an earlier install put under `installed`, and only those: an icon the
/// user never had isn't added, and a failure here leaves a working install, so it's only logged.
fn refresh_icons(from: &Path, installed: &Path) {
    let mut stack = vec![from.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(relative) = path.strip_prefix(from) else { continue };
            let destination = installed.join(relative);
            if destination.is_file() {
                if let Err(e) = std::fs::copy(&path, &destination) {
                    tracing::warn!(target: "majik", "refreshing {}: {e}", destination.display());
                }
            }
        }
    }
}

/// Nothing to do until the app exits: the installer can't replace a running exe, so it is run by
/// `AutoUpdater::restart` (or on quit) once this process is gone.
pub fn stage_windows(installer: &Path) -> Installed {
    Installed::StagedInstaller { program: installer.to_path_buf(), arguments: WINDOWS_INSTALLER_SWITCHES.iter().map(OsString::from).collect() }
}

fn with_trailing_slash(path: &Path) -> OsString {
    let mut path = path.as_os_str().to_os_string();
    path.push("/");
    path
}

fn run(command: &mut Command, what: &str) -> Result<()> {
    let output = command.output().with_context(|| format!("{what}: couldn't run {:?}", command.get_program()))?;
    anyhow::ensure!(output.status.success(), "{what}: {:?} failed: {}", command.get_program(), String::from_utf8_lossy(&output.stderr).trim());
    Ok(())
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A feed that answers what it is told and serves `bytes` as the package.
    pub struct FakeFeed {
        pub latest: Mutex<Result<ReleaseAsset, String>>,
        pub bytes: Mutex<Vec<u8>>,
        pub fail_download: AtomicBool,
        pub downloads: AtomicUsize,
        pub asked: Mutex<Vec<(String, String)>>,
    }

    impl FakeFeed {
        pub fn offering(version: &str) -> Arc<Self> {
            let bytes = format!("<majik {version}>").into_bytes();
            let sha256 = {
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect::<String>()
            };
            Arc::new(Self {
                latest: Mutex::new(Ok(ReleaseAsset { version: version.into(), url: format!("https://feed.test/{version}"), sha256: Some(sha256) })),
                bytes: Mutex::new(bytes),
                fail_download: AtomicBool::new(false),
                downloads: AtomicUsize::new(0),
                asked: Mutex::new(Vec::new()),
            })
        }

        pub fn offer(&self, version: &str) {
            let bytes = format!("<majik {version}>").into_bytes();
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let sha256 = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
            *self.bytes.lock().unwrap() = bytes;
            *self.latest.lock().unwrap() = Ok(ReleaseAsset { version: version.into(), url: format!("https://feed.test/{version}"), sha256: Some(sha256) });
        }

        pub fn fail(&self, message: &str) {
            *self.latest.lock().unwrap() = Err(message.into());
        }

        pub fn set_sha256(&self, sha256: Option<&str>) {
            if let Ok(asset) = &mut *self.latest.lock().unwrap() {
                asset.sha256 = sha256.map(String::from);
            }
        }
    }

    impl ReleaseFeed for FakeFeed {
        fn latest(&self, os: &str, arch: &str) -> Result<ReleaseAsset> {
            self.asked.lock().unwrap().push((os.into(), arch.into()));
            self.latest.lock().unwrap().clone().map_err(|message| anyhow::anyhow!("{message}"))
        }

        fn download(&self, _url: &str, to: &Path, progress: &mut dyn FnMut(Option<f32>)) -> Result<()> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            if self.fail_download.load(Ordering::SeqCst) {
                anyhow::bail!("the network went away");
            }
            let bytes = self.bytes.lock().unwrap().clone();
            let half = bytes.len() / 2;
            std::fs::write(to, &bytes[..half])?;
            progress(Some(0.5));
            std::fs::write(to, &bytes)?;
            progress(Some(1.0));
            Ok(())
        }
    }

    /// Installs by copying the package into a folder standing in for the app, or refuses.
    pub struct FakeInstaller {
        pub app: tempfile::TempDir,
        pub installed: Mutex<Vec<Vec<u8>>>,
        pub fail: AtomicBool,
        /// Answer like Windows: nothing installed yet, an installer staged for restart.
        pub stage: AtomicBool,
        pub package_dirs: Mutex<Vec<PathBuf>>,
    }

    impl FakeInstaller {
        pub fn new() -> Arc<Self> {
            Arc::new(Self { app: tempfile::tempdir().unwrap(), installed: Mutex::new(Vec::new()), fail: AtomicBool::new(false), stage: AtomicBool::new(false), package_dirs: Mutex::new(Vec::new()) })
        }

        pub fn app_path(&self) -> PathBuf {
            self.app.path().join("Majik.app")
        }
    }

    impl Installer for FakeInstaller {
        fn package_dir(&self) -> Result<PackageDir> {
            if self.stage.load(Ordering::SeqCst) {
                // Like Windows: a fixed folder beside the app, which outlives the download.
                let dir = self.app.path().join("updates");
                std::fs::create_dir_all(&dir)?;
                return Ok(PackageDir { path: dir, _temp: None });
            }
            let temp = tempfile::Builder::new().prefix(TEMP_DIR_PREFIX).tempdir()?;
            self.package_dirs.lock().unwrap().push(temp.path().to_path_buf());
            Ok(PackageDir { path: temp.path().to_path_buf(), _temp: Some(temp) })
        }

        fn install(&self, package: &Path, running_app: &Path) -> Result<Installed> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("{} isn't writable", running_app.display());
            }
            if self.stage.load(Ordering::SeqCst) {
                return Ok(stage_windows(package));
            }
            assert_eq!(running_app, self.app_path());
            self.installed.lock().unwrap().push(std::fs::read(package)?);
            Ok(Installed::InPlace)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeFeed, FakeInstaller};
    use super::*;
    use crate::test_support::env;
    use gpui::TestAppContext;
    use std::sync::atomic::Ordering;

    fn updater(cx: &mut TestAppContext, feed: Option<Arc<FakeFeed>>, installer: Arc<FakeInstaller>) -> Entity<AutoUpdater> {
        let app_path = installer.app_path();
        cx.update(|cx| AutoUpdater::init(Version::new(0, 1, 0), feed.map(|feed| feed as Arc<dyn ReleaseFeed>), installer, Some(app_path), cx))
    }

    fn status(updater: &Entity<AutoUpdater>, cx: &mut TestAppContext) -> UpdateStatus {
        updater.read_with(cx, |updater, _| updater.status())
    }

    fn check(updater: &Entity<AutoUpdater>, kind: CheckType, cx: &mut TestAppContext) {
        updater.update(cx, |updater, cx| updater.poll(kind, cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn no_newer_version_stays_idle(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.1.0"), FakeInstaller::new());
        let updater = updater(cx, Some(feed.clone()), installer.clone());
        check(&updater, CheckType::Manual, cx);
        assert_eq!(status(&updater, cx), UpdateStatus::Idle);
        assert!(updater.read_with(cx, |updater, _| updater.has_checked()));
        assert_eq!(feed.downloads.load(Ordering::SeqCst), 0);
        assert_eq!(feed.asked.lock().unwrap()[0], (std::env::consts::OS.to_string(), std::env::consts::ARCH.to_string()), "the feed is asked for this machine's build");
        assert!(installer.installed.lock().unwrap().is_empty());
        // An older release on the feed (a rollback there) is not an update either.
        feed.offer("0.0.9");
        check(&updater, CheckType::Manual, cx);
        assert_eq!(status(&updater, cx), UpdateStatus::Idle);
    }

    #[gpui::test]
    fn a_newer_version_is_downloaded_installed_and_ready(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        let updater = updater(cx, Some(feed.clone()), installer.clone());
        cx.update(crate::windows::open_library);
        cx.run_until_parked();
        let toasts_before = cx.update(|cx| crate::ui::toast_generation(cx));
        check(&updater, CheckType::Manual, cx);
        assert_eq!(status(&updater, cx), UpdateStatus::Updated { version: Version::new(0, 2, 0) });
        assert_eq!(installer.installed.lock().unwrap().as_slice(), [b"<majik 0.2.0>".to_vec()], "the package the feed served is what was installed");
        assert_eq!(feed.downloads.load(Ordering::SeqCst), 1);
        assert_eq!(cx.update(|cx| cx.global::<Config>().updated_from.clone()).as_deref(), Some("0.1.0"), "the next launch will know it was updated");
        assert_eq!(cx.update(|cx| crate::ui::toast_generation(cx)), toasts_before + 1, "one 'ready' toast");
        for dir in installer.package_dirs.lock().unwrap().iter() {
            assert!(!dir.exists(), "the download folder is gone once installed: {}", dir.display());
        }
        assert!(e.events_named("Update Failed").is_empty());
    }

    #[gpui::test]
    fn an_already_installed_version_is_not_downloaded_again(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        let updater = updater(cx, Some(feed.clone()), installer.clone());
        check(&updater, CheckType::Manual, cx);
        check(&updater, CheckType::Automatic, cx);
        assert_eq!(feed.downloads.load(Ordering::SeqCst), 1, "0.2.0 is installed and waiting; the next check compares against it");
        assert_eq!(status(&updater, cx), UpdateStatus::Updated { version: Version::new(0, 2, 0) }, "and the status stays ready");
        // A release after that one does replace it.
        feed.offer("0.2.1");
        check(&updater, CheckType::Automatic, cx);
        assert_eq!(feed.downloads.load(Ordering::SeqCst), 2);
        assert_eq!(status(&updater, cx), UpdateStatus::Updated { version: Version::new(0, 2, 1) });
    }

    #[test]
    fn versions_are_compared_without_prerelease_and_build_suffixes() {
        let current = Version::parse("0.1.0").unwrap();
        assert_eq!(newer_version(&current, None, &Version::parse("0.1.1").unwrap()), Some(Version::new(0, 1, 1)));
        assert_eq!(newer_version(&current, None, &Version::parse("0.1.0").unwrap()), None);
        assert_eq!(newer_version(&current, None, &Version::parse("0.0.9").unwrap()), None);
        assert_eq!(newer_version(&current, None, &Version::parse("0.2.0-beta.1+abc").unwrap()), Some(Version::new(0, 2, 0)), "the feed never sends suffixes, and one wouldn't change the answer");
        assert_eq!(newer_version(&Version::parse("0.1.0-dev").unwrap(), None, &Version::parse("0.1.0").unwrap()), None);
        let staged = Version::new(0, 2, 0);
        assert_eq!(newer_version(&current, Some(&staged), &Version::new(0, 2, 0)), None, "already installed and waiting for a restart");
        assert_eq!(newer_version(&current, Some(&staged), &Version::new(0, 2, 1)), Some(Version::new(0, 2, 1)));
    }

    #[gpui::test]
    fn an_automatic_check_fails_quietly(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        feed.fail("the update server answered 503");
        let updater = updater(cx, Some(feed.clone()), installer);
        check(&updater, CheckType::Automatic, cx);
        assert_eq!(status(&updater, cx), UpdateStatus::Idle, "offline is normal; nothing to show");
        let failed = e.events_named("Update Failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].event_properties["stage"], "check");
        assert_eq!(failed[0].event_properties["check"], "automatic");
        assert!(!serde_json::to_string(&failed[0]).unwrap().contains("503"), "the error text stays out of telemetry");
    }

    #[gpui::test]
    fn a_manual_check_shows_the_error(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        feed.fail("the update server answered 503");
        let updater = updater(cx, Some(feed.clone()), installer);
        check(&updater, CheckType::Manual, cx);
        let UpdateStatus::Errored { stage, message } = status(&updater, cx) else { panic!("a manual check reports its failure") };
        assert_eq!(stage, Stage::Check);
        assert!(message.contains("503"), "{message}");
        assert_eq!(e.events_named("Update Failed")[0].event_properties["check"], "manual");
        // A download failing is reported the same way…
        feed.offer("0.2.0");
        feed.fail_download.store(true, Ordering::SeqCst);
        check(&updater, CheckType::Manual, cx);
        assert!(matches!(status(&updater, cx), UpdateStatus::Errored { stage: Stage::Download, .. }), "{:?}", status(&updater, cx));
        // …and quietly when automatic.
        check(&updater, CheckType::Automatic, cx);
        assert_eq!(status(&updater, cx), UpdateStatus::Idle);
    }

    #[gpui::test]
    fn a_bad_checksum_refuses_the_package(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        feed.set_sha256(Some("00".repeat(32).as_str()));
        let updater = updater(cx, Some(feed.clone()), installer.clone());
        check(&updater, CheckType::Manual, cx);
        let UpdateStatus::Errored { stage, message } = status(&updater, cx) else { panic!("refused: {:?}", status(&updater, cx)) };
        assert_eq!(stage, Stage::Verify);
        assert!(message.contains("checksum"), "{message}");
        assert!(installer.installed.lock().unwrap().is_empty(), "never installed");
        assert_eq!(e.events_named("Update Failed")[0].event_properties["stage"], "verify");
        // Without a checksum on the feed the download is taken as is; with a matching one, too.
        feed.set_sha256(None);
        check(&updater, CheckType::Manual, cx);
        assert!(status(&updater, cx).is_updated());
        feed.offer("0.2.1");
        check(&updater, CheckType::Manual, cx);
        assert_eq!(status(&updater, cx), UpdateStatus::Updated { version: Version::new(0, 2, 1) });
    }

    #[gpui::test]
    fn an_install_failure_is_reported_even_when_automatic(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        installer.fail.store(true, Ordering::SeqCst);
        let updater = updater(cx, Some(feed), installer.clone());
        check(&updater, CheckType::Automatic, cx);
        let UpdateStatus::Errored { stage, message } = status(&updater, cx) else { panic!("a package that can't be installed is worth a word") };
        assert_eq!(stage, Stage::Install);
        assert!(message.contains("isn't writable"), "{message}");
        assert_eq!(e.events_named("Update Failed")[0].event_properties["stage"], "install");
        assert_eq!(cx.update(|cx| cx.global::<Config>().updated_from.clone()), None);
    }

    #[gpui::test]
    fn a_binary_that_is_not_installed_anywhere_checks_but_does_not_install(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        let updater = cx.update(|cx| AutoUpdater::init(Version::new(0, 1, 0), Some(feed.clone() as Arc<dyn ReleaseFeed>), installer.clone(), None, cx));
        check(&updater, CheckType::Manual, cx);
        assert!(matches!(status(&updater, cx), UpdateStatus::Errored { stage: Stage::Install, .. }), "{:?}", status(&updater, cx));
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[gpui::test]
    fn a_manual_check_upgrades_a_running_automatic_one(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        feed.fail("down");
        let updater = updater(cx, Some(feed), installer);
        // Neither check gets to run before the second arrives: the executor is not parked yet.
        updater.update(cx, |updater, cx| {
            updater.poll(CheckType::Automatic, cx);
            assert_eq!(updater.check_type(), CheckType::Automatic);
            updater.poll(CheckType::Manual, cx);
            assert_eq!(updater.check_type(), CheckType::Manual, "the running check now answers as a manual one");
        });
        cx.run_until_parked();
        assert!(matches!(status(&updater, cx), UpdateStatus::Errored { stage: Stage::Check, .. }));
        // The other way round, a manual check is not demoted.
        updater.update(cx, |updater, cx| {
            updater.poll(CheckType::Manual, cx);
            updater.poll(CheckType::Automatic, cx);
            assert_eq!(updater.check_type(), CheckType::Manual);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn polling_follows_the_setting_and_the_clock(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.1.0"), FakeInstaller::new());
        let updater = updater(cx, Some(feed.clone()), installer);
        assert!(updater.read_with(cx, |updater, _| updater.is_polling()), "on by default");
        cx.run_until_parked();
        assert_eq!(feed.asked.lock().unwrap().len(), 0, "not at once: launch is busy");
        cx.background_executor.advance_clock(FIRST_CHECK_DELAY);
        cx.run_until_parked();
        assert_eq!(feed.asked.lock().unwrap().len(), 1);
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(feed.asked.lock().unwrap().len(), 2, "an hour later");

        cx.update(|cx| set_auto_update(false, cx));
        cx.run_until_parked();
        assert!(!updater.read_with(cx, |updater, _| updater.is_polling()));
        cx.background_executor.advance_clock(POLL_INTERVAL * 3);
        cx.run_until_parked();
        assert_eq!(feed.asked.lock().unwrap().len(), 2, "off means off");
        check(&updater, CheckType::Manual, cx);
        assert_eq!(feed.asked.lock().unwrap().len(), 3, "Check for Updates… still works");

        cx.update(|cx| set_auto_update(true, cx));
        cx.run_until_parked();
        cx.background_executor.advance_clock(FIRST_CHECK_DELAY);
        cx.run_until_parked();
        assert_eq!(feed.asked.lock().unwrap().len(), 4, "back on");
    }

    #[gpui::test]
    fn a_build_without_a_feed_never_polls(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let updater = updater(cx, None, FakeInstaller::new());
        assert!(!updater.read_with(cx, |updater, _| updater.is_polling() || updater.has_feed()));
        check(&updater, CheckType::Manual, cx);
        assert_eq!(status(&updater, cx), UpdateStatus::Idle, "a manual check has nowhere to go and says nothing");
    }

    #[gpui::test]
    fn waking_from_sleep_restarts_a_check_but_not_an_install(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        let updater = updater(cx, Some(feed.clone()), installer);
        updater.update(cx, |updater, cx| {
            updater.poll(CheckType::Manual, cx);
            assert_eq!(updater.status(), UpdateStatus::Checking);
            updater.restart_after_wake(cx);
            assert_eq!(updater.status(), UpdateStatus::Checking, "started over, still manual");
            assert_eq!(updater.check_type(), CheckType::Manual);
        });
        cx.run_until_parked();
        assert!(status(&updater, cx).is_updated());
        assert_eq!(feed.asked.lock().unwrap().len(), 1, "the first check was dropped before it ran");
        updater.update(cx, |updater, cx| {
            updater.restart_after_wake(cx);
            assert!(updater.status().is_updated(), "an installed update is left alone");
        });
    }

    #[gpui::test]
    async fn restart_relaunches_the_app_in_place(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        let updater = updater(cx, Some(feed), installer);
        let mut will_restart = cx.expect_restart();
        updater.update(cx, |updater, cx| updater.restart(cx));
        assert!(will_restart.try_recv().unwrap().is_none(), "nothing to restart into yet");
        check(&updater, CheckType::Manual, cx);
        cx.update(restart_to_update);
        let (path, arguments) = will_restart.await.unwrap();
        assert_eq!(path, None, "the app was replaced in place: gpui reopens the same bundle");
        assert!(arguments.is_empty());
    }

    #[gpui::test]
    fn a_staged_installer_is_kept_for_the_restart(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (feed, installer) = (FakeFeed::offering("0.2.0"), FakeInstaller::new());
        installer.stage.store(true, Ordering::SeqCst);
        let updater = updater(cx, Some(feed), installer.clone());
        check(&updater, CheckType::Manual, cx);
        assert!(status(&updater, cx).is_updated());
        let (program, arguments) = updater.read_with(cx, |updater, _| updater.staged_installer()).expect("the installer is staged");
        assert_eq!(program.file_name().unwrap(), package_file_name(None));
        assert!(program.exists(), "kept for the restart");
        assert_eq!(arguments, WINDOWS_INSTALLER_SWITCHES.map(OsString::from));
        assert!(installer.installed.lock().unwrap().is_empty(), "nothing replaced while the app runs");
    }

    #[gpui::test]
    fn the_launch_after_an_update_says_so_once(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        cx.update(|cx| update_config(cx, |config| config.updated_from = Some("0.0.1".into())));
        cx.update(report_update_applied);
        cx.run_until_parked();
        let applied = e.events_named("Update Applied");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].event_properties["from_version"], "0.0.1");
        assert_eq!(cx.update(|cx| cx.global::<Config>().updated_from.clone()), None, "cleared");
        cx.update(report_update_applied);
        cx.run_until_parked();
        assert_eq!(e.events_named("Update Applied").len(), 1, "once");
        // An install that didn't take (the old binary is still running) is forgotten, not celebrated.
        cx.update(|cx| update_config(cx, |config| config.updated_from = Some(env!("CARGO_PKG_VERSION").into())));
        cx.update(report_update_applied);
        cx.run_until_parked();
        assert_eq!(e.events_named("Update Applied").len(), 1);
        assert_eq!(cx.update(|cx| cx.global::<Config>().updated_from.clone()), None);
    }

    #[test]
    fn the_feed_answer_parses_with_or_without_a_checksum() {
        let asset: ReleaseAsset = serde_json::from_str(r#"{"version":"0.2.0","url":"https://example.test/Majik.dmg"}"#).unwrap();
        assert_eq!(asset, ReleaseAsset { version: "0.2.0".into(), url: "https://example.test/Majik.dmg".into(), sha256: None });
        let asset: ReleaseAsset = serde_json::from_str(r#"{"version":"0.2.0","url":"u","sha256":"ab"}"#).unwrap();
        assert_eq!(asset.sha256.as_deref(), Some("ab"));
    }

    #[test]
    fn sha256_of_a_file_matches_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"seedbodyseed").unwrap();
        // `printf 'seedbodyseed' | shasum -a 256`
        assert_eq!(sha256_file(&path).unwrap(), "4efb65bc417a61cd876ee8416c9a12c8d54131282054a44438199456d49af75b");
    }

    /// A handle to a directory whose modified time can be set. Windows refuses to open a
    /// directory as a file unless asked for backup semantics.
    fn open_dir(path: &Path) -> std::fs::File {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            std::fs::OpenOptions::new().write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).unwrap()
        }
        #[cfg(not(windows))]
        std::fs::File::open(path).unwrap()
    }

    #[test]
    fn stale_download_folders_are_swept_and_fresh_ones_kept() {
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join("majik-update-old");
        let fresh = temp.path().join("majik-update-new");
        let other = temp.path().join("something-else");
        for dir in [&stale, &fresh, &other] {
            std::fs::create_dir(dir).unwrap();
        }
        let long_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        open_dir(&stale).set_modified(long_ago).unwrap();
        open_dir(&other).set_modified(long_ago).unwrap();
        sweep_stale_temp_dirs(temp.path(), STALE_TEMP_DIR_AGE);
        assert!(!stale.exists());
        assert!(fresh.exists(), "may belong to an update in flight");
        assert!(other.exists(), "not ours");
    }

    #[test]
    fn every_update_event_is_documented_and_carries_no_error_text() {
        let source = include_str!("auto_update.rs");
        let telemetry_docs = include_str!("../../../docs/telemetry.md");
        let updates_docs = include_str!("../../../docs/updates.md");
        let mut events = 0;
        for line in source.lines().filter(|line| line.contains("majik_telemetry::event!(\"Update ")) {
            events += 1;
            let name = line.split("event!(\"").nth(1).and_then(|rest| rest.split('"').next()).expect("an event name");
            assert!(telemetry_docs.contains(&format!("| {name} |")), "{name} is missing from docs/telemetry.md");
            assert!(updates_docs.contains(&format!("`{name}`")), "{name} is missing from docs/updates.md");
            // The stage and the check type are enough to see a rollout failing; the message can
            // name the library folder or the user's home.
            assert!(!line.contains("message") && !line.contains("error ="), "{line}");
        }
        assert_eq!(events, 2, "Update Failed and Update Applied");
    }

    /// A fake Linux tarball: `majik-linux-x86_64/bin/majik` holding `binary`, plus one icon.
    #[cfg(unix)]
    fn linux_tarball(dir: &Path, binary: &str) -> PathBuf {
        let root = dir.join("package").join("majik-linux-x86_64");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin").join("majik"), binary).unwrap();
        let icons = root.join("share").join("icons").join("hicolor").join("512x512").join("apps");
        std::fs::create_dir_all(&icons).unwrap();
        std::fs::write(icons.join("com.app.majik.png"), "new icon").unwrap();
        let tarball = dir.join("majik.tar.gz");
        run(Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(dir.join("package")).arg("majik-linux-x86_64"), "packing the fake tarball").unwrap();
        tarball
    }

    /// A fake `<prefix>` an `install.sh` filled: the old binary and two icons.
    #[cfg(unix)]
    fn linux_prefix(dir: &Path) -> PathBuf {
        let prefix = dir.join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::write(prefix.join("bin").join("majik"), "old binary").unwrap();
        for size in ["512x512", "256x256"] {
            let icons = prefix.join("share").join("icons").join("hicolor").join(size).join("apps");
            std::fs::create_dir_all(&icons).unwrap();
            std::fs::write(icons.join("com.app.majik.png"), "old icon").unwrap();
        }
        prefix
    }

    #[cfg(unix)]
    #[test]
    fn the_linux_installer_renames_the_binary_into_place_and_refreshes_the_icons() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let tarball = linux_tarball(dir.path(), "new binary");
        let prefix = linux_prefix(dir.path());
        let binary = prefix.join("bin").join("majik");
        assert_eq!(install_linux(&tarball, &binary).unwrap(), Installed::InPlace);
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "new binary");
        assert_ne!(std::fs::metadata(&binary).unwrap().permissions().mode() & 0o111, 0, "executable");
        assert!(!prefix.join("bin").join("majik.new").exists(), "nothing staged is left behind");
        let icon = |size: &str| std::fs::read_to_string(prefix.join("share").join("icons").join("hicolor").join(size).join("apps").join("com.app.majik.png")).unwrap();
        assert_eq!(icon("512x512"), "new icon", "an icon the install had is refreshed");
        assert_eq!(icon("256x256"), "old icon", "one the package lacks is left alone");
    }

    #[cfg(unix)]
    #[test]
    fn the_linux_installer_leaves_a_binary_outside_a_prefix_alone_but_replaces_it() {
        // Run in place from the unpacked tarball, or anywhere else: the binary is still replaced,
        // and no icons are touched because there is no `share` beside a `bin`.
        let dir = tempfile::tempdir().unwrap();
        let tarball = linux_tarball(dir.path(), "new binary");
        let elsewhere = dir.path().join("somewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let binary = elsewhere.join("majik");
        std::fs::write(&binary, "old binary").unwrap();
        assert_eq!(install_linux(&tarball, &binary).unwrap(), Installed::InPlace);
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "new binary");
    }

    #[cfg(unix)]
    #[test]
    fn the_linux_installer_reports_a_folder_it_cannot_write() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let tarball = linux_tarball(dir.path(), "new binary");
        let prefix = linux_prefix(dir.path());
        let bin = prefix.join("bin");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = install_linux(&tarball, &bin.join("majik"));
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("can't replace") && error.contains("by hand"), "{error}");
        assert_eq!(std::fs::read_to_string(bin.join("majik")).unwrap(), "old binary", "untouched");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_installer_copies_the_bundle_out_of_the_disk_image() {
        let dir = tempfile::tempdir().unwrap();
        // The new bundle, on a DMG whose volume carries the version, as `script/bundle-mac` names it.
        let image_root = dir.path().join("image");
        let new_bundle = image_root.join("Majik.app");
        std::fs::create_dir_all(new_bundle.join("Contents").join("MacOS")).unwrap();
        std::fs::write(new_bundle.join("Contents").join("MacOS").join("majik"), "new binary").unwrap();
        std::fs::write(new_bundle.join("Contents").join("Info.plist"), "new plist").unwrap();
        let download = dir.path().join("download");
        std::fs::create_dir_all(&download).unwrap();
        let dmg = download.join("Majik.dmg");
        run(Command::new("hdiutil").args(["create", "-volname", "Majik 9.9.9", "-srcfolder"]).arg(&image_root).args(["-ov", "-format", "UDZO", "-quiet"]).arg(&dmg), "making the fake disk image").unwrap();
        // The installed bundle: an old binary and a file the new version no longer ships.
        let running_app = dir.path().join("Applications").join("Majik.app");
        std::fs::create_dir_all(running_app.join("Contents").join("MacOS")).unwrap();
        std::fs::write(running_app.join("Contents").join("MacOS").join("majik"), "old binary").unwrap();
        std::fs::write(running_app.join("Contents").join("stale.txt"), "gone after").unwrap();

        assert_eq!(install_macos(&dmg, &running_app).unwrap(), Installed::InPlace);
        assert_eq!(std::fs::read_to_string(running_app.join("Contents").join("MacOS").join("majik")).unwrap(), "new binary");
        assert_eq!(std::fs::read_to_string(running_app.join("Contents").join("Info.plist")).unwrap(), "new plist");
        assert!(!running_app.join("Contents").join("stale.txt").exists(), "the copy replaces, it doesn't merge");
        let mounted: Vec<PathBuf> = std::fs::read_dir(download.join("mount")).unwrap().flatten().map(|entry| entry.path()).collect();
        assert!(mounted.is_empty(), "the image is unmounted again: {mounted:?}");

        let error = format!("{:#}", install_macos(&dmg, &dir.path().join("target").join("debug")).unwrap_err());
        assert!(error.contains("not an app bundle"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn the_linux_installer_replaces_an_appimage_as_one_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let download = dir.path().join("majik.AppImage");
        std::fs::write(&download, "new appimage").unwrap();
        let running = dir.path().join("Downloads").join("majik-linux-aarch64.AppImage");
        std::fs::create_dir_all(running.parent().unwrap()).unwrap();
        std::fs::write(&running, "old appimage").unwrap();
        assert_eq!(install_linux(&download, &running).unwrap(), Installed::InPlace);
        assert_eq!(std::fs::read_to_string(&running).unwrap(), "new appimage");
        assert_ne!(std::fs::metadata(&running).unwrap().permissions().mode() & 0o111, 0, "still executable");
        assert!(!running.with_extension("AppImage.new").exists() && !dir.path().join("extracted").exists(), "nothing unpacked, nothing left");
    }

    #[test]
    fn the_package_name_follows_how_the_app_is_packaged() {
        let expected = match std::env::consts::OS {
            "macos" => "Majik.dmg",
            "windows" => "MajikSetup.exe",
            _ => "majik.tar.gz",
        };
        assert_eq!(package_file_name(None), expected);
        assert_eq!(package_file_name(Some(Path::new("/usr/local/bin/majik"))), expected);
        let appimage = package_file_name(Some(Path::new("/home/kos/Downloads/majik-linux-aarch64.AppImage")));
        if std::env::consts::OS == "linux" {
            assert_eq!(appimage, "majik.AppImage");
        } else {
            assert_eq!(appimage, expected, "only Linux runs as an AppImage");
        }
    }

    #[test]
    fn the_windows_installer_is_staged_with_the_silent_update_switches() {
        let installer = Path::new("C:\\Users\\kos\\AppData\\Local\\Programs\\Majik\\updates\\MajikSetup.exe");
        assert_eq!(stage_windows(installer), Installed::StagedInstaller { program: installer.to_path_buf(), arguments: WINDOWS_INSTALLER_SWITCHES.map(OsString::from).to_vec() });
    }

    /// A one-shot HTTP server answering `body` with the headers given.
    fn serve_once(status: &'static str, headers: &'static str, body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let n = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..n]).to_string();
            stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            request
        });
        (url, handle)
    }

    #[test]
    fn the_http_feed_asks_for_this_channel_and_platform() {
        let (url, server) = serve_once("200 OK", "Content-Type: application/json\r\n", br#"{"version":"0.2.0","url":"https://example.test/Majik.dmg","sha256":"ab"}"#.to_vec());
        let feed = HttpFeed::new(url, None);
        let asset = feed.latest("macos", "aarch64").unwrap();
        assert_eq!(asset.version, "0.2.0");
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /dev/latest?os=macos&arch=aarch64 HTTP/1.1"), "{request}");
        assert!(request.contains(concat!("user-agent: majik/", env!("CARGO_PKG_VERSION"))), "{request}");
        // An AppImage says so, and the feed names an AppImage back.
        let (url, server) = serve_once("200 OK", "", br#"{"version":"0.2.0","url":"https://example.test/majik-linux-aarch64.AppImage"}"#.to_vec());
        HttpFeed::new(url, Some("appimage")).latest("linux", "aarch64").unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /dev/latest?os=linux&arch=aarch64&package=appimage HTTP/1.1"), "{request}");
    }

    #[test]
    fn the_http_feed_reports_a_refusal_and_a_bad_answer() {
        let (url, server) = serve_once("404 Not Found", "", b"no build for linux/aarch64".to_vec());
        let error = HttpFeed::new(url, None).latest("linux", "aarch64").unwrap_err().to_string();
        assert!(error.contains("404") && error.contains("no build"), "{error}");
        server.join().unwrap();
        let (url, server) = serve_once("200 OK", "", b"<html>".to_vec());
        let error = HttpFeed::new(url, None).latest("linux", "x86_64").unwrap_err().to_string();
        assert!(error.contains("isn't what was expected"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn the_http_feed_downloads_with_progress_and_leaves_no_part_file() {
        let body = vec![7u8; 200_000];
        let (url, server) = serve_once("200 OK", "", body.clone());
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("Majik.dmg");
        let mut reported = Vec::new();
        HttpFeed::new(String::new(), None).download(&format!("{url}/Majik.dmg"), &to, &mut |fraction| reported.push(fraction)).unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), body);
        assert!(!dir.path().join("Majik.dmg.part").exists());
        assert_eq!(reported.last().copied().flatten(), Some(1.0));
        assert!(reported.iter().flatten().all(|fraction| (0.0..=1.0).contains(fraction)));
        assert!(reported.windows(2).all(|pair| pair[0] <= pair[1]), "never goes backwards: {reported:?}");
    }
}
