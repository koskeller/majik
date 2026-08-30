//! The Library window: the sidebar on the left and the composer panel on the right (both
//! collapsible, widths persisted) around the feed, on every library screen (Library, Favorites,
//! albums, tool feeds). The detail instead covers the whole window: it grows out of its feed cell
//! and shrinks back into it, drawn over the split meanwhile. The dialog and notification layers sit
//! on top. While `Config.onboarding_completed` is false the window shows the onboarding flow
//! instead (port of `ContentView`'s onboarding check).
//!
//! The composer used to be its own window. As a panel, Recreate / Use Image / ⌘N reach it through
//! events and actions handled here, never through a window handle, which can't re-enter the
//! window that is dispatching the action.

use gpui::{prelude::*, px, App, Context, Entity, FocusHandle, Pixels, Size, Task, Window};
use gpui_component::notification::Notification;
use gpui_component::resizable::{h_resizable, resizable_panel, ResizablePanel, ResizablePanelEvent, ResizableState};
use gpui_component::button::{ButtonVariants as _};
use gpui_component::menu::AppMenuBar;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Root, Selectable as _, Sizable as _, Theme, TitleBar, WindowExt as _};
use majik_core::FeedFilter;
use std::ops::Range;
use std::time::Duration;

use crate::actions::{CloseWindow, FocusFeed, NewComposition, ToggleComposer, ToggleSidebar};
use crate::config::{update_config, Config};
use crate::state::{LibraryEvent, PendingCompose};
use crate::ui::{button, icon};
use crate::views::compose::ComposeView;
use crate::views::detail::{DetailEvent, DetailView};
use crate::views::feed::{FeedEvent, FeedView};
use crate::views::onboarding::OnboardingView;
use crate::views::sidebar::{SidebarEvent, SidebarView};

pub struct LibraryWindow {
    sidebar: Entity<SidebarView>,
    feed: Entity<FeedView>,
    compose: Entity<ComposeView>,
    detail: Option<Entity<DetailView>>,
    resizable: Entity<ResizableState>,
    focus: FocusHandle,
    /// Present while onboarding hasn't been completed; created/dropped in `render` from `Config`.
    onboarding: Option<Entity<OnboardingView>>,
    /// The collapsible panels either side of the feed, indexed by `Side`.
    panels: [SidePanel; 2],
    /// Viewport when the detail opened: the feed's cell boxes are only valid at that size.
    detail_viewport: Size<Pixels>,
    /// Generations that finished since the last notification: (completed, failed).
    finished: (usize, usize),
    /// Fires the coalesced notification; re-armed by every finished generation.
    notify_task: Option<Task<()>>,
    /// The menus drawn in our own title bar: gpui renders a native menu bar only on macOS, and
    /// merely stores the menus on Windows and Linux (see `actions::menus`).
    menu_bar: Entity<AppMenuBar>,
}

/// The two collapsible panels around the feed. Everything that differs between them lives here;
/// showing, hiding and persisting the width are shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Sidebar,
    Composer,
}

impl Side {
    const ALL: [Side; 2] = [Side::Sidebar, Side::Composer];

    /// Index among the split's panels (sidebar, feed, composer).
    fn panel_index(self) -> usize {
        match self {
            Side::Sidebar => 0,
            Side::Composer => 2,
        }
    }

    /// Width until the user drags the handle. The composer opens at the narrowest width its layout
    /// allows, so the feed keeps as many columns as possible until the user widens it.
    pub(crate) fn default_width(self) -> Pixels {
        match self {
            Side::Sidebar => px(230.),
            Side::Composer => self.width_range().start,
        }
    }

    /// The range the handle allows.
    pub(crate) fn width_range(self) -> Range<Pixels> {
        match self {
            Side::Sidebar => px(170.)..px(380.),
            Side::Composer => px(380.)..px(600.),
        }
    }

    fn clamp(self, width: Pixels) -> Pixels {
        let range = self.width_range();
        width.max(range.start).min(range.end)
    }

    fn saved(self, config: &Config) -> (bool, Option<f32>) {
        match self {
            Side::Sidebar => (config.sidebar_open, config.sidebar_width),
            Side::Composer => (config.compose_panel_open, config.compose_panel_width),
        }
    }

    fn save_open(self, config: &mut Config, open: bool) {
        match self {
            Side::Sidebar => config.sidebar_open = open,
            Side::Composer => config.compose_panel_open = open,
        }
    }

    fn save_width(self, config: &mut Config, width: Pixels) {
        let width = Some(f32::from(width));
        match self {
            Side::Sidebar => config.sidebar_width = width,
            Side::Composer => config.compose_panel_width = width,
        }
    }

    /// `debug_bounds` key of the panel's content box.
    fn selector(self) -> &'static str {
        match self {
            Side::Sidebar => "sidebar-panel",
            Side::Composer => "compose-panel",
        }
    }
}

/// A side panel's state, mirrored in `Config` (see `Side::saved`).
#[derive(Clone, Copy, Debug)]
struct SidePanel {
    open: bool,
    /// Applied as the panel's initial size whenever it is (re)shown.
    width: Pixels,
}

impl SidePanel {
    fn from_config(side: Side, config: &Config) -> Self {
        let (open, width) = side.saved(config);
        Self { open, width: width.map(px).map_or(side.default_width(), |w| side.clamp(w)) }
    }
}

/// Marker for the completion notification slot: a newer batch replaces the previous notification.
struct GenerationDone;
/// Generations finishing within this window are reported as one notification.
const NOTIFY_COALESCE: Duration = Duration::from_millis(1500);

