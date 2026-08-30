//! The singleton Library window (the composer is a panel inside it), the singleton Settings window
//! (a window of its own, the way Zed's `settings_ui` does it), and frame persistence for both.

use gpui::{point, px, size, App, AppContext as _, Bounds, Context, DisplayId, Global, Pixels, SharedString, Size, Task, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions};
use gpui_component::{Root, TitleBar};
use std::collections::HashMap;
use std::time::Duration;

use crate::config;
use crate::config::{update_config, Config, WindowFrame};
use crate::views::library_window::LibraryWindow;
use crate::views::settings::{SettingsTarget, SettingsWindow};

#[derive(Default)]
pub struct Windows {
    pub(crate) library: Option<WindowHandle<Root>>,
    pub(crate) settings: Option<WindowHandle<Root>>,
    /// Pending debounced frame saves, one per window (replacing one cancels it).
    frame_saves: HashMap<Singleton, Task<()>>,
}

impl Global for Windows {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Singleton {
    Library,
    Settings,
}

/// `Library`, or `Library (Dev)` on a dev build.
fn library_title() -> &'static str {
    match crate::config::channel() {
        crate::config::Channel::Stable => "Library",
        crate::config::Channel::Dev => "Library (Dev)",
    }
}

impl Singleton {
    /// Title, default size and minimum size. The minimum has to leave the composer panel
    /// (sidebar 230 + composer 440) room for a few feed columns beside it.
    fn spec(self) -> (&'static str, Size<Pixels>, Size<Pixels>) {
        match self {
            // The dev build says so in its title: with two installs running, Mission Control and
            // the Window menu are the only things that tell them apart.
            Singleton::Library => (library_title(), size(px(1100.), px(760.)), size(px(900.), px(600.))),
            // Nav pane + enough content width that setting rows don't wrap (Zed's floor is 626).
            Singleton::Settings => ("Settings", size(px(820.), px(600.)), size(px(680.), px(440.))),
        }
    }

    fn saved(self, config: &Config) -> Option<&WindowFrame> {
        match self {
            Singleton::Library => config.library_frame.as_ref(),
            Singleton::Settings => config.settings_frame.as_ref(),
        }
    }

    fn set_saved(self, config: &mut Config, frame: WindowFrame) {
        match self {
            Singleton::Library => config.library_frame = Some(frame),
            Singleton::Settings => config.settings_frame = Some(frame),
        }
    }
}

/// At least this much of a restored window must land on its display, else it is re-centred
/// (a monitor that was unplugged, or a frame saved off-screen).
const MIN_VISIBLE: (f32, f32) = (120., 60.);
/// Moves and resizes come in bursts; the frame is written this long after the last one.
const FRAME_SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Clamp a saved frame to `display` (never below `min`) and re-centre it when too little of it
/// would be visible.
pub(crate) fn validated_frame(saved: &WindowFrame, display: Bounds<Pixels>, min: Size<Pixels>) -> Bounds<Pixels> {
    let size = size(px(saved.width).max(min.width).min(display.size.width), px(saved.height).max(min.height).min(display.size.height));
    let bounds = Bounds { origin: point(px(saved.x), px(saved.y)), size };
    let visible = bounds.intersect(&display).size;
    if visible.width < px(MIN_VISIBLE.0) || visible.height < px(MIN_VISIBLE.1) {
        Bounds::centered_at(display.center(), size)
    } else {
        bounds
    }
}

/// The saved frame validated against the display it was saved on (falling back to the primary
/// one), or the centred default.
fn restored_bounds(kind: Singleton, cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let (_, default, min) = kind.spec();
    let centred = || (WindowBounds::Windowed(Bounds::centered(None, default, cx)), None);
    let Some(frame) = kind.saved(cx.global::<Config>()).cloned() else { return centred() };
    let display = frame
        .display
        .as_deref()
        .and_then(|uuid| cx.displays().into_iter().find(|d| d.uuid().ok().map(|u| u.to_string()).as_deref() == Some(uuid)))
        .or_else(|| cx.primary_display());
    let Some(display) = display else { return centred() };
    let bounds = validated_frame(&frame, display.bounds(), min);
    let window_bounds = if frame.maximized { WindowBounds::Maximized(bounds) } else { WindowBounds::Windowed(bounds) };
    (window_bounds, Some(display.id()))
}

