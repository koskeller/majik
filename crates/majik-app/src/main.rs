//! The Majik desktop application.
//!
//! `main` parses the argument-only modes (`--version`, `--channel`, and the crash server the app
//! runs a second copy of itself as, `--crash-handler`), installs the tracing subscriber and the
//! [`config`] globals, starts telemetry and the crash handler, then hands off to [`windows`], which
//! owns the Library and Settings windows. Everything below this crate is plain Rust; GPUI lives
//! here and nowhere else.
//!
//! State flows one way: [`state::AppState`] holds the [`state::LibraryModel`] that wraps the library
//! and the generation engine; views observe it and rebuild from it, and never treat a copy of a
//! domain type as the source of truth. See CLAUDE.md for the architecture and the vocabulary the
//! code uses.

mod actions;
mod assets;
mod auto_update;
mod composer_state;
mod config;
mod credentials;
mod drafts;
mod grid_motion;
mod image_cache;
mod morph;
mod paging;
mod reliability;
mod state;
mod telemetry;
#[cfg(test)]
mod test_support;
mod ui;
mod views;
mod windows;

use gpui::{App, AppContext as _, Global};
use gpui_component::{Theme, ThemeMode};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::state::{AppState, LibraryModel};
use crate::telemetry::{Route, Telemetry};

/// The connection to the crash server, once it is up (`majik_crashes::init`).
struct CrashHandler(Arc<majik_crashes::Client>);

impl Global for CrashHandler {}

