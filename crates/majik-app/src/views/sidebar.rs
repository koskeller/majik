//! Sidebar: Library / Favorites, Albums (create / rename / delete, cells dropped in), Settings.

use gpui::{prelude::*, px, App, ClickEvent, Context, ElementId, Entity, EventEmitter, PromptLevel, SharedString, WeakEntity, Window};
use gpui_component::button::{ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::DropdownMenu as _;
use gpui_component::menu::PopupMenuItem;
use gpui_component::sidebar::{Sidebar, SidebarGroup, SidebarItem, SidebarMenuItem};
use gpui_component::{ActiveTheme as _, Collapsible, Side};
use gpui_component::{Sizable as _, WindowExt as _};
use majik_core::model::AlbumId;
use majik_core::FeedFilter;

use crate::state::{self, DraggedAssets, LibraryModel};
use crate::ui::{button, icon, Raised as _};

pub enum SidebarEvent {
    Select(FeedFilter),
    OpenSettings,
}

pub struct SidebarView {
    /// The row drawn as current; the window moves it with the feed (Library / Favorites / Assets).
    pub(crate) selected: FeedFilter,
    library: Entity<LibraryModel>,
    /// The footer offers a restart once an update is installed.
    updater: Option<Entity<crate::auto_update::AutoUpdater>>,
}

impl EventEmitter<SidebarEvent> for SidebarView {}

impl SidebarView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let library = state::library(cx);
        cx.observe(&library, |_, _, cx| cx.notify()).detach();
        let updater = crate::auto_update::updater(cx);
        if let Some(updater) = &updater {
            cx.observe(updater, |_, _, cx| cx.notify()).detach();
        }
        Self { selected: FeedFilter::Library, library, updater }
    }

    /// Whether an update is installed and waiting for a restart.
    fn update_ready(&self, cx: &App) -> bool {
        self.updater.as_ref().is_some_and(|updater| updater.read(cx).status().is_updated())
    }

    pub fn select(&mut self, filter: FeedFilter, cx: &mut Context<Self>) {
        if self.selected != filter {
            self.selected = filter.clone();
            cx.emit(SidebarEvent::Select(filter));
            cx.notify();
        }
    }

    /// Opens the New Album dialog and returns its name field, focused and ready to type into.
    pub fn open_new_album(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Album name"));
        let library = self.library.clone();
        let this = cx.weak_entity();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let dialog = dialog.raised(cx);
            let input = dialog_input.clone();
            let library = library.clone();
            let this = this.clone();
            let field = input.clone();
            dialog
                .title("New Album")
                .w(px(380.))
                .content(move |content, _, _| content.py_1().child(Input::new(&field)))
                .on_ok(move |_, _, cx| {
                    let name = input.read(cx).value().trim().to_string();
                    if name.is_empty() {
                        return false;
                    }
                    let id = library.update(cx, |m, cx| m.create_album(name, cx));
                    this.update(cx, |s, cx| s.select(FeedFilter::Album(id), cx)).ok();
                    true
                })
        });
        // `open_dialog` focuses the dialog itself; the name field has to win.
        input.update(cx, |s, cx| s.focus(window, cx));
        input
    }

    fn open_rename_album(&mut self, id: AlbumId, current: String, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Album name").default_value(current));
        let library = self.library.clone();
        let dialog_input = input.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let dialog = dialog.raised(cx);
            let input = dialog_input.clone();
            let library = library.clone();
            let id = id.clone();
            let field = input.clone();
            dialog.title("Rename Album").w(px(380.)).content(move |content, _, _| content.py_1().child(Input::new(&field))).on_ok(move |_, _, cx| {
                let name = input.read(cx).value().trim().to_string();
                if name.is_empty() {
                    return false;
                }
                library.update(cx, |m, cx| m.rename_album(&id, name, cx));
                true
            })
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        input
    }

    fn confirm_delete_album(&mut self, id: AlbumId, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete “{name}”?"),
            Some("Items in this album won't be deleted from your library."),
            &["Delete", "Cancel"],
            cx,
        );
        let library = self.library.clone();
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                cx.update(|cx| library.update(cx, |m, cx| m.delete_album(&id, cx)));
                this.update(cx, |s, cx| {
                    if s.selected == FeedFilter::Album(id.clone()) {
                        s.select(FeedFilter::Library, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Cells dragged from a grid and dropped on an album row: the generations behind them join
    /// the album (assets no generation made are skipped, an album never holds a row twice).
    pub fn drop_on_album(&mut self, album: &AlbumId, dragged: &DraggedAssets, window: &mut Window, cx: &mut Context<Self>) {
        let ids = dragged.generations();
        let Some((name, before)) = self.library.read(cx).lib.album(album).map(|a| (a.name.clone(), a.items.len())) else { return };
        if ids.is_empty() {
            return;
        }
        let after = self.library.update(cx, |m, cx| {
            m.add_to_album(album, &ids, cx);
            m.lib.album(album).map_or(before, |a| a.items.len())
        });
        let message = match after - before {
            0 => format!("Already in {name}"),
            1 => format!("Added to {name}"),
            n => format!("Added {n} items to {name}"),
        };
        crate::ui::toast(window, message, cx);
    }

    fn menu(&self, cx: &Context<Self>) -> Menu {
        Menu { rows: Vec::new(), collapsed: false, sidebar: cx.weak_entity() }
    }

    fn item(&self, label: impl Into<SharedString>, icon_name: &'static str, filter: FeedFilter, cx: &mut Context<Self>) -> SidebarMenuItem {
        let active = self.selected == filter;
        SidebarMenuItem::new(label).icon(icon(icon_name)).active(active).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.select(filter.clone(), cx);
        }))
    }
}

