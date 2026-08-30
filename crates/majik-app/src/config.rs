//! App-level preferences (`config.json` in the OS app-data dir) and where the app keeps its files.
//! Library state lives in the library DB.

use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The build channel, stamped at compile time: `script/lib/release.sh` exports `MAJIK_CHANNEL=stable`,
/// so the shipped bundle is the only [`Channel::Stable`] build. Everything else — `cargo run`,
/// `cargo run --release`, `cargo test` — is [`Channel::Dev`] and keeps its own library, preferences
/// and app identity, so it can be wiped without touching the app you actually use.
///
/// Deliberately not `cfg!(debug_assertions)`: a release-profile build for profiling is still
/// development and must not open the real library.
const CHANNEL_NAME: &str = match option_env!("MAJIK_CHANNEL") {
    Some(name) => name,
    None => "dev",
};

/// A misspelled stamp would fall through to `Dev` and ship a bundle whose `Info.plist` says
/// `com.app.majik` while it writes to the dev folder. Fail the build instead.
const _: () = assert!(is_known_channel(CHANNEL_NAME), "MAJIK_CHANNEL must be \"dev\" or \"stable\"");

const fn is_known_channel(name: &str) -> bool {
    matches!(name.as_bytes(), b"dev" | b"stable")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Dev,
    Stable,
}

/// This build's channel.
pub fn channel() -> Channel {
    channel_const()
}

/// The same mapping as [`channel`], in a `const fn` so [`CHANNEL_MARKER`] can be a constant.
const fn channel_const() -> Channel {
    match CHANNEL_NAME.as_bytes() {
        b"stable" => Channel::Stable,
        _ => Channel::Dev,
    }
}

/// This build's [`Channel::marker`], baked into the binary so the packaging scripts can prove the
/// artifact they are about to ship carries the stamp. `majik --channel` prints it, which is also
/// what keeps the linker from dropping it.
pub const CHANNEL_MARKER: &str = channel_const().marker();

/// `majik --version`, and the version row on Settings → About.
pub fn version_line() -> String {
    format!("majik {} ({})", env!("CARGO_PKG_VERSION"), channel().name())
}

impl Channel {
    /// The channel's name, as `MAJIK_CHANNEL` spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Dev => "dev",
        }
    }

    /// A namespaced string the packaging scripts grep for in the built binary, to prove it was
    /// compiled with the channel they are about to package it as. Namespaced because `"stable"`
    /// alone appears in the binary for a dozen unrelated reasons.
    ///
    /// `script/lib/release.sh` and `script/bundle-windows.ps1` grep for this; renaming it without
    /// updating them would silently disable that guard, so a test pins the three together.
    pub const fn marker(self) -> &'static str {
        match self {
            Channel::Stable => "majik-channel:stable",
            Channel::Dev => "majik-channel:dev",
        }
    }

    /// Bundle / application id (`packaging/Info.plist`, system notifications, the macOS data folder).
    pub fn bundle_id(self) -> &'static str {
        match self {
            Channel::Stable => "com.app.majik",
            Channel::Dev => "com.app.majik-dev",
        }
    }

    /// Shown in the app menu and on Settings → About.
    pub fn app_name(self) -> &'static str {
        match self {
            Channel::Stable => "Majik",
            Channel::Dev => "Majik Dev",
        }
    }

    /// The folder name under the platform's app-data roots (the bundle id on macOS).
    fn dir_name(self) -> &'static str {
        if cfg!(target_os = "macos") {
            self.bundle_id()
        } else if cfg!(target_os = "windows") {
            match self {
                Channel::Stable => "Majik",
                Channel::Dev => "Majik Dev",
            }
        } else {
            match self {
                Channel::Stable => "majik",
                Channel::Dev => "majik-dev",
            }
        }
    }
}

pub fn bundle_id() -> &'static str {
    channel().bundle_id()
}

pub fn app_name() -> &'static str {
    channel().app_name()
}

