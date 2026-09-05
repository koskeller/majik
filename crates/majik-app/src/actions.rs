//! Actions, key bindings and the native menu bar.

use gpui::{actions, Action, App, KeyBinding, Keystroke, Menu, MenuItem, OsAction, SystemMenuType, Window};
use gpui_component::input::{Cut as CutText, Paste as PasteText, Redo, Undo};
use gpui_component::kbd::Kbd;

use crate::config::GridLayout;
use crate::views::settings::{SettingsPage, SettingsTarget};

actions!(
    majik,
    [
        About,
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        CloseWindow,
        NewGeneration,
        NewAlbum,
        ImportFiles,
        ShowLibrary,
        ShowFavorites,
        ShowAssets,
        OpenSelection,
        Generate,
        ImprovePrompt,
        ClearPrompt,
        SaveMedia,
        CopyMedia,
        SelectAll,
        ClearSelection,
        DeleteMedia,
        ToggleFavorite,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        LayoutSquare,
        LayoutAspectRatio,
        LayoutMasonry,
        ShowInfo,
        Recreate,
        Retry,
        NextItem,
        PrevItem,
        Back,
        OpenSettings,
        TogglePlayback,
        PasteImage,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        ToggleComposer,
        ToggleSidebar,
        FocusFeed,
        ViewTelemetryLog,
        ShowLogs,
        CheckForUpdates,
        RestartToUpdate,
    ]
);

/// Keys are written with gpui's `secondary` modifier (⌘ on macOS, Ctrl on Windows and Linux),
/// never `cmd`, which is the Windows / Super key off macOS.
pub const NEW_GENERATION_KEYS: &str = "secondary-n";

/// The platform's spelling of a binding for help text (`⌘N` on macOS, `Ctrl+N` elsewhere), the
/// same formatting the Shortcuts page and tooltips use, so text never hard-codes a glyph.
pub fn keystroke_label(keys: &str) -> String {
    match Keystroke::parse(keys) {
        Ok(keystroke) => Kbd::format(&keystroke),
        Err(e) => {
            tracing::warn!(target: "majik", "unparseable keystroke {keys:?}: {e:#}");
            keys.to_string()
        }
    }
}

/// One row of the Settings → Shortcuts page: a label plus the bindings that trigger it (the same
/// keys in several contexts, or several keys for one action). [`init`] installs exactly these, so
/// the page can never disagree with the keymap.
pub struct Shortcut {
    pub group: &'static str,
    pub label: &'static str,
    pub bindings: Vec<KeyBinding>,
}

fn shortcut<A: Action + Clone>(group: &'static str, label: &'static str, keys: &[&str], action: A, contexts: &[Option<&str>]) -> Shortcut {
    let action = &action;
    let bindings = keys.iter().flat_map(|keys| contexts.iter().map(move |context| KeyBinding::new(keys, action.clone(), *context))).collect();
    Shortcut { group, label, bindings }
}