impl Render for SidebarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let albums: Vec<(AlbumId, String)> = self.library.read(cx).lib.albums().iter().map(|a| (a.id.clone(), a.name.clone())).collect();

        let mut album_menu = self.menu(cx);
        for (id, name) in albums {
            let this = cx.weak_entity();
            let (rid, rname) = (id.clone(), name.clone());
            let (did, dname) = (id.clone(), name.clone());
            let btn_id = SharedString::from(format!("album-menu-{}", id.0));
            let suffix = move |_w: &mut Window, _cx: &mut gpui::App| {
                let this_r = this.clone();
                let (rid, rname) = (rid.clone(), rname.clone());
                let this_d = this.clone();
                let (did, dname) = (did.clone(), dname.clone());
                button(btn_id.clone()).icon(icon("ellipsis")).ghost().xsmall().dropdown_menu(move |menu, _, _| {
                    let this_r = this_r.clone();
                    let (rid, rname) = (rid.clone(), rname.clone());
                    let this_d = this_d.clone();
                    let (did, dname) = (did.clone(), dname.clone());
                    menu.item(PopupMenuItem::new("Rename…").icon(icon("pencil")).on_click(move |_, window, cx| {
                        this_r.update(cx, |s, cx| s.open_rename_album(rid.clone(), rname.clone(), window, cx)).ok();
                    }))
                    .separator()
                    .item(PopupMenuItem::new("Delete").icon(icon("trash-2")).on_click(move |_, window, cx| {
                        this_d.update(cx, |s, cx| s.confirm_delete_album(did.clone(), dname.clone(), window, cx)).ok();
                    }))
                })
            };
            album_menu = album_menu.drop_target(self.item(name, "album", FeedFilter::Album(id.clone()), cx).suffix(suffix), id);
        }
        album_menu = album_menu.child(
            SidebarMenuItem::new("New Album…").icon(icon("folder-plus")).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_new_album(window, cx);
            })),
        );

        Sidebar::new("sidebar")
            .side(Side::Left)
            .w_full()
            .h_full()
            .child(
                SidebarGroup::new("Library").child(
                    self.menu(cx)
                        .child(self.item("Library", "folder-library", FeedFilter::Library, cx))
                        .child(self.item("Favorites", "heart", FeedFilter::Favorites, cx))
                        .child(self.item("Assets", "layers", FeedFilter::Assets, cx)),
                ),
            )
            .child(SidebarGroup::new("Albums").child(album_menu))
            .footer(
                gpui_component::v_flex()
                    .gap_0p5()
                    .when(self.update_ready(cx), |this| {
                        this.child(
                            gpui::div().debug_selector(|| "restart-to-update".into()).child(
                                SidebarMenuItem::new("Restart to Update")
                                    .icon(icon("download"))
                                    .on_click(|_: &ClickEvent, window, cx| window.dispatch_action(Box::new(crate::actions::RestartToUpdate), cx))
                                    .render("restart-to-update", window, cx),
                            ),
                        )
                    })
                    .child(
                        SidebarMenuItem::new("Settings")
                            .icon(icon("settings"))
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(SidebarEvent::OpenSettings)))
                            .render("settings", window, cx),
                    ),
            )
    }
}