/// Where the app keeps its own files, following each platform's convention. Every path carries the
/// [`Channel`], so a dev build and the shipped app are two independent installs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDirs {
    /// `config.json`, `drafts.json`.
    pub config: PathBuf,
    /// The default library (media, database, thumbnails) — potentially gigabytes, so never a
    /// roaming location.
    pub data: PathBuf,
    /// Regenerable downloads (voice previews); safe to delete at any time.
    pub cache: PathBuf,
}

/// Stable's paths, with Dev's folder name in brackets:
/// - macOS: one `~/Library/Application Support/com.app.majik[-dev]` folder for config and data,
///   `~/Library/Caches/com.app.majik[-dev]` for the cache.
/// - Windows: `%APPDATA%\Majik[ Dev]` for config, `%LOCALAPPDATA%\Majik[ Dev]` for data and cache.
/// - Linux (XDG): `~/.config/majik[-dev]`, `~/.local/share/majik[-dev]`, `~/.cache/majik[-dev]`.
pub fn app_dirs() -> Option<AppDirs> {
    app_dirs_for(channel())
}

fn app_dirs_for(channel: Channel) -> Option<AppDirs> {
    let base = directories::BaseDirs::new()?;
    Some(app_dirs_in(base.config_dir(), base.data_local_dir(), base.cache_dir(), channel))
}

fn app_dirs_in(config_base: &Path, data_local_base: &Path, cache_base: &Path, channel: Channel) -> AppDirs {
    let name = channel.dir_name();
    if cfg!(target_os = "macos") {
        let dir = data_local_base.join(name);
        AppDirs { config: dir.clone(), data: dir, cache: cache_base.join(name) }
    } else if cfg!(target_os = "windows") {
        // `BaseDirs::cache_dir` is `%LOCALAPPDATA%` on Windows, i.e. the data root, so the cache
        // gets its own subfolder rather than sitting beside the library.
        AppDirs { config: config_base.join(name), data: data_local_base.join(name), cache: data_local_base.join(name).join("cache") }
    } else {
        AppDirs { config: config_base.join(name), data: data_local_base.join(name), cache: cache_base.join(name) }
    }
}

/// The library folder to open: `MAJIK_LIBRARY`, then the first CLI argument, then the configured
/// folder (Settings → Library folder), then `<data dir>/Library`.
pub fn resolve_library_root(env: Option<String>, arg: Option<String>, config: &Config) -> PathBuf {
    env.or(arg).or_else(|| config.library_root.clone()).map(PathBuf::from).unwrap_or_else(default_library_root)
}

pub fn default_library_root() -> PathBuf {
    app_dirs().map(|d| d.data).unwrap_or_else(|| PathBuf::from(".")).join("Library")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// The provider picked in the composer's provider menu; generations and tools go to it while
    /// it has an API key (see `state::selected_provider`).
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_appearance")]
    pub appearance: String,
    #[serde(default)]
    pub library_root: Option<String>,
    #[serde(default)]
    pub onboarding_completed: bool,
    /// Feed zoom level: the minimum tile width in px, one of `majik_core::feed::ZOOM_LEVELS`.
    #[serde(default = "default_zoom")]
    pub grid_zoom: u32,
    /// Feed cells crop thumbnails to squares or show them whole (Photos' "Square / Aspect Ratio").
    #[serde(default)]
    pub thumbnail_shape: ThumbnailShape,
    /// Composer draft prompt (global, not per provider or tab).
    #[serde(default)]
    pub draft_prompt: String,
    /// Skip animations (`App::set_reduce_motion`). GPUI doesn't read the OS setting, so this is an
    /// in-app preference instead.
    #[serde(default)]
    pub reduce_motion: bool,
    /// Last frame of the Library window.
    #[serde(default)]
    pub library_frame: Option<WindowFrame>,
    #[serde(default)]
    pub settings_frame: Option<WindowFrame>,
    /// Whether the sidebar on the left of the Library window is shown, and its width once resized.
    #[serde(default = "default_true")]
    pub sidebar_open: bool,
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// Whether the composer panel on the right of the Library window is shown, and its width once
    /// resized.
    #[serde(default = "default_true")]
    pub compose_panel_open: bool,
    #[serde(default)]
    pub compose_panel_width: Option<f32>,
}

