//! Actions, key bindings and the native menu bar.

use gpui::{actions, Action, App, KeyBinding, Keystroke, Menu, MenuItem, OsAction};
use gpui_component::kbd::Kbd;

actions!(
    majik,
    [
        Quit,
        CloseWindow,
        NewComposition,
        ShowLibrary,
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
        ToggleThumbnailShape,
        ShowInfo,
        Recreate,
        Upscale,
        RemoveBackground,
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
    ]
);

/// Keys are written with gpui's `secondary` modifier — ⌘ on macOS, Ctrl on Windows and Linux —
/// never `cmd`, which is the Windows / Super key off macOS.
pub const NEW_COMPOSITION_KEYS: &str = "secondary-n";

/// The platform's spelling of a binding for help text (`⌘N` on macOS, `Ctrl+N` elsewhere) —
/// the same formatting the Shortcuts page and tooltips use, so copy never hard-codes a glyph.
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
/// the page can never drift from the keymap.
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
    vec![
        app("Quit", &["secondary-q"], Quit),
        app("Close Window", &["secondary-w"], CloseWindow),
        app("New Composition", &[NEW_COMPOSITION_KEYS], NewComposition),
        app("Library", &["secondary-1"], ShowLibrary),
        app("Settings", &["secondary-,"], OpenSettings),
        // ⌘⌥S is what Finder / Photos / Mail use for their sidebar.
        shortcut("Library window", "Show / Hide Sidebar", &["secondary-alt-s"], ToggleSidebar, &[Some("Library")]),
        feed("Open", &["secondary-o", "enter"], OpenSelection),
        feed("Select All", &["secondary-a"], SelectAll),
        feed("Clear Selection", &["escape"], ClearSelection),
        feed("Select Previous", &["left"], SelectLeft),
        feed("Select Next", &["right"], SelectRight),
        feed("Select Above", &["up"], SelectUp),
        feed("Select Below", &["down"], SelectDown),
        media("Save…", &["secondary-s"], SaveMedia),
        media("Copy", &["secondary-c"], CopyMedia),
        media("Favorite", &["secondary-shift-f"], ToggleFavorite),
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
        // The prompt's own Escape handler propagates, so `FocusFeed` fires right after it.
        composer("Generate", &["secondary-enter"], Generate),
        composer("Improve Prompt", &["secondary-shift-i"], ImprovePrompt),
        composer("Paste Image", &["secondary-shift-v"], PasteImage),
        composer("Focus Feed", &["escape"], FocusFeed),
        settings("Close Settings", &["escape"], CloseWindow, "Settings"),
        settings("Previous Page", &["up"], SelectUp, "SettingsNav"),
        settings("Next Page", &["down"], SelectDown, "SettingsNav"),
    ]
}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| {
        crate::windows::save_all_frames(cx);
        cx.quit()
    });
    cx.on_action(|_: &ShowLibrary, cx| crate::windows::open_library(cx));
    // App-level so it works from every window (and from the menu with none focused); the Settings
    // window itself just comes forward.
    cx.on_action(|_: &OpenSettings, cx| crate::windows::open_settings(Default::default(), cx));
    cx.bind_keys(shortcuts().into_iter().flat_map(|shortcut| shortcut.bindings).collect::<Vec<_>>());

    // gpui only *stores* the menus off macOS; the Library window draws them from `GlobalState`.
    // `Menu::owned` consumes the menu, so the list is built once for each store.
    gpui_component::global_state::GlobalState::global_mut(cx).set_app_menus(menus().into_iter().map(Menu::owned).collect());
    cx.set_menus(menus());
}