fn options(kind: Singleton, cx: &App) -> WindowOptions {
    let (title, _, min) = kind.spec();
    let (window_bounds, display_id) = restored_bounds(kind, cx);
    let title = Some(SharedString::from(title));
    match kind {
        // Like Zed's main window: a transparent native title bar with the traffic lights at (9, 9)
        // and `LibraryWindow` drawing its own 34px `TitleBar` row underneath on every platform, so
        // the bar follows the app theme instead of the system appearance and carries our controls
        // (window controls too, off macOS). `TitleBar::window_options` hands it the dragging.
        Singleton::Library => WindowOptions {
            window_bounds: Some(window_bounds),
            display_id,
            titlebar: Some(TitlebarOptions { title, ..TitleBar::title_bar_options() }),
            window_min_size: Some(min),
            // Wayland reports this as the toplevel's app id, which is how the desktop matches the
            // window to the installed `.desktop` file (`StartupWMClass`) for its icon and taskbar
            // grouping. `cx.set_app_identity` only covers notifications, not the window.
            app_id: Some(config::bundle_id().to_string()),
            ..TitleBar::window_options()
        },
        // Like Zed's settings window: no title bar, the nav pane runs up behind the traffic lights
        // (`SettingsWindow` pads for them on macOS and draws a `TitleBar` row with window controls
        // elsewhere, which needs `TitleBar::window_options`' drag ownership).
        Singleton::Settings => WindowOptions {
            window_bounds: Some(window_bounds),
            display_id,
            titlebar: Some(TitlebarOptions { title, appears_transparent: true, traffic_light_position: Some(point(px(12.), px(12.))) }),
            window_min_size: Some(min),
            app_id: Some(config::bundle_id().to_string()),
            ..if cfg!(target_os = "macos") { WindowOptions::default() } else { TitleBar::window_options() }
        },
    }
}

/// The frame to persist for a window at `window_bounds` whose content size is `viewport`, or `None`
/// while it is fullscreen: the pre-fullscreen frame was saved when the window last moved, and
/// meanwhile `viewport` is the whole display (pairing it with the restore origin would bring the
/// window back display-sized and hanging off-screen).
///
/// The size is the content size (`viewport_size`), not `window_bounds().size`: on macOS the latter is
/// the NSWindow frame including the title bar, while `WindowOptions::window_bounds` is applied as the
/// content rect, so saving the frame height would grow the window by one title bar per launch.
pub(crate) fn frame_to_save(window_bounds: WindowBounds, viewport: Size<Pixels>, display: Option<String>) -> Option<WindowFrame> {
    let (bounds, size, maximized) = match window_bounds {
        WindowBounds::Windowed(b) => (b, viewport, false),
        WindowBounds::Maximized(b) => (b, b.size, true),
        WindowBounds::Fullscreen(_) => return None,
    };
    Some(WindowFrame { x: f32::from(bounds.origin.x), y: f32::from(bounds.origin.y), width: f32::from(size.width), height: f32::from(size.height), maximized, display })
}

/// Remember `kind`'s current frame (no-op when unchanged or fullscreen, see [`frame_to_save`]).
pub fn save_frame(kind: Singleton, window: &Window, cx: &mut App) {
    let display = window.display(cx).and_then(|d| d.uuid().ok()).map(|u| u.to_string());
    let Some(frame) = frame_to_save(window.window_bounds(), window.viewport_size(), display) else { return };
    if kind.saved(cx.global::<Config>()) != Some(&frame) {
        update_config(cx, |c| kind.set_saved(c, frame));
    }
}

/// Persist `kind`'s frame after moves / resizes (debounced) and when the window closes. Root views
/// call this from their constructor.
pub fn track_frame<V: 'static>(kind: Singleton, window: &mut Window, cx: &mut Context<V>) {
    window.on_window_should_close(cx, move |window, cx| {
        save_frame(kind, window, cx);
        true
    });
    cx.observe_window_bounds(window, move |_, window, cx| {
        let task = window.spawn(cx, async move |cx| {
            cx.background_executor().timer(FRAME_SAVE_DEBOUNCE).await;
            cx.update(|window, cx| save_frame(kind, window, cx)).ok();
        });
        cx.global_mut::<Windows>().frame_saves.insert(kind, task);
    })
    .detach();
}