/// A `SidebarMenu` whose rows can take a drop: album rows accept cells dragged from a grid.
/// Rows sit tighter than gpui-component's default `gap_2`; group titles keep their own spacing
/// from the `SidebarGroup`.
#[derive(Clone)]
struct Menu {
    rows: Vec<(SidebarMenuItem, Option<AlbumId>)>,
    collapsed: bool,
    sidebar: WeakEntity<SidebarView>,
}

impl Menu {
    fn child(mut self, item: SidebarMenuItem) -> Self {
        self.rows.push((item, None));
        self
    }

    fn drop_target(mut self, item: SidebarMenuItem, album: AlbumId) -> Self {
        self.rows.push((item, Some(album)));
        self
    }
}

impl Collapsible for Menu {
    fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }
}

/// Whether an album row lights up for and takes `value`: only grid drags with a generation behind
/// them (an import from the Assets grid has none, and files from outside are not generations).
fn album_accepts(value: &dyn std::any::Any) -> bool {
    value.downcast_ref::<DraggedAssets>().is_some_and(|d| !d.generations().is_empty())
}

impl SidebarItem for Menu {
    fn render(self, id: impl Into<ElementId>, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = id.into();
        let radius = cx.theme().radius;
        let accent = cx.theme().sidebar_accent;
        gpui_component::v_flex().gap_0p5().children(self.rows.into_iter().enumerate().map(|(ix, (item, album))| {
            let row = item.collapsed(self.collapsed).render(SharedString::from(format!("{id}-{ix}")), window, cx).into_any_element();
            let Some(album) = album else { return row };
            let sidebar = self.sidebar.clone();
            let selector = format!("album-drop-{}", album.0);
            gpui::div()
                .id(("album-drop", ix))
                .w_full()
                .rounded(radius)
                .debug_selector(move || selector.clone())
                .can_drop(|value, _, _| album_accepts(value))
                .drag_over::<DraggedAssets>(move |style, _, _, _| style.bg(accent))
                .on_drop(move |dragged: &DraggedAssets, window, cx| {
                    sidebar.update(cx, |sidebar, cx| sidebar.drop_on_album(&album, dragged, window, cx)).ok();
                })
                .child(row)
                .into_any_element()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env, seed_item, Seed};
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use majik_core::model::MediaType;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Stands in for `LibraryWindow`: the sidebar plus the dialog layer, inside a `Root`, so the
    /// album dialogs are actually drawn and their name field is on the focus path.
    struct Host {
        sidebar: Entity<SidebarView>,
    }

    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            gpui::div().size_full().child(self.sidebar.clone()).children(gpui_component::Root::render_dialog_layer(window, cx))
        }
    }

    fn sidebar_window(cx: &mut TestAppContext) -> (Entity<SidebarView>, &mut VisualTestContext) {
        let slot: Rc<RefCell<Option<Entity<SidebarView>>>> = Default::default();
        let slot_for_window = slot.clone();
        let (_root, vcx) = cx.add_window_view(move |window, cx| {
            let sidebar = cx.new(SidebarView::new);
            *slot_for_window.borrow_mut() = Some(sidebar.clone());
            let host = cx.new(|_| Host { sidebar });
            gpui_component::Root::new(gpui::AnyView::from(host), window, cx)
        });
        vcx.run_until_parked();
        let view = slot.borrow().clone().unwrap();
        (view, vcx)
    }

    #[gpui::test]
    async fn the_footer_offers_a_restart_once_an_update_is_installed(cx: &mut TestAppContext) {
        use crate::auto_update::test_support::{FakeFeed, FakeInstaller};
        use crate::auto_update::{AutoUpdater, CheckType};
        let _e = env(cx, 0, "Mock");
        let installer = FakeInstaller::new();
        let app_path = installer.app_path();
        let feed = FakeFeed::offering("0.1.0");
        let updater = cx.update(|cx| AutoUpdater::init(semver::Version::new(0, 1, 0), Some(feed.clone()), installer, Some(app_path), cx));
        let (_view, vcx) = sidebar_window(cx);
        let draw = |vcx: &mut VisualTestContext| {
            vcx.run_until_parked();
            vcx.update(|window, cx| window.draw(cx).clear(cx));
        };
        draw(vcx);
        assert!(vcx.debug_bounds("restart-to-update").is_none(), "nothing to restart into");
        feed.offer("0.2.0");
        updater.update(vcx, |updater, cx| updater.poll(CheckType::Manual, cx));
        draw(vcx);
        let bounds = vcx.debug_bounds("restart-to-update").expect("the footer offers the restart once the update is installed");
        let will_restart = vcx.expect_restart();
        vcx.simulate_click(bounds.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        let (path, _) = will_restart.await.expect("clicking it restarts the app");
        assert_eq!(path, None, "into the same, now replaced, app");
    }

    #[gpui::test]
    fn album_crud_and_selection_events(cx: &mut TestAppContext) {
        let e = env(cx, 3, "Mock");
        let (view, vcx) = cx.add_window_view(|_window, cx| SidebarView::new(cx));
        vcx.run_until_parked();

        // Collect emitted selection filters.
        let selections: Rc<RefCell<Vec<FeedFilter>>> = Default::default();
        let s = selections.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&view, move |_, ev: &SidebarEvent, _| {
                if let SidebarEvent::Select(f) = ev {
                    s.borrow_mut().push(f.clone());
                }
            })
            .detach();
        });