/// Every key binding, grouped the way the Shortcuts page shows them. Bindings resolve along the
/// focus path, so feed shortcuts don't fire while the prompt is focused.
pub fn shortcuts() -> Vec<Shortcut> {
    fn app<A: Action + Clone>(label: &'static str, keys: &[&str], action: A) -> Shortcut {
        shortcut("Application", label, keys, action, &[None])
    }
    fn feed<A: Action + Clone>(label: &'static str, keys: &[&str], action: A) -> Shortcut {
        shortcut("Feed", label, keys, action, &[Some("Feed")])
    }
    fn media<A: Action + Clone>(label: &'static str, keys: &[&str], action: A) -> Shortcut {
        shortcut("Feed & Detail", label, keys, action, &[Some("Feed"), Some("Detail")])
    }
    fn detail<A: Action + Clone>(label: &'static str, keys: &[&str], action: A) -> Shortcut {
        shortcut("Detail", label, keys, action, &[Some("Detail")])
    }
    fn composer<A: Action + Clone>(label: &'static str, keys: &[&str], action: A) -> Shortcut {
        shortcut("Composer", label, keys, action, &[Some("Compose")])
    }
    fn settings<A: Action + Clone>(label: &'static str, keys: &[&str], action: A, context: &str) -> Shortcut {
        shortcut("Settings", label, keys, action, &[Some(context)])
    }
    fn library_window<A: Action + Clone>(label: &'static str, keys: &[&str], action: A) -> Shortcut {
        shortcut("Library window", label, keys, action, &[Some("Library")])
    }
    vec![
        app("Quit", &["secondary-q"], Quit),
        app("Close Window", &["secondary-w"], CloseWindow),
        app("New Generation", &[NEW_GENERATION_KEYS], NewGeneration),
        // Opens the Library window when it is closed, so unlike Favorites and Assets below it is
        // bound everywhere rather than inside the window.
        app("Library", &["secondary-1"], ShowLibrary),
        app("Settings", &["secondary-,"], OpenSettings),
        library_window("New Album", &["secondary-shift-n"], NewAlbum),
        library_window("Favorites", &["secondary-2"], ShowFavorites),
        library_window("Assets", &["secondary-3"], ShowAssets),
        // ⇧⌘I is Import in Photos and Lightroom.
        library_window("Import…", &["secondary-shift-i"], ImportFiles),
        // ⌘B is the sidebar in VS Code, Cursor and Zed. Not Finder's ⌘⌥S: Windows reports AltGr as
        // Ctrl+Alt, so on a Polish or Czech layout Ctrl+Alt+S is the key that types "ś".
        library_window("Show / Hide Sidebar", &["secondary-b"], ToggleSidebar),
        // ⌘L is the prompt pane in Cursor and Windsurf, and the browsers' "put me in the text
        // field". ⌘N only ever opens the composer; this is the key that closes it.
        library_window("Show / Hide Composer", &["secondary-l"], ToggleComposer),
        feed("Open", &["secondary-o", "enter"], OpenSelection),
        feed("Select All", &["secondary-a"], SelectAll),
        feed("Clear Selection", &["escape"], ClearSelection),
        feed("Select Previous", &["left"], SelectLeft),
        feed("Select Next", &["right"], SelectRight),
        feed("Select Above", &["up"], SelectUp),
        feed("Select Below", &["down"], SelectDown),
        media("Save…", &["secondary-s"], SaveMedia),
        media("Copy", &["secondary-c"], CopyMedia),
        // "." is Photos' favorite key; neither context holds a text input, so the bare key is safe.
        media("Favorite", &["secondary-shift-f", "."], ToggleFavorite),
        media("Recreate", &["secondary-r"], Recreate),
        media("Zoom In", &["secondary-="], ZoomIn),
        media("Zoom Out", &["secondary--"], ZoomOut),
        media("Actual Size", &["secondary-0"], ResetZoom),
        media("Delete", &["delete", "backspace"], DeleteMedia),
        media("Show Info", &["secondary-i"], ShowInfo),
        detail("Previous Item", &["left"], PrevItem),
        detail("Next Item", &["right"], NextItem),
        detail("Back", &["escape"], Back),
        detail("Play / Pause", &["space"], TogglePlayback),
        // Bound in the window as well as the panel: while the composer is open these work from
        // the feed and the detail too (`LibraryWindow` hands them to the panel), so ⌘⏎ generates
        // without clicking back into the prompt.
        shortcut("Composer", "Generate", &["secondary-enter"], Generate, &[Some("Compose"), Some("Library")]),
        // P for prompt: no peer app has a convention for this. Not ⌘E, which gpui-component's
        // text input binds as Ctrl+E off macOS and would swallow while typing.
        shortcut("Composer", "Improve Prompt", &["secondary-shift-p"], ImprovePrompt, &[Some("Compose"), Some("Library")]),
        shortcut("Composer", "Paste Image", &["secondary-shift-v"], PasteImage, &[Some("Compose"), Some("Library")]),
        // The prompt's own Escape handler propagates, so `FocusFeed` fires right after it.
        composer("Focus Feed", &["escape"], FocusFeed),
        settings("Close Settings", &["escape"], CloseWindow, "Settings"),
        settings("Previous Page", &["up"], SelectUp, "SettingsNav"),
        settings("Next Page", &["down"], SelectDown, "SettingsNav"),
    ]
    .into_iter()
    // Hiding an app is a macOS idea: gpui's Linux backend logs and ignores it, and Windows has no
    // equivalent, so the keys — and the menu items below — only exist where they do something.
    // ⌘M is a macOS convention too; Ctrl+M means nothing on Windows or Linux, where the window
    // manager already offers minimize, so the Window menu item keeps working without a key there.
    .chain(
        cfg!(target_os = "macos")
            .then(|| [app("Hide", &["secondary-h"], Hide), app("Hide Others", &["secondary-alt-h"], HideOthers), app("Minimize", &["secondary-m"], Minimize)])
            .into_iter()
            .flatten(),
    )
    .collect()
}