/// ⌘Q bypasses `on_window_should_close`, so save the frame before quitting.
pub fn save_all_frames(cx: &mut App) {
    let Windows { library, settings, .. } = cx.global::<Windows>();
    for (kind, handle) in [(Singleton::Library, *library), (Singleton::Settings, *settings)] {
        if let Some(handle) = handle {
            handle.update(cx, |_, window, cx| save_frame(kind, window, cx)).ok();
        }
    }
}

pub fn open_library(cx: &mut App) {
    if let Some(handle) = cx.global::<Windows>().library {
        // Deferred: ⌘1 arrives while the Library window itself is dispatching it, and a window can't
        // be updated from within its own dispatch — activating directly would fail and open a
        // second Library window. A handle whose window is gone falls through to opening one.
        cx.defer(move |cx| {
            if handle.update(cx, |_, window, _| window.activate_window()).is_err() {
                cx.global_mut::<Windows>().library = None;
                open_library(cx);
            }
        });
        return;
    }
    let opts = options(Singleton::Library, cx);
    let handle = cx
        .open_window(opts, |window, cx| {
            let view = cx.new(|cx| LibraryWindow::new(window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("open library window");
    cx.global_mut::<Windows>().library = Some(handle);
}

/// Bring the Settings window forward on `target`'s page, opening it if needed. Like Zed's
/// `open_settings_editor`: one window, re-targeted when it already exists. Deferred because the
/// action can arrive while a window is dispatching it (⌘, inside Settings itself) and a window can't
/// be updated from within its own dispatch.
pub fn open_settings(target: SettingsTarget, cx: &mut App) {
    cx.defer(move |cx| {
        if let Some(handle) = cx.global::<Windows>().settings {
            let shown = handle.update(cx, |root, window, cx| {
                window.activate_window();
                if let Ok(view) = root.view().clone().downcast::<SettingsWindow>() {
                    view.update(cx, |settings, cx| settings.show(target.clone(), window, cx));
                }
            });
            if shown.is_ok() {
                return;
            }
        }
        let opts = options(Singleton::Settings, cx);
        let opened = cx.open_window(opts, |window, cx| {
            let view = cx.new(|cx| SettingsWindow::new(target, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        });
        match opened {
            Ok(handle) => cx.global_mut::<Windows>().settings = Some(handle),
            Err(e) => tracing::warn!(target: "majik", "opening the settings window: {e:#}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env;
    use gpui::TestAppContext;

    fn frame(x: f32, y: f32, width: f32, height: f32) -> WindowFrame {
        WindowFrame { x, y, width, height, maximized: false, display: None }
    }

    fn display() -> Bounds<Pixels> {
        Bounds { origin: point(px(0.), px(0.)), size: size(px(1920.), px(1080.)) }
    }

    #[gpui::test]
    fn both_windows_carry_the_channel_app_id(cx: &mut TestAppContext) {
        env(cx, 0, "Mock");
        cx.update(|cx| {
            // Wayland reports this as the toplevel app id; the shipped `.desktop` file matches on it.
            // The channel is part of the bundle id, so a dev window never claims the shipped app's
            // launcher entry.
            for kind in [Singleton::Library, Singleton::Settings] {
                assert_eq!(options(kind, cx).app_id.as_deref(), Some(config::bundle_id()), "{kind:?}");
            }
        });
    }

    #[test]
    fn validated_frame_keeps_onscreen_frames() {
        let min = size(px(750.), px(600.));
        let b = validated_frame(&frame(100., 50., 900., 700.), display(), min);
        assert_eq!(b, Bounds { origin: point(px(100.), px(50.)), size: size(px(900.), px(700.)) });
        // Partly off the edge is fine as long as enough is visible.
        let b = validated_frame(&frame(1700., 50., 900., 700.), display(), min);
        assert_eq!(b.origin.x, px(1700.));
    }

    #[test]
    fn validated_frame_recenters_offscreen_frames() {
        let min = size(px(750.), px(600.));
        let b = validated_frame(&frame(5000., 5000., 900., 700.), display(), min);
        assert_eq!(b.size, size(px(900.), px(700.)));
        assert_eq!(b.center(), display().center());
        // Only a sliver visible counts as off-screen too.
        let b = validated_frame(&frame(1850., 50., 900., 700.), display(), min);
        assert_eq!(b.center(), display().center());
    }

    #[test]
    fn validated_frame_clamps_to_min_and_display() {
        let min = size(px(750.), px(600.));
        let b = validated_frame(&frame(0., 0., 100., 100.), display(), min);
        assert_eq!(b.size, min);
        let b = validated_frame(&frame(0., 0., 5000., 5000.), display(), min);
        assert_eq!(b.size, display().size);
    }

    #[test]
    fn frame_to_save_uses_the_content_size_when_windowed() {
        // The NSWindow frame is one title bar taller than the content; the content size is saved.
        let bounds = Bounds { origin: point(px(100.), px(50.)), size: size(px(900.), px(728.)) };
        let frame = frame_to_save(WindowBounds::Windowed(bounds), size(px(900.), px(700.)), None).expect("windowed frames are saved");
        assert_eq!((frame.x, frame.y, frame.width, frame.height, frame.maximized), (100., 50., 900., 700., false));
        let frame = frame_to_save(WindowBounds::Maximized(bounds), size(px(900.), px(700.)), None).expect("maximized frames are saved");
        assert!(frame.maximized);
    }

    #[test]
    fn frame_to_save_skips_fullscreen() {
        let restore = Bounds { origin: point(px(100.), px(50.)), size: size(px(900.), px(700.)) };
        let viewport = size(px(2560.), px(1440.));
        assert!(frame_to_save(WindowBounds::Fullscreen(restore), viewport, None).is_none(), "the display-sized viewport must not be saved at the restore origin");
    }

    #[gpui::test]
    fn both_windows_have_transparent_title_bars(cx: &mut TestAppContext) {
        env(cx, 0, "Mock");
        cx.update(|cx| {
            let settings = options(Singleton::Settings, cx).titlebar.expect("settings title bar options");
            assert!(settings.appears_transparent, "the nav pane runs up behind the traffic lights");
            assert_eq!(settings.traffic_light_position, Some(point(px(12.), px(12.))));
            assert_eq!(settings.title.as_deref(), Some("Settings"), "still named in Mission Control / the Window menu");
            let library = options(Singleton::Library, cx);
            let titlebar = library.titlebar.expect("library title bar options");
            assert!(titlebar.appears_transparent, "the window draws its own title bar row");
            assert_eq!(titlebar.traffic_light_position, Some(point(px(9.), px(9.))), "centred in the 34px row, as in Zed");
            assert_eq!(titlebar.title.as_deref(), Some(library_title()));
            assert!(library.app_owns_titlebar_drag, "the drawn title bar moves the window itself");
        });
    }

    #[gpui::test]
    fn open_library_restores_saved_frame(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| {
            cx.global_mut::<Config>().library_frame = Some(frame(100., 50., 900., 700.));
            open_library(cx);
            let handle = cx.global::<Windows>().library.expect("opened");
            let bounds = handle.update(cx, |_, window, _| window.bounds()).unwrap();
            assert_eq!(bounds, Bounds { origin: point(px(100.), px(50.)), size: size(px(900.), px(700.)) });
        });
    }

    #[gpui::test]
    fn library_window_enforces_the_900_by_600_minimum(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| {
            assert_eq!(options(Singleton::Library, cx).window_min_size, Some(size(px(900.), px(600.))), "the OS is told the minimum");
            // A saved frame below the minimum (an older build, a hand-edited config) grows back to it.
            cx.global_mut::<Config>().library_frame = Some(frame(100., 50., 300., 200.));
            open_library(cx);
            let handle = cx.global::<Windows>().library.expect("opened");
            let bounds = handle.update(cx, |_, window, _| window.bounds()).unwrap();
            assert_eq!(bounds.size, size(px(900.), px(600.)));
        });
    }

    #[gpui::test]
    fn open_library_recenters_offscreen_frame(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| {
            cx.global_mut::<Config>().library_frame = Some(frame(9000., 9000., 1000., 800.));
            open_library(cx);
            let handle = cx.global::<Windows>().library.expect("opened");
            let bounds = handle.update(cx, |_, window, _| window.bounds()).unwrap();
            assert_eq!(bounds.size, size(px(1000.), px(800.)));
            assert!(bounds.origin.x < px(9000.), "re-centred on the display");
        });
    }
}