        // Selecting Favorites or Assets emits the filter.
        view.update(vcx, |v, cx| v.select(FeedFilter::Favorites, cx));
        assert_eq!(selections.borrow().last(), Some(&FeedFilter::Favorites));
        view.update(vcx, |v, cx| v.select(FeedFilter::Assets, cx));
        assert_eq!(selections.borrow().last(), Some(&FeedFilter::Assets));
        assert_eq!(view.read_with(vcx, |v, _| v.selected.clone()), FeedFilter::Assets);

        // Create an album via the library, then select it.
        let album = e.library.update(vcx, |m, cx| m.create_album("Trip".into(), cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.albums().len()), 1);
        view.update(vcx, |v, cx| v.select(FeedFilter::Album(album.clone()), cx));
        assert_eq!(selections.borrow().last(), Some(&FeedFilter::Album(album.clone())));

        // Rename + delete round-trip through the model.
        e.library.update(vcx, |m, cx| m.rename_album(&album, "Renamed".into(), cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.albums()[0].name.clone()), "Renamed");
        e.library.update(vcx, |m, cx| m.delete_album(&album, cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.albums().len()), 0);
    }

    /// What a drag of every generation in the library carries, as the feed would build it.
    fn dragged_generations(library: &Entity<LibraryModel>, vcx: &VisualTestContext) -> DraggedAssets {
        library.read_with(vcx, |m, _| {
            let assets = m
                .lib
                .feed(&FeedFilter::Library, majik_core::MediaFilter::All)
                .iter()
                .filter_map(|id| m.lib.get(id))
                .filter_map(|item| {
                    let asset = m.lib.asset(item.output_asset_id.as_ref()?)?;
                    Some(crate::state::DraggedAsset { id: asset.id.clone(), kind: asset.kind, path: asset.path.clone(), generation: Some(item.id.clone()) })
                })
                .collect();
            DraggedAssets { assets }
        })
    }

    #[gpui::test]
    fn dropping_cells_on_an_album_adds_their_generations_once(cx: &mut TestAppContext) {
        let e = env(cx, 3, "Mock");
        let (view, vcx) = sidebar_window(cx);
        let album = e.library.update(vcx, |m, cx| m.create_album("Trip".into(), cx));
        let dragged = dragged_generations(&e.library, vcx);
        assert_eq!(dragged.generations().len(), 3);
        let toasts = vcx.update(|_, cx| crate::ui::toast_generation(cx));

        view.update_in(vcx, |v, window, cx| v.drop_on_album(&album, &dragged, window, cx));
        let items = e.library.read_with(vcx, |m, _| m.lib.album(&album).unwrap().items.clone());
        assert_eq!(items, dragged.generations(), "every dragged generation joined, in drag order");
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), toasts + 1, "the drop is confirmed");

        // Dropping the same cells again changes nothing but still answers.
        view.update_in(vcx, |v, window, cx| v.drop_on_album(&album, &dragged, window, cx));
        assert_eq!(e.library.read_with(vcx, |m, _| m.lib.album(&album).unwrap().items.len()), 3);
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), toasts + 2);
    }

    #[gpui::test]
    fn a_drop_with_nothing_behind_it_leaves_the_album_alone(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (view, vcx) = sidebar_window(cx);
        let album = e.library.update(vcx, |m, cx| m.create_album("Trip".into(), cx));
        let import = crate::test_support::seed_asset(&e.library, vcx, MediaType::Image, 9);
        let (kind, path) = e.library.read_with(vcx, |m, _| { let a = m.lib.asset(&import).unwrap(); (a.kind, a.path.clone()) });
        let dragged = DraggedAssets { assets: vec![crate::state::DraggedAsset { id: import, kind, path, generation: None }] };
        let toasts = vcx.update(|_, cx| crate::ui::toast_generation(cx));

        // The row doesn't light up for it and, should a drop land anyway, nothing happens.
        assert!(!album_accepts(&dragged));
        assert!(!album_accepts(&gpui::ExternalPaths::default()), "files from outside aren't generations");
        view.update_in(vcx, |v, window, cx| v.drop_on_album(&album, &dragged, window, cx));
        assert!(e.library.read_with(vcx, |m, _| m.lib.album(&album).unwrap().items.is_empty()));
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), toasts);

        // A payload dropped on an album that is gone is ignored too.
        seed_item(&e.library, vcx, Seed::default());
        let real = dragged_generations(&e.library, vcx);
        assert_eq!(real.generations().len(), 1);
        view.update_in(vcx, |v, window, cx| v.drop_on_album(&AlbumId("missing".into()), &real, window, cx));
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), toasts);
    }

    #[gpui::test]
    fn new_album_dialog_opens_with_the_name_field_focused(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = sidebar_window(cx);

        let input = view.update_in(vcx, |v, window, cx| v.open_new_album(window, cx));
        vcx.run_until_parked();

        assert!(vcx.update(|window, cx| window.has_active_dialog(cx)), "New Album is a dialog over the window");
        assert!(vcx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window)), "the name field takes focus so the user can type straight away");
        vcx.simulate_keystrokes("T r i p");
        assert_eq!(input.read_with(vcx, |s, _| s.value().to_string()), "Trip", "typing lands in the name field");
    }

    #[gpui::test]
    fn a_click_outside_the_new_album_dialog_closes_it(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (view, vcx) = sidebar_window(cx);
        view.update_in(vcx, |v, window, cx| {
            v.open_new_album(window, cx);
        });
        vcx.run_until_parked();
        assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
        vcx.simulate_click(gpui::point(gpui::px(4.), gpui::px(200.)), gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(!vcx.update(|window, cx| window.has_active_dialog(cx)), "the dialog has no close button; a click on the backdrop dismisses it");
    }

    #[gpui::test]
    fn rename_album_dialog_opens_with_the_name_field_focused(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (view, vcx) = sidebar_window(cx);
        let album = e.library.update(vcx, |m, cx| m.create_album("Trip".into(), cx));
        vcx.run_until_parked();

        let input = view.update_in(vcx, |v, window, cx| v.open_rename_album(album, "Trip".into(), window, cx));
        vcx.run_until_parked();

        assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
        assert!(vcx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window)), "the name field takes focus so the user can type straight away");
        assert_eq!(input.read_with(vcx, |s, _| s.value().to_string()), "Trip", "the field starts with the current name");
    }
}