/// How the feed fits a thumbnail into its square cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThumbnailShape {
    /// Fill the cell, cropping the long edge.
    #[default]
    Square,
    /// Fit the whole image, letterboxed inside the cell.
    AspectRatio,
}

impl ThumbnailShape {
    pub fn toggled(self) -> Self {
        match self {
            Self::Square => Self::AspectRatio,
            Self::AspectRatio => Self::Square,
        }
    }
}

/// A window frame to restore on relaunch, in logical pixels relative to `display`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
    /// `PlatformDisplay::uuid` the frame is relative to; window bounds are per display on macOS.
    #[serde(default)]
    pub display: Option<String>,
}

fn default_provider() -> String {
    majik_providers::ProviderId::FAL.to_string()
}
fn default_appearance() -> String {
    "system".into()
}
fn default_zoom() -> u32 {
    majik_core::feed::DEFAULT_ZOOM
}
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            appearance: default_appearance(),
            library_root: None,
            onboarding_completed: false,
            grid_zoom: default_zoom(),
            thumbnail_shape: ThumbnailShape::default(),
            draft_prompt: String::new(),
            reduce_motion: false,
            library_frame: None,
            settings_frame: None,
            sidebar_open: true,
            sidebar_width: None,
            compose_panel_open: true,
            compose_panel_width: None,
        }
    }
}

impl Global for Config {}

use std::sync::OnceLock;

/// Directory for `config.json` / `drafts.json`. Set once by `main`; when unset (tests, headless),
/// preferences live only in the GPUI global and nothing touches disk.
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR.set(dir);
}

pub fn config_dir() -> Option<&'static PathBuf> {
    CONFIG_DIR.get()
}

/// The default OS config directory (used by `main` to initialize [`set_config_dir`]).
pub fn default_config_dir() -> PathBuf {
    app_dirs().map(|d| d.config).unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.json"))
}

/// Where API keys are persisted — the one thing every channel shares, so a provider key is entered
/// once and wiping the dev folder doesn't lose it. It is always the *stable* folder, which means a
/// dev build on a machine that has never run the shipped app creates that folder. The whole key map
/// is stored as one item, so removing a key in one channel removes it in the other.
///
/// `None` until `main` sets the config dir, which keeps tests off the real file.
pub fn credentials_dir() -> Option<PathBuf> {
    config_dir().and_then(|_| app_dirs_for(Channel::Stable).map(|d| d.config))
}

/// This channel's regenerable cache (voice previews), so wiping the dev folder wipes its cache too.
pub fn cache_dir() -> Option<PathBuf> {
    app_dirs().map(|d| d.cache)
}

impl Config {
    pub fn load() -> Self {
        config_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(target: "majik", "saving config: {e}");
            }
        }
    }

    pub fn provider_id(&self) -> majik_providers::ProviderId {
        majik_providers::ProviderId(self.provider.clone())
    }
}