/// "Generation complete" / "3 generations complete", with "2 ready · 1 failed" as the body.
pub(crate) fn notification_copy(completed: usize, failed: usize) -> (String, String) {
    let total = completed + failed;
    let title = if total == 1 { "Generation complete".to_string() } else { format!("{total} generations complete") };
    let mut parts = Vec::new();
    if completed > 0 {
        parts.push(format!("{completed} ready"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    (title, parts.join(" · "))
}

impl LibraryWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(SidebarView::new);
        let feed = cx.new(|cx| FeedView::new(window, cx));
        // After the feed: it takes focus in its constructor and keeps it at launch.
        let compose = cx.new(|cx| ComposeView::new(window, cx));
        let resizable = cx.new(|_| ResizableState::default());
        let config = cx.global::<Config>();
        let panels = Side::ALL.map(|side| SidePanel::from_config(side, config));

        cx.subscribe(&sidebar, |this, _, ev: &SidebarEvent, cx| match ev {
            SidebarEvent::Select(filter) => {
                this.detail = None;
                this.feed.update(cx, |f, cx| f.set_filter(filter.clone(), cx));
                let album = match filter {
                    FeedFilter::Album(id) => Some(id.clone()),
                    _ => None,
                };
                this.compose.update(cx, |c, cx| c.set_album(album, cx));
                cx.notify();
            }
            SidebarEvent::OpenSettings => crate::windows::open_settings(Default::default(), cx),
        })
        .detach();

        cx.subscribe_in(&feed, window, |this, _, ev: &FeedEvent, window, cx| match ev {
            FeedEvent::Open { ids, index, origin } => {
                let thumbnails = this.feed.read(cx).image_cache();
                let detail = cx.new(|cx| DetailView::new(ids.clone(), *index, *origin, thumbnails, cx));
                this.detail_viewport = window.viewport_size();
                // The layout below follows the morph, so re-render whenever the detail does.
                cx.observe(&detail, |_, _, cx| cx.notify()).detach();
                cx.subscribe_in(&detail, window, |this, detail, ev: &DetailEvent, window, cx| match ev {
                    DetailEvent::WillClose { id } => {
                        // Cell boxes recorded before a resize point at the wrong place, so fade instead.
                        let same_viewport = window.viewport_size() == this.detail_viewport;
                        let cell = this.feed.update(cx, |f, cx| f.land_on(id, cx)).filter(|_| same_viewport);
                        detail.update(cx, |d, cx| d.close_towards(cell, cx));
                    }
                    DetailEvent::Close => {
                        let current = detail.read(cx).current_id();
                        this.detail = None;
                        this.feed.update(cx, |f, cx| {
                            if let Some(id) = &current {
                                f.land_on(id, cx);
                            }
                            f.focus(cx);
                        });
                        cx.notify();
                    }
                    DetailEvent::Compose(pending) => {
                        // Straight to the split (no close morph): the panel is under the cover,
                        // and closing the detail normally would hand focus back to the feed.
                        let current = detail.read(cx).current_id();
                        this.detail = None;
                        if let Some(id) = &current {
                            this.feed.update(cx, |f, cx| {
                                f.land_on(id, cx);
                            });
                        }
                        this.show_composer(Some(pending.clone()), window, cx);
                    }
                })
                .detach();
                this.detail = Some(detail);
                cx.notify();
            }
            FeedEvent::Compose(pending) => this.show_composer(Some(pending.clone()), window, cx),
        })
        .detach();

        cx.subscribe(&resizable, |this, state, _: &ResizablePanelEvent, cx| {
            for side in Side::ALL {
                if !this.panel(side).open {
                    continue;
                }
                let Some(width) = state.read(cx).sizes().get(side.panel_index()).copied() else { continue };
                // Dragging a handle away from a panel can grow it past its range; the layout
                // clamps that, the state doesn't.
                let width = side.clamp(width);
                if width != this.panel(side).width {
                    this.panel_mut(side).width = width;
                    update_config(cx, |c| side.save_width(c, width));
                }
            }
        })
        .detach();

        cx.observe_window_appearance(window, |_, window, cx| follow_system_appearance(window, cx)).detach();

        let library = crate::state::library(cx);
        cx.subscribe_in(&library, window, |this, _, ev: &LibraryEvent, window, cx| match ev {
            LibraryEvent::GenerationFinished { ok } => this.note_finished(*ok, window, cx),
            LibraryEvent::Error { message } => crate::ui::toast(window, message.clone(), cx),
            LibraryEvent::Changed => {}
        })
        .detach();

        // `onboarding_completed` flips via `update_config` (onboarding finish / debug reset);
        // re-render so the check below picks it up. Other config writes (draft prompt, appearance)
        // don't concern this window.
        cx.observe_global::<Config>(|this, cx| {
            if cx.global::<Config>().onboarding_completed == this.onboarding.is_some() {
                cx.notify();
            }
        })
        .detach();

        window.set_window_title("Library");
        crate::windows::track_frame(crate::windows::Singleton::Library, window, cx);
        let focus = cx.focus_handle();
        Self {
            sidebar,
            feed,
            compose,
            detail: None,
            resizable,
            focus,
            onboarding: None,
            panels,
            detail_viewport: Size::default(),
            finished: (0, 0),
            notify_task: None,
            menu_bar: AppMenuBar::new(cx),
        }
    }

    fn panel(&self, side: Side) -> &SidePanel {
        &self.panels[side as usize]
    }

    fn panel_mut(&mut self, side: Side) -> &mut SidePanel {
        &mut self.panels[side as usize]
    }

    fn set_open(&mut self, side: Side, open: bool, cx: &mut Context<Self>) {
        if self.panel(side).open == open {
            return;
        }
        self.panel_mut(side).open = open;
        // A hidden panel isn't laid out, so the split keeps its last bounds and size preference.
        // Resetting them keeps the other panels from rescaling against a phantom sibling, and lets
        // the saved width apply again as the panel's initial size when it comes back.
        let index = side.panel_index();
        self.resizable.update(cx, |state, cx| {
            if state.sizes().len() > index {
                state.reset_panel(index, cx);
            }
        });
        update_config(cx, |c| side.save_open(c, open));
        cx.notify();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        let open = self.panel(Side::Sidebar).open;
        self.set_open(Side::Sidebar, !open, cx);
    }

    /// Show the composer (dismissing a detail, which would cover it) and either hand it `pending`
    /// or just focus the prompt.
    pub fn show_composer(&mut self, pending: Option<PendingCompose>, window: &mut Window, cx: &mut Context<Self>) {
        self.detail = None;
        self.set_open(Side::Composer, true, cx);
        self.compose.update(cx, |compose, cx| match pending {
            Some(pending) => compose.apply(pending, window, cx),
            None => compose.focus_prompt(window, cx),
        });
        cx.notify();
    }

    /// ⌘N cycles like a dock toggle: closed → open + focus the prompt; open but elsewhere → focus
    /// the prompt; already typing → put the panel away.
    fn new_composition(&mut self, _: &NewComposition, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel(Side::Composer).open && self.compose.read(cx).contains_focus(window, cx) {
            self.hide_composer(window, cx);
        } else {
            self.show_composer(None, window, cx);
        }
    }

    fn hide_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Keystrokes routed to a node that is no longer rendered stop at the root.
        if self.compose.read(cx).contains_focus(window, cx) {
            self.feed.read(cx).focus_handle().focus(window, cx);
        }
        self.set_open(Side::Composer, false, cx);
    }

    fn toggle_composer(&mut self, _: &ToggleComposer, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel(Side::Composer).open {
            self.hide_composer(window, cx);
        } else {
            self.show_composer(None, window, cx);
        }
    }

    fn focus_feed(&mut self, _: &FocusFeed, window: &mut Window, cx: &mut Context<Self>) {
        self.feed.read(cx).focus_handle().focus(window, cx);
    }

    fn note_finished(&mut self, ok: bool, window: &mut Window, cx: &mut Context<Self>) {
        if ok {
            self.finished.0 += 1;
        } else {
            self.finished.1 += 1;
        }
        self.notify_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(NOTIFY_COALESCE).await;
            this.update_in(cx, |this, window, cx| this.flush_notification(window, cx)).ok();
        }));
    }

    /// Post one OS notification for everything that finished, and only while the user is
    /// elsewhere: an active Majik window already shows the result. gpui posts it through the
    /// platform's notification centre (macOS needs the `.app` bundle; Windows the app identity set
    /// in `main`).
    fn flush_notification(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (completed, failed) = std::mem::take(&mut self.finished);
        self.notify_task = None;
        if completed + failed == 0 {
            return;
        }
        // This window can't be re-entered through its handle while we're inside it.
        let app_active = window.is_window_active() || cx.windows().into_iter().any(|handle| handle.update(cx, |_, window, _| window.is_window_active()).unwrap_or(false));
        if app_active {
            return;
        }
        let (title, body) = notification_copy(completed, failed);
        window.push_notification(Notification::new().title(title).message(body).system().id::<GenerationDone>(), cx);
    }

    /// Keep `self.onboarding` in sync with `Config.onboarding_completed`.
    fn sync_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let completed = cx.global::<Config>().onboarding_completed;
        match (&self.onboarding, completed) {
            (None, false) => self.onboarding = Some(cx.new(|cx| OnboardingView::new(window, cx))),
            (Some(_), true) => {
                self.onboarding = None;
                self.feed.update(cx, |f, cx| f.focus(cx));
            }
            _ => {}
        }
    }

    /// One of the split's side panels: hidden rather than omitted while collapsed so the panel
    /// indices in the shared state stay stable. `flex_none` keeps a dragged width from growing
    /// with the window or when the other side collapses, since the feed is the flexible panel.
    fn side_panel(&self, side: Side, content: impl IntoElement, cx: &App) -> ResizablePanel {
        let panel = self.panel(side);
        let content = gpui::div().id(side.selector()).debug_selector(|| side.selector().into()).size_full().overflow_hidden().child(content);
        let content = match side {
            Side::Sidebar => content,
            Side::Composer => content.border_l_1().border_color(cx.theme().border),
        };
        resizable_panel().visible(panel.open).size(panel.width).size_range(side.width_range()).flex_none().child(content)
    }
}