/// What the Library window can act on right now, so the menu can grey out what would do nothing.
///
/// macOS greys items itself: AppKit asks `validateMenuItem:` and gpui answers with
/// `is_action_available`, which is true exactly when the action reaches a handler on the focus
/// path. The menu bar we draw on Windows and Linux has no such hook — it only reads the `disabled`
/// flag baked into each item — so [`refresh`] recomputes these flags whenever the window's state
/// changes. The conditions below therefore mirror where `LibraryWindow` and its views install
/// their handlers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MenuState {
    /// A Library window is showing the library (not onboarding), so its views can act at all.
    pub library: bool,
    /// The composer panel is open, so the prompt and its actions exist.
    pub composer_open: bool,
    /// The detail is covering the window; it always shows exactly one item.
    pub detail_open: bool,
    /// At least one cell is selected in the feed.
    pub selection: bool,
    /// The feed's layout, so the View menu ticks the current one.
    pub grid_layout: GridLayout,
}

impl MenuState {
    /// The feed is the visible surface: on screen and not covered by the detail.
    fn feed(&self) -> bool {
        self.library && !self.detail_open
    }

    /// There is media to act on: the detail's item, or the feed's selection.
    fn media(&self) -> bool {
        self.detail_open || (self.feed() && self.selection)
    }
}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| {
        crate::windows::save_all_frames(cx);
        cx.quit()
    });
    cx.on_action(|_: &ShowLibrary, cx| crate::windows::open_library(cx));
    cx.on_action(|_: &About, cx| crate::windows::open_settings(SettingsTarget { page: SettingsPage::About, ..Default::default() }, cx));
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    // Whichever window is in front, as the Window menu means it.
    cx.on_action(|_: &Minimize, cx| active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| active_window(cx, |window| window.zoom_window()));
    // App-level so it works from every window (and from the menu with none focused); the Settings
    // window itself just comes forward.
    cx.on_action(|_: &OpenSettings, cx| crate::windows::open_settings(Default::default(), cx));
    // The Help menu: what telemetry sent (Zed's Help → View Telemetry Log) and where the logs are.
    cx.on_action(|_: &ViewTelemetryLog, cx| crate::windows::open_settings(SettingsTarget { page: SettingsPage::Telemetry, ..Default::default() }, cx));
    cx.on_action(|_: &ShowLogs, cx| crate::views::settings::reveal_logs(cx));
    // Both app-level: the check opens Settings → About, where its result shows, and the restart
    // is offered from the sidebar and that page once an update is installed.
    cx.on_action(|_: &CheckForUpdates, cx| crate::auto_update::check_for_updates(cx));
    cx.on_action(|_: &RestartToUpdate, cx| crate::auto_update::restart_to_update(cx));
    cx.bind_keys(shortcuts().into_iter().flat_map(|shortcut| shortcut.bindings).collect::<Vec<_>>());

    // gpui only *stores* the menus off macOS; the Library window draws them from `GlobalState`.
    // `Menu::owned` consumes the menu, so the list is built once for each store.
    let state = MenuState::default();
    gpui_component::global_state::GlobalState::global_mut(cx).set_app_menus(menus(state).into_iter().map(Menu::owned).collect());
    cx.set_menus(menus(state));
}