/// The application menus, mirrored in two places: `cx.set_menus` drives the native macOS menu
/// bar, and `GlobalState::set_app_menus` feeds the [`AppMenuBar`](gpui_component::menu::AppMenuBar)
/// the Library window draws itself on Windows and Linux, where gpui only stores them.
pub fn menus() -> Vec<Menu> {
    vec![
        Menu {
            // The channel's name, so a dev build says so wherever we draw the menu ourselves. On
            // macOS the application menu's own title is AppKit's (the bundle / executable name).
            name: crate::config::app_name().into(),
            disabled: false,
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action(format!("Quit {}", crate::config::app_name()), Quit),
            ],
        },
        Menu {
            name: "File".into(),
            disabled: false,
            items: vec![
                MenuItem::action("New Composition", NewComposition),
                MenuItem::action("Close Window", CloseWindow),
                MenuItem::separator(),
                MenuItem::action("Open", OpenSelection),
                MenuItem::action("Save…", SaveMedia),
            ],
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: vec![
                MenuItem::os_action("Copy", CopyMedia, OsAction::Copy),
                MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
                MenuItem::action("Clear Selection", ClearSelection),
                MenuItem::separator(),
                MenuItem::action("Improve Prompt", ImprovePrompt),
                MenuItem::action("Clear Prompt", ClearPrompt),
            ],
        },
        Menu {
            name: "View".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Actual Size", ResetZoom),
                MenuItem::action("Square / Aspect Ratio", ToggleThumbnailShape),
                MenuItem::separator(),
                MenuItem::action("Show Info", ShowInfo),
                MenuItem::action("Play / Pause", TogglePlayback),
                MenuItem::separator(),
                MenuItem::action("Show / Hide Sidebar", ToggleSidebar),
                MenuItem::action("Show / Hide Composer", ToggleComposer),
                MenuItem::action("Library", ShowLibrary),
            ],
        },
        Menu {
            name: "Media".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Generate", Generate),
                MenuItem::action("Recreate", Recreate),
                MenuItem::action("Favorite", ToggleFavorite),
                MenuItem::separator(),
                MenuItem::action("Upscale", Upscale),
                MenuItem::action("Remove Background", RemoveBackground),
                MenuItem::action("Retry", Retry),
                MenuItem::separator(),
                MenuItem::action("Delete", DeleteMedia),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AsKeystroke as _, TestAppContext};

    /// With two installs side by side, the menu is where you check which one you're driving.
    #[test]
    fn the_app_menu_carries_the_channel_name() {
        let menus = menus();
        let app_menu = &menus[0];
        assert_eq!(app_menu.name.as_ref(), crate::config::app_name());
        let quit = app_menu.items.last().expect("a Quit item");
        let MenuItem::Action { name, .. } = quit else { panic!("Quit is an action item") };
        assert_eq!(name.as_ref(), format!("Quit {}", crate::config::app_name()));
    }

    #[test]
    fn keystroke_label_spells_the_secondary_modifier_for_the_platform() {
        let expected = if cfg!(target_os = "macos") { "⌘N" } else { "Ctrl+N" };
        assert_eq!(keystroke_label(NEW_COMPOSITION_KEYS), expected);
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
    /// gpui's own store: `init` fills both from the same list, so the two can never disagree.
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

    #[gpui::test]
    fn init_feeds_the_native_and_the_drawn_menu_bar_the_same_menus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
            let drawn = gpui_component::global_state::GlobalState::global(cx).app_menus().to_vec();
            let expected: Vec<String> = menus().into_iter().map(|menu| menu.name.to_string()).collect();
            assert_eq!(drawn.iter().map(|menu| menu.name.to_string()).collect::<Vec<_>>(), expected, "every menu is drawn off macOS");
            assert!(!expected.is_empty(), "there are menus to draw");
            let items: usize = drawn.iter().map(|menu| menu.items.len()).sum();
            assert_eq!(items, menus().iter().map(|menu| menu.items.len()).sum::<usize>(), "with all of their items");
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
            assert!(Kbd::binding_for_action(&SaveMedia, Some("Compose"), window).is_none(), "context-scoped");
            assert!(Kbd::binding_for_action(&FocusFeed, Some("Feed"), window).is_none(), "context-scoped");
        });
    }
}
