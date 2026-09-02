//! The Majik desktop application.
//!
//! `main` parses the two argument-only modes (`--version`, `--channel`), installs the tracing
//! subscriber and the [`config`] globals, then hands off to [`windows`], which owns the Library and
//! Settings windows. Everything below this crate is plain Rust; GPUI lives here and nowhere else.
//!
//! State flows one way: [`state::AppState`] holds the [`state::LibraryModel`] that wraps the library
//! and the generation engine; views observe it and rebuild from it, and never treat a copy of a
//! domain type as the source of truth. See CLAUDE.md for the architecture and the vocabulary the
//! code uses.

mod actions;
mod assets;
mod composer_state;
mod config;
mod credentials;
mod drafts;
mod grid_motion;
mod image_cache;
mod morph;
mod paging;
mod state;
#[cfg(test)]
mod test_support;
mod ui;
mod views;
mod windows;

use gpui::{App, AppContext as _};
use gpui_component::{Theme, ThemeMode};
use std::sync::Arc;

use crate::config::Config;
use crate::state::{AppState, LibraryModel};

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
        _ => {}
    }

    // `gpui_macos::text_system` warns about every duplicate PostScript face it skips, and macOS's
    // `.AppleSystemUIFont` family always lists a couple, so that target is capped at errors.
    // symphonia's AAC decoder logs an error for every frame it rejects, which is a decode error
    // it also returns and `majik_audio` reports once, so its target is off.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("majik=info".parse().unwrap())
                .add_directive("gpui=warn".parse().unwrap())
                .add_directive("gpui_macos::text_system=error".parse().unwrap())
                .add_directive("symphonia_codec_aac=off".parse().unwrap()),
        )
        .init();

    config::set_config_dir(config::default_config_dir());
    let mut config = Config::load();
    if std::env::var_os("MAJIK_COMPOSE").is_some() {
        config.compose_panel_open = true;
    }
    let root = config::resolve_library_root(std::env::var("MAJIK_LIBRARY").ok(), std::env::args().nth(1), &config);
    // The channel is the only way to tell which install a running process is; log it up front.
    tracing::info!(target: "majik", "{} · library: {} · provider: {}", config::app_name(), root.display(), config.provider);

    gpui_platform::application().with_assets(assets::Assets).run(move |cx: &mut App| {
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

        let keys = Arc::new(crate::credentials::ApiKeys::for_environment());
        let keys_for_lib = keys.clone();
        let library = cx.new(|cx| LibraryModel::open(root, keys_for_lib, cx).expect("open library"));
        library.update(cx, |m, cx| m.start_thumbnails(cx));
        let load_keys = keys.load(cx);
        let library_to_recover = library.clone();
        cx.set_global(AppState { library, keys });
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