impl LibraryWindow {
    /// The window's own title bar, as in Zed's main window: a 34px row under the transparent native
    /// bar (the traffic lights sit in its left padding on macOS, the window controls at its end
    /// elsewhere) that follows the app theme. It carries the panel toggles while the library screen
    /// is showing; onboarding and the detail cover the window and have no panels to toggle.
    fn render_title_bar(&self) -> impl IntoElement {
        let library_screen = self.onboarding.is_none() && self.detail.is_none();
        let toggle = |id: &'static str, icon_name: &'static str, side: Side| {
            let button = button(id).icon(icon(icon_name)).ghost().small().selected(self.panel(side).open);
            let button = match side {
                Side::Sidebar => button.tooltip_with_action("Show or hide the sidebar", &ToggleSidebar, Some("Library")).on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSidebar), cx)),
                Side::Composer => button.tooltip_with_action("Show or hide the composer", &NewComposition, None).on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleComposer), cx)),
            };
            gpui::div().debug_selector(move || id.into()).child(button)
        };
        gpui::div().w_full().flex_none().debug_selector(|| "title-bar".into()).child(
            TitleBar::new()
                .child(
                    h_flex()
                        .h_full()
                        .items_center()
                        .pl_1()
                        .when(library_screen, |this| this.child(toggle("title-sidebar", "panel-left", Side::Sidebar)))
                        // macOS draws the real menu bar at the top of the screen; elsewhere it is ours to draw.
                        .when(!cfg!(target_os = "macos"), |this| this.child(self.menu_bar.clone())),
                )
                .child(h_flex().h_full().items_center().pr_1().when(library_screen, |this| this.child(toggle("title-composer", "panel-right", Side::Composer)))),
        )
    }
}

impl Render for LibraryWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_onboarding(window, cx);

        let body: gpui::AnyElement = match (&self.onboarding, &self.detail) {
            (Some(onboarding), _) => onboarding.clone().into_any_element(),
            (None, Some(detail)) if !detail.read(cx).is_transitioning() => detail.clone().into_any_element(),
            (None, detail) => {
                let split = h_resizable("library-split")
                    .with_state(&self.resizable)
                    .child(self.side_panel(Side::Sidebar, self.sidebar.clone(), cx))
                    .child(resizable_panel().child(gpui::div().size_full().overflow_hidden().child(self.feed.clone())))
                    .child(self.side_panel(Side::Composer, self.compose.clone(), cx));
                match detail {
                    // The morph plays over the feed: its cell shows under the travelling box.
                    Some(detail) => gpui::div()
                        .size_full()
                        .relative()
                        .child(split)
                        .child(gpui::div().absolute().inset_0().child(detail.clone()))
                        .into_any_element(),
                    None => split.into_any_element(),
                }
            }
        };

        gpui::div()
            .id("library-window")
            .key_context("Library")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(Self::new_composition))
            .on_action(cx.listener(Self::toggle_composer))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::focus_feed))
            .child(v_flex().size_full().child(self.render_title_bar()).child(gpui::div().flex_1().min_h_0().w_full().child(body)))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
            .children(crate::ui::toast_layer(window, cx))
    }
}