fn active_window(cx: &mut App, act: impl FnOnce(&Window)) {
    let Some(window) = cx.active_window() else { return };
    if let Err(e) = window.update(cx, |_, window, _| act(window)) {
        tracing::warn!(target: "majik", "the front window is gone: {e:#}");
    }
}

/// Re-grey the menu bar we draw ourselves for `state`. The native macOS bar isn't rebuilt: it asks
/// about each item as it opens the menu and is always current, and rebuilding `NSMenu` on every
/// selection change would be churn for nothing.
pub fn refresh(state: MenuState, cx: &mut App) {
    gpui_component::global_state::GlobalState::global_mut(cx).set_app_menus(menus(state).into_iter().map(Menu::owned).collect());
}

/// Rebuild the native menu bar too. Unlike greying, a tick (the View menu's layout) is baked into
/// the `NSMenuItem` when the menus are set, so it goes stale unless they are set again; the
/// window calls this only when the ticked item changed.
pub fn rebuild_native(state: MenuState, cx: &mut App) {
    cx.set_menus(menus(state));
}

/// The application menus, mirrored in two places: `cx.set_menus` drives the native macOS menu
/// bar, and `GlobalState::set_app_menus` feeds the [`AppMenuBar`](gpui_component::menu::AppMenuBar)
/// the Library window draws itself on Windows and Linux, where gpui only stores them.
pub fn menus(state: MenuState) -> Vec<Menu> {
    // Text editing is the composer prompt's; the settings inputs are in their own window.
    let text = state.composer_open;
    let (library, feed, media) = (state.library, state.feed(), state.media());
    // The variants are built by hand rather than through `MenuItem::action`, which takes an owned
    // `impl Action` and so can't be handed one action per call site from a shared helper.
    let entry = |name: &str, action: &dyn Action, os_action: Option<OsAction>, enabled: bool, checked: bool| MenuItem::Action {
        name: name.to_string().into(),
        action: action.boxed_clone(),
        os_action,
        checked,
        disabled: !enabled,
    };
    let item = move |name: &str, action: &dyn Action, enabled: bool| entry(name, action, None, enabled, false);
    let os_item = move |name: &str, action: &dyn Action, os: OsAction, enabled: bool| entry(name, action, Some(os), enabled, false);
    // The View menu's layouts: a group of choices, the current one ticked.
    let layout = move |layout: GridLayout, action: &dyn Action| entry(layout.label(), action, None, feed, state.grid_layout == layout);
    vec![
        Menu {
            // The channel's name, so a dev build says so wherever we draw the menu ourselves. On
            // macOS the application menu's own title is AppKit's (the bundle / executable name).
            name: crate::config::app_name().into(),
            disabled: false,
            items: app_menu_items(),
        },
        Menu {
            name: "File".into(),
            disabled: false,
            items: vec![
                item("New Generation", &NewGeneration, library),
                item("New Album…", &NewAlbum, library),
                MenuItem::action("Close Window", CloseWindow),
                MenuItem::separator(),
                item("Open", &OpenSelection, feed && state.selection),
                item("Import…", &ImportFiles, library),
                item("Save…", &SaveMedia, media),
            ],
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: vec![
                // The text actions are gpui-component's own, the ones its inputs already bind.
                os_item("Undo", &Undo, OsAction::Undo, text),
                os_item("Redo", &Redo, OsAction::Redo, text),
                MenuItem::separator(),
                os_item("Cut", &CutText, OsAction::Cut, text),
                os_item("Copy", &CopyMedia, OsAction::Copy, media),
                os_item("Paste", &PasteText, OsAction::Paste, text),
                item("Paste Image", &PasteImage, text),
                MenuItem::separator(),
                os_item("Select All", &SelectAll, OsAction::SelectAll, feed),
                item("Clear Selection", &ClearSelection, feed && state.selection),
                MenuItem::separator(),
                item("Improve Prompt", &ImprovePrompt, text),
                item("Clear Prompt", &ClearPrompt, text),
            ],
        },
        Menu {
            name: "View".into(),
            disabled: false,
            items: vec![
                item("Zoom In", &ZoomIn, library),
                item("Zoom Out", &ZoomOut, library),
                item("Actual Size", &ResetZoom, library),
                MenuItem::separator(),
                layout(GridLayout::Square, &LayoutSquare),
                layout(GridLayout::AspectRatio, &LayoutAspectRatio),
                layout(GridLayout::Masonry, &LayoutMasonry),
                MenuItem::separator(),
                item("Show Info", &ShowInfo, state.detail_open),
                item("Play / Pause", &TogglePlayback, state.detail_open),
                item("Previous Item", &PrevItem, state.detail_open),
                item("Next Item", &NextItem, state.detail_open),
                item("Back", &Back, state.detail_open),
                MenuItem::separator(),
                item("Show / Hide Sidebar", &ToggleSidebar, library),
                item("Show / Hide Composer", &ToggleComposer, library),
                MenuItem::separator(),
                MenuItem::action("Library", ShowLibrary),
                item("Favorites", &ShowFavorites, library),
                item("Assets", &ShowAssets, library),
            ],
        },
        Menu {
            name: "Media".into(),
            disabled: false,
            items: vec![
                item("Generate", &Generate, state.composer_open),
                item("Recreate", &Recreate, media),
                item("Favorite", &ToggleFavorite, media),
                MenuItem::separator(),
                item("Retry", &Retry, media),
                MenuItem::separator(),
                item("Delete", &DeleteMedia, media),
            ],
        },
        Menu {
            name: "Window".into(),
            disabled: false,
            items: vec![MenuItem::action("Minimize", Minimize), MenuItem::action("Zoom", Zoom)],
        },
        Menu {
            name: "Help".into(),
            disabled: false,
            items: vec![MenuItem::action("View Telemetry Log", ViewTelemetryLog), MenuItem::action("Show Logs", ShowLogs)],
        },
    ]
}