/// Mutate + persist the global config.
pub fn update_config(cx: &mut App, f: impl FnOnce(&mut Config)) {
    let cfg = cx.global_mut::<Config>();
    f(cfg);
    cfg.save();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(channel: Channel) -> AppDirs {
        app_dirs_in(Path::new("/cfg"), Path::new("/data"), Path::new("/cache"), channel)
    }

    #[test]
    fn channel_defaults_to_dev() {
        // Nothing stamps `MAJIK_CHANNEL` for `cargo test`, so the suite can never be pointed at the
        // shipped app's library. (`MAJIK_CHANNEL=stable cargo test` legitimately flips this.)
        assert_eq!(channel(), Channel::Dev);
    }

    #[test]
    fn the_channel_marker_names_this_build() {
        assert_eq!(CHANNEL_MARKER, Channel::Dev.marker(), "the test suite is a dev build");
        assert_eq!(Channel::Stable.marker(), "majik-channel:stable");
        assert_eq!(Channel::Dev.marker(), "majik-channel:dev");
        // The marker has to be findable in the binary without matching by accident: `stable` alone
        // occurs throughout it, so the prefix is the whole point.
        assert!(Channel::Stable.marker().starts_with("majik-channel:"));
    }

    #[test]
    fn every_bundle_script_greps_for_the_marker_the_binary_emits() {
        // Renaming the marker without updating the scripts would leave them grepping for a string no
        // build emits — the "forgot MAJIK_CHANNEL" guard would pass every time, on every platform.
        let shared = include_str!("../../../script/lib/release.sh");
        let windows = include_str!("../../../script/bundle-windows.ps1");
        for (name, script) in [("script/lib/release.sh", shared), ("script/bundle-windows.ps1", windows)] {
            assert!(script.contains("majik-channel:"), "{name} no longer greps for the channel marker");
        }
        // The scripts build the marker from the prefix and the channel name, so both halves must match.
        assert!(shared.contains("CHANNEL_MARKER_PREFIX=\"majik-channel:\""));
        assert!(shared.contains(&format!("RELEASE_CHANNEL=\"{}\"", Channel::Stable.name())));
    }

    #[test]
    fn the_version_line_names_the_version_and_the_channel() {
        let line = version_line();
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.contains(channel().name()), "{line}");
        assert_eq!(line, format!("majik {} (dev)", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn stable_dirs_are_exactly_the_shipped_ones() {
        // Renaming any of these orphans a real user's library: they are the installed app's paths.
        let stable = dirs(Channel::Stable);
        if cfg!(target_os = "macos") {
            let dir = PathBuf::from("/data/com.app.majik");
            assert_eq!(stable, AppDirs { config: dir.clone(), data: dir, cache: PathBuf::from("/cache/com.app.majik") });
        } else if cfg!(target_os = "windows") {
            assert_eq!(
                stable,
                AppDirs { config: PathBuf::from("/cfg/Majik"), data: PathBuf::from("/data/Majik"), cache: PathBuf::from("/data/Majik/cache") }
            );
        } else {
            assert_eq!(
                stable,
                AppDirs { config: PathBuf::from("/cfg/majik"), data: PathBuf::from("/data/majik"), cache: PathBuf::from("/cache/majik") }
            );
        }
        assert_eq!(Channel::Stable.bundle_id(), "com.app.majik");
        assert_eq!(Channel::Stable.app_name(), "Majik");
    }

    #[test]
    fn dev_dirs_never_overlap_the_stable_ones() {
        let (dev, stable) = (dirs(Channel::Dev), dirs(Channel::Stable));
        for (dev, stable) in [(&dev.config, &stable.config), (&dev.data, &stable.data), (&dev.cache, &stable.cache)] {
            assert_ne!(dev, stable);
            // Nesting either way would make `rm -rf <dev folder>` reach into the shipped app's files.
            assert!(!dev.starts_with(stable) && !stable.starts_with(dev), "{} / {}", dev.display(), stable.display());
        }
        assert_eq!(Channel::Dev.bundle_id(), "com.app.majik-dev");
        assert_eq!(Channel::Dev.app_name(), "Majik Dev");
    }

    #[test]
    fn credentials_are_shared_and_stay_off_disk_in_tests() {
        assert!(credentials_dir().is_none(), "no config dir is set in tests, so keys never touch disk");
        // Once `main` sets it, it resolves to the stable folder on every channel — that is what
        // makes the key store shared while everything else is split.
        assert_ne!(dirs(Channel::Dev).config, dirs(Channel::Stable).config);
    }

    #[test]
    fn the_bundle_plist_matches_the_stable_identity() {
        // `bundle-mac` never templates these, so the plist and the code can only be kept in step here.
        let plist = include_str!("../../../packaging/Info.plist");
        let id = format!("<key>CFBundleIdentifier</key><string>{}</string>", Channel::Stable.bundle_id());
        let name = format!("<key>CFBundleName</key><string>{}</string>", Channel::Stable.app_name());
        assert!(plist.contains(&id), "{id} missing from packaging/Info.plist");
        assert!(plist.contains(&name), "{name} missing from packaging/Info.plist");
    }

    #[test]
    fn the_bundle_plist_templates_the_version() {
        let plist = include_str!("../../../packaging/Info.plist");
        // `script/bundle-mac` substitutes both CFBundleVersion and CFBundleShortVersionString.
        assert_eq!(plist.matches("__VERSION__").count(), 2, "packaging/Info.plist lost a version placeholder");
        // A hardcoded version would survive the substitution and ship stale.
        assert!(!plist.contains(env!("CARGO_PKG_VERSION")), "packaging/Info.plist hardcodes a version");
        // The plist names the binary the bundle script copies in, which is `[[bin]] name`.
        assert!(plist.contains("<key>CFBundleExecutable</key><string>majik</string>"));
    }

    #[test]
    fn the_entitlements_are_the_ones_we_reviewed() {
        let entitlements = include_str!("../../../packaging/majik.entitlements");
        assert!(entitlements.contains("com.apple.security.cs.allow-jit"));
        // `get-task-allow` lets any process attach a debugger to the shipped app, and notarization
        // rejects it; `app-sandbox` would silently cut off the library folder.
        assert!(!entitlements.contains("get-task-allow"), "a debug entitlement must never ship");
        assert!(!entitlements.contains("com.apple.security.app-sandbox"));
        // Majik plays audio and never records it. Matched as a declared key, so the file's own
        // comment explaining why it is absent doesn't trip the assertion.
        assert!(!entitlements.contains("<key>com.apple.security.device.audio-input</key>"));
    }

    #[test]
    fn the_desktop_entry_matches_the_window_app_id() {
        let desktop = include_str!("../../../packaging/majik.desktop.in");
        // Both are substituted from APP_ID in `script/bundle-linux`, which is the bundle id. The
        // desktop shell matches a window to its launcher by StartupWMClass, so a mismatch costs the
        // app its icon and its taskbar grouping.
        assert!(desktop.contains("StartupWMClass=$APP_ID"));
        assert!(desktop.contains("Icon=$APP_ID"));
        assert!(desktop.contains("Name=$APP_NAME"));
        let bundle_linux = include_str!("../../../script/bundle-linux");
        assert!(bundle_linux.contains(&format!("APP_ID=\"{}\"", Channel::Stable.bundle_id())));
        assert!(bundle_linux.contains(&format!("APP_NAME=\"{}\"", Channel::Stable.app_name())));
    }

    #[test]
    fn the_installer_app_id_is_never_changed() {
        // Windows decides whether an installer upgrades an existing install or sits beside it by
        // this GUID. Changing it orphans every install that came before — the Windows counterpart
        // of `stable_dirs_are_exactly_the_shipped_ones`.
        let iss = include_str!("../../../packaging/majik.iss");
        assert!(iss.contains("#define AppId \"{92561171-E8BA-4C40-BC5E-9A8C3191D8D3}\""));
        assert!(iss.contains("OutputBaseFilename=MajikSetup-{#Arch}"));
    }

    #[test]
    fn library_root_precedence() {
        let configured = Config { library_root: Some("/configured".into()), ..Config::default() };
        let unset = Config::default();
        assert_eq!(resolve_library_root(Some("/env".into()), Some("/arg".into()), &configured), PathBuf::from("/env"));
        assert_eq!(resolve_library_root(None, Some("/arg".into()), &configured), PathBuf::from("/arg"));
        assert_eq!(resolve_library_root(None, None, &configured), PathBuf::from("/configured"));
        assert_eq!(resolve_library_root(None, None, &unset), default_library_root());
        assert!(default_library_root().ends_with("Library"));
        // The overrides are deliberately channel-independent: that is how a dev build opens a copy
        // of the real library. Only the fallback follows the channel.
        assert!(default_library_root().starts_with(app_dirs().map(|d| d.data).unwrap_or_else(|| PathBuf::from("."))));
    }
}