fn main() {
    // Answered before anything is initialised, and before the first argument is read as a library
    // path below (a path never starts with `--`). `--channel` is what the packaging scripts'
    // `require_channel_marker` checks, and referencing the constant stops the linker dropping it.
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-v") => {
            println!("{}", config::version_line());
            return;
        }
        Some("--channel") => {
            println!("{}", config::CHANNEL_MARKER);
            return;
        }
        // The crash server: the app spawns `majik --crash-handler <socket>` and connects to it.
        Some("--crash-handler") => {
            let Some(socket) = std::env::args().nth(2) else {
                eprintln!("usage: majik --crash-handler <socket>");
                std::process::exit(2);
            };
            config::set_config_dir(config::default_config_dir());
            // The server shares the app's log: a report it could not write is otherwise lost twice.
            init_logging();
            // `--crash-test` points the reports somewhere a test can look.
            let logs_dir = std::env::var_os("MAJIK_CRASH_LOGS_DIR").map(PathBuf::from).or_else(config::logs_dir).unwrap_or_else(|| PathBuf::from("."));
            if let Err(e) = majik_crashes::crash_server(std::path::Path::new(&socket), logs_dir) {
                eprintln!("majik --crash-handler: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        // The crash path end to end, for `tests/crash.rs`: bring the crash server up with its
        // reports going to <dir>, then panic.
        Some("--crash-test") => {
            let Some(dir) = std::env::args().nth(2) else {
                eprintln!("usage: majik --crash-test <dir>");
                std::process::exit(2);
            };
            crash_test(PathBuf::from(dir));
            return;
        }
        _ => {}
    }

    config::set_config_dir(config::default_config_dir());
    init_logging();
    let mut config = Config::load();
    // An install from before ids existed has finished onboarding; it is not a first launch.
    let first_launch = config.ensure_installation_id() && !config.onboarding_completed;
    if std::env::var_os("MAJIK_COMPOSE").is_some() {
        config.compose_panel_open = true;
    }
    let root = config::resolve_library_root(std::env::var("MAJIK_LIBRARY").ok(), std::env::args().nth(1), &config);
    // The channel is the only way to tell which install a running process is; log it up front.
    tracing::info!(target: "majik", "{} · library: {} · provider: {}", config::app_name(), root.display(), config.provider);
    let session_id = uuid::Uuid::new_v4().to_string();

    let app = gpui_platform::application().with_assets(assets::Assets);

    // A Stable build with somewhere to send reports gets the crash server; a dev build gets a
    // backtrace instead, unless it asks (`MAJIK_GENERATE_MINIDUMPS=1`) to exercise the real thing.
    // The backtrace hook goes in first either way: the server takes a moment to come up, and a
    // panic before then (the library failing to open) would otherwise have no hook at all.
    majik_crashes::force_backtrace();
    let crash_handler = if should_install_crash_handler() {
        let executor = app.background_executor();
        let spawner = app.background_executor();
        Some(executor.spawn(majik_crashes::init(
            crash_init(&session_id),
            move |task| spawner.spawn(task).detach(),
            |pid| config::cache_dir().unwrap_or_else(std::env::temp_dir).join(format!("majik-crash-handler-{pid}")),
            {
                let executor = app.background_executor();
                move |duration| executor.timer(duration)
            },
        )))
    } else {
        None
    };

    app.run(move |cx: &mut App| {
        gpui_component::init(cx);
        ui::install_theme(cx);
        match config.appearance.as_str() {
            "light" => Theme::change(ThemeMode::Light, None, cx),
            "dark" => Theme::change(ThemeMode::Dark, None, cx),
            _ => Theme::sync_system_appearance(None, cx),
        }
        cx.set_reduce_motion(config.reduce_motion);
        cx.set_global(config);
        // Bundle id from `packaging/Info.plist`; Windows needs it to post system notifications from an
        // unpackaged binary, the other platforms ignore it. It carries the channel, so a dev build
        // doesn't overwrite the shipped app's Action Center registration.
        cx.set_app_identity(config::bundle_id(), config::app_name());
        actions::init(cx);

        let installation_id = cx.global::<Config>().installation_id.clone();
        let telemetry = Telemetry::new(telemetry::transport_for_build(), installation_id, session_id, Route::Process, cx);
        if first_launch {
            majik_telemetry::event!("App First Opened");
        } else {
            majik_telemetry::event!("App Opened");
        }
        cx.on_app_quit({
            let telemetry = telemetry.clone();
            move |cx| {
                if let Some(handler) = cx.try_global::<CrashHandler>() {
                    majik_crashes::shutdown_crash_handler(&handler.0);
                }
                telemetry.shutdown()
            }
        })
        .detach();

        let keys = Arc::new(crate::credentials::ApiKeys::for_environment());
        let keys_for_lib = keys.clone();
        let library = cx.new(|cx| LibraryModel::open(root, keys_for_lib, cx).expect("open library"));
        library.update(cx, |m, cx| m.start_thumbnails(cx));
        let load_keys = keys.load(cx);
        let library_to_recover = library.clone();
        cx.set_global(AppState { library, keys, telemetry: telemetry.clone() });
        // The app opens either way; a failed load just means no keys until the user re-enters them.
        cx.spawn(async move |cx| {
            if let Err(e) = load_keys.await {
                tracing::warn!(target: "majik", "loading API keys: {e:#}");
            }
            // Resuming needs the keys, so rows left in flight are only recovered now.
            library_to_recover.update(cx, |m, cx| {
                m.recover_in_flight();
                m.changed(cx);
            });
        })
        .detach();
        cx.set_global(windows::Windows::default());

        windows::open_library(cx);
        if let Some(crash_handler) = crash_handler {
            cx.spawn(async move |cx| {
                match crash_handler.await {
                    Ok(client) => {
                        cx.update(|cx| {
                            // The GPU the app draws with, for the report; the window exists by now.
                            let specs = cx.global::<windows::Windows>().library.and_then(|handle| handle.update(cx, |_, window, _| window.gpu_specs()).ok().flatten());
                            if let Some(specs) = specs {
                                majik_crashes::set_gpu_info(
                                    &client,
                                    majik_crashes::GpuSpecs {
                                        is_software_emulated: specs.is_software_emulated,
                                        device_name: specs.device_name,
                                        driver_name: specs.driver_name,
                                        driver_info: specs.driver_info,
                                    },
                                );
                            }
                            cx.set_global(CrashHandler(client));
                        });
                    }
                    Err(e) => tracing::warn!(target: "majik", "no crash reports this session: {e:#}"),
                }
            })
            .detach();
        }
        report_previous_crashes(telemetry, cx);
        start_auto_update(cx);
        if let Ok(prompt) = std::env::var("MAJIK_GENERATE") {
            // Debug: dispatch a single mock image generation to exercise the pipeline end-to-end.
            let lib = crate::state::library(cx);
            lib.update(cx, |m, cx| {
                let model = majik_providers::catalog::image::ALL[0].clone();
                let req = majik_generation::Request::new(
                    majik_providers::ProviderId::mock(),
                    majik_generation::GenerationType::Image(majik_providers::ImageGenerationSettings {
                        model,
                        aspect_ratio: majik_providers::AspectRatio::Square,
                        resolution: majik_providers::ImageResolution::Hd,
                    }),
                    prompt,
                    vec![],
                );
                m.generate(vec![req], &[], None, cx);
            });
        }

        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

/// The updater (`auto_update`): polling when the build has a feed and the setting is on, and the
/// word that the last launch's update took. `app_path` is the `.app` on macOS and the exe
/// elsewhere; a binary gpui can't place (`cargo run` outside a bundle) can check but not install.
fn start_auto_update(cx: &mut App) {
    // An AppImage runs from a mount of itself; `APPIMAGE` is the one file to replace, and the
    // feed is asked for an AppImage rather than the tarball.
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|path| path.is_file());
    let feed = config::update_base_url().map(|base_url| Arc::new(auto_update::HttpFeed::new(base_url, appimage.is_some().then_some("appimage"))) as Arc<dyn auto_update::ReleaseFeed>);
    let running_app = match appimage {
        Some(appimage) => Some(appimage),
        None => match cx.app_path() {
            Ok(path) => Some(path),
            Err(e) => {
                tracing::info!(target: "majik", "not an installed app, so no self-update: {e:#}");
                None
            }
        },
    };
    let version = match semver::Version::parse(env!("CARGO_PKG_VERSION")) {
        Ok(version) => version,
        Err(e) => {
            tracing::warn!(target: "majik", "the crate version isn't a version; no self-update: {e}");
            return;
        }
    };
    auto_update::AutoUpdater::init(version, feed, Arc::new(auto_update::PlatformInstaller), running_app, cx);
    auto_update::report_update_applied(cx);
    cx.background_spawn(async { auto_update::sweep_stale_downloads() }).detach();
}

/// Whether this process runs the crash server: asked for outright, or a Stable build with
/// somewhere to send the reports (Zed's `should_install_crash_handler`).
fn should_install_crash_handler() -> bool {
    matches!(std::env::var("MAJIK_GENERATE_MINIDUMPS").as_deref(), Ok("true" | "1"))
        || (config::channel() != config::Channel::Dev && config::telemetry_base_url().is_some())
}

fn crash_init(session_id: &str) -> majik_crashes::InitCrashHandler {
    majik_crashes::InitCrashHandler {
        session_id: session_id.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        binary: "majik".to_string(),
        release_channel: config::channel().name().to_string(),
        commit_sha: config::commit_sha().map(String::from),
    }
}

/// Upload what the last session's crash left behind (`reliability`), then tell the user in the
/// Library window. Zed sends silently; a crash in a creative tool deserves a word. A report that
/// stays on disk (the switch is off, the network is down) says nothing: it would be repeated on
/// every launch, and the Telemetry page and Show Logs are where it can be found.
fn report_previous_crashes(telemetry: Arc<Telemetry>, cx: &mut App) {
    let Some(logs_dir) = config::logs_dir() else { return };
    let diagnostics = telemetry.settings().diagnostics;
    cx.spawn(async move |cx| {
        let outcome = cx.background_spawn(async move { reliability::upload_previous_minidumps(&telemetry, &logs_dir, diagnostics) }).await;
        if outcome.uploaded == 0 {
            return;
        }
        let message = "Majik crashed last time. A crash report was sent.";
        cx.update(|cx| {
            if let Some(handle) = cx.global::<windows::Windows>().library {
                handle.update(cx, |_, window, cx| ui::toast(window, message, cx)).ok();
            }
        });
    })
    .detach();
}

/// `majik --crash-test <dir>`: the real crash path without a window, so a test can watch the
/// server write `<session>.dmp` and `<session>.json` into `dir`.
fn crash_test(dir: PathBuf) {
    // The server is a child process, so it learns where to write through the environment.
    std::env::set_var("MAJIK_CRASH_LOGS_DIR", &dir);
    let socket_dir = dir.clone();
    let connect = majik_crashes::init(
        crash_init("crash-test"),
        |task| {
            std::thread::spawn(move || futures::executor::block_on(task));
        },
        move |pid| socket_dir.join(format!("majik-crash-handler-{pid}")),
        |duration| async move { std::thread::sleep(duration) },
    );
    let client = match futures::executor::block_on(connect) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("majik --crash-test: {e:#}");
            std::process::exit(1);
        }
    };
    let _keep = client;
    panic!("crash test");
}

/// How big `majik.log` may grow before a launch moves it aside as `majik.log.old` (Zed's
/// `Zed.log` / `Zed.log.old` rotation).
const LOG_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The tracing subscriber: stderr for `cargo run`, plus `majik.log` in the channel's logs folder so
/// a user has something to send with a bug report.
///
/// `gpui_macos::text_system` warns about every duplicate PostScript face it skips, and macOS's
/// `.AppleSystemUIFont` family always lists a couple, so that target is capped at errors.
/// symphonia's AAC decoder logs an error for every frame it rejects, which is a decode error it
/// also returns and `majik_audio` reports once, so its target is off.
fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("majik=info".parse().unwrap())
        .add_directive("gpui=warn".parse().unwrap())
        .add_directive("gpui_macos::text_system=error".parse().unwrap())
        .add_directive("symphonia_codec_aac=off".parse().unwrap());
    let file = config::logs_dir().and_then(|dir| match open_log_file(&dir) {
        Ok(file) => Some(std::sync::Mutex::new(file)),
        Err(e) => {
            eprintln!("majik: can't open the log file in {}: {e}", dir.display());
            None
        }
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(file.map(|file| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file)))
        .init();
}

/// Open `majik.log` for appending, rotating the previous one aside once it is over
/// [`LOG_FILE_MAX_BYTES`].
fn open_log_file(dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("majik.log");
    if std::fs::metadata(&path).map(|m| m.len() >= LOG_FILE_MAX_BYTES).unwrap_or(false) {
        std::fs::rename(&path, dir.join("majik.log.old"))?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}