/// The window's appearance changed (the OS switched, or the window just reported it). Only a
/// "system" preference follows it; an explicit light/dark choice stays as it is. Syncing regardless
/// turned a saved "dark" light again the moment the window opened.
fn follow_system_appearance(window: &mut Window, cx: &mut App) {
    if cx.global::<Config>().appearance == "system" {
        Theme::sync_system_appearance(Some(window), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Recreate;
    use crate::composer_state::{ComposeTab, DraftAsset};
    use crate::test_support::{env, seed_asset, seed_item, seed_request, Seed, TestEnv};
    use gpui::{size, TestAppContext, VisualTestContext};
    use majik_core::model::{EntryId, MediaType, ToolId};
    use majik_generation::{AssetInput, Request};
    use majik_providers::{catalog, AssetRole, ProviderId};

    #[test]
    fn notification_copy_counts() {
        assert_eq!(notification_copy(1, 0), ("Generation complete".into(), "1 ready".into()));
        assert_eq!(notification_copy(2, 1), ("3 generations complete".into(), "2 ready · 1 failed".into()));
        assert_eq!(notification_copy(0, 1), ("Generation complete".into(), "1 failed".into()));
    }

    /// A fresh install opens the composer at the narrowest width its layout allows, leaving the feed
    /// as many columns as possible; the sidebar keeps its roomier default.
    #[test]
    fn the_composer_opens_at_its_narrowest_width() {
        assert_eq!(Side::Composer.default_width(), Side::Composer.width_range().start);
        assert!(Side::Sidebar.default_width() > Side::Sidebar.width_range().start);
    }

    /// The window wrapped in a `Root`, which owns the notification layer.
    fn root_window(cx: &mut TestAppContext) -> (Entity<LibraryWindow>, &mut VisualTestContext) {
        let slot: std::rc::Rc<std::cell::RefCell<Option<Entity<LibraryWindow>>>> = Default::default();
        let slot2 = slot.clone();
        let (_root, vcx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| LibraryWindow::new(window, cx));
            *slot2.borrow_mut() = Some(view.clone());
            Root::new(gpui::AnyView::from(view), window, cx)
        });
        vcx.run_until_parked();
        let view = slot.borrow().clone().expect("view created");
        (view, vcx)
    }

    /// A Library window past onboarding over `images` seeded images, drawn once.
    fn library(cx: &mut TestAppContext, images: usize) -> (TestEnv, Entity<LibraryWindow>, &mut VisualTestContext) {
        let e = env(cx, images, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().onboarding_completed = true);
        let (window, vcx) = root_window(cx);
        draw(vcx);
        (e, window, vcx)
    }

    /// `dispatch_action` doesn't redraw first (keystrokes do): draw so nodes that appeared since
    /// the last frame are in the dispatch tree and `debug_bounds` is current.
    fn draw(vcx: &mut VisualTestContext) {
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// Whether `side` is open, checking that the view, the layout and `Config` agree.
    fn is_open(side: Side, window: &Entity<LibraryWindow>, vcx: &mut VisualTestContext) -> bool {
        draw(vcx);
        let open = window.read_with(vcx, |w, _| w.panel(side).open);
        assert_eq!(vcx.debug_bounds(side.selector()).is_some(), open, "{side:?} is laid out exactly when open");
        assert_eq!(vcx.update(|_, cx| side.saved(cx.global::<Config>()).0), open, "{side:?} persisted");
        open
    }

    fn panel_open(window: &Entity<LibraryWindow>, vcx: &mut VisualTestContext) -> bool {
        is_open(Side::Composer, window, vcx)
    }

    fn laid_out_width(side: Side, vcx: &mut VisualTestContext) -> Pixels {
        draw(vcx);
        vcx.debug_bounds(side.selector()).unwrap_or_else(|| panic!("{side:?} shown")).size.width
    }

    fn resize(side: Side, width: Pixels, window: &Entity<LibraryWindow>, vcx: &mut VisualTestContext) {
        window.update_in(vcx, |w, window, cx| w.resizable.update(cx, |s, cx| s.resize_panel(side.panel_index(), width, window, cx)));
        vcx.run_until_parked();
    }

    fn toggle(side: Side, vcx: &mut VisualTestContext) {
        match side {
            Side::Sidebar => vcx.dispatch_action(ToggleSidebar),
            Side::Composer => vcx.dispatch_action(ToggleComposer),
        }
    }

    fn prompt_focused(window: &Entity<LibraryWindow>, vcx: &mut VisualTestContext) -> bool {
        window.update_in(vcx, |w, window, cx| w.compose.read(cx).contains_focus(window, cx))
    }

    fn feed_focused(window: &Entity<LibraryWindow>, vcx: &mut VisualTestContext) -> bool {
        window.update_in(vcx, |w, window, cx| w.feed.read(cx).focus_handle().is_focused(window))
    }

    fn open_detail(window: &Entity<LibraryWindow>, vcx: &mut VisualTestContext, index: usize) -> Entity<DetailView> {
        let feed = window.read_with(vcx, |w, _| w.feed.clone());
        feed.update(vcx, |f, cx| f.open_at(index, cx));
        vcx.run_until_parked();
        let detail = window.read_with(vcx, |w, _| w.detail.clone()).expect("the feed's Open puts a detail up");
        // Let the open morph finish: it is ticked by `render`, which the test drives by a notify.
        vcx.background_executor.advance_clock(crate::morph::DURATION);
        detail.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
        detail.read_with(vcx, |d, _| assert!(!d.is_transitioning(), "landed"));
        detail
    }

    #[gpui::test]
    fn explicit_appearance_survives_a_window_appearance_change(cx: &mut TestAppContext) {
        use gpui_component::ThemeMode;
        let (_e, _window, vcx) = library(cx, 0);
        // The test platform reports a light window; a saved "dark" must not follow it.
        vcx.update(|window, cx| {
            cx.global_mut::<Config>().appearance = "dark".into();
            Theme::change(ThemeMode::Dark, Some(window), cx);
            follow_system_appearance(window, cx);
            assert!(cx.theme().mode.is_dark(), "explicit dark stays dark");
            cx.global_mut::<Config>().appearance = "system".into();
            follow_system_appearance(window, cx);
            assert!(!cx.theme().mode.is_dark(), "\"system\" follows the window's appearance");
        });
    }

    #[gpui::test]
    fn finished_generations_post_one_system_notification(cx: &mut TestAppContext) {
        use majik_core::images::solid_png;
        use majik_generation::Event;
        let e = env(cx, 0, "Mock");
        cx.update(|cx| cx.set_app_identity(crate::config::bundle_id(), crate::config::app_name()));
        let (view, vcx) = root_window(cx);
        let ids: Vec<_> = e.library.update(vcx, |m, cx| {
            let ids: Vec<_> = (0..3).map(|_| m.lib.add_generating(MediaType::Image, None, None, None, None)).collect();
            m.changed(cx);
            ids
        });
        e.library.update(vcx, |m, cx| {
            m.apply(Event::Completed { id: ids[0].clone(), job: m.attempt(&ids[0].clone()), bytes: solid_png(8, 8, [1, 2, 3]), is_upscaled: false }, cx);
            m.apply(Event::Completed { id: ids[1].clone(), job: m.attempt(&ids[1].clone()), bytes: solid_png(8, 8, [3, 2, 1]), is_upscaled: false }, cx);
            m.apply(Event::Cancelled { id: ids[2].clone(), job: m.attempt(&ids[2].clone()) }, cx);
        });
        view.update(vcx, |w, _| assert_eq!(w.finished, (2, 0), "cancellations don't count"));
        vcx.background_executor.advance_clock(NOTIFY_COALESCE + Duration::from_millis(100));
        vcx.run_until_parked();
        let shown = vcx.shown_system_notifications();
        assert_eq!(shown.len(), 1, "one notification for the batch");
        assert_eq!(shown[0].title.as_ref(), "2 generations complete");
        assert_eq!(shown[0].body.as_ref(), "2 ready");
        view.update(vcx, |w, _| assert_eq!(w.finished, (0, 0)));
    }

    #[gpui::test]
    fn cmd_1_brings_the_library_window_back_from_settings_and_keeps_the_detail(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 2);
        // Register this window as the singleton, as `open_library` does for the real one.
        let handle = vcx.update(|window, _| window.window_handle().downcast::<Root>().expect("root window"));
        vcx.update(|_, cx| cx.global_mut::<crate::windows::Windows>().library = Some(handle));
        let detail = open_detail(&window, vcx, 0);
        draw(vcx);
        window.update_in(vcx, |_, window, cx| detail.read(cx).focus_handle().focus(window, cx));
        vcx.simulate_keystrokes("secondary-,");
        vcx.run_until_parked();
        assert_eq!(vcx.windows().len(), 2, "settings opened");
        assert!(!vcx.update(|window, _| window.is_window_active()), "settings took the focus");
        vcx.simulate_keystrokes("secondary-1");
        vcx.run_until_parked();
        assert_eq!(vcx.windows().len(), 2, "no second Library window");
        assert!(vcx.update(|window, _| window.is_window_active()), "the Library window is forward again");
        assert!(vcx.debug_bounds("compose-panel").is_none(), "the detail stays where it was");
    }

    #[gpui::test]
    fn text_field_copy_cut_paste_edit_the_prompt_not_the_selection(cx: &mut TestAppContext) {
        let (e, window, vcx) = library(cx, 2);
        let feed = window.read_with(vcx, |w, _| w.feed.clone());
        vcx.simulate_keystrokes("secondary-n");
        vcx.simulate_input("a cat");
        vcx.simulate_keystrokes("secondary-a secondary-c");
        assert_eq!(vcx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("a cat"), "⌘C copied the prompt text, not media");
        feed.read_with(vcx, |f, _| assert_eq!(f.selected_count(), 0, "the grid selection is untouched"));
        vcx.simulate_keystrokes("secondary-x");
        window.read_with(vcx, |w, cx| assert_eq!(w.compose.read(cx).prompt_text(cx), "", "⌘X cut the text"));
        vcx.simulate_keystrokes("secondary-v");
        window.read_with(vcx, |w, cx| assert_eq!(w.compose.read(cx).prompt_text(cx), "a cat", "⌘V pasted it back"));
        e.library.read_with(vcx, |m, _| assert_eq!(m.lib.generations().len(), 2, "no media was pasted or copied"));
    }

    fn finish_one(e: &TestEnv, vcx: &mut VisualTestContext) {
        e.library.update(vcx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Image, None, None, None, None);
            m.changed(cx);
            m.apply(majik_generation::Event::Completed { job: m.attempt(&id), id, bytes: majik_core::images::solid_png(8, 8, [1, 2, 3]), is_upscaled: false }, cx);
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn notifications_coalesce_within_the_window_and_split_beyond_it(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        cx.update(|cx| cx.set_app_identity(crate::config::bundle_id(), crate::config::app_name()));
        let (view, vcx) = root_window(cx);
        finish_one(&e, vcx);
        vcx.background_executor.advance_clock(Duration::from_millis(1000));
        vcx.run_until_parked();
        finish_one(&e, vcx);
        vcx.background_executor.advance_clock(Duration::from_millis(1000));
        vcx.run_until_parked();
        assert!(vcx.shown_system_notifications().is_empty(), "the second finish restarted the 1.5 s window");
        view.read_with(vcx, |w, _| assert_eq!(w.finished, (2, 0)));
        vcx.background_executor.advance_clock(Duration::from_millis(600));
        vcx.run_until_parked();
        let shown = vcx.shown_system_notifications();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].title.as_ref(), "2 generations complete");
        // A finish after the flush is a new batch.
        finish_one(&e, vcx);
        vcx.background_executor.advance_clock(NOTIFY_COALESCE + Duration::from_millis(100));
        vcx.run_until_parked();
        let shown = vcx.shown_system_notifications();
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[1].title.as_ref(), "Generation complete");
        assert_eq!(shown[1].body.as_ref(), "1 ready");
    }

    #[gpui::test]
    fn no_notification_while_a_majik_window_is_active(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        cx.update(|cx| cx.set_app_identity(crate::config::bundle_id(), crate::config::app_name()));
        let (view, vcx) = root_window(cx);
        vcx.update(|window, _| window.activate_window());
        vcx.run_until_parked();
        assert!(vcx.update(|window, _| window.is_window_active()));
        finish_one(&e, vcx);
        vcx.background_executor.advance_clock(NOTIFY_COALESCE + Duration::from_millis(100));
        vcx.run_until_parked();
        assert!(vcx.shown_system_notifications().is_empty(), "the user is looking at the result already");
        view.read_with(vcx, |w, _| assert_eq!(w.finished, (0, 0), "the batch is still drained"));
    }

    #[gpui::test]
    fn first_launch_shows_onboarding_and_a_completed_one_skips_it(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| assert!(!cx.global::<Config>().onboarding_completed, "a fresh config"));
        let (window, vcx) = root_window(cx);
        window.read_with(vcx, |w, _| assert!(w.onboarding.is_some(), "first launch: onboarding covers the window"));
        assert!(vcx.debug_bounds("compose-panel").is_none() && vcx.debug_bounds("sidebar-panel").is_none(), "no library chrome behind it");
        // Finishing (or a second launch with the flag saved) drops it for the library.
        vcx.update(|_, cx| update_config(cx, |c| c.onboarding_completed = true));
        vcx.run_until_parked();
        window.read_with(vcx, |w, _| assert!(w.onboarding.is_none()));
        // The debug "Reset Onboarding" clears the flag: the next frame shows it again.
        vcx.update(|_, cx| update_config(cx, |c| c.onboarding_completed = false));
        vcx.run_until_parked();
        window.read_with(vcx, |w, _| assert!(w.onboarding.is_some(), "reset shows onboarding again"));
    }

    #[gpui::test]
    fn resize_persists_frame_after_debounce(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (_view, vcx) = cx.add_window_view(LibraryWindow::new);
        vcx.simulate_resize(size(px(900.), px(700.)));
        vcx.update(|_, cx| assert!(cx.global::<Config>().library_frame.is_none(), "not written before the debounce"));
        vcx.background_executor.advance_clock(std::time::Duration::from_millis(300));
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            let frame = cx.global::<Config>().library_frame.clone().expect("saved");
            assert_eq!((frame.width, frame.height), (900., 700.));
        });
    }

    #[gpui::test]
    fn close_saves_frame(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let (_view, vcx) = cx.add_window_view(LibraryWindow::new);
        assert!(vcx.simulate_close(), "close proceeds");
        vcx.update(|_, cx| assert!(cx.global::<Config>().library_frame.is_some()));
    }

    #[gpui::test]
    fn detail_opens_from_the_feed_and_closes_after_the_morph(cx: &mut TestAppContext) {
        let _env = env(cx, 3, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().onboarding_completed = true);
        let (window, vcx) = root_window(cx);
        let feed = window.read_with(vcx, |w, _| w.feed.clone());
        feed.update(vcx, |f, cx| f.open_at(1, cx));
        vcx.run_until_parked();
        let detail = window.read_with(vcx, |w, _| w.detail.clone()).expect("the feed's Open puts a detail up");
        detail.read_with(vcx, |d, _| assert!(d.is_transitioning(), "growing out of its cell"));
        // Esc during the open morph is ignored; let it finish first. The morph is ticked by
        // `render`, which the app drives per animation frame and the test by a notify.
        vcx.background_executor.advance_clock(crate::morph::DURATION);
        detail.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
        detail.read_with(vcx, |d, _| assert!(!d.is_transitioning(), "landed"));

        detail.update_in(vcx, |d, w, cx| d.back(&crate::actions::Back, w, cx));
        vcx.run_until_parked();
        window.read_with(vcx, |w, _| assert!(w.detail.is_some(), "still up while the close morph plays"));
        detail.read_with(vcx, |d, _| assert!(d.is_transitioning()));

        vcx.background_executor.advance_clock(crate::morph::DURATION);
        vcx.run_until_parked();
        window.read_with(vcx, |w, _| assert!(w.detail.is_none(), "gone once the morph has played"));
    }

    #[gpui::test]
    fn title_bar_row_tops_every_screen(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 3);
        let bar = vcx.debug_bounds("title-bar").expect("the library screen has the title bar");
        assert_eq!(bar.origin.y, px(0.));
        assert_eq!(bar.size.height, gpui_component::TITLE_BAR_HEIGHT, "Zed's 34px row");
        assert_eq!(bar.size.width, vcx.update(|window, _| window.viewport_size().width), "spans the window");
        assert!(vcx.debug_bounds("sidebar-panel").expect("sidebar").origin.y >= bar.bottom(), "the panels start under it");

        open_detail(&window, vcx, 0);
        draw(vcx);
        assert_eq!(vcx.debug_bounds("title-bar").map(|b| b.size.height), Some(gpui_component::TITLE_BAR_HEIGHT), "kept over a detail");

        vcx.update(|_, cx| update_config(cx, |c| c.onboarding_completed = false));
        draw(vcx);
        window.read_with(vcx, |w, _| assert!(w.onboarding.is_some()));
        assert_eq!(vcx.debug_bounds("title-bar").map(|b| b.size.height), Some(gpui_component::TITLE_BAR_HEIGHT), "kept over onboarding");
    }

    #[gpui::test]
    fn title_bar_toggles_open_and_close_the_panels(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        assert!(is_open(Side::Sidebar, &window, vcx) && panel_open(&window, vcx));
        for (selector, side) in [("title-sidebar", Side::Sidebar), ("title-composer", Side::Composer)] {
            let button = vcx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} in the title bar"));
            assert!(button.bottom() <= gpui_component::TITLE_BAR_HEIGHT, "{selector} sits in the title bar row");
            vcx.simulate_click(button.center(), gpui::Modifiers::default());
            assert!(!is_open(side, &window, vcx), "{side:?} closed by its title bar toggle");
            vcx.simulate_click(button.center(), gpui::Modifiers::default());
            assert!(is_open(side, &window, vcx), "{side:?} reopened by its title bar toggle");
        }
    }

    #[gpui::test]
    fn title_bar_toggles_leave_while_a_cover_is_up(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 2);
        assert!(vcx.debug_bounds("title-sidebar").is_some() && vcx.debug_bounds("title-composer").is_some());

        let detail = open_detail(&window, vcx, 0);
        draw(vcx);
        assert!(vcx.debug_bounds("title-sidebar").is_none() && vcx.debug_bounds("title-composer").is_none(), "nothing to toggle under a detail");
        detail.update_in(vcx, |d, w, cx| d.back(&crate::actions::Back, w, cx));
        vcx.background_executor.advance_clock(crate::morph::DURATION);
        draw(vcx);
        window.read_with(vcx, |w, _| assert!(w.detail.is_none()));
        assert!(vcx.debug_bounds("title-sidebar").is_some() && vcx.debug_bounds("title-composer").is_some(), "back with the library screen");

        vcx.update(|_, cx| update_config(cx, |c| c.onboarding_completed = false));
        draw(vcx);
        assert!(vcx.debug_bounds("title-sidebar").is_none() && vcx.debug_bounds("title-composer").is_none(), "nothing to toggle under onboarding");
    }

    #[gpui::test]
    fn both_panels_are_open_by_default_with_the_feed_focused(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        for side in Side::ALL {
            assert!(is_open(side, &window, vcx));
            assert_eq!(laid_out_width(side, vcx), side.default_width());
        }
        assert!(feed_focused(&window, vcx), "the composer doesn't take focus at launch");
        assert!(!prompt_focused(&window, vcx));
    }

    /// Press a feed cell, drag it past GPUI's threshold onto the composer's reference card and let
    /// go: the composer's draft references the cell's output asset, and nothing is left dragging.
    #[gpui::test]
    fn dragging_a_feed_cell_onto_the_composer_attaches_its_asset(cx: &mut TestAppContext) {
        let (e, window, vcx) = library(cx, 2);
        vcx.simulate_resize(gpui::size(px(1400.), px(900.)));
        draw(vcx);
        let (feed, compose) = window.read_with(vcx, |w, _| (w.feed.clone(), w.compose.clone()));
        let first = feed.read_with(vcx, |f, _| f.entry_ids()[0].clone());
        let output = e.library.read_with(vcx, |m, _| m.lib.get(first.media().unwrap()).unwrap().output_asset_id.clone().unwrap());
        let cell = feed.read_with(vcx, |f, _| f.cell_bounds(&first)).expect("first cell drawn");
        let card = vcx.debug_bounds("asset-add-reference_image").expect("the reference card is drawn");
        let press = cell.center();
        vcx.simulate_mouse_down(press, gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_move(press + gpui::point(px(12.), px(12.)), gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(vcx.update(|_, cx| cx.has_active_drag()));
        vcx.simulate_mouse_move(card.center(), gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        vcx.simulate_mouse_up(card.center(), gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(!vcx.update(|_, cx| cx.has_active_drag()), "the drop ended the drag");
        let draft = compose.read_with(vcx, |c, _| c.draft_assets());
        assert_eq!(draft.len(), 1);
        assert_eq!((draft[0].asset.clone(), draft[0].role), (output, majik_providers::AssetRole::ReferenceImage));
        e.library.read_with(vcx, |m, _| assert_eq!(m.lib.assets().len(), 2, "referenced, not copied"));
    }

    #[gpui::test]
    fn panels_show_on_every_library_screen(cx: &mut TestAppContext) {
        let (e, window, vcx) = library(cx, 2);
        let album = e.library.update(vcx, |m, cx| {
            let id = m.lib.create_album("Cats");
            m.changed(cx);
            id
        });
        let sidebar = window.read_with(vcx, |w, _| w.sidebar.clone());
        let screens = [FeedFilter::Favorites, FeedFilter::Album(album), FeedFilter::Assets, FeedFilter::Library];
        for filter in screens {
            sidebar.update(vcx, |s, cx| s.select(filter.clone(), cx));
            for side in Side::ALL {
                assert!(is_open(side, &window, vcx), "{side:?} on {filter:?}");
            }
        }
    }

    #[gpui::test]
    fn toggle_sidebar_collapses_expands_and_persists(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        vcx.dispatch_action(ToggleSidebar);
        assert!(!is_open(Side::Sidebar, &window, vcx));
        assert!(is_open(Side::Composer, &window, vcx), "independent of the composer");
        vcx.simulate_keystrokes("secondary-alt-s");
        assert!(is_open(Side::Sidebar, &window, vcx));
        vcx.simulate_keystrokes("secondary-alt-s");
        assert!(!is_open(Side::Sidebar, &window, vcx));
        // With the sidebar away, Settings is still one keystroke off.
        vcx.simulate_keystrokes("secondary-,");
        vcx.run_until_parked();
        assert!(vcx.update(|_, cx| cx.global::<crate::windows::Windows>().settings.is_some()), "settings window opened");
    }

    #[gpui::test]
    fn collapsing_one_side_keeps_the_other_at_its_width(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        for (hidden, kept) in [(Side::Composer, Side::Sidebar), (Side::Sidebar, Side::Composer)] {
            toggle(hidden, vcx);
            assert!(!is_open(hidden, &window, vcx));
            assert_eq!(laid_out_width(kept, vcx), kept.default_width(), "{kept:?} doesn't grow into {hidden:?}'s space");
            toggle(hidden, vcx);
            assert_eq!(laid_out_width(hidden, vcx), hidden.default_width());
        }
    }

    #[gpui::test]
    fn toggle_composer_collapses_expands_and_persists(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        vcx.dispatch_action(ToggleComposer);
        assert!(!panel_open(&window, vcx));
        vcx.dispatch_action(ToggleComposer);
        assert!(panel_open(&window, vcx));
        assert!(prompt_focused(&window, vcx), "reopening is an invitation to type");
        vcx.dispatch_action(ToggleComposer);
        assert!(!panel_open(&window, vcx));
        assert!(feed_focused(&window, vcx), "focus leaves the hidden panel");
    }

    #[gpui::test]
    fn new_composition_puts_the_panel_away_when_already_typing(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        vcx.simulate_keystrokes("secondary-n");
        assert!(prompt_focused(&window, vcx));
        vcx.simulate_keystrokes("secondary-n");
        assert!(!panel_open(&window, vcx));
        assert!(feed_focused(&window, vcx));
        vcx.simulate_keystrokes("secondary-n");
        assert!(panel_open(&window, vcx));
        assert!(prompt_focused(&window, vcx));
    }

    /// A saved closed state with an out-of-range width comes back closed, and clamped when shown.
    fn restores_saved_state(cx: &mut TestAppContext, side: Side) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| {
            let config = cx.global_mut::<Config>();
            config.onboarding_completed = true;
            side.save_open(config, false);
            side.save_width(config, px(9999.));
        });
        let (window, vcx) = root_window(cx);
        assert!(!is_open(side, &window, vcx));
        let max = side.width_range().end;
        window.read_with(vcx, |w, _| assert_eq!(w.panel(side).width, max, "clamped into the handle's range"));
        toggle(side, vcx);
        assert_eq!(laid_out_width(side, vcx), max);
    }

    #[gpui::test]
    fn saved_composer_state_is_restored(cx: &mut TestAppContext) {
        restores_saved_state(cx, Side::Composer);
    }

    #[gpui::test]
    fn saved_sidebar_state_is_restored(cx: &mut TestAppContext) {
        restores_saved_state(cx, Side::Sidebar);
    }

    #[gpui::test]
    fn new_composition_expands_the_panel_and_focuses_the_prompt(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        vcx.dispatch_action(ToggleComposer);
        assert!(!panel_open(&window, vcx));
        vcx.simulate_keystrokes("secondary-n");
        assert!(panel_open(&window, vcx));
        assert!(prompt_focused(&window, vcx));
        // Already open: ⌘N just focuses.
        window.update_in(vcx, |w, window, cx| w.feed.read(cx).focus_handle().focus(window, cx));
        vcx.simulate_keystrokes("secondary-n");
        assert!(panel_open(&window, vcx));
        assert!(prompt_focused(&window, vcx));
    }

    #[gpui::test]
    fn escape_in_the_prompt_returns_focus_to_the_feed_without_collapsing(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 1);
        vcx.simulate_keystrokes("secondary-n");
        assert!(prompt_focused(&window, vcx));
        vcx.simulate_keystrokes("escape");
        assert!(feed_focused(&window, vcx));
        assert!(panel_open(&window, vcx));
    }

    #[gpui::test]
    fn feed_shortcuts_do_not_fire_while_the_prompt_is_focused(cx: &mut TestAppContext) {
        let (_e, window, vcx) = library(cx, 3);
        vcx.simulate_keystrokes("secondary-n");
        vcx.simulate_input("a cat");
        vcx.simulate_keystrokes("secondary-a");
        let feed = window.read_with(vcx, |w, _| w.feed.clone());
        feed.read_with(vcx, |f, _| assert_eq!(f.selected_count(), 0, "⌘A selected the prompt text, not the grid"));
        window.read_with(vcx, |w, cx| assert_eq!(w.compose.read(cx).prompt_text(cx), "a cat"));
        // And the other way round: with the feed focused, ⌘A is the grid's.
        vcx.simulate_keystrokes("escape secondary-a");
        feed.read_with(vcx, |f, _| assert_eq!(f.selected_count(), 3));
    }

    #[gpui::test]
    fn generate_from_the_panel_inserts_placeholder_rows(cx: &mut TestAppContext) {
        let (e, window, vcx) = library(cx, 0);
        vcx.simulate_keystrokes("secondary-n");
        vcx.simulate_input("a cat");
        vcx.simulate_keystrokes("secondary-enter");
        e.library.read_with(vcx, |m, _| assert_eq!(m.lib.in_flight().len(), 1, "one generating row"));
        window.read_with(vcx, |w, cx| assert_eq!(w.compose.read(cx).prompt_text(cx), "", "prompt cleared after submit"));
        assert!(panel_open(&window, vcx), "the composer stays open while generating");
    }

    #[gpui::test]
    fn selected_album_is_the_generation_target(cx: &mut TestAppContext) {
        let (e, window, vcx) = library(cx, 0);
        let album = e.library.update(vcx, |m, cx| {
            let id = m.lib.create_album("Cats");
            m.changed(cx);
            id
        });
        let sidebar = window.read_with(vcx, |w, _| w.sidebar.clone());
        sidebar.update(vcx, |s, cx| s.select(FeedFilter::Album(album.clone()), cx));
        vcx.simulate_keystrokes("secondary-n");
        vcx.simulate_input("a cat");
        vcx.simulate_keystrokes("secondary-enter");
        e.library.read_with(vcx, |m, _| {
            let generating = m.lib.in_flight();
            assert_eq!(generating.len(), 1);
            assert!(m.lib.album(&album).expect("album").items.contains(&generating[0].id), "landed in the selected album");
        });
        // Back to the whole library: new generations go nowhere in particular.
        sidebar.update(vcx, |s, cx| s.select(FeedFilter::Library, cx));
        vcx.simulate_input("another");
        vcx.simulate_keystrokes("secondary-enter");
        e.library.read_with(vcx, |m, _| {
            assert_eq!(m.lib.in_flight().len(), 2);
            assert_eq!(m.lib.album(&album).expect("album").items.len(), 1);
        });
    }

    /// Press a cell, move past GPUI's drag threshold, release over the album's sidebar row: the
    /// generation is filed into the album, the way a user does it.
    #[gpui::test]
    fn dragging_a_cell_onto_an_album_row_files_it_there(cx: &mut TestAppContext) {
        let (e, window, vcx) = library(cx, 2);
        vcx.simulate_resize(size(px(1000.), px(700.)));
        let album = e.library.update(vcx, |m, cx| {
            let id = m.lib.create_album("Cats");
            m.changed(cx);
            id
        });
        draw(vcx);
        let ids = e.library.read_with(vcx, |m, _| m.lib.feed(&FeedFilter::Library, majik_core::MediaFilter::All));
        let feed = window.read_with(vcx, |w, _| w.feed.clone());
        let cell = feed.read_with(vcx, |f, _| f.cell_bounds(&EntryId::Generation(ids[0].clone())).expect("first cell drawn"));
        // `debug_bounds` wants a `'static` selector; the test's leak is one short string.
        let selector: &'static str = Box::leak(format!("album-drop-{}", album.0).into_boxed_str());
        let row = vcx.debug_bounds(selector).expect("the album row is a drop target");

        let press = cell.center();
        vcx.simulate_mouse_down(press, gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_move(press + gpui::point(px(12.), px(12.)), gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(vcx.update(|_, cx| cx.has_active_drag()), "the cell is being dragged");
        vcx.simulate_mouse_move(row.center(), gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        vcx.simulate_mouse_up(row.center(), gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();

        assert!(!vcx.update(|_, cx| cx.has_active_drag()));
        e.library.read_with(vcx, |m, _| assert_eq!(m.lib.album(&album).expect("album").items, vec![ids[0].clone()], "the dragged cell joined the album"));
        assert_eq!(feed.read_with(vcx, |f, _| f.filter().clone()), FeedFilter::Library, "a drop doesn't open the album");
    }

    #[gpui::test]
    fn recreate_from_the_feed_expands_the_panel_and_loads_the_request(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().onboarding_completed = true);
        seed_item(&e.library, cx, Seed::default());
        let (window, vcx) = root_window(cx);
        vcx.dispatch_action(ToggleComposer);
        assert!(!panel_open(&window, vcx));
        vcx.simulate_keystrokes("secondary-a");
        vcx.dispatch_action(Recreate);
        assert!(panel_open(&window, vcx));
        assert!(prompt_focused(&window, vcx));
        window.read_with(vcx, |w, cx| assert_eq!(w.compose.read(cx).prompt_text(cx), "seeded", "the stored request's prompt"));
    }

    #[gpui::test]
    fn recreate_of_an_upscale_row_opens_the_upscale_tab_with_its_input(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().onboarding_completed = true);
        // What the Upscale menu leaves behind: a tool row whose request names the model and whose
        // one input is the image it ran over.
        let input = seed_asset(&e.library, cx, MediaType::Image, 9);
        let request = Request::tool(ProviderId::mock(), &catalog::tool::MOCK_UPSCALE, AssetInput::new(AssetRole::ReferenceImage, "image/png", vec![]));
        seed_request(&e.library, cx, &request, &[("reference_image", input.clone())]);
        let (window, vcx) = root_window(cx);
        assert!(panel_open(&window, vcx));
        window.update_in(vcx, |w, window, cx| w.compose.update(cx, |c, cx| c.focus_prompt(window, cx)));
        vcx.simulate_input("keep me");
        vcx.simulate_keystrokes("escape");
        vcx.simulate_keystrokes("secondary-a");
        vcx.dispatch_action(Recreate);
        assert!(panel_open(&window, vcx));
        assert!(prompt_focused(&window, vcx));
        window.read_with(vcx, |w, cx| {
            let compose = w.compose.read(cx);
            assert_eq!(compose.composer_state().tab, ComposeTab::Tool(ToolId::Upscale));
            assert_eq!(compose.composer_state().active_tool_model().map(|m| m.id), Some("mock-upscale"));
            assert_eq!(compose.draft_assets(), vec![DraftAsset { asset: input, role: AssetRole::ReferenceImage }], "the one image, ready to run again");
            assert_eq!(compose.prompt_text(cx), "keep me", "a tool has no prompt to load over what is typed");
        });
    }

    /// A dragged width is written to `Config` and a fresh window (relaunch) comes up at it.
    fn width_persists(cx: &mut TestAppContext, side: Side, width: Pixels) {
        let (_e, window, vcx) = library(cx, 0);
        resize(side, width, &window, vcx);
        window.read_with(vcx, |w, cx| {
            assert_eq!(w.panel(side).width, width);
            assert_eq!(side.saved(cx.global::<Config>()).1, Some(f32::from(width)));
        });
        assert_eq!(laid_out_width(side, vcx), width);
        let (again, vcx) = root_window(cx);
        again.read_with(vcx, |w, _| assert_eq!(w.panel(side).width, width));
        assert_eq!(laid_out_width(side, vcx), width);
    }

    #[gpui::test]
    fn composer_width_persists_and_is_restored(cx: &mut TestAppContext) {
        width_persists(cx, Side::Composer, px(500.));
    }

    #[gpui::test]
    fn sidebar_width_persists_and_is_restored(cx: &mut TestAppContext) {
        width_persists(cx, Side::Sidebar, px(300.));
    }

    /// Dragging one side's handle while the other is hidden must not be corrected against the
    /// hidden panel's saved width, and showing it again keeps both.
    fn drag_while_other_hidden(cx: &mut TestAppContext, hidden: Side, dragged: Side, width: Pixels) {
        let (_e, window, vcx) = library(cx, 0);
        toggle(hidden, vcx);
        draw(vcx);
        resize(dragged, width, &window, vcx);
        assert_eq!(laid_out_width(dragged, vcx), width, "{dragged:?} not snapped by the phantom {hidden:?}");
        toggle(hidden, vcx);
        assert_eq!(laid_out_width(dragged, vcx), width);
        assert_eq!(laid_out_width(hidden, vcx), hidden.default_width());
    }

    #[gpui::test]
    fn sidebar_drag_while_the_composer_is_hidden_keeps_the_sidebar_width(cx: &mut TestAppContext) {
        drag_while_other_hidden(cx, Side::Composer, Side::Sidebar, px(260.));
    }

    #[gpui::test]
    fn composer_drag_while_the_sidebar_is_hidden_keeps_the_composer_width(cx: &mut TestAppContext) {
        drag_while_other_hidden(cx, Side::Sidebar, Side::Composer, px(520.));
    }
}