/// The application menu. macOS puts the app's own commands here and expects Services and the hide
/// commands among them; Windows and Linux have neither, so off macOS it is About, Settings and Quit.
fn app_menu_items() -> Vec<MenuItem> {
    let name = crate::config::app_name();
    let mut items = vec![MenuItem::action(format!("About {name}"), About), MenuItem::action("Check for Updates…", CheckForUpdates), MenuItem::separator(), MenuItem::action("Settings…", OpenSettings)];
    if cfg!(target_os = "macos") {
        items.push(MenuItem::separator());
        items.push(MenuItem::os_submenu("Services", SystemMenuType::Services));
        items.push(MenuItem::separator());
        items.push(MenuItem::action(format!("Hide {name}"), Hide));
        items.push(MenuItem::action("Hide Others", HideOthers));
        items.push(MenuItem::action("Show All", ShowAll));
    }
    items.push(MenuItem::separator());
    items.push(MenuItem::action(format!("Quit {name}"), Quit));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AsKeystroke as _, OwnedMenu, OwnedMenuItem, TestAppContext};

    /// Whether the item called `name` is greyed out, and that there is exactly one such item.
    fn greyed(menus: &[Menu], name: &str) -> bool {
        let mut found: Vec<bool> = Vec::new();
        for menu in menus {
            for item in &menu.items {
                if let MenuItem::Action { name: item_name, disabled, .. } = item {
                    if item_name.as_ref() == name {
                        found.push(*disabled);
                    }
                }
            }
        }
        assert_eq!(found.len(), 1, "exactly one menu item is called {name:?}");
        found[0]
    }

    /// With two installs side by side, the menu is where you check which one you're driving.
    #[test]
    fn the_app_menu_carries_the_channel_name() {
        let menus = menus(MenuState::default());
        let app_menu = &menus[0];
        assert_eq!(app_menu.name.as_ref(), crate::config::app_name());
        let quit = app_menu.items.last().expect("a Quit item");
        let MenuItem::Action { name, .. } = quit else { panic!("Quit is an action item") };
        assert_eq!(name.as_ref(), format!("Quit {}", crate::config::app_name()));
    }

    #[test]
    fn keystroke_label_spells_the_secondary_modifier_for_the_platform() {
        let expected = if cfg!(target_os = "macos") { "⌘N" } else { "Ctrl+N" };
        assert_eq!(keystroke_label(NEW_GENERATION_KEYS), expected);
    }

    /// `cmd-` parses to the same modifier as `secondary-` on macOS, so the keymap source is checked
    /// on every platform and the parsed bindings only where the two differ.
    #[test]
    fn no_binding_uses_the_cmd_modifier() {
        // Every source file, not just this one: a test that simulates `cmd-z` presses the Super key
        // off macOS, where the binding it means is `ctrl-z`, and only fails on the other platforms.
        // Built at runtime so this test's own lines don't match.
        let needle = format!("\"{}-", "cmd");
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("reading the crate's sources").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).expect("reading a source file");
                    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    offenders.extend(
                        source
                            .lines()
                            .filter(|line| line.contains(&needle) && !line.trim_start().starts_with("//") && !line.contains("offenders"))
                            .map(|line| format!("{name}: {}", line.trim())),
                    );
                }
            }
        }
        assert!(offenders.is_empty(), "keystrokes must use `secondary-`, not `cmd-` (the Windows / Super key off macOS): {offenders:#?}");
        if !cfg!(target_os = "macos") {
            for shortcut in shortcuts() {
                for binding in &shortcut.bindings {
                    for key in binding.keystrokes() {
                        let keystroke = key.as_keystroke();
                        assert!(!keystroke.modifiers.platform, "{}: {keystroke:?} is bound to the Windows / Super key", shortcut.label);
                    }
                }
            }
        }
    }

    /// The drawn menu bar (Windows / Linux) reads `GlobalState`, the native one (macOS) reads
    /// gpui's own store. `init` fills both from the same list, so the two can never disagree.
    /// Two actions on the same keystroke in the same context would make one of them unreachable,
    /// and which one wins is an accident of table order.
    #[test]
    fn no_keystroke_is_bound_twice_in_one_context() {
        let mut seen: Vec<(String, String, &str)> = Vec::new();
        for shortcut in shortcuts() {
            for binding in &shortcut.bindings {
                let keys = binding.keystrokes().iter().map(|k| k.as_keystroke().unparse()).collect::<Vec<_>>().join(" ");
                let context = binding.predicate().map(|p| p.to_string()).unwrap_or_default();
                if let Some((_, _, other)) = seen.iter().find(|(k, c, _)| *k == keys && *c == context) {
                    panic!("{keys:?} is bound to both {other:?} and {:?} in context {context:?}", shortcut.label);
                }
                seen.push((keys, context, shortcut.label));
            }
        }
    }

    /// Everything a menu item can do is greyed out when the window can't do it, so the bar we draw
    /// ourselves says the same thing macOS says by asking `is_action_available` (which the Library
    /// window's `every_menu_action_reaches_a_handler` checks against the real dispatch tree).
    #[test]
    fn menus_grey_out_what_the_window_cannot_do() {
        // Onboarding, or no Library window at all: the library can't act on anything.
        let none = menus(MenuState::default());
        for name in ["New Generation", "New Album…", "Import…", "Generate", "Save…", "Show Info", "Favorites", "Show / Hide Sidebar", "Masonry"] {
            assert!(greyed(&none, name), "{name} is greyed with no library on screen");
        }
        let about = format!("About {}", crate::config::app_name());
        for menus in [&none, &menus(MenuState { library: true, ..Default::default() })] {
            for name in ["Settings…", "Check for Updates…", "Close Window", "Library", "Minimize", "Zoom", &about] {
                assert!(!greyed(menus, name), "{name} always works");
            }
        }

        // The feed, with nothing selected: navigation and the panels work, media doesn't.
        let feed = menus(MenuState { library: true, ..Default::default() });
        for name in ["New Generation", "New Album…", "Import…", "Favorites", "Assets", "Square", "Aspect Ratio", "Masonry", "Select All", "Zoom In"] {
            assert!(!greyed(&feed, name), "{name} works on a feed");
        }
        for name in ["Open", "Save…", "Copy", "Delete", "Recreate", "Retry", "Clear Selection", "Show Info", "Play / Pause", "Generate", "Paste", "Improve Prompt"] {
            assert!(greyed(&feed, name), "{name} needs more than an empty feed");
        }

        // A selection makes the media items work; they act on the detail's item just as well.
        let selected = menus(MenuState { library: true, selection: true, ..Default::default() });
        let detail = menus(MenuState { library: true, detail_open: true, ..Default::default() });
        for name in ["Open", "Save…", "Copy", "Delete", "Recreate", "Retry", "Favorite"] {
            assert!(!greyed(&selected, name), "{name} acts on the selection");
        }
        for name in ["Save…", "Copy", "Delete", "Recreate", "Show Info", "Play / Pause", "Next Item", "Previous Item", "Back"] {
            assert!(!greyed(&detail, name), "{name} acts on the detail's item");
        }
        for name in ["Show Info", "Play / Pause", "Next Item", "Previous Item", "Back"] {
            assert!(greyed(&selected, name), "{name} needs a detail, not a selection");
        }
        for name in ["Open", "Select All", "Square", "Aspect Ratio", "Masonry"] {
            assert!(greyed(&detail, name), "{name} belongs to the feed, which the detail covers");
        }

        // The prompt only exists while the composer is open, and so do its actions.
        let composer = menus(MenuState { library: true, composer_open: true, ..Default::default() });
        for name in ["Generate", "Improve Prompt", "Clear Prompt", "Paste Image", "Undo", "Redo", "Cut", "Paste"] {
            assert!(greyed(&feed, name) && !greyed(&composer, name), "{name} needs the composer");
        }
    }

    #[gpui::test]
    fn init_feeds_the_native_and_the_drawn_menu_bar_the_same_menus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
            let drawn = gpui_component::global_state::GlobalState::global(cx).app_menus().to_vec();
            let names = |menu: &OwnedMenu| menu.items.iter().filter_map(|item| match item {
                OwnedMenuItem::Action { name, .. } => Some(name.to_string()),
                OwnedMenuItem::Separator => Some("-".into()),
                _ => None,
            }).collect::<Vec<_>>();
            let expected: Vec<OwnedMenu> = menus(MenuState::default()).into_iter().map(Menu::owned).collect();
            assert!(!expected.is_empty(), "there are menus to draw");
            assert_eq!(drawn.len(), expected.len(), "every menu is drawn off macOS");
            for (drawn, expected) in drawn.iter().zip(&expected) {
                assert_eq!(drawn.name, expected.name);
                assert_eq!(names(drawn), names(expected), "{} has the same items in the same order", expected.name);
            }
        });
    }

    /// The drawn bar has no way to ask whether an action would do anything (macOS does, and greys
    /// the item itself), so the flags it draws from are recomputed as the window's state changes.
    #[gpui::test]
    fn refresh_regreys_the_drawn_menu_bar(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
            let disabled = |cx: &App, name: &str| {
                gpui_component::global_state::GlobalState::global(cx)
                    .app_menus()
                    .iter()
                    .flat_map(|menu| menu.items.iter())
                    .find_map(|item| match item {
                        OwnedMenuItem::Action { name: item, disabled, .. } if item.as_str() == name => Some(*disabled),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("{name} is in the drawn menus"))
            };
            assert!(disabled(cx, "Generate"), "no window yet");
            refresh(MenuState { library: true, composer_open: true, ..Default::default() }, cx);
            assert!(!disabled(cx, "Generate"), "the composer is open");
            assert!(disabled(cx, "Save…"), "nothing is selected");
            refresh(MenuState { library: true, selection: true, ..Default::default() }, cx);
            assert!(!disabled(cx, "Save…"));
            assert!(disabled(cx, "Generate"), "the composer closed again");
        });
    }

    /// The View menu's layouts are a group of choices: exactly the current one is ticked, in
    /// both the native menus and the drawn bar.
    #[gpui::test]
    fn menus_check_exactly_the_current_grid_layout(cx: &mut TestAppContext) {
        fn checked(menus: &[Menu]) -> Vec<String> {
            menus.iter().flat_map(|menu| menu.items.iter()).filter_map(|item| match item {
                MenuItem::Action { name, checked: true, .. } => Some(name.to_string()),
                _ => None,
            }).collect()
        }
        for layout in GridLayout::ALL {
            let state = MenuState { library: true, grid_layout: layout, ..Default::default() };
            assert_eq!(checked(&menus(state)), [layout.label()], "{layout:?}");
        }
        assert_eq!(checked(&menus(MenuState::default())), ["Square"], "the default layout is ticked even with no window");
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
            refresh(MenuState { library: true, grid_layout: GridLayout::Masonry, ..Default::default() }, cx);
            let drawn: Vec<String> = gpui_component::global_state::GlobalState::global(cx)
                .app_menus()
                .iter()
                .flat_map(|menu| menu.items.iter())
                .filter_map(|item| match item {
                    OwnedMenuItem::Action { name, checked: true, .. } => Some(name.to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(drawn, ["Masonry"], "the drawn bar ticks what the state says");
        });
    }

    /// The tooltips built with `tooltip_with_action` show these bindings; make sure they resolve.
    #[gpui::test]
    fn tooltip_bindings_resolve(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        let vcx = cx.add_empty_window();
        vcx.update(|window, _| {
            assert!(Kbd::binding_for_action(&SaveMedia, Some("Detail"), window).is_some());
            assert!(Kbd::binding_for_action(&ToggleFavorite, Some("Detail"), window).is_some());
            assert!(Kbd::binding_for_action(&Generate, Some("Compose"), window).is_some());
            assert!(Kbd::binding_for_action(&SelectLeft, Some("Feed"), window).is_some());
            assert!(Kbd::binding_for_action(&FocusFeed, Some("Compose"), window).is_some());
            assert!(Kbd::binding_for_action(&ImprovePrompt, Some("Compose"), window).is_some());
            assert!(Kbd::binding_for_action(&ToggleSidebar, Some("Library"), window).is_some());
            assert!(Kbd::binding_for_action(&ToggleComposer, Some("Library"), window).is_some());
            assert!(Kbd::binding_for_action(&ImportFiles, Some("Library"), window).is_some());
            assert!(Kbd::binding_for_action(&SaveMedia, Some("Compose"), window).is_none(), "context-scoped");
            assert!(Kbd::binding_for_action(&FocusFeed, Some("Feed"), window).is_none(), "context-scoped");
        });
    }
}
