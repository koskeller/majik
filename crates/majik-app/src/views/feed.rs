//! Feed grid: virtualized square cells, Finder-style selection, zoom, filter, context menu.

use gpui::{
    prelude::*, point, px, App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Image, ImageFormat, MouseButton, Point,
    ExternalDragPayload, ExternalPaths, FileDragPaths, MouseDownEvent, PathPromptOptions, Pixels, PromptLevel, ScrollHandle, SharedString, Task, Window, relative,
};
use std::cell::RefCell;
use std::rc::Rc;
use gpui_component::button::{ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _};
use majik_core::model::{Asset, AssetId, Entry, EntryId, GenerationId, Generation, MediaType, Status};
use std::path::{Path, PathBuf};
use majik_core::{feed, thumbnails, FeedFilter, MediaFilter, Modifiers, Selection};
use std::collections::HashMap;
use std::time::Instant;

use crate::actions::*;
use crate::config::{update_config, Config, ThumbnailShape};
use crate::grid_motion::{CellStyle, Change, Ghost, GridMotion, Place, Visual};
use crate::image_cache::LruImageCache;
use crate::state::{self, DraggedAsset, DraggedAssets, LibraryModel, PendingCompose};
use crate::ui::{BoundsSlot, bounds_slot, button, cover_image, format_duration, icon, measure_then, now, record_bounds, slot_size, spin, toolbar};

/// The floating thumbnail that follows the pointer while cells are dragged out (in-app and
/// promoted native drags). GPUI anchors the drag view where the pressed cell's top-left was
/// relative to the pointer, so the box is offset by `press_offset` to sit centred under the cursor
/// instead, as Finder and Photos do.
struct DragPreview {
    image: Option<std::path::PathBuf>,
    count: usize,
    /// Where inside the pressed cell the drag started.
    press_offset: Point<Pixels>,
}

impl DragPreview {
    const SIZE: Pixels = px(96.);

    /// Top-left of the preview box relative to the drag view's origin (the cell's top-left).
    fn box_origin(&self) -> Point<Pixels> {
        self.press_offset - point(Self::SIZE / 2., Self::SIZE / 2.)
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let size = Self::SIZE;
        let origin = self.box_origin();
        gpui::div().relative().size_0().child(
            gpui::div()
            .absolute()
            .left(origin.x)
            .top(origin.y)
            .w(size)
            .h(size)
            .rounded_md()
            .overflow_hidden()
            .bg(gpui::black().opacity(0.25))
            .when_some(self.image.clone(), |d, path| d.child(cover_image(path)))
            .when(self.count > 1, |d| {
                d.child(
                    gpui::div()
                        .absolute()
                        .top_1()
                        .right_1()
                        .min_w(px(20.))
                        .h(px(20.))
                        .px_1()
                        .rounded_full()
                        .bg(gpui::rgb(0x2563eb))
                        .text_xs()
                        .text_color(gpui::white())
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(format!("{}", self.count)),
                )
            }),
        )
    }
}

pub enum FeedEvent {
    /// Open the detail on `ids[index]`. `origin` is the cell's box in window coordinates, for the
    /// detail to grow out of; `None` when the cell wasn't on screen (keyboard open after scrolling).
    Open { ids: Vec<EntryId>, index: usize, origin: Option<Bounds<Pixels>> },
    /// Hand the selection to the composer panel (Recreate / Use Image); the owner shows the panel.
    Compose(PendingCompose),
    /// Files were just imported as assets; the owner shows the Assets feed, where they landed.
    Imported,
}


/// What a drawn cell was drawn from: an owned copy of its entry, so exit ghosts and the zoom
/// crossfade can keep drawing it after the library has changed.
#[derive(Clone, Debug)]
enum CellSnapshot {
    Media(Generation),
    Asset(Asset),
}

impl CellSnapshot {
    fn id(&self) -> EntryId {
        self.entry().id()
    }

    fn entry(&self) -> Entry<'_> {
        match self {
            CellSnapshot::Media(item) => Entry::Generation(item),
            CellSnapshot::Asset(asset) => Entry::Asset(asset),
        }
    }
}

impl From<Entry<'_>> for CellSnapshot {
    fn from(entry: Entry<'_>) -> Self {
        match entry {
            Entry::Generation(item) => CellSnapshot::Media(item.clone()),
            Entry::Asset(asset) => CellSnapshot::Asset(asset.clone()),
        }
    }
}

pub struct FeedView {
    /// Square-cropped or whole thumbnails (persisted in `Config`).
    shape: ThumbnailShape,
    filter: FeedFilter,
    media_filter: MediaFilter,
    /// The toolbar's favorites-only toggle: like `media_filter`, the grid's own and never stored.
    favorites_only: bool,
    /// Zoom level: the minimum tile width (persisted in `Config`).
    zoom: u32,
    /// Columns the current layout uses — derived from `zoom` and the measured width, never set
    /// directly; `relayout` is the only writer.
    columns: usize,
    ids: Vec<EntryId>,
    selection: Selection<EntryId>,
    last_click: Option<(EntryId, Instant)>,
    content_size: BoundsSlot,
    scroll: ScrollHandle,
    focus: FocusHandle,
    library: Entity<LibraryModel>,
    image_cache: Entity<LruImageCache>,
    debug_open: Option<usize>,
    ticker: Option<gpui::Task<()>>,
    cell_px: gpui::Pixels,
    /// Left-button-down on a cell: (id, window position) until a drag threshold is crossed.
    /// Plain-click on an item already in a multi-selection: collapse to it on mouse-up, unless a
    /// drag starts first (so dragging a member of a multi-selection drags the whole set).
    deferred_click: Option<(EntryId, usize)>,
    /// Cell transitions (pop-in/out, filter fade, zoom crossfade, thumbnail fade); see `grid_motion`.
    motion: GridMotion<CellSnapshot, EntryId>,
    /// The cells the last `uniform_list` pass drew: a render cache, not the source of truth. An
    /// exit ghost keeps drawing from this snapshot after the library has already dropped the item.
    last_rendered: HashMap<EntryId, CellSnapshot>,
    /// The window's scale factor as of the last render: cells are sized in points, thumbnails in
    /// device pixels, and the tier depends on the second.
    scale_factor: f32,
    /// Deferred large-tier request (cancel-on-drop), so flinging through the library doesn't queue
    /// a thumbnail render for every row it passes — only for the rows the scroll settles on.
    large_request: Option<Task<()>>,
    /// What the last render drew: scroll offset, cell side and columns. A change means different
    /// rows are on screen, which is when the large tier is worth asking about.
    last_viewport: Option<(Pixels, Pixels, usize)>,
    /// Window-space boxes of the cells drawn last frame, for the detail's open/close morph.
    cell_bounds: Rc<RefCell<HashMap<EntryId, Bounds<Pixels>>>>,
}

impl EventEmitter<FeedEvent> for FeedView {}

const GAP: f32 = feed::GRID_GAP;

/// Height of the badges in a cell's bottom corners (favourite, duration, HD).
const CELL_BADGE_HEIGHT: Pixels = px(18.);

/// A cell's bottom-corner badge strip. The caller picks the corner (`.left_1()` / `.right_1()`)
/// and fills it with [`cell_badge`]s: they lay out side by side, so a clip that is also upscaled
/// shows its length and HD next to each other instead of one on top of the other.
fn cell_badges() -> gpui::Div {
    h_flex().absolute().bottom_1().gap_1()
}

/// The badges a cell draws in its bottom-right corner, in order: how long a clip runs, then HD
/// when its output came out of an upscaler. An upscaled clip carries both, which is why they sit
/// in a strip rather than in the corner on top of each other.
fn right_badges(duration_secs: Option<f64>, media_type: MediaType, is_upscaled: bool) -> Vec<SharedString> {
    let mut badges = Vec::new();
    if let Some(secs) = duration_secs.filter(|_| media_type != MediaType::Image) {
        badges.push(format_duration(secs).into());
    }
    if is_upscaled {
        badges.push("HD".into());
    }
    badges
}

/// A pill in a badge strip: one height and one backdrop for every badge, whether it holds an icon
/// or text.
fn cell_badge() -> gpui::Div {
    gpui::div()
        .flex_none()
        .h(CELL_BADGE_HEIGHT)
        .min_w(CELL_BADGE_HEIGHT)
        .px_1p5()
        .rounded_full()
        .bg(gpui::black().opacity(0.55))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(gpui::white())
}

/// How long the feed has to be still before the large thumbnail tier is rendered for what is on
/// screen. Long enough that a fling costs nothing, short enough to feel immediate when you stop.
const LARGE_TIER_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Cell side for `columns` across `width` (with 2 pt gutters).
fn cell_for(width: Pixels, columns: usize) -> Pixels {
    let columns = columns.max(1);
    if width > px(0.) {
        (width - px(GAP) * (columns as f32 - 1.)) / columns as f32
    } else {
        px(160.)
    }
}

/// Pixel position of a place in the grid content for the current width.
fn visual_for(width: Pixels, place: Place) -> Visual {
    let columns = place.columns.max(1);
    let size = f32::from(cell_for(width, columns));
    let pitch = size + GAP;
    Visual { x: (place.index % columns) as f32 * pitch, y: (place.index / columns) as f32 * pitch, size }
}

impl FeedView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let library = state::library(cx);
        cx.observe(&library, |this, _, cx| this.refresh(Change::Library, cx)).detach();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let zoom = feed::sanitize_zoom(cx.global::<Config>().grid_zoom);
        let shape = cx.global::<Config>().thumbnail_shape;
        let mut this = Self {
            shape,
            filter: FeedFilter::Library,
            media_filter: MediaFilter::All,
            favorites_only: false,
            zoom,
            // The width isn't measured yet; the first render lays the grid out for real.
            columns: feed::columns_for(0., zoom),
            ids: Vec::new(),
            selection: Selection::default(),
            last_click: None,
            content_size: bounds_slot(),
            scroll: ScrollHandle::new(),
            focus,
            library,
            image_cache: LruImageCache::new(cx),
            debug_open: None,
            ticker: None,
            cell_px: px(160.),
            deferred_click: None,
            // Disabled for the initial load; `render` enables it from then on.
            motion: GridMotion::new(false),
            last_rendered: HashMap::new(),
            scale_factor: 1.,
            large_request: None,
            last_viewport: None,
            cell_bounds: Rc::new(RefCell::new(HashMap::new())),
        };
        this.refresh(Change::Library, cx);
        if let Ok(v) = std::env::var("MAJIK_OPEN") {
            this.debug_open = if v == "video" || v == "audio" {
                let want = if v == "video" { MediaType::Video } else { MediaType::Audio };
                let lib = this.library.read(cx);
                this.ids.iter().position(|id| lib.lib.entry(id).is_some_and(|e| e.kind() == want))
            } else {
                v.parse::<usize>().ok()
            };
        }
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn focus(&self, cx: &mut Context<Self>) {
        let handle = self.focus.clone();
        cx.defer(move |cx| {
            if let Some(window) = cx.active_window() {
                window.update(cx, |_, window, cx| handle.focus(window, cx)).ok();
            }
        });
    }

    #[cfg(test)]
    pub fn filter(&self) -> &FeedFilter {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: FeedFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.selection.clear();
        self.refresh(Change::Filter, cx);
    }

    pub fn set_favorites_only(&mut self, favorites_only: bool, cx: &mut Context<Self>) {
        if self.favorites_only == favorites_only {
            return;
        }
        self.favorites_only = favorites_only;
        self.refresh(Change::Filter, cx);
    }

    fn refresh(&mut self, change: Change, cx: &mut Context<Self>) {
        let now = now(cx);
        let ids = self.library.read(cx).lib.entries(&self.filter, self.media_filter, self.favorites_only);
        if ids != self.ids {
            // The boxes recorded by the last frame belong to the old layout. The grid may not be
            // drawn again before someone asks for one (the detail closing after a delete), so
            // forget them rather than hand out a cell that has since moved.
            self.cell_bounds.borrow_mut().clear();
        }
        self.ids = ids;
        let rendered = &self.last_rendered;
        let width = slot_size(&self.content_size).width;
        self.motion.apply(&self.ids, self.columns, change, |id| rendered.get(id).cloned(), |place| visual_for(width, place), now);
        let lib = &self.library.read(cx).lib;
        self.motion.sync_thumbnails(self.ids.iter().map(|id| (id, lib.entry(id).is_some_and(|e| e.thumbnail().is_some()))), now);
        self.selection.retain_in(&self.ids);
        // A row that just finished, or just got its standard thumbnail, moves nothing the render's
        // viewport check watches, so it would draw that thumbnail stretched until the next scroll.
        // Debounced, and rows already on the large tier are skipped, so a busy library costs
        // one pass over the visible ids.
        self.schedule_large_thumbnails(cx);
        // Keep the elapsed timers on generating cells moving (1 Hz) while any are in flight.
        let generating = self.library.read(cx).lib.in_flight().len();
        if generating > 0 && self.ticker.is_none() {
            self.ticker = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let keep = this.update(cx, |f, cx| {
                    let n = f.library.read(cx).lib.in_flight().len();
                    cx.notify();
                    n > 0
                });
                if !matches!(keep, Ok(true)) {
                    this.update(cx, |f, _| f.ticker = None).ok();
                    break;
                }
            }));
        }
        cx.notify();
    }

    fn title(&self, cx: &App) -> String {
        match &self.filter {
            FeedFilter::Library => "Library".into(),
            FeedFilter::Favorites => "Favorites".into(),
            FeedFilter::Album(id) => self.library.read(cx).lib.album(id).map(|a| a.name.clone()).unwrap_or_else(|| "Album".into()),
            FeedFilter::Assets => "Assets".into(),
        }
    }

    fn current_album(&self) -> Option<majik_core::model::AlbumId> {
        match &self.filter {
            FeedFilter::Album(id) => Some(id.clone()),
            _ => None,
        }
    }

    /// The selected generations (asset entries have none).
    fn selected_ids(&self) -> Vec<GenerationId> {
        self.selection.ids.iter().filter_map(|id| id.media().cloned()).collect()
    }

    /// How many cells are selected; what the menu greys its media items on.
    pub(crate) fn selected_count(&self) -> usize {
        self.selection.ids.len()
    }

    #[cfg(test)]
    pub(crate) fn entry_ids(&self) -> Vec<EntryId> {
        self.ids.clone()
    }

    /// The on-disk paths a drag from `id` should carry (the selection if `id` is in it, else just
    /// `id`). Only completed items have a file to drag.
    #[cfg(test)]
    fn drag_paths(&self, id: &EntryId, cx: &App) -> Vec<PathBuf> {
        self.dragged(id, cx).paths()
    }

    /// What a drag from `id` carries: the assets behind the selection when `id` is in it, else
    /// just `id`'s. Only entries with a file are dragged (a generation in flight has none).
    fn dragged(&self, id: &EntryId, cx: &App) -> DraggedAssets {
        let lib = &self.library.read(cx).lib;
        let ids: Vec<EntryId> = if self.selection.contains(id) { self.selection.ids.iter().cloned().collect() } else { vec![id.clone()] };
        let assets = ids
            .iter()
            .filter_map(|i| lib.entry(i))
            .filter_map(|entry| {
                let path = entry.file()?.to_path_buf();
                let (id, generation) = match entry {
                    Entry::Generation(item) => (item.output_asset_id.clone()?, Some(item.id.clone())),
                    Entry::Asset(asset) => (asset.id.clone(), lib.generation_producing(&asset.id)),
                };
                Some(DraggedAsset { id, kind: entry.kind(), path, generation })
            })
            .collect();
        DraggedAssets { assets }
    }

    /// The selected generations, for the actions that only apply to them.
    fn selected_items(&self, cx: &App) -> Vec<Generation> {
        let lib = &self.library.read(cx).lib;
        self.selection.ids.iter().filter_map(|id| id.media()).filter_map(|id| lib.get(id).cloned()).collect()
    }

    /// The selected assets (the Assets feed).
    fn selected_assets(&self, cx: &App) -> Vec<Asset> {
        let lib = &self.library.read(cx).lib;
        self.selection.ids.iter().filter_map(|id| id.asset()).filter_map(|id| lib.asset(id).cloned()).collect()
    }

    /// The files behind the selection, whichever kind of entry: what Copy / Save / Reveal
    /// work on.
    fn selected_exportables(&self, cx: &App) -> Vec<Exportable> {
        let lib = &self.library.read(cx).lib;
        self.selection.ids.iter().filter_map(|id| lib.entry(id)).filter_map(|entry| Exportable::of_entry(&entry)).collect()
    }

    /// Import files as assets (Import… and drops on the Assets grid); the user hears about every
    /// file that couldn't be, or how many were.
    pub(crate) fn import_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let (ids, failures) = self.library.update(cx, |m, cx| m.import_files(&paths, cx));
        if !failures.is_empty() {
            crate::ui::toast(window, failures.join("\n"), cx);
        } else {
            crate::ui::toast(window, format!("Imported {} asset(s)", ids.len()), cx);
        }
        if !ids.is_empty() {
            cx.emit(FeedEvent::Imported);
        }
    }

    pub(crate) fn pick_imports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Import".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |feed, window, cx| feed.import_paths(paths, window, cx)).ok();
            }
        })
        .detach();
    }

    // ----- mouse ------------------------------------------------------------

    fn cell_mouse_down(&mut self, ix: usize, id: &EntryId, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let now = Instant::now();
        let double = ev.click_count >= 2
            || feed::is_double_click(self.last_click.as_ref().map(|(i, _)| i), id, self.last_click.as_ref().map(|(_, t)| *t).unwrap_or(now), now);
        if double && !ev.modifiers.platform && !ev.modifiers.shift {
            self.open_at(ix, cx);
            self.last_click = None;
            return;
        }
        self.last_click = Some((id.clone(), now));
        let mods = Modifiers { command: ev.modifiers.platform, shift: ev.modifiers.shift };
        let plain = !mods.command && !mods.shift;
        if plain && self.selection.contains(id) && self.selection.len() > 1 {
            // Finder-style: keep the multi-selection now so a drag can carry all of it; collapse
            // to just this item on mouse-up if no drag happened.
            self.deferred_click = Some((id.clone(), ix));
            return;
        }
        self.deferred_click = None;
        self.selection.click(id, ix, mods, &self.ids);
        cx.notify();
    }

    pub(crate) fn open_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        let ids = self.ids.clone();
        if let Some(id) = ids.get(ix) {
            let origin = self.cell_bounds(id);
            cx.emit(FeedEvent::Open { ids, index: ix, origin });
        }
    }

    /// Where `id`'s cell was last drawn, in window coordinates; `None` if it wasn't on screen.
    pub fn cell_bounds(&self, id: &EntryId) -> Option<Bounds<Pixels>> {
        self.cell_bounds.borrow().get(id).copied()
    }

    /// The cache holding the decoded thumbnails, so the detail's morph can draw them without a
    /// re-decode.
    pub fn image_cache(&self) -> Entity<LruImageCache> {
        self.image_cache.clone()
    }

    // ----- actions ----------------------------------------------------------

    fn open_selection(&mut self, _: &OpenSelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selection.single().cloned() {
            if let Some(ix) = self.ids.iter().position(|i| i == &id) {
                self.open_at(ix, cx);
            }
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selection.select_all(&self.ids);
        cx.notify();
    }

    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        self.selection.clear();
        cx.notify();
    }

    /// Arrow keys: move the (single) selection and keep it on screen.
    fn move_selection(&mut self, arrow: feed::Arrow, cx: &mut Context<Self>) {
        let Some(next) = feed::step_selection(self.selection.last_index, arrow, self.columns.max(1), self.ids.len()) else { return };
        let Some(id) = self.ids.get(next).cloned() else { return };
        self.selection.click(&id, next, Modifiers::default(), &self.ids);
        self.scroll_to_index(next, false);
        cx.notify();
    }

    /// Scroll so the row holding `index` is visible (`center`: put it in the middle).
    fn scroll_to_index(&mut self, index: usize, center: bool) {
        let pitch = f32::from(self.cell_px) + GAP;
        let cell = f32::from(self.cell_px);
        let row_y = (index / self.columns.max(1)) as f32 * pitch;
        let viewport = f32::from(slot_size(&self.content_size).height);
        let current = -f32::from(self.scroll.offset().y);
        let target = if center {
            row_y - (viewport - cell) / 2.
        } else if row_y < current {
            row_y
        } else if row_y + cell > current + viewport {
            row_y + cell - viewport
        } else {
            return;
        };
        self.scroll.set_offset(point(px(0.), px(-target.max(0.))));
    }

    /// Select `id` for the closing detail to return to. Returns its cell's box when it is on screen
    /// (the detail shrinks back into it); otherwise scrolls the row into the middle and returns
    /// `None`.
    pub fn land_on(&mut self, id: &EntryId, cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let ix = self.ids.iter().position(|i| i == id)?;
        self.selection.click(id, ix, Modifiers::default(), &self.ids);
        cx.notify();
        let visible = self.cell_bounds(id);
        if visible.is_none() {
            self.scroll_to_index(ix, true);
        }
        visible
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(feed::zoom_in(self.zoom), cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(feed::zoom_out(self.zoom), cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(feed::DEFAULT_ZOOM, cx);
    }

    /// The toolbar's zoom buttons grey out at the ends of the range (`canZoomIn` / `canZoomOut`).
    fn can_zoom_in(&self) -> bool {
        feed::zoom_in(self.zoom) != self.zoom
    }

    fn can_zoom_out(&self) -> bool {
        feed::zoom_out(self.zoom) != self.zoom
    }

    /// Index of the topmost visible row.
    fn top_row(&self) -> usize {
        let pitch = f32::from(self.cell_px) + GAP;
        (-f32::from(self.scroll.offset().y) / pitch).floor().max(0.) as usize
    }

    /// Change the zoom level: every cell slides and resizes to its new place
    /// (`.animation(.easeInOut(0.25), value: gridColumnCount)`). A level that fits the same column
    /// count in the current width (a narrow window) changes nothing on screen but still persists.
    fn set_zoom(&mut self, zoom: u32, cx: &mut Context<Self>) {
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        let width = slot_size(&self.content_size).width;
        if self.relayout(feed::columns_for(f32::from(width), zoom)) {
            self.refresh(Change::Zoom, cx);
        }
        update_config(cx, |c| c.grid_zoom = zoom);
    }

    /// The measured width changed (window or panel resize): fit the zoom level's columns to it.
    /// A changed count re-places every cell without motion, so the next zoom or library change
    /// animates from this layout rather than the previous one.
    fn fit_columns(&mut self, cx: &mut Context<Self>) {
        let width = slot_size(&self.content_size).width;
        if width > px(0.) && self.relayout(feed::columns_for(f32::from(width), self.zoom)) {
            let now = now(cx);
            self.motion.apply(&self.ids, self.columns, Change::Resize, |_| None, |place| visual_for(width, place), now);
        }
        cx.notify();
    }

    /// Adopt a column count, keeping the top row's first item on screen. Returns whether it changed;
    /// the caller decides how the cells get to their new places.
    fn relayout(&mut self, columns: usize) -> bool {
        if columns == self.columns {
            return false;
        }
        let top_item = self.top_row() * self.columns.max(1);
        self.columns = columns;
        let width = slot_size(&self.content_size).width;
        self.cell_px = cell_for(width, columns);
        let pitch = f32::from(self.cell_px) + GAP;
        self.scroll.set_offset(point(px(0.), px(-((top_item / columns.max(1)) as f32 * pitch))));
        true
    }

    /// Photos' "Square / Aspect Ratio" toggle: crop thumbnails to the cell or show them whole.
    fn toggle_shape(&mut self, _: &ToggleThumbnailShape, _: &mut Window, cx: &mut Context<Self>) {
        self.shape = self.shape.toggled();
        update_config(cx, |c| c.thumbnail_shape = self.shape);
        cx.notify();
    }

    /// Width and height of a cell's frame as fractions of the cell: the whole cell, or the largest
    /// box with the item's aspect ratio that fits when showing thumbnails whole.
    fn frame_fractions(&self, aspect_ratio: Option<f32>) -> (f32, f32) {
        match (self.shape, aspect_ratio) {
            (ThumbnailShape::AspectRatio, Some(ratio)) if ratio > 1.0 => (1.0, 1.0 / ratio),
            (ThumbnailShape::AspectRatio, Some(ratio)) if ratio < 1.0 => (ratio, 1.0),
            _ => (1.0, 1.0),
        }
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        let items = self.selected_items(cx);
        if items.is_empty() {
            return;
        }
        let set = feed::should_set_favorite(items.iter().map(|i| i.is_favorite));
        let ids: Vec<_> = items.iter().map(|i| i.id.clone()).collect();
        self.library.update(cx, |m, cx| m.set_favorite(&ids, set, cx));
    }

    /// Delete the selected generations (their files stay as assets), or — on the Assets feed — trash
    /// the selected assets, which only works while no live generation references them. A selection
    /// holding both kinds deletes the generations only.
    fn delete(&mut self, _: &DeleteMedia, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_ids();
        if !ids.is_empty() {
            let msg = if ids.len() == 1 { "Delete this item?".to_string() } else { format!("Delete {} items?", ids.len()) };
            let answer = window.prompt(PromptLevel::Warning, &msg, Some("The files stay in the library as assets."), &["Delete", "Cancel"], cx);
            let library = self.library.clone();
            cx.spawn(async move |_, cx| {
                if answer.await == Ok(0) {
                    cx.update(|cx| library.update(cx, |m, cx| m.delete(&ids, cx)));
                }
            })
            .detach();
            return;
        }
        let assets = self.selected_assets(cx);
        if assets.is_empty() {
            return;
        }
        if let Some(used) = assets.iter().find(|a| self.library.read(cx).lib.is_referenced(&a.id)) {
            crate::ui::toast(window, format!("{} is used by a generation and can't be deleted.", used.file_name()), cx);
            return;
        }
        let ids: Vec<AssetId> = assets.iter().map(|a| a.id.clone()).collect();
        let msg = if ids.len() == 1 { "Delete this asset?".to_string() } else { format!("Delete {} assets?", ids.len()) };
        let answer = window.prompt(PromptLevel::Warning, &msg, Some("The files are moved to the library's .majik/trash folder."), &["Delete", "Cancel"], cx);
        let library = self.library.clone();
        cx.spawn_in(window, async move |_, cx| {
            if answer.await == Ok(0) {
                cx.update(|window, cx| {
                    if let Err(e) = library.update(cx, |m, cx| m.delete_assets(&ids, cx)) {
                        crate::ui::toast(window, format!("Couldn't delete: {e:#}"), cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn copy(&mut self, _: &CopyMedia, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.selected_exportables(cx);
        copy_items(&items, window, cx);
    }

    fn save(&mut self, _: &SaveMedia, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.selected_exportables(cx);
        save_items(items, window, cx);
    }

    fn recreate(&mut self, _: &Recreate, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.selected_items(cx);
        if let Some(item) = items.iter().find(|i| i.can_recreate()) {
            cx.emit(FeedEvent::Compose(PendingCompose { recreate: Some(item.id.clone()) }));
        } else {
            crate::ui::toast(window, "This item can't be recreated.", cx);
        }
    }

    fn retry(&mut self, _: &Retry, _: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_ids();
        self.library.update(cx, |m, cx| m.retry(&ids, cx));
    }

    // ----- context menu -----------------------------------------------------

    fn menu_info(&self, cx: &App) -> MenuInfo {
        let assets = self.selected_assets(cx);
        let lib = &self.library.read(cx).lib;
        MenuInfo {
            items: self.selected_items(cx),
            assets_deletable: assets.iter().all(|a| !lib.is_referenced(&a.id)),
            assets,
            album: self.current_album(),
            library: self.library.clone(),
            focus: self.focus.clone(),
        }
    }

    // ----- rendering --------------------------------------------------------

    /// The per-filter empty state: (icon, title, hint).
    fn empty_state(&self, cx: &App) -> (&'static str, &'static str, SharedString) {
        if self.favorites_only && self.shows_favorites_toggle() {
            return ("heart", "No Favorites Here", "Click the heart on an item to add it to your favorites, or turn the favorites filter off".into());
        }
        match &self.filter {
            FeedFilter::Library => ("images", "Nothing Here Yet", format!("Press {} to open the composer", crate::actions::keystroke_label(crate::actions::NEW_GENERATION_KEYS)).into()),
            FeedFilter::Favorites => ("heart", "No Favorites Yet", "Click the heart on an item to add it to your favorites".into()),
            FeedFilter::Album(id) if self.library.read(cx).lib.album(id).is_some() => ("layers", "Empty Album", "Add items from the library using the context menu".into()),
            FeedFilter::Album(_) => ("layers", "Album Unavailable", "This album has been deleted".into()),
            FeedFilter::Assets => ("layers", "No Assets Yet", "Generated files and the inputs you add to the composer collect here".into()),
        }
    }

    /// The favorites-only toggle is for feeds that list generations of every kind: the Favorites
    /// feed is already that filter, and assets carry no favorite.
    fn shows_favorites_toggle(&self) -> bool {
        !matches!(self.filter, FeedFilter::Favorites | FeedFilter::Assets)
    }

    /// The thumbnail to draw in a cell: the large tier once a cell is bigger than the standard one
    /// (`THUMB_MAX`) in device pixels, so big tiles stop being a stretched 400 px image. Falls back
    /// to the standard tier until the large one has been rendered — see
    /// [`LibraryModel::request_large_thumbnails`], which the zoom and scroll paths drive.
    fn thumbnail_for_cell(&self, standard: &std::path::Path, cx: &App) -> PathBuf {
        if self.thumbnail_tier() == thumbnails::THUMB_MAX {
            return standard.to_path_buf();
        }
        state::library(cx).read(cx).large_thumbnail(standard).map(std::path::Path::to_path_buf).unwrap_or_else(|| standard.to_path_buf())
    }

    /// The tier this feed's cells want, from the cell size the last render measured.
    fn thumbnail_tier(&self) -> u32 {
        if f32::from(self.cell_px) * self.scale_factor > thumbnails::THUMB_MAX as f32 {
            thumbnails::THUMB_LARGE
        } else {
            thumbnails::THUMB_MAX
        }
    }

    /// [`Self::request_large_thumbnails`] once the view has been still for [`LARGE_TIER_SETTLE`].
    /// Each call replaces the last, so a continuous scroll asks once, at the end.
    fn schedule_large_thumbnails(&mut self, cx: &mut Context<Self>) {
        if self.thumbnail_tier() == thumbnails::THUMB_MAX {
            self.large_request = None;
            return;
        }
        self.large_request = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LARGE_TIER_SETTLE).await;
            this.update(cx, |this, cx| this.request_large_thumbnails(cx)).ok();
        }));
    }

    /// Ask for the large tier of everything currently on screen, if the cells are big enough to
    /// need it. Called from the code that changes what is visible — the viewport, and the library
    /// itself — never from a render.
    fn request_large_thumbnails(&mut self, cx: &mut Context<Self>) {
        if self.thumbnail_tier() == thumbnails::THUMB_MAX {
            return;
        }
        let visible: Vec<AssetId> = self.last_rendered.keys().filter_map(|id| self.asset_of(id, cx)).collect();
        if visible.is_empty() {
            return;
        }
        state::library(cx).update(cx, |model, cx| model.request_large_thumbnails(&visible, cx));
    }

    /// The asset a cell draws: a generation's output, or the asset itself.
    fn asset_of(&self, id: &EntryId, cx: &App) -> Option<AssetId> {
        let library = state::library(cx).read(cx);
        match id {
            EntryId::Asset(asset) => Some(asset.clone()),
            EntryId::Generation(generation) => library.lib.get(generation)?.output_asset_id.clone(),
        }
    }

    /// The visual cell: thumbnail / spinner / badges / selection ring, sized by
    /// [`Self::frame_fractions`] so the caller centres it in the cell. No listeners, so exit ghosts
    /// and the zoom crossfade can draw it too.
    fn render_cell_body(&self, item: &Generation, thumbnail_opacity: f32, cx: &mut Context<Self>) -> gpui::Div {
        let selected = self.selection.contains(&EntryId::Generation(item.id.clone()));
        let (frame_w, frame_h) = self.frame_fractions(item.aspect_ratio_f32());
        let theme = cx.theme();
        let accent = theme.blue;
        let muted = theme.muted;
        let muted_fg = theme.muted_foreground;

        let content: gpui::AnyElement = match (item.status, &item.thumbnail, item.media_type) {
            (Status::Generating, _, _) => {
                // The attempt's own time: a retry counts from when it was asked for.
                let elapsed = majik_core::now_ms().saturating_sub(item.queued_at_ms) / 1000;
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(spin(icon("loader-circle").size_6().text_color(muted_fg)))
                    .child(gpui::div().text_xs().text_color(muted_fg).child(format!("{}  ·  {}", item.media_type.label(), format_duration(elapsed as f64))))
                    .into_any_element()
            }
            (Status::Failed, _, _) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(icon("circle-alert").size_6().text_color(theme.danger))
                .child(gpui::div().text_xs().text_color(muted_fg).child("Failed"))
                .into_any_element(),
            (Status::Missing, _, _) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(icon("file-x").size_6().text_color(theme.danger))
                .child(gpui::div().text_xs().text_color(muted_fg).child("File missing"))
                .into_any_element(),
            (_, _, MediaType::Audio) => v_flex().size_full().items_center().justify_center().child(icon("audio-lines").size_8().text_color(muted_fg)).into_any_element(),
            (_, Some(thumb), _) => {
                let thumb = self.thumbnail_for_cell(thumb, cx);
                let selector = format!("thumb-{}", item.id);
                gpui::div().size_full().opacity(thumbnail_opacity).child(cover_image(thumb).debug_selector(move || selector.clone())).into_any_element()
            }
            (_, None, _) => v_flex().size_full().items_center().justify_center().child(icon("image").size_6().text_color(muted_fg)).into_any_element(),
        };

        let badges = right_badges(item.duration_secs, item.media_type, item.is_upscaled);
        gpui::div()
            .w(relative(frame_w))
            .h(relative(frame_h))
            .relative()
            .overflow_hidden()
            .bg(muted)
            .child(content)
            .when(item.is_favorite, |d| d.child(cell_badges().left_1().child(cell_badge().child(icon("heart").size_3()))))
            .when(!badges.is_empty(), |d| d.child(cell_badges().right_1().children(badges.into_iter().map(|badge| cell_badge().child(badge)))))
            .when(selected, |d| d.child(gpui::div().absolute().inset_0().border_3().border_color(accent)))
    }

    /// An asset's cell: its thumbnail (audio: an icon; a file that went missing: an error), a
    /// duration badge and the selection ring — no generation state, no favourite, no HD.
    fn render_asset_body(&self, asset: &Asset, thumbnail_opacity: f32, cx: &mut Context<Self>) -> gpui::Div {
        let selected = self.selection.contains(&EntryId::Asset(asset.id.clone()));
        let (frame_w, frame_h) = self.frame_fractions(asset.aspect_ratio_f32());
        let theme = cx.theme();
        let (accent, muted, muted_fg, danger) = (theme.blue, theme.muted, theme.muted_foreground, theme.danger);
        let content: gpui::AnyElement = match (asset.missing, &asset.thumbnail, asset.kind) {
            (true, _, _) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(icon("file-x").size_6().text_color(danger))
                .child(gpui::div().text_xs().text_color(muted_fg).child("File missing"))
                .into_any_element(),
            (_, _, MediaType::Audio) => v_flex().size_full().items_center().justify_center().child(icon("audio-lines").size_8().text_color(muted_fg)).into_any_element(),
            (_, Some(thumb), _) => {
                let thumb = self.thumbnail_for_cell(thumb, cx);
                gpui::div().size_full().opacity(thumbnail_opacity).child(cover_image(thumb)).into_any_element()
            }
            (_, None, _) => v_flex().size_full().items_center().justify_center().child(icon("image").size_6().text_color(muted_fg)).into_any_element(),
        };
        let badges = right_badges(asset.duration_secs, asset.kind, false);
        gpui::div()
            .w(relative(frame_w))
            .h(relative(frame_h))
            .relative()
            .overflow_hidden()
            .bg(muted)
            .child(content)
            .when(!badges.is_empty(), |d| d.child(cell_badges().right_1().children(badges.into_iter().map(|badge| cell_badge().child(badge)))))
            .when(selected, |d| d.child(gpui::div().absolute().inset_0().border_3().border_color(accent)))
    }

    fn render_snapshot_body(&self, snapshot: &CellSnapshot, thumbnail_opacity: f32, cx: &mut Context<Self>) -> gpui::Div {
        match snapshot {
            CellSnapshot::Media(item) => self.render_cell_body(item, thumbnail_opacity, cx),
            CellSnapshot::Asset(asset) => self.render_asset_body(asset, thumbnail_opacity, cx),
        }
    }

    /// A cell's box, absolutely positioned in the grid content. GPUI has no transforms on divs, so
    /// `style.scale` becomes symmetric padding (the body shrinks toward its centre) and
    /// `style.opacity` applies to the whole box.
    fn cell_slot(id: impl Into<gpui::ElementId>, visual: Visual, style: CellStyle) -> gpui::Stateful<gpui::Div> {
        let side = px(visual.size);
        gpui::div()
            .id(id)
            .absolute()
            .left(px(visual.x))
            .top(px(visual.y))
            .w(side)
            .h(side)
            .p(side * ((1.0 - style.scale) / 2.0))
            .opacity(style.opacity)
            .flex()
            .items_center()
            .justify_center()
    }

    /// Interactive cell around [`Self::render_cell_body`]: selection, drag-out and the context menu.
    fn render_cell(&self, ix: usize, snapshot: &CellSnapshot, visual: Visual, style: CellStyle, thumbnail_opacity: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let id = snapshot.id();
        let id_right = id.clone();
        let id_menu = id.clone();
        let feed = cx.weak_entity();
        let feed_for_drag = feed.clone();
        let dragged = self.dragged(&id, cx);
        let entry = snapshot.entry();
        let drag_preview_img = entry.file().and_then(|file| crate::ui::picture_for(entry.kind(), entry.thumbnail(), file));
        let body = self
            .render_snapshot_body(snapshot, thumbnail_opacity, cx)
            .child(gpui::div().absolute().inset_0().child(record_bounds(self.cell_bounds.clone(), id.clone())));

        // Keyed by id, not index, so element state survives neighbours being removed.
        Self::cell_slot(gpui::SharedString::from(format!("cell-{id}")), visual, style)
            .child(body)
            .when(!dragged.assets.is_empty(), move |d| {
                let count = dragged.assets.len();
                let preview = drag_preview_img.clone();
                let feed = feed_for_drag;
                d.on_drag(dragged, move |_, press_offset, _window, cx| {
                    // A drag that ends over the composer never reaches the feed's mouse-up, which
                    // would otherwise collapse the selection on the next click.
                    feed.update(cx, |feed, _| feed.deferred_click = None).ok();
                    let preview = preview.clone();
                    cx.new(|_| DragPreview { image: preview, count, press_offset })
                })
                .external_drag_payload::<DraggedAssets>(|dragged, _window, _cx| {
                    Some(ExternalDragPayload::Files(FileDragPaths::new(dragged.paths().into_iter().map(|p| (p, false)))))
                })
            })
            // Stopping propagation keeps the feed's own mouse-down (which clears the selection)
            // out of a cell click, but it also stops gpui's focus-on-click from reaching the
            // feed's `track_focus`, so the click focuses the grid itself: a user who clicks a
            // thumbnail expects the arrow keys to move from it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.focus.focus(window, cx);
                    this.cell_mouse_down(ix, &id, ev, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.selection.right_click(&id_right, ix);
                    cx.notify();
                }),
            )
            .context_menu(move |menu, _, cx| {
                let Some(feed) = feed.upgrade() else { return menu };
                feed.update(cx, |this, cx| {
                    this.selection.right_click(&id_menu, ix);
                    cx.notify();
                });
                let info = feed.read(cx).menu_info(cx);
                build_context_menu(info, menu)
            })
    }

    /// A removed item still fading where it was, drawn from its render snapshot.
    fn render_ghost(&self, ix: usize, ghost: &Ghost<CellSnapshot, EntryId>, style: CellStyle, cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.render_snapshot_body(&ghost.snapshot, 1.0, cx);
        Self::cell_slot(("ghost", ix), ghost.from, style).child(body)
    }
}

/// Everything the context menu needs, snapshotted so the feed entity is not borrowed while building.
struct MenuInfo {
    items: Vec<Generation>,
    assets: Vec<Asset>,
    /// No selected asset is referenced by a live generation.
    assets_deletable: bool,
    album: Option<majik_core::model::AlbumId>,
    library: Entity<LibraryModel>,
    focus: FocusHandle,
}

/// One row of the feed's context menu as the user sees it: a label and whether it can be chosen.
/// Built by [`context_menu_entries`] from the selection alone so the menu is testable without a
/// `PopupMenu` (whose items are private to gpui-component).
struct MenuEntry {
    label: SharedString,
    enabled: bool,
    kind: MenuEntryKind,
}

enum MenuEntryKind {
    Action(Box<dyn gpui::Action>),
    AddToAlbum(Vec<GenerationId>),
    RemoveFromAlbum(majik_core::model::AlbumId, Vec<GenerationId>),
    CancelGeneration(Vec<GenerationId>),
    Separator,
}

impl MenuEntry {
    fn action(label: impl Into<SharedString>, action: impl gpui::Action) -> Self {
        Self { label: label.into(), enabled: true, kind: MenuEntryKind::Action(Box::new(action)) }
    }

    fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    fn custom(label: impl Into<SharedString>, kind: MenuEntryKind) -> Self {
        Self { label: label.into(), enabled: true, kind }
    }

    fn separator() -> Self {
        Self { label: SharedString::default(), enabled: false, kind: MenuEntryKind::Separator }
    }
}

/// The menu for a selection of assets (the Assets feed): the export actions and Delete — which
/// only applies while no live generation references any of them. Nothing about the composer: an
/// asset is dragged there. A selection mixing generations and assets gets the export actions alone.
fn asset_menu_entries(assets: &[Asset], deletable: bool) -> Vec<MenuEntry> {
    let exportable = assets.iter().any(|a| a.file().is_some());
    vec![
        MenuEntry::action("Copy", CopyMedia).enabled(exportable),
        MenuEntry::action("Save…", SaveMedia).enabled(exportable),
        MenuEntry::separator(),
        MenuEntry::action(if assets.len() > 1 { "Delete Selected" } else { "Delete" }, DeleteMedia).enabled(deletable),
    ]
}

fn export_menu_entries() -> Vec<MenuEntry> {
    vec![
        MenuEntry::action("Copy", CopyMedia),
        MenuEntry::action("Save…", SaveMedia),
    ]
}

/// The context menu for whatever is selected: generations, assets, or both.
fn selection_menu_entries(info: &MenuInfo) -> Vec<MenuEntry> {
    match (info.items.is_empty(), info.assets.is_empty()) {
        (false, true) => context_menu_entries(&info.items, info.album.as_ref()),
        (true, false) => asset_menu_entries(&info.assets, info.assets_deletable),
        (false, false) => export_menu_entries(),
        (true, true) => Vec::new(),
    }
}

/// The feed's context menu. It has Open but never Reveal in Finder: the library folder belongs to
/// the app, and files leave it through Save, Copy and drag. Rows that don't apply to the selection
/// stay visible but disabled (single selection), except that a multi-selection drops Recreate
/// entirely. There is no Use Image; an item is dragged into the composer instead. A single
/// generating item offers Cancel and, since its request is already stored, Recreate too, so a
/// variation can be queued without waiting for it. Items whose file went missing get Retry,
/// which regenerates in place, and Delete, like failed ones; a single one of either also gets
/// Recreate, so the request can be changed in the composer before it runs again.
/// The tools (Upscale, Remove Background) are not here either: they run from the composer's
/// tool tabs.
fn context_menu_entries(items: &[Generation], in_album: Option<&majik_core::model::AlbumId>) -> Vec<MenuEntry> {
    let n = items.len();
    let ids: Vec<GenerationId> = items.iter().map(|i| i.id.clone()).collect();
    let all_completed = n > 0 && items.iter().all(|i| i.status == Status::Completed);
    let all_failed = n > 0 && items.iter().all(|i| i.status == Status::Failed);
    let all_missing = n > 0 && items.iter().all(|i| i.status == Status::Missing);

    let mut entries = Vec::new();
    if all_completed {
        entries.extend([
            MenuEntry::action("Open", OpenSelection).enabled(n == 1),
                MenuEntry::action("Copy", CopyMedia),
            MenuEntry::action("Save…", SaveMedia),
            MenuEntry::separator(),
        ]);
        if n == 1 {
            entries.push(MenuEntry::action("Recreate", Recreate).enabled(items.iter().any(|i| i.can_recreate())));
        }
        entries.extend([MenuEntry::separator(), MenuEntry::custom("Add to Album…", MenuEntryKind::AddToAlbum(ids.clone()))]);
        if let Some(album) = in_album {
            entries.push(MenuEntry::custom("Remove from Album", MenuEntryKind::RemoveFromAlbum(album.clone(), ids.clone())));
        }
        let favorite = feed::should_set_favorite(items.iter().map(|i| i.is_favorite));
        entries.extend([
            MenuEntry::separator(),
            MenuEntry::action(if favorite { "Favorite" } else { "Unfavorite" }, ToggleFavorite),
            MenuEntry::action(if n > 1 { "Delete Selected" } else { "Delete" }, DeleteMedia),
        ]);
    } else if all_failed || all_missing {
        entries.push(MenuEntry::action(if n > 1 { "Retry Selected" } else { "Retry" }, Retry).enabled(all_failed || items.iter().any(|i| i.can_retry())));
        if n == 1 {
            entries.push(MenuEntry::action("Recreate", Recreate).enabled(items.iter().any(|i| i.can_recreate())));
        }
        entries.push(MenuEntry::action(if n > 1 { "Delete Selected" } else { "Delete" }, DeleteMedia));
    } else {
        let generating: Vec<GenerationId> = items.iter().filter(|i| i.status == Status::Generating).map(|i| i.id.clone()).collect();
        if !generating.is_empty() {
            let label = if generating.len() > 1 { "Cancel Generations" } else { "Cancel Generation" };
            entries.push(MenuEntry::custom(label, MenuEntryKind::CancelGeneration(generating)));
        }
        if n == 1 {
            entries.push(MenuEntry::action("Recreate", Recreate).enabled(items.iter().any(|i| i.can_recreate())));
        }
        entries.push(MenuEntry::action(if n > 1 { "Delete Selected" } else { "Delete" }, DeleteMedia));
    }
    entries
}

fn build_context_menu(info: MenuInfo, menu: PopupMenu) -> PopupMenu {
    let entries = selection_menu_entries(&info);
    let menu = menu.action_context(info.focus.clone());
    render_menu_entries(&entries, &info.library, menu)
}

fn render_menu_entries(entries: &[MenuEntry], library: &Entity<LibraryModel>, mut menu: PopupMenu) -> PopupMenu {
    for entry in entries {
        let label = entry.label.clone();
        let disabled = !entry.enabled;
        menu = match &entry.kind {
            MenuEntryKind::Separator => menu.separator(),
            MenuEntryKind::Action(action) => menu.menu_with_disabled(label, action.boxed_clone(), disabled),
            MenuEntryKind::AddToAlbum(ids) => {
                let ids = ids.clone();
                menu.item(PopupMenuItem::new(label).disabled(disabled).on_click(move |_, window, cx| {
                    crate::views::album_picker::open_album_picker(ids.clone(), window, cx);
                }))
            }
            MenuEntryKind::RemoveFromAlbum(album, ids) => {
                let (library, album, ids) = (library.clone(), album.clone(), ids.clone());
                menu.item(PopupMenuItem::new(label).disabled(disabled).on_click(move |_, _, cx| {
                    library.update(cx, |m, cx| m.remove_from_album(&album, &ids, cx));
                }))
            }
            MenuEntryKind::CancelGeneration(ids) => {
                let (library, ids) = (library.clone(), ids.clone());
                menu.item(PopupMenuItem::new(label).disabled(disabled).on_click(move |_, _, cx| {
                    library.update(cx, |m, _| m.cancel(&ids));
                }))
            }
        };
    }
    menu
}

/// A file the export actions (Copy / Save / Reveal) work on — a generation's output or any
/// asset — so those actions don't care which kind of entry is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exportable {
    pub path: PathBuf,
    pub kind: MediaType,
    /// The name the file is saved under.
    pub name: String,
}

impl Exportable {
    /// `None` while the generation has no file to read.
    pub fn of_item(item: &Generation) -> Option<Self> {
        Some(Self { path: item.file()?.to_path_buf(), kind: item.media_type, name: item.file_name() })
    }

    pub fn of_asset(asset: &Asset) -> Option<Self> {
        Some(Self { path: asset.file()?.to_path_buf(), kind: asset.kind, name: asset.file_name() })
    }

    pub fn of_entry(entry: &Entry<'_>) -> Option<Self> {
        match entry {
            Entry::Generation(item) => Self::of_item(item),
            Entry::Asset(asset) => Self::of_asset(asset),
        }
    }
}

/// Copy media with both a file URL and raw bytes per item (native pasteboard), falling back to gpui.
pub fn copy_items(items: &[Exportable], window: &mut Window, cx: &mut App) {
    let media: Vec<majik_platform::clipboard::ClipboardMedia> = items
        .iter()
        .map(|i| {
            let content_type = match i.kind {
                MediaType::Image => match i.path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
                    Some("jpg") | Some("jpeg") => "image/jpeg",
                    _ => "image/png",
                },
                MediaType::Video => "video/mp4",
                MediaType::Audio => "audio/mpeg",
            };
            majik_platform::clipboard::ClipboardMedia { path: i.path.clone(), content_type: content_type.into() }
        })
        .collect();
    if media.is_empty() {
        return;
    }
    if majik_platform::clipboard::SUPPORTED && majik_platform::clipboard::copy_media(&media).is_ok() {
        crate::ui::toast(window, if media.len() > 1 { format!("Copied {} items", media.len()) } else { "Copied".to_string() }, cx);
        return;
    }
    // Off macOS gpui's clipboard holds one useful flavour: it drops file paths on every backend,
    // and only Windows keeps more than one entry. So write the bitmap for a single image and the
    // paths as text for anything else.
    let mut image = false;
    if let Some(first) = items.first().filter(|i| i.kind == MediaType::Image) {
        if let Ok(bytes) = std::fs::read(&first.path) {
            let format = match first.path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
                Some("jpg") | Some("jpeg") => ImageFormat::Jpeg,
                Some("webp") => ImageFormat::Webp,
                Some("gif") => ImageFormat::Gif,
                _ => ImageFormat::Png,
            };
            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(format, bytes)));
            image = true;
        }
    }
    if !image {
        let paths: Vec<String> = items.iter().map(|i| i.path.to_string_lossy().into_owned()).collect();
        cx.write_to_clipboard(ClipboardItem::new_string(paths.join("\n")));
    }
    // Then the files themselves alongside it, where the platform can carry them, so a paste into a
    // file manager or a mail composer yields the files and not their names.
    let mut files = false;
    if majik_platform::clipboard::ADDS_FILE_REFERENCES {
        let paths: Vec<PathBuf> = items.iter().map(|i| i.path.clone()).collect();
        match majik_platform::clipboard::add_file_references(&paths) {
            Ok(()) => files = true,
            Err(e) => tracing::warn!(target: "majik", "adding the files to the clipboard: {e:#}"),
        }
    }
    crate::ui::toast(window, copy_toast(items.len(), image, files), cx);
}

/// What the toast says about a copy that used the portable path: the files themselves when the
/// platform carried them, otherwise whichever single flavour was written.
fn copy_toast(count: usize, image: bool, files: bool) -> String {
    match (files, image, count) {
        (true, _, 1) => "Copied".to_string(),
        (true, _, count) => format!("Copied {count} items"),
        (false, true, _) => "Copied image".to_string(),
        (false, false, 1) => "Copied file path".to_string(),
        (false, false, count) => format!("Copied {count} file paths"),
    }
}

/// Save: one item → save panel; several → folder chooser. Files are copied.
/// Outcome of saving one item through the OS save panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Cancelled,
    Failed(String),
}

/// Where the save panel opens: the folder the last save went to, while it still exists, else the
/// user's home folder, resolved per platform (`$HOME` is unset on Windows, where it is
/// `%USERPROFILE%`).
pub fn save_panel_directory(cx: &App) -> PathBuf {
    let remembered = cx.global::<Config>().save_directory.as_deref().filter(|dir| dir.is_dir());
    match remembered {
        Some(dir) => dir.to_path_buf(),
        None => directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()).unwrap_or_default(),
    }
}

/// Remember `dir` as where saves go, so the next save panel opens there.
fn remember_save_directory(dir: &Path, cx: &mut App) {
    if cx.global::<Config>().save_directory.as_deref() != Some(dir) {
        update_config(cx, |config| config.save_directory = Some(dir.to_path_buf()));
    }
}

/// Save `item` via the save panel; resolves when the copy finished or the panel was dismissed.
pub fn save_item(item: Exportable, window: &mut Window, cx: &mut App) -> Task<SaveOutcome> {
    let source = item.path.clone();
    let rx = cx.prompt_for_new_path(&save_panel_directory(cx), Some(&item.name));
    window.spawn(cx, async move |cx| {
        let Ok(Ok(Some(dest))) = rx.await else { return SaveOutcome::Cancelled };
        let copied = cx.background_spawn(async move { std::fs::copy(&source, &dest).map(|_| dest) }).await;
        match copied {
            Ok(dest) => {
                if let Some(dir) = dest.parent() {
                    cx.update(|_, cx| remember_save_directory(dir, cx)).ok();
                }
                SaveOutcome::Saved
            }
            Err(e) => SaveOutcome::Failed(e.to_string()),
        }
    })
}

pub fn save_items(items: Vec<Exportable>, window: &mut Window, cx: &mut App) {
    if let [item] = items.as_slice() {
        let task = save_item(item.clone(), window, cx);
        window
            .spawn(cx, async move |cx| {
                let outcome = task.await;
                cx.update(|window, cx| match outcome {
                    SaveOutcome::Saved => crate::ui::toast(window, "Saved", cx),
                    SaveOutcome::Failed(e) => crate::ui::toast(window, format!("Save failed: {e}"), cx),
                    SaveOutcome::Cancelled => {}
                })
                .ok();
            })
            .detach();
    } else if !items.is_empty() {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Save".into()) });
        window
            .spawn(cx, async move |cx| {
                if let Ok(Ok(Some(dirs))) = rx.await {
                    if let Some(dir) = dirs.first() {
                        let mut failed = 0;
                        for item in &items {
                            if std::fs::copy(&item.path, dir.join(&item.name)).is_err() {
                                failed += 1;
                            }
                        }
                        cx.update(|window, cx| {
                            remember_save_directory(dir, cx);
                            let msg = if failed == 0 { format!("Saved {} files", items.len()) } else { format!("Saved with {failed} failure(s)") };
                            crate::ui::toast(window, msg, cx)
                        })
                        .ok();
                    }
                }
            })
            .detach();
    }
}

impl Render for FeedView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(ix) = self.debug_open.take() {
            self.open_at(ix, cx);
        }
        let now = now(cx);
        self.motion.set_enabled(!cx.reduce_motion());
        self.motion.tick(now);
        if self.motion.is_animating() {
            window.request_animation_frame();
        }
        let size = slot_size(&self.content_size);
        let width = size.width;
        let cols = self.columns.max(1);
        let cell = cell_for(width, cols);
        self.cell_px = cell;
        // Decode thumbnails at the size the cells actually draw them, not at the size they were
        // stored: at the smallest zoom a cell is a fifth of a 400 px thumbnail's pixel count, and
        // the difference is paid twice over, in decoded bytes and in sprite-atlas space.
        self.scale_factor = window.scale_factor();
        let cell_device_px = (f32::from(cell) * self.scale_factor).ceil().max(1.) as u32;
        // Whatever moved the grid — the wheel, an arrow key's `scroll_to_index`, a scrollbar drag,
        // a zoom, or simply the first frame after launch — different rows are on screen now, and
        // they may want the large tier. Only ever replaces a timer (see the field), never any work.
        let viewport = (self.scroll.offset().y, cell, cols);
        if self.last_viewport != Some(viewport) {
            self.last_viewport = Some(viewport);
            self.schedule_large_thumbnails(cx);
        }
        self.image_cache.update(cx, |cache, cx| cache.set_target(cell_device_px, window, cx));
        let count = self.ids.len();
        let title = self.title(cx);
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;

        let filter_button = button("filter").icon(icon("list-filter")).ghost().small().selected(self.media_filter != MediaFilter::All).tooltip("Filter by type").dropdown_menu({
            let this = cx.weak_entity();
            let current = self.media_filter;
            move |mut menu, _, _| {
                for f in MediaFilter::ALL {
                    let this = this.clone();
                    menu = menu.item(PopupMenuItem::new(f.label()).checked(f == current).on_click(move |_, _, cx| {
                        this.update(cx, |v, cx| {
                            v.media_filter = f;
                            v.refresh(Change::Filter, cx);
                        })
                        .ok();
                    }));
                }
                menu
            }
        });

        let favorites_button = button("favorites-only")
            .icon(icon(if self.favorites_only { "heart-filled" } else { "heart" }))
            .ghost()
            .small()
            .selected(self.favorites_only)
            .tooltip("Favorites only")
            .on_click(cx.listener(|this, _, _, cx| this.set_favorites_only(!this.favorites_only, cx)));

        let assets_feed = self.filter == FeedFilter::Assets;
        // The panel toggles are the Library window's, in its title bar.
        let toolbar = toolbar(cx)
            .child(gpui::div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
            .child(gpui::div().text_xs().text_color(muted_fg).child(format!("{count} items")))
            .child(gpui::div().flex_1())
            .when(assets_feed, |t| {
                t.child(button("import").icon(icon("upload")).ghost().small().tooltip("Import files as assets").on_click(cx.listener(|this, _, window, cx| this.pick_imports(window, cx))))
            })
            .when(self.shows_favorites_toggle(), |t| t.child(favorites_button))
            .child(filter_button)
            .child(
                button("shape")
                    .icon(icon(match self.shape {
                        ThumbnailShape::Square => "square",
                        ThumbnailShape::AspectRatio => "ratio",
                    }))
                    .ghost()
                    .small()
                    .tooltip_with_action("Square or full-aspect thumbnails", &ToggleThumbnailShape, Some("Feed"))
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_shape(&ToggleThumbnailShape, w, cx))),
            )
            .child(button("zoom-out").icon(icon("zoom-out")).ghost().small().disabled(!self.can_zoom_out()).tooltip_with_action("Smaller thumbnails", &ZoomOut, Some("Feed")).on_click(cx.listener(|this, _, w, cx| this.zoom_out(&ZoomOut, w, cx))))
            .child(button("zoom-in").icon(icon("zoom-in")).ghost().small().disabled(!self.can_zoom_in()).tooltip_with_action("Larger thumbnails", &ZoomIn, Some("Feed")).on_click(cx.listener(|this, _, w, cx| this.zoom_in(&ZoomIn, w, cx))));

        // Virtualized grid of absolutely positioned cells: the rows around the viewport, plus any
        // cell still sliding in from elsewhere and the fading ghosts.
        let pitch = f32::from(cell) + GAP;
        let rows = count.div_ceil(cols);
        let content_height = px(rows as f32 * pitch);
        let viewport = f32::from(size.height);
        // The handle clamps its offset only when it paints; a single large wheel delta (a fling)
        // would otherwise pick rows past the end and draw an empty frame.
        let scroll_y = (-f32::from(self.scroll.offset().y)).clamp(0., (f32::from(content_height) - viewport).max(0.));
        let visible = |v: &Visual| v.y + v.size >= scroll_y - pitch && v.y <= scroll_y + viewport + pitch;
        let first = ((scroll_y / pitch).floor().max(0.) as usize).saturating_sub(1) * cols;
        let last = (((scroll_y + viewport) / pitch).ceil() as usize + 1) * cols;
        let mut indices: Vec<usize> = (first.min(count)..last.min(count)).collect();
        for id in self.motion.moving_ids() {
            if let Some(place) = self.motion.place(id) {
                if place.index < first || place.index >= last {
                    indices.push(place.index);
                }
            }
        }
        let mut cells: Vec<gpui::AnyElement> = Vec::with_capacity(indices.len());
        let mut rendered = HashMap::new();
        self.cell_bounds.borrow_mut().clear();
        for ix in indices {
            let Some(id) = self.ids.get(ix).cloned() else { continue };
            let Some(snapshot) = self.library.read(cx).lib.entry(&id).map(CellSnapshot::from) else { continue };
            let target = visual_for(width, Place { index: ix, columns: cols });
            let (visual, style) = self.motion.cell(&id, target, now);
            if !visible(&visual) && !visible(&target) {
                continue;
            }
            let thumbnail_opacity = self.motion.thumbnail_opacity(&id, now);
            cells.push(self.render_cell(ix, &snapshot, visual, style, thumbnail_opacity, cx).into_any_element());
            rendered.insert(id, snapshot);
        }
        let ghosts: Vec<gpui::AnyElement> = self
            .motion
            .ghosts()
            .enumerate()
            .filter(|(_, g)| visible(&g.from))
            .map(|(ix, g)| self.render_ghost(ix, g, self.motion.ghost_style(g, now), cx).into_any_element())
            .collect();
        self.last_rendered = rendered;
        let grid = gpui::div()
            .id("feed-grid")
            .debug_selector(|| "feed-grid".into())
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(gpui::div().relative().w_full().h(content_height).children(ghosts).children(cells));

        let empty = (count == 0).then(|| {
            let (glyph, title, hint) = self.empty_state(cx);
            v_flex()
                .absolute()
                .inset_0()
                .items_center()
                .justify_center()
                .gap_2()
                .child(icon(glyph).size_8().text_color(muted_fg))
                .child(gpui::div().text_color(muted_fg).child(title))
                .child(gpui::div().text_sm().text_color(muted_fg).child(hint))
        });
        gpui::div()
            .image_cache(self.image_cache.clone())
            .id("feed")
            .key_context("Feed")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::open_selection))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.move_selection(feed::Arrow::Left, cx)))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.move_selection(feed::Arrow::Right, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.move_selection(feed::Arrow::Up, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.move_selection(feed::Arrow::Down, cx)))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::toggle_shape))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::recreate))
            .on_action(cx.listener(Self::retry))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window, cx);
                    this.selection.clear();
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some((id, ix)) = this.deferred_click.take() {
                        this.selection.click(&id, ix, Modifiers::default(), &this.ids);
                        cx.notify();
                    }
                }),
            )
            // Files dropped anywhere on the Assets grid are imported.
            .when(assets_feed, |d| d.on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.import_paths(paths.paths().to_vec(), window, cx))))
            .child(toolbar)
            .child(
                // `min_h_0`: a flex child's minimum height is otherwise its content's, and the grid's
                // content box is as tall as every row — the viewport would grow to fit it and
                // nothing could scroll.
                gpui::div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .p(px(GAP))
                    // Measured inside the padding, which is the box the grid gets; the row is
                    // laid out to fill exactly this width, so the last column ends at the edge.
                    .child(gpui::div().absolute().inset(px(GAP)).child(measure_then(self.content_size.clone(), cx.weak_entity(), Self::fit_columns)))
                    .child(grid)
                    .children(empty),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env, seed_item, Seed, TestEnv};
    use gpui::{Modifiers as GModifiers, MouseButton, MouseDownEvent, Point, ScrollWheelEvent, TestAppContext, VisualTestContext};
    use std::path::PathBuf;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    fn down(pos_x: f32, cmd: bool, shift: bool, count: usize) -> MouseDownEvent {
        MouseDownEvent {
            button: MouseButton::Left,
            position: Point { x: px(pos_x), y: px(0.) },
            modifiers: GModifiers { control: false, alt: false, shift, platform: cmd, function: false },
            click_count: count,
            first_mouse: false,
        }
    }

    /// Build a feed window over a seeded library; returns (view, VisualTestContext, env).
    macro_rules! feed_window {
        ($cx:ident, $n:expr) => {{
            let env = env($cx, $n, "Mock");
            let (view, vcx) = $cx.add_window_view(FeedView::new);
            vcx.run_until_parked();
            (view, vcx, env)
        }};
    }

    /// Columns the zoom level fits in the measured width: what a user sees, computed independently
    /// of the view's own bookkeeping.
    fn fitted_columns(f: &FeedView) -> usize {
        feed::columns_for(f32::from(slot_size(&f.content_size).width), f.zoom)
    }

    /// A file removed from the folder behind the app's back keeps its place in the grid (as a
    /// "File missing" cell), can't be dragged out, and Retry regenerates it under the same id.
    #[gpui::test]
    fn missing_file_stays_in_the_grid_and_retry_regenerates_it(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 2);
        let missing = seed_item(&env.library, vcx, Seed { status: Status::Missing, favorite: true, ..Seed::default() });
        let entry = EntryId::Generation(missing.clone());
        vcx.run_until_parked();
        view.update(vcx, |f, cx| {
            assert_eq!(f.ids.len(), 3, "still listed");
            assert!(f.ids.contains(&entry));
            assert!(f.drag_paths(&entry, cx).is_empty(), "nothing on disk to drag");
            let item = f.library.read(cx).lib.get(&missing).cloned().unwrap();
            assert!(item.is_favorite, "metadata survives");
            assert!(item.path.is_some(), "the expected location is known");
        });
        view.update(vcx, |f, cx| {
            f.set_filter(FeedFilter::Favorites, cx);
            assert_eq!(f.ids, vec![entry.clone()], "favourites still include it");
            f.set_filter(FeedFilter::Library, cx);
        });

        let ix = view.update(vcx, |f, _| f.ids.iter().position(|i| i == &entry).unwrap());
        view.update(vcx, |f, cx| f.cell_mouse_down(ix, &entry, &down(0., false, false, 1), cx));
        vcx.dispatch_action(Retry);
        vcx.run_until_parked();
        env.library.read_with(vcx, |m, _| {
            let item = m.lib.get(&missing).unwrap();
            assert_eq!(item.status, Status::Generating, "regenerating in place");
            assert!(item.is_favorite);
        });
    }

    #[gpui::test]
    fn deleting_a_missing_file_removes_the_row(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 1);
        let missing = seed_item(&env.library, vcx, Seed { status: Status::Missing, ..Seed::default() });
        let entry = EntryId::Generation(missing.clone());
        vcx.run_until_parked();
        let ix = view.update(vcx, |f, _| f.ids.iter().position(|i| i == &entry).unwrap());
        view.update(vcx, |f, cx| f.cell_mouse_down(ix, &entry, &down(0., false, false, 1), cx));
        vcx.dispatch_action(DeleteMedia);
        vcx.simulate_prompt_answer("Delete");
        vcx.run_until_parked();
        view.update(vcx, |f, cx| {
            assert_eq!(f.ids.len(), 1);
            assert!(f.library.read(cx).lib.get(&missing).is_none());
        });
    }

    fn first_item_path(view: &Entity<FeedView>, vcx: &mut VisualTestContext, env: &TestEnv) -> (GenerationId, PathBuf) {
        let id = view.read_with(vcx, |f, _| f.ids[0].media().expect("a generation").clone());
        let path = env.library.read_with(vcx, |m, _| m.lib.get(&id).unwrap().path.clone().expect("completed item has a file"));
        (id, path)
    }

    fn trashed_names(env: &TestEnv) -> Vec<String> {
        std::fs::read_dir(env.dir.path().join(".majik/trash")).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
    }

    #[gpui::test]
    fn delete_confirms_then_removes_the_item_and_keeps_its_file_as_an_asset(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 1);
        let (id, path) = first_item_path(&view, vcx, &env);
        let output = env.library.read_with(vcx, |m, _| m.lib.get(&id).unwrap().output_asset_id.clone().unwrap());
        view.update(vcx, |f, cx| f.cell_mouse_down(0, &EntryId::Generation(id.clone()), &down(0., false, false, 1), cx));
        vcx.dispatch_action(DeleteMedia);
        let (message, detail) = vcx.pending_prompt().expect("delete asks first");
        assert_eq!(message, "Delete this item?");
        assert!(detail.contains("stay"), "{detail}");
        vcx.simulate_prompt_answer("Delete");
        vcx.run_until_parked();
        assert!(path.exists(), "the file is an asset in its own right and stays");
        assert!(trashed_names(&env).is_empty());
        env.library.read_with(vcx, |m, _| {
            assert!(m.lib.get(&id).is_none());
            assert!(m.lib.asset(&output).is_some() && !m.lib.is_referenced(&output), "the asset outlives the generation, unreferenced");
        });
        view.read_with(vcx, |f, _| assert!(f.ids.is_empty()));
    }

    #[gpui::test]
    fn cancelling_the_delete_prompt_keeps_the_item(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 1);
        let (id, path) = first_item_path(&view, vcx, &env);
        view.update(vcx, |f, cx| f.cell_mouse_down(0, &EntryId::Generation(id.clone()), &down(0., false, false, 1), cx));
        vcx.dispatch_action(DeleteMedia);
        vcx.simulate_prompt_answer("Cancel");
        vcx.run_until_parked();
        assert!(path.exists());
        assert!(trashed_names(&env).is_empty());
        env.library.read_with(vcx, |m, _| assert!(m.lib.get(&id).is_some()));
    }

    #[gpui::test]
    fn multi_select_delete_prompt_shows_the_count(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 3);
        view.update(vcx, |f, cx| {
            f.selection.select_all(&f.ids);
            cx.notify();
        });
        vcx.simulate_keystrokes("backspace");
        assert_eq!(vcx.pending_prompt().map(|(m, _)| m).as_deref(), Some("Delete 3 items?"));
        vcx.simulate_prompt_answer("Delete");
        vcx.run_until_parked();
        env.library.read_with(vcx, |m, _| assert!(m.lib.generations().is_empty() && m.lib.assets().len() == 3));
        view.read_with(vcx, |f, _| assert!(f.ids.is_empty()));
    }

    #[gpui::test]
    fn delete_with_nothing_selected_does_not_prompt(cx: &mut TestAppContext) {
        let (_view, vcx, _env) = feed_window!(cx, 2);
        vcx.dispatch_action(DeleteMedia);
        assert!(!vcx.has_pending_prompt());
    }

    /// The relaunch case: a row the previous run left generating with a provider handle keeps
    /// spinning in the grid (resumed, not failed) and completes in place when the result arrives.
    #[gpui::test]
    fn a_resumed_generation_keeps_spinning_and_completes_in_place(cx: &mut TestAppContext) {
        let env = env(cx, 1, "Mock");
        let resumed = env.library.update(cx, |m, _| {
            let id = m.lib.add_generating(MediaType::Image, None, Some("mock".into()), Some("Mock".into()), None);
            m.lib.mark_running(&id, Some("mock-image-1".into()), None);
            id
        });
        let (library, jobs) = crate::test_support::reopen_recording(&env, cx);
        assert!(matches!(jobs.lock().unwrap().as_slice(), [majik_generation::Job::Resume { .. }]));
        let (view, vcx) = cx.add_window_view(FeedView::new);
        vcx.run_until_parked();
        view.read_with(vcx, |f, cx| {
            assert_eq!(f.ids.len(), 2);
            assert_eq!(f.library.read(cx).lib.get(&resumed).unwrap().status, Status::Generating, "shown as generating, with Cancel, not as failed");
        });
        library.update(vcx, |m, cx| m.apply(majik_generation::Event::Completed { id: resumed.clone(), job: m.attempt(&resumed.clone()), bytes: majik_core::images::solid_png(8, 8, [9, 8, 7]), is_upscaled: false }, cx));
        vcx.run_until_parked();
        view.read_with(vcx, |f, cx| {
            let item = f.library.read(cx).lib.get(&resumed).unwrap();
            assert_eq!(item.status, Status::Completed);
            assert!(item.path.as_ref().is_some_and(|p| p.is_file()));
            assert_eq!(f.ids.len(), 2, "same row, same place");
        });
    }

    #[gpui::test]
    fn feed_lists_and_filters(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 6);
        view.update(vcx, |f, _| assert_eq!(f.ids.len(), 6));
        // Filter to video → none of the seeded items match.
        view.update(vcx, |f, cx| {
            f.media_filter = MediaFilter::Video;
            f.refresh(Change::Filter, cx);
            assert_eq!(f.ids.len(), 0);
            f.media_filter = MediaFilter::Image;
            f.refresh(Change::Filter, cx);
            assert_eq!(f.ids.len(), 6);
        });
    }

    #[gpui::test]
    fn favorites_only_lists_only_favorited_items(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 3);
        let favorite = seed_item(&env.library, vcx, Seed { favorite: true, ..Seed::default() });
        vcx.run_until_parked();
        view.update(vcx, |f, cx| {
            assert!(!f.favorites_only, "off by default");
            assert_eq!(f.ids.len(), 4);
            f.set_favorites_only(true, cx);
            assert_eq!(f.ids, vec![EntryId::Generation(favorite.clone())]);
            // Combines with the media filter rather than replacing it.
            f.media_filter = MediaFilter::Video;
            f.refresh(Change::Filter, cx);
            assert!(f.ids.is_empty());
            f.media_filter = MediaFilter::All;
            f.set_favorites_only(false, cx);
            assert_eq!(f.ids.len(), 4, "off shows everything again");
        });
    }

    #[gpui::test]
    fn favorites_only_follows_the_library_and_survives_feed_changes(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 2);
        let album = env.library.update(vcx, |m, cx| m.create_album("Trips".into(), cx));
        let all: Vec<GenerationId> = view.read_with(vcx, |f, _| f.ids.iter().filter_map(|id| id.media().cloned()).collect());
        env.library.update(vcx, |m, cx| m.add_to_album(&album, &all, cx));
        view.update(vcx, |f, cx| f.set_favorites_only(true, cx));
        vcx.run_until_parked();
        view.update(vcx, |f, _| assert!(f.ids.is_empty()));

        // Favoriting from elsewhere (the detail, another window) brings the item into the grid…
        env.library.update(vcx, |m, cx| m.set_favorite(std::slice::from_ref(&all[0]), true, cx));
        vcx.run_until_parked();
        view.update(vcx, |f, cx| {
            assert_eq!(f.ids, vec![EntryId::Generation(all[0].clone())]);
            f.selection.select_all(&[EntryId::Generation(all[0].clone())]);
            // …and the toggle applies to albums too, keeping its state across feeds.
            f.set_filter(FeedFilter::Album(album.clone()), cx);
            assert!(f.favorites_only);
            assert_eq!(f.ids, vec![EntryId::Generation(all[0].clone())]);
            f.set_filter(FeedFilter::Library, cx);
            f.selection.select_all(&[EntryId::Generation(all[0].clone())]);
        });
        // …and unfavoriting drops the cell and its selection.
        env.library.update(vcx, |m, cx| m.set_favorite(std::slice::from_ref(&all[0]), false, cx));
        vcx.run_until_parked();
        view.update(vcx, |f, _| {
            assert!(f.ids.is_empty());
            assert!(f.selection.is_empty());
        });
    }

    #[gpui::test]
    fn favorites_only_toggle_is_offered_where_favorites_can_differ(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 1);
        view.update(vcx, |f, cx| {
            assert!(f.shows_favorites_toggle());
            f.set_filter(FeedFilter::Album(majik_core::model::AlbumId("any".into())), cx);
            assert!(f.shows_favorites_toggle());
            f.set_filter(FeedFilter::Favorites, cx);
            assert!(!f.shows_favorites_toggle(), "the Favorites feed already is that filter");
            f.set_filter(FeedFilter::Assets, cx);
            assert!(!f.shows_favorites_toggle(), "assets carry no favorite");
            // The Assets feed lists everything even while the toggle is on.
            f.set_favorites_only(true, cx);
            assert_eq!(f.ids.len(), 1);
        });
    }

    #[gpui::test]
    fn favorites_only_empty_state_names_the_filter(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 2);
        view.update(vcx, |f, cx| {
            f.set_favorites_only(true, cx);
            assert!(f.ids.is_empty());
            assert_eq!(f.empty_state(cx).1, "No Favorites Here");
            f.set_filter(FeedFilter::Favorites, cx);
            assert_eq!(f.empty_state(cx).1, "No Favorites Yet", "the feed's own copy where the toggle is not offered");
        });
    }

    #[gpui::test]
    fn selection_click_cmd_shift_and_right_click(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 5);
        view.update(vcx, |f, cx| {
            let ids = f.ids.clone();
            // Plain click selects one.
            f.cell_mouse_down(0, &ids[0], &down(0., false, false, 1), cx);
            assert_eq!(f.selection.len(), 1);
            // Cmd-click adds.
            f.cell_mouse_down(2, &ids[2], &down(0., true, false, 1), cx);
            assert_eq!(f.selection.len(), 2);
            assert!(f.selection.contains(&ids[0]) && f.selection.contains(&ids[2]));
            // Shift-click extends the range from index 2 to 4.
            f.cell_mouse_down(4, &ids[4], &down(0., false, true, 1), cx);
            assert!(f.selection.contains(&ids[3]) && f.selection.contains(&ids[4]));
            // Right-click on an unselected item replaces the selection.
            f.selection.right_click(&ids[1], 1);
            assert_eq!(f.selection.ids, [ids[1].clone()].into_iter().collect());
            // Right-click on a member of a multi-selection preserves it.
            f.selection.select_all(&ids);
            f.selection.right_click(&ids[2], 2);
            assert_eq!(f.selection.len(), 5);
        });
    }

    #[gpui::test]
    fn multi_selection_drag_defers_collapse(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 4);
        view.update(vcx, |f, cx| {
            let ids = f.ids.clone();
            f.selection.select_all(&ids);
            // Plain press on a member must NOT collapse the selection yet (so a drag can carry all).
            f.cell_mouse_down(1, &ids[1], &down(0., false, false, 1), cx);
            assert_eq!(f.selection.len(), 4, "multi-selection preserved on press");
            assert!(f.deferred_click.is_some());
            // drag_items therefore carries the whole set.
            assert_eq!(f.drag_paths(&ids[1], cx).len(), 4);
        });
    }

    /// Focus can be anywhere when the user clicks a thumbnail (the composer, the window chrome
    /// after the click that brought the app forward); the click brings it to the grid so the
    /// arrow keys and shortcuts work from what was just selected.
    #[gpui::test]
    fn clicking_a_cell_focuses_the_grid(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 4);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let cells: Vec<_> = view.update(vcx, |f, _| f.ids.iter().map(|id| f.cell_bounds(id).expect("cell drawn")).collect());
        vcx.update(|window, cx| window.blur(cx));
        view.update_in(vcx, |f, window, _| assert!(!f.focus.is_focused(window)));
        vcx.simulate_mouse_down(cells[1].center(), MouseButton::Left, GModifiers::default());
        vcx.simulate_mouse_up(cells[1].center(), MouseButton::Left, GModifiers::default());
        vcx.run_until_parked();
        view.update_in(vcx, |f, window, _| {
            assert!(f.focus.is_focused(window), "the click focused the grid");
            assert_eq!(f.selection.len(), 1, "and selected the cell");
        });
        vcx.simulate_keystrokes("right");
        view.update(vcx, |f, _| assert!(f.selection.contains(&f.ids[2]), "the arrow moved on from the clicked cell"));
    }

    /// A square cell shows the middle of a tall or wide picture. gpui's image element gives itself
    /// the picture's aspect ratio when its box is not pinned, which made the frame as tall as the
    /// scaled picture and the clipped cell show its top; the frame has to be the cell's own size.
    #[gpui::test]
    fn a_square_cell_frames_a_tall_picture_to_the_cell(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 3);
        env.library.update(vcx, |m, cx| m.start_thumbnails(cx));
        vcx.run_until_parked();
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        // Two frames: the first asks the cache for the pictures, the second draws them decoded,
        // which is when gpui knows their aspect.
        for _ in 0..2 {
            vcx.update(|window, cx| window.draw(cx).clear(cx));
            vcx.run_until_parked();
        }
        let (ids, side) = view.update(vcx, |f, _| (f.ids.clone(), f.cell_px));
        view.update(vcx, |f, _| assert_eq!(f.shape, ThumbnailShape::Square));
        // Seeded as 64×64, 96×48 and 48×96.
        for id in &ids {
            let frame = view.update(vcx, |f, _| f.cell_bounds(id).expect("cell drawn"));
            assert!((frame.size.width - side).abs() < px(1.) && (frame.size.height - side).abs() < px(1.), "{id}: {:?} in a {side:?} cell", frame.size);
            // `debug_bounds` wants a static selector; a leaked string per cell is fine in a test.
            let selector: &'static str = Box::leak(format!("thumb-{}", id.media().expect("a generation")).into_boxed_str());
            let picture = vcx.debug_bounds(selector).expect("the thumbnail is drawn");
            assert_eq!(picture, frame, "{id}: the picture element is the frame, not its own aspect");
        }

        // Full-aspect frames: the picture is still exactly its frame.
        vcx.dispatch_action(ToggleThumbnailShape);
        for _ in 0..2 {
            vcx.update(|window, cx| window.draw(cx).clear(cx));
            vcx.run_until_parked();
        }
        view.update(vcx, |f, _| assert_eq!(f.shape, ThumbnailShape::AspectRatio));
        for id in &ids {
            let frame = view.update(vcx, |f, _| f.cell_bounds(id).expect("cell drawn"));
            let selector: &'static str = Box::leak(format!("thumb-{}", id.media().expect("a generation")).into_boxed_str());
            assert_eq!(vcx.debug_bounds(selector).expect("the thumbnail is drawn"), frame, "{id}");
        }
    }

    /// The cells fill the grid's width exactly, so the last column's right edge (and its selection
    /// ring) is inside the grid that clips it. The width was measured on a box that included the
    /// grid's padding, which laid the row out 4 px wider than the grid.
    #[gpui::test]
    fn the_last_column_ends_at_the_grids_edge(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 8);
        for width in [800., 1100., 640.] {
            vcx.simulate_resize(gpui::size(px(width), px(600.)));
            vcx.run_until_parked();
            vcx.update(|window, cx| window.draw(cx).clear(cx));
            let grid = vcx.debug_bounds("feed-grid").expect("the grid is drawn");
            view.update(vcx, |f, _| {
                let right = f.ids.iter().filter_map(|id| f.cell_bounds(id)).map(|b| b.right()).fold(px(0.), Pixels::max);
                assert!(right <= grid.right() + px(0.5), "at {width} px: the row ends at {right:?}, the grid at {:?}", grid.right());
                let left = f.ids.iter().filter_map(|id| f.cell_bounds(id)).map(|b| b.left()).fold(px(f32::MAX), Pixels::min);
                assert!(left >= grid.left() - px(0.5), "at {width} px: the row starts at {left:?}, the grid at {:?}", grid.left());
            });
        }
    }

    /// Pressing a cell and moving past GPUI's drag threshold starts a drag-out of that cell.
    #[gpui::test]
    fn press_and_move_on_a_cell_starts_a_drag(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 4);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let cell = view.update(vcx, |f, _| f.cell_bounds(&f.ids[0]).expect("first cell drawn"));
        let press = cell.center();
        vcx.simulate_mouse_down(press, MouseButton::Left, GModifiers::default());
        vcx.run_until_parked();
        assert!(!vcx.update(|_, cx| cx.has_active_drag()), "a press alone is not a drag");
        vcx.simulate_mouse_move(press + point(px(12.), px(12.)), MouseButton::Left, GModifiers::default());
        vcx.run_until_parked();
        assert!(vcx.update(|_, cx| cx.has_active_drag()), "moving past the threshold starts the drag");
    }

    #[gpui::test]
    fn a_drag_carries_the_assets_behind_the_cells(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 2);
        let generating = seed_item(&env.library, vcx, Seed { status: Status::Generating, ..Seed::default() });
        let import = crate::test_support::seed_asset(&env.library, vcx, MediaType::Audio, 3);
        vcx.run_until_parked();
        view.update(vcx, |f, cx| {
            let outputs: Vec<AssetId> = f.ids.iter().filter_map(|id| id.media()).filter_map(|id| f.library.read(cx).lib.get(id).and_then(|i| i.output_asset_id.clone())).collect();
            assert_eq!(outputs.len(), 2);
            f.selection.select_all(&f.ids);
            let dragged = f.dragged(&f.ids[0].clone(), cx);
            let ids: Vec<AssetId> = dragged.assets.iter().map(|a| a.id.clone()).collect();
            assert_eq!(ids.len(), 2, "a generation drags its output; one in flight has nothing to drag");
            assert!(outputs.iter().all(|o| ids.contains(o)));
            assert!(dragged.assets.iter().all(|a| a.kind == MediaType::Image && a.path.exists()));
            let mut finished: Vec<GenerationId> = f.ids.iter().filter_map(|id| id.media()).filter(|id| f.library.read(cx).lib.get(id).is_some_and(|i| i.output_asset_id.is_some())).cloned().collect();
            let mut named = dragged.generations();
            named.sort();
            finished.sort();
            assert_eq!(named, finished, "a generation cell names its row, for an album drop");
            f.selection.clear();
            assert!(f.dragged(&EntryId::Generation(generating), cx).assets.is_empty());
        });
        assets_feed(&view, vcx);
        view.update(vcx, |f, cx| {
            let dragged = f.dragged(&EntryId::Asset(import.clone()), cx);
            assert_eq!(dragged.assets.len(), 1, "an unselected cell drags itself alone");
            assert_eq!((dragged.assets[0].id.clone(), dragged.assets[0].kind), (import, MediaType::Audio));
            assert_eq!(dragged.assets[0].generation, None, "an import has no generation behind it");
            let output = f.ids.iter().filter_map(|id| id.asset().cloned()).find(|a| f.library.read(cx).lib.generation_producing(a).is_some()).expect("an output in the Assets grid");
            let dragged = f.dragged(&EntryId::Asset(output.clone()), cx);
            assert_eq!(dragged.generations(), vec![f.library.read(cx).lib.generation_producing(&output).unwrap()], "an output names the generation that made it");
        });
    }

    /// The preview follows the pointer: its box is centred on the point where the cell was pressed,
    /// not pinned to the cell's top-left corner.
    #[gpui::test]
    fn drag_preview_is_centred_under_the_cursor(_cx: &mut TestAppContext) {
        let preview = DragPreview { image: None, count: 1, press_offset: point(px(150.), px(70.)) };
        let half = DragPreview::SIZE / 2.;
        assert_eq!(preview.box_origin(), point(px(150.) - half, px(70.) - half));
    }

    /// Zoom steps through the tile-width levels; the column count follows so no tile is narrower
    /// than its level.
    #[gpui::test]
    fn zoom_actions_step_the_tile_width(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 30);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let check = |view: &Entity<FeedView>, vcx: &mut VisualTestContext, zoom: u32| {
            vcx.run_until_parked();
            view.update(vcx, |f, _| {
                assert_eq!(f.zoom, zoom);
                assert_eq!(f.columns, fitted_columns(f));
                assert!(f.cell_px >= px(zoom as f32), "{:?} px tiles at level {zoom}", f.cell_px);
            });
        };
        check(&view, vcx, feed::DEFAULT_ZOOM);
        view.update(vcx, |f, _| assert_eq!(f.columns, 4, "160 px tiles across 796 px"));
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        check(&view, vcx, 200);
        view.update(vcx, |f, _| assert_eq!(f.columns, 3));
        vcx.dispatch_action(super::super::super::actions::ZoomOut);
        vcx.dispatch_action(super::super::super::actions::ZoomOut);
        check(&view, vcx, 120);
        view.update(vcx, |f, _| assert_eq!(f.columns, 6));
        vcx.dispatch_action(super::super::super::actions::ResetZoom);
        check(&view, vcx, feed::DEFAULT_ZOOM);
    }

    /// The toolbar's zoom buttons grey out at the ends of the range and come back one step in.
    #[gpui::test]
    fn zoom_buttons_disable_at_the_ends(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 3);
        let can = |view: &Entity<FeedView>, vcx: &mut VisualTestContext| view.update(vcx, |f, _| (f.can_zoom_in(), f.can_zoom_out()));
        assert_eq!(can(&view, vcx), (true, true), "the default level is in the middle");
        for _ in 0..feed::ZOOM_LEVELS.len() {
            vcx.dispatch_action(super::super::super::actions::ZoomIn);
        }
        view.update(vcx, |f, _| assert_eq!(f.zoom, *feed::ZOOM_LEVELS.last().unwrap()));
        assert_eq!(can(&view, vcx), (false, true), "largest tiles: only zoom out");
        vcx.dispatch_action(super::super::super::actions::ZoomOut);
        assert_eq!(can(&view, vcx), (true, true));
        for _ in 0..feed::ZOOM_LEVELS.len() {
            vcx.dispatch_action(super::super::super::actions::ZoomOut);
        }
        view.update(vcx, |f, _| assert_eq!(f.zoom, feed::ZOOM_LEVELS[0]));
        assert_eq!(can(&view, vcx), (true, false), "smallest tiles: only zoom in");
        vcx.dispatch_action(super::super::super::actions::ResetZoom);
        assert_eq!(can(&view, vcx), (true, true));
    }

    /// Narrowing the window drops columns rather than shrinking tiles below the zoom level; the
    /// reflow is instant and leaves the motion baseline on the new layout.
    #[gpui::test]
    fn narrowing_the_window_drops_columns_instead_of_shrinking_tiles(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 60);
        arm_motion(&view, vcx);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(Duration::from_millis(300));
        vcx.run_until_parked();
        view.update(vcx, |f, _| assert_eq!((f.zoom, f.columns), (200, 3)));
        for (window, columns) in [(500., 2), (320., 1)] {
            vcx.simulate_resize(gpui::size(px(window), px(600.)));
            vcx.run_until_parked();
            view.update(vcx, |f, cx| {
                assert_eq!(f.columns, columns, "{window} px window");
                assert!(f.cell_px >= px(200.) || columns == 1, "{:?} px tiles", f.cell_px);
                f.motion.tick(now(cx));
                assert!(!f.motion.is_animating(), "a resize reflows without motion");
                assert_eq!(f.motion.moving_ids().count(), 0);
                assert_eq!(f.motion.place(&f.ids[1]), Some(Place { index: 1, columns }), "places follow the resize");
            });
        }
    }

    /// Widening adds columns rather than growing tiles past twice the level.
    #[gpui::test]
    fn widening_the_window_adds_columns(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 60);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        view.update(vcx, |f, _| assert_eq!(f.columns, 4));
        vcx.simulate_resize(gpui::size(px(1600.), px(600.)));
        vcx.run_until_parked();
        view.update(vcx, |f, _| {
            assert_eq!(f.columns, 9);
            assert!(f.cell_px >= px(160.) && f.cell_px < px(2. * 160. + GAP), "{:?}", f.cell_px);
        });
    }

    /// A resize that changes the column count keeps the item that led the top row on top.
    #[gpui::test]
    fn resize_keeps_the_top_row(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 60);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let over_grid = point(px(400.), px(300.));
        vcx.simulate_mouse_move(over_grid, None, gpui::Modifiers::default());
        let three_rows = view.update(vcx, |f, _| 3. * (f32::from(f.cell_px) + GAP));
        vcx.simulate_event(ScrollWheelEvent { position: over_grid, delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-three_rows))), modifiers: gpui::Modifiers::default(), touch_phase: gpui::TouchPhase::Moved });
        let top_item = view.update(vcx, |f, _| {
            assert_eq!(f.top_row(), 3);
            f.ids[f.top_row() * f.columns].clone()
        });
        vcx.simulate_resize(gpui::size(px(500.), px(600.)));
        vcx.run_until_parked();
        view.update(vcx, |f, _| {
            assert_eq!(f.columns, 3);
            assert_eq!(f.ids[f.top_row() * f.columns], top_item, "the item that led the top row still does");
        });
    }

    /// The instant resize reflow doesn't poison the baseline: the next zoom still animates every
    /// cell from where the resized layout put it.
    #[gpui::test]
    fn zoom_after_resize_still_animates(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 12);
        arm_motion(&view, vcx);
        vcx.simulate_resize(gpui::size(px(500.), px(600.)));
        vcx.run_until_parked();
        view.update(vcx, |f, _| assert_eq!(f.columns, 3));
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        view.update(vcx, |f, _| {
            assert_eq!(f.columns, 2);
            assert!(f.ids.iter().all(|id| f.motion.is_moving(id)), "every cell slides/resizes");
            assert_eq!(f.motion.ghost_count(), 0);
        });
    }

    /// In a window too narrow for the column count to change, a zoom step still persists the level.
    #[gpui::test]
    fn zoom_persists_even_when_the_column_count_cannot_change(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 6);
        vcx.simulate_resize(gpui::size(px(250.), px(600.)));
        vcx.run_until_parked();
        view.update(vcx, |f, _| assert_eq!((f.zoom, f.columns), (160, 1)));
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        view.update(vcx, |f, cx| {
            assert_eq!((f.zoom, f.columns), (200, 1));
            assert_eq!(cx.global::<Config>().grid_zoom, 200);
            assert!(!f.motion.is_animating());
        });
    }

    /// The grid is bounded by the window and scrolls under the wheel: the rows beyond the viewport
    /// are reachable, and only the rows around it are drawn.
    #[gpui::test]
    fn four_hundred_items_draw_only_the_rows_around_the_viewport(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 400);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let (drawn, columns, first_drawn, last_drawn) = view.update(vcx, |f, _| {
            assert_eq!(f.ids.len(), 400);
            (f.last_rendered.len(), f.columns, f.last_rendered.contains_key(&f.ids[0]), f.last_rendered.contains_key(&f.ids[399]))
        });
        assert!(drawn <= columns * 12, "a 600 px window draws a few rows plus overscan, not {drawn} cells");
        assert!(first_drawn && !last_drawn, "top of the grid is what's drawn");
        // A fling far past the end stops on the last rows, and draws them on that very frame.
        let over_grid = point(px(500.), px(300.));
        vcx.simulate_mouse_move(over_grid, None, gpui::Modifiers::default());
        vcx.simulate_event(ScrollWheelEvent { position: over_grid, delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-1_000_000.))), modifiers: gpui::Modifiers::default(), touch_phase: gpui::TouchPhase::Moved });
        vcx.run_until_parked();
        let (drawn, first_drawn, last_drawn) = view.update(vcx, |f, _| (f.last_rendered.len(), f.last_rendered.contains_key(&f.ids[0]), f.last_rendered.contains_key(&f.ids[399])));
        assert!(drawn <= columns * 12, "{drawn}");
        assert!(!first_drawn && last_drawn, "scrolled to the bottom, only the last rows are drawn ({drawn} cells)");
    }

    #[gpui::test]
    fn wheel_scrolls_the_grid(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 40);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let (viewport, content, drawn, total) = view.update(vcx, |f, _| {
            let rows = f.ids.len().div_ceil(f.columns);
            (slot_size(&f.content_size).height, px(rows as f32 * (f32::from(f.cell_px) + GAP)), f.last_rendered.len(), f.ids.len())
        });
        assert!(viewport > px(0.) && viewport <= px(600.), "the grid is clipped to the window, not sized to its rows: {viewport:?}");
        assert!(content > viewport, "40 items overflow a 600 px window: {content:?}");
        assert!(drawn < total, "only the rows around the viewport are drawn: {drawn} of {total}");

        let over_grid = point(px(500.), px(300.));
        vcx.simulate_mouse_move(over_grid, None, gpui::Modifiers::default());
        let wheel = |delta: gpui::ScrollDelta| ScrollWheelEvent { position: over_grid, delta, modifiers: gpui::Modifiers::default(), touch_phase: gpui::TouchPhase::Moved };
        vcx.simulate_event(wheel(gpui::ScrollDelta::Pixels(point(px(0.), px(-200.)))));
        let after_trackpad = view.update(vcx, |f, _| f.scroll.offset().y);
        assert_eq!(after_trackpad, px(-200.), "a trackpad scroll moves the grid by its pixel delta");
        vcx.simulate_event(wheel(gpui::ScrollDelta::Lines(point(0., -3.))));
        let after_wheel = view.update(vcx, |f, _| f.scroll.offset().y);
        assert!(after_wheel < after_trackpad, "a wheel mouse scrolls too: {after_wheel:?}");
        vcx.simulate_event(wheel(gpui::ScrollDelta::Pixels(point(px(0.), px(10_000.)))));
        view.update(vcx, |f, _| assert_eq!(f.scroll.offset().y, px(0.), "clamped at the top"));
    }

    /// Zooming a scrolled feed keeps the top row's first item on screen and leaves the grid
    /// scrollable at the new pitch.
    #[gpui::test]
    fn zoom_keeps_the_top_row_and_still_scrolls(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 40);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let over_grid = point(px(500.), px(300.));
        let wheel = |dy: f32| ScrollWheelEvent { position: over_grid, delta: gpui::ScrollDelta::Pixels(point(px(0.), px(dy))), modifiers: gpui::Modifiers::default(), touch_phase: gpui::TouchPhase::Moved };
        vcx.simulate_mouse_move(over_grid, None, gpui::Modifiers::default());
        // `cell_px` is only refreshed by a render; derive the pitch from the measured width instead.
        let three_rows = view.update(vcx, |f, _| 3. * (f32::from(cell_for(slot_size(&f.content_size).width, f.columns)) + GAP));
        vcx.simulate_event(wheel(-three_rows));
        let top_item = view.update(vcx, |f, _| {
            assert_eq!(f.top_row(), 3);
            f.ids[f.top_row() * f.columns].clone()
        });

        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        vcx.run_until_parked();
        view.update(vcx, |f, _| {
            assert_eq!((f.zoom, f.columns), (200, 3));
            let viewport = slot_size(&f.content_size).height;
            assert!(viewport > px(0.) && viewport <= px(600.), "still clipped to the window: {viewport:?}");
            assert_eq!(f.ids[f.top_row() * f.columns], top_item, "the item that led the top row still does");
        });
        let before = view.update(vcx, |f, _| f.scroll.offset().y);
        vcx.simulate_event(wheel(-100.));
        view.update(vcx, |f, _| assert_eq!(f.scroll.offset().y, before - px(100.), "scrolls at the new pitch"));
        vcx.simulate_event(wheel(10_000.));
        view.update(vcx, |f, _| assert_eq!(f.scroll.offset().y, px(0.), "clamped at the top"));
    }

    #[gpui::test]
    fn recreate_hands_the_rows_id_to_the_composer_tools_included(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 0);
        let id = seed_item(&env.library, vcx, Seed { upscaled: true, ..Seed::default() });
        let handed: Rc<std::cell::RefCell<Vec<PendingCompose>>> = Default::default();
        let h = handed.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&view, move |_, ev: &FeedEvent, _| {
                if let FeedEvent::Compose(pending) = ev {
                    h.borrow_mut().push(pending.clone());
                }
            })
            .detach();
        });
        view.update_in(vcx, |f, w, cx| {
            f.selection.clear();
            f.selection.ids.insert(EntryId::Generation(id.clone()));
            f.recreate(&Recreate, w, cx);
        });
        vcx.run_until_parked();
        assert_eq!(handed.borrow().iter().map(|p| p.recreate.clone()).collect::<Vec<_>>(), vec![Some(id)], "the composer reads the request itself");
    }

    #[gpui::test]
    fn double_click_emits_open(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 4);
        let opened: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        let o = opened.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&view, move |_, ev: &FeedEvent, _| {
                if let FeedEvent::Open { index, .. } = ev {
                    o.set(Some(*index));
                }
            })
            .detach();
        });
        view.update(vcx, |f, cx| {
            let id = f.ids[2].clone();
            f.cell_mouse_down(2, &id, &down(0., false, false, 2), cx);
        });
        vcx.run_until_parked();
        assert_eq!(opened.get(), Some(2));
    }

    /// Pretend every item was drawn last frame (headless tests never paint), and turn motion on the
    /// way `render` does after the initial load.
    fn arm_motion(view: &Entity<FeedView>, vcx: &mut gpui::VisualTestContext) {
        view.update(vcx, |f, cx| {
            f.motion.set_enabled(!cx.reduce_motion());
            let lib = f.library.read(cx);
            f.last_rendered = f.ids.iter().filter_map(|id| lib.lib.entry(id).map(|e| (id.clone(), CellSnapshot::from(e)))).collect();
        });
    }

    #[gpui::test]
    fn favoriting_into_favorites_feed_marks_cell_entering(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 3);
        let first = view.update(vcx, |f, _| f.ids[0].clone());
        let first_media = first.media().unwrap().clone();
        view.update(vcx, |f, cx| f.set_filter(FeedFilter::Favorites, cx));
        arm_motion(&view, vcx);
        view.update(vcx, |f, _| assert!(f.ids.is_empty()));
        env.library.update(vcx, |m, cx| m.set_favorite(std::slice::from_ref(&first_media), true, cx));
        view.update(vcx, |f, cx| {
            assert_eq!(f.ids, vec![first.clone()]);
            assert!(f.motion.is_entering(&first));
            let (_, style) = f.motion.cell(&first, Visual::default(), now(cx));
            assert_eq!(style.scale, crate::grid_motion::ENTER_SCALE);
        });
        vcx.background_executor.advance_clock(Duration::from_millis(800));
        view.update(vcx, |f, cx| {
            f.motion.tick(now(cx));
            assert!(!f.motion.is_animating());
        });
    }

    #[gpui::test]
    fn delete_of_rendered_cell_fades_it_and_slides_neighbours(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 4);
        arm_motion(&view, vcx);
        let (victim, after) = view.update(vcx, |f, _| (f.ids[1].clone(), f.ids[2].clone()));
        env.library.update(vcx, |m, cx| m.delete(std::slice::from_ref(victim.media().unwrap()), cx));
        view.update(vcx, |f, _| {
            assert_eq!(f.ids.len(), 3);
            assert_eq!(f.motion.ghost_count(), 1);
            assert_eq!(f.motion.ghosts().next().map(|g| g.snapshot.id()), Some(victim.clone()));
            assert!(f.motion.is_moving(&after), "the cell after the victim slides into its place");
            assert!(!f.motion.is_moving(&f.ids[0]));
            assert!(f.motion.is_animating());
        });
        vcx.background_executor.advance_clock(crate::grid_motion::EXIT_DURATION + Duration::from_millis(10));
        view.update(vcx, |f, cx| {
            f.motion.tick(now(cx));
            assert_eq!(f.motion.ghost_count(), 0, "fast fade");
            assert!(f.motion.is_moving(&after), "slide still settling");
        });
        vcx.background_executor.advance_clock(Duration::from_millis(800));
        view.update(vcx, |f, cx| {
            f.motion.tick(now(cx));
            assert!(!f.motion.is_animating());
        });
    }

    #[gpui::test]
    fn bulk_delete_skips_ghosts(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 8);
        arm_motion(&view, vcx);
        let victims: Vec<GenerationId> = view.update(vcx, |f, _| f.ids[..6].iter().filter_map(|id| id.media().cloned()).collect());
        env.library.update(vcx, |m, cx| m.delete(&victims, cx));
        view.update(vcx, |f, _| {
            assert_eq!(f.ids.len(), 2);
            assert_eq!(f.motion.ghost_count(), 0);
            assert!(!f.motion.is_animating());
        });
    }

    #[gpui::test]
    fn media_filter_change_ghosts_all_rendered_cells(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 6);
        arm_motion(&view, vcx);
        view.update(vcx, |f, cx| {
            f.media_filter = MediaFilter::Video;
            f.refresh(Change::Filter, cx);
            assert!(f.ids.is_empty());
            assert_eq!(f.motion.ghost_count(), 6, "bulk rule does not apply to filter changes");
        });
        vcx.background_executor.advance_clock(crate::grid_motion::REFLOW_DURATION);
        view.update(vcx, |f, cx| {
            f.motion.tick(now(cx));
            assert_eq!(f.motion.ghost_count(), 0);
        });
    }

    #[gpui::test]
    fn zoom_reflows_every_cell_and_persists_the_level(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 3);
        arm_motion(&view, vcx);
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        view.update(vcx, |f, cx| {
            assert_eq!(f.zoom, 200);
            let columns = fitted_columns(f);
            assert_eq!(f.columns, columns);
            assert!(f.ids.iter().all(|id| f.motion.is_moving(id)), "every cell slides/resizes");
            assert_eq!(f.motion.ghost_count(), 0, "nothing fades on zoom");
            assert_eq!(f.motion.place(&f.ids[1]), Some(Place { index: 1, columns }));
            assert_eq!(cx.global::<Config>().grid_zoom, 200);
        });
        vcx.background_executor.advance_clock(Duration::from_millis(300));
        view.update(vcx, |f, cx| {
            f.motion.tick(now(cx));
            assert!(!f.motion.is_animating());
        });
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        view.update(vcx, |f, _| assert_eq!(f.zoom, 240));
        vcx.background_executor.advance_clock(Duration::from_millis(300));
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        view.update(vcx, |f, _| assert_eq!(f.zoom, 300));
        // Zooming at the limit is a no-op.
        vcx.background_executor.advance_clock(Duration::from_millis(300));
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        view.update(vcx, |f, cx| {
            f.motion.tick(now(cx));
            assert_eq!((f.zoom, cx.global::<Config>().grid_zoom), (300, 300));
            assert!(!f.motion.is_animating());
        });
    }

    #[gpui::test]
    fn zoom_restores_from_config(cx: &mut TestAppContext) {
        let _env = env(cx, 0, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().grid_zoom = 120);
        let (view, vcx) = cx.add_window_view(FeedView::new);
        view.update(vcx, |f, _| assert_eq!(f.zoom, 120));
        vcx.update(|_, cx| cx.global_mut::<Config>().grid_zoom = 100);
        let (view, vcx) = cx.add_window_view(FeedView::new);
        view.update(vcx, |f, _| assert_eq!(f.zoom, feed::DEFAULT_ZOOM, "unknown levels fall back"));
    }

    #[gpui::test]
    fn thumbnail_shape_toggles_and_persists(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 3);
        view.update(vcx, |f, _| assert_eq!(f.shape, ThumbnailShape::Square, "Photos' default"));
        vcx.dispatch_action(ToggleThumbnailShape);
        view.update(vcx, |f, cx| {
            assert_eq!(f.shape, ThumbnailShape::AspectRatio);
            assert_eq!(cx.global::<Config>().thumbnail_shape, ThumbnailShape::AspectRatio);
        });
        vcx.dispatch_action(ToggleThumbnailShape);
        view.update(vcx, |f, cx| {
            assert_eq!(f.shape, ThumbnailShape::Square);
            assert_eq!(cx.global::<Config>().thumbnail_shape, ThumbnailShape::Square);
        });
    }

    #[gpui::test]
    fn thumbnail_shape_restores_from_config(cx: &mut TestAppContext) {
        let _env = env(cx, 0, "Mock");
        cx.update(|cx| cx.global_mut::<Config>().thumbnail_shape = ThumbnailShape::AspectRatio);
        let (view, vcx) = cx.add_window_view(FeedView::new);
        view.update(vcx, |f, _| assert_eq!(f.shape, ThumbnailShape::AspectRatio));
    }

    /// Square cells crop every thumbnail to the full cell; aspect-ratio cells shrink the frame (and
    /// with it the selection ring and the morph origin) to the largest box of the image's shape,
    /// centred in the cell. Seeded images are 64×64, 96×48 and 48×96.
    #[gpui::test]
    fn aspect_ratio_shape_fits_the_frame_to_the_image(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 3);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        // A resize alone doesn't redraw the headless window; the frames below need the new layout.
        view.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
        let by_dims = |f: &FeedView, cx: &App, w: u32, h: u32| -> EntryId {
            let lib = f.library.read(cx);
            f.ids.iter().find(|id| lib.lib.entry(id).is_some_and(|e| matches!(e, Entry::Generation(i) if i.width == Some(w) && i.height == Some(h)))).cloned().expect("seeded image")
        };
        let (square, wide, tall) = view.update(vcx, |f, cx| (by_dims(f, cx, 64, 64), by_dims(f, cx, 96, 48), by_dims(f, cx, 48, 96)));
        let frame = |vcx: &mut gpui::VisualTestContext, id: &EntryId| view.update(vcx, |f, _| f.cell_bounds(id).expect("cell drawn"));
        let close = |a: Pixels, b: Pixels| (f32::from(a) - f32::from(b)).abs() < 1.0;

        let wide_cell = frame(vcx, &wide);
        let tall_cell = frame(vcx, &tall);
        let side = wide_cell.size.width;
        assert!(close(wide_cell.size.height, side), "square mode fills the cell");
        assert!(close(tall_cell.size.width, side) && close(tall_cell.size.height, side));

        vcx.dispatch_action(ToggleThumbnailShape);
        vcx.run_until_parked();
        let square_frame = frame(vcx, &square);
        assert!(close(square_frame.size.width, side) && close(square_frame.size.height, side), "a square image still fills the cell");
        let wide_frame = frame(vcx, &wide);
        assert!(close(wide_frame.size.width, side) && close(wide_frame.size.height, side / 2.), "2:1 image: full width, half height ({wide_frame:?})");
        assert!(close(wide_frame.origin.y, wide_cell.origin.y + side / 4.), "centred vertically");
        assert!(close(wide_frame.origin.x, wide_cell.origin.x));
        let tall_frame = frame(vcx, &tall);
        assert!(close(tall_frame.size.width, side / 2.) && close(tall_frame.size.height, side), "1:2 image: half width, full height ({tall_frame:?})");
        assert!(close(tall_frame.origin.x, tall_cell.origin.x + side / 4.), "centred horizontally");
        assert!(close(tall_frame.origin.y, tall_cell.origin.y));

        vcx.dispatch_action(ToggleThumbnailShape);
        vcx.run_until_parked();
        assert_eq!(frame(vcx, &wide), wide_cell, "back to the full cell");
    }

    #[gpui::test]
    fn reduced_motion_skips_cell_motion(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 3);
        vcx.update(|_, cx| cx.set_reduce_motion(true));
        arm_motion(&view, vcx);
        let victim = view.update(vcx, |f, _| f.ids[0].media().unwrap().clone());
        env.library.update(vcx, |m, cx| m.delete(std::slice::from_ref(&victim), cx));
        vcx.dispatch_action(super::super::super::actions::ZoomIn);
        view.update(vcx, |f, _| {
            assert_eq!(f.zoom, 200);
            assert_eq!(f.columns, fitted_columns(f));
            assert_eq!(f.motion.ghost_count(), 0);
            assert!(!f.motion.is_animating());
        });
    }

    /// Zooming in past what a 400 px thumbnail can fill switches the cells to the large tier.
    /// Driven the way a user does it, so the trigger is covered and not just the lookup.
    #[gpui::test]
    fn zooming_in_asks_for_and_draws_the_large_thumbnail_tier(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 2);
        env.library.update(vcx, |m, cx| m.start_thumbnails(cx));
        vcx.run_until_parked();
        let standard = env.library.read_with(vcx, |m, _| m.lib.assets()[0].thumbnail.clone().expect("thumbnailed"));
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();

        // The default zoom draws cells the standard tier covers (test windows are 2x).
        view.update(vcx, |feed, cx| {
            assert_eq!(feed.thumbnail_tier(), thumbnails::THUMB_MAX);
            assert_eq!(feed.thumbnail_for_cell(&standard, cx), standard, "no other tier is wanted yet");
        });

        // Zoom to the largest tiles the way the user does, then let the request settle.
        vcx.dispatch_action(ZoomIn);
        vcx.dispatch_action(ZoomIn);
        vcx.run_until_parked();
        view.update(vcx, |feed, _| assert_eq!(feed.thumbnail_tier(), thumbnails::THUMB_LARGE, "cells outgrew the standard tier"));
        vcx.background_executor.advance_clock(LARGE_TIER_SETTLE * 2);
        vcx.run_until_parked();

        let large = view.update(vcx, |feed, cx| feed.thumbnail_for_cell(&standard, cx));
        assert_ne!(large, standard, "the big cells draw the large tier");
        assert_eq!(thumbnails::sized_thumb_path(&standard, thumbnails::THUMB_LARGE), Some(large.clone()));
        assert!(large.exists(), "{} was rendered", large.display());
    }

    /// Nothing but the wheel used to ask for the tier, so arrowing through a zoomed-in feed left
    /// every newly revealed row on the stretched standard tier.
    #[gpui::test]
    fn scrolling_by_keyboard_asks_for_the_tier_of_the_rows_it_reveals(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 40);
        env.library.update(vcx, |m, cx| m.start_thumbnails(cx));
        vcx.run_until_parked();
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.dispatch_action(ZoomIn);
        vcx.dispatch_action(ZoomIn);
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(LARGE_TIER_SETTLE * 2);
        vcx.run_until_parked();

        // Walk the selection down past the fold: `scroll_to_index` moves the grid, no wheel event.
        for _ in 0..12 {
            vcx.dispatch_action(SelectDown);
        }
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(LARGE_TIER_SETTLE * 2);
        vcx.run_until_parked();

        let drawn: Vec<PathBuf> = view.update(vcx, |feed, cx| {
            feed.last_rendered.keys().filter_map(|id| feed.asset_of(id, cx)).filter_map(|asset| state::library(cx).read(cx).lib.asset(&asset)?.thumbnail.clone()).map(|standard| feed.thumbnail_for_cell(&standard, cx)).collect()
        });
        assert!(!drawn.is_empty(), "rows are on screen");
        assert!(drawn.iter().all(|path| path.to_string_lossy().contains("@800")), "every visible row draws the large tier: {drawn:?}");
    }

    /// Only the viewport (scroll, zoom, resize) used to ask for the tier, so a generation that
    /// finished while the feed sat still at the largest zoom drew its fresh 400 px thumbnail
    /// stretched across a cell twice that size, and stayed soft until the user scrolled.
    #[gpui::test]
    fn a_generation_finishing_in_a_zoomed_in_feed_draws_the_large_tier(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 2);
        env.library.update(vcx, |m, cx| m.start_thumbnails(cx));
        vcx.run_until_parked();
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.dispatch_action(ZoomIn);
        vcx.dispatch_action(ZoomIn);
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(LARGE_TIER_SETTLE * 2);
        vcx.run_until_parked();

        // A new image completes with nothing else moving: no wheel, no zoom, no resize.
        let id = env.library.update(vcx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Image, None, None, None, None);
            m.apply(majik_generation::Event::Completed { id: id.clone(), job: m.attempt(&id), bytes: majik_core::images::solid_png(64, 64, [9, 8, 7]), is_upscaled: false }, cx);
            id
        });
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(LARGE_TIER_SETTLE * 2);
        vcx.run_until_parked();

        let standard = env.library.read_with(vcx, |m, _| m.lib.get(&id).unwrap().thumbnail.clone().expect("the new row was thumbnailed"));
        let drawn = view.update(vcx, |feed, cx| feed.thumbnail_for_cell(&standard, cx));
        assert_eq!(thumbnails::sized_thumb_path(&standard, thumbnails::THUMB_LARGE), Some(drawn.clone()), "the new row draws the large tier, not the stretched standard one");
        assert!(drawn.exists(), "{} was rendered", drawn.display());
    }

    /// Scrolling the whole feed does not grow the thumbnail cache without bound. The cache this
    /// replaced (gpui's `RetainAllImageCache`) kept every thumbnail it ever decoded: 5.8 GB of
    /// resident memory after one pass over a 10 000-generation library.
    #[gpui::test]
    fn scrolling_the_feed_keeps_the_thumbnail_cache_bounded(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 60);
        // The shipping budget holds ~800 full-size thumbnails; these are 64 px test images, so ask
        // for a few of them instead of seeding a library big enough to fill 512 MB. Set before any
        // thumbnail exists, the way the real cache is built with its budget already in place.
        let cache = view.update(vcx, |f, _| f.image_cache());
        let budget = 6 * 64 * 64 * 4;
        cache.update(vcx, |cache, _| cache.set_budget(budget));

        // An inert library never thumbnails, so point every row at its own PNG — the cells then
        // decode a real file each, which is what fills the cache.
        let rows: Vec<(GenerationId, PathBuf)> = view.update(vcx, |f, cx| {
            let library = f.library.read(cx);
            f.ids.iter().filter_map(|id| library.lib.get(id.media()?)).filter_map(|item| Some((item.id.clone(), item.path.clone()?))).collect()
        });
        assert_eq!(rows.len(), 60);
        env.library.update(vcx, |m, cx| {
            for (id, path) in rows {
                m.lib.set_thumbnail(&id, path);
            }
            m.changed(cx);
        });
        vcx.simulate_resize(gpui::size(px(400.), px(300.)));
        vcx.run_until_parked();
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        vcx.run_until_parked();

        let over_grid = point(px(200.), px(150.));
        vcx.simulate_mouse_move(over_grid, None, gpui::Modifiers::default());
        for _ in 0..20 {
            vcx.simulate_event(ScrollWheelEvent {
                position: over_grid,
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-120.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            vcx.run_until_parked();
            // Decodes report back on the next frame; tests have no frame loop of their own.
            vcx.update(|window, cx| window.simulate_next_frame(cx));
            vcx.run_until_parked();
            let (bytes, budget) = cache.read_with(vcx, |cache, _| (cache.bytes(), cache.budget()));
            assert!(bytes <= budget, "{bytes} bytes held against a {budget} budget mid-scroll");
        }

        let (bytes, held) = cache.read_with(vcx, |cache, _| (cache.bytes(), cache.len()));
        assert!(bytes > 0, "the cells did decode their thumbnails");
        assert!(held < 60, "scrolling the whole feed kept only some of the thumbnails, not all 60 ({held})");
    }

    #[gpui::test]
    fn thumbnail_arrival_fades_in(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 2);
        arm_motion(&view, vcx);
        let (id, path) = view.update(vcx, |f, cx| {
            let item = f.library.read(cx).lib.get(f.ids[0].media().unwrap()).cloned().unwrap();
            assert!(item.thumbnail.is_none(), "inert libraries don't thumbnail");
            (item.id, item.path.unwrap())
        });
        env.library.update(vcx, |m, cx| {
            m.lib.set_thumbnail(&id, path);
            m.changed(cx);
        });
        let id = EntryId::Generation(id);
        view.update(vcx, |f, cx| assert_eq!(f.motion.thumbnail_opacity(&id, now(cx)), 0.0));
        vcx.background_executor.advance_clock(crate::grid_motion::THUMBNAIL_FADE / 2);
        view.update(vcx, |f, cx| assert!((f.motion.thumbnail_opacity(&id, now(cx)) - 0.5).abs() < 0.01));
        vcx.background_executor.advance_clock(crate::grid_motion::THUMBNAIL_FADE);
        view.update(vcx, |f, cx| {
            f.motion.tick(now(cx));
            assert_eq!(f.motion.thumbnail_opacity(&id, now(cx)), 1.0);
            assert!(!f.motion.is_animating());
        });
    }

    #[gpui::test]
    fn empty_state_copy_follows_filter(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 0);
        let album = env.library.update(vcx, |m, cx| m.create_album("Trips".into(), cx));
        view.update(vcx, |f, cx| {
            assert_eq!(f.empty_state(cx).1, "Nothing Here Yet");
            f.set_filter(FeedFilter::Favorites, cx);
            assert_eq!(f.empty_state(cx).1, "No Favorites Yet");
            f.set_filter(FeedFilter::Album(album.clone()), cx);
            assert_eq!(f.empty_state(cx).1, "Empty Album");
            f.set_filter(FeedFilter::Album(majik_core::model::AlbumId("gone".into())), cx);
            assert_eq!(f.empty_state(cx).1, "Album Unavailable");
        });
    }

    #[gpui::test]
    fn empty_library_hint_names_the_platform_shortcut(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 0);
        let expected = if cfg!(target_os = "macos") { "Press ⌘N to open the composer" } else { "Press Ctrl+N to open the composer" };
        view.update(vcx, |f, cx| assert_eq!(f.empty_state(cx).2.as_ref(), expected));
    }

    #[gpui::test]
    fn arrow_keys_move_selection_and_clamp(cx: &mut TestAppContext) {
        use super::super::super::actions::{SelectDown, SelectLeft, SelectRight, SelectUp};
        let (view, vcx, _env) = feed_window!(cx, 8);
        vcx.simulate_resize(gpui::size(px(800.), px(600.)));
        vcx.run_until_parked();
        let cols = view.update(vcx, |f, _| f.columns);
        assert_eq!(cols, 4, "160 px tiles across 796 px");
        vcx.dispatch_action(SelectRight);
        view.update(vcx, |f, _| assert_eq!(f.selection.last_index, Some(0), "first arrow selects the first item"));
        vcx.dispatch_action(SelectRight);
        vcx.dispatch_action(SelectDown);
        view.update(vcx, |f, _| {
            assert_eq!(f.selection.last_index, Some(1 + cols));
            assert_eq!(f.selection.single(), Some(&f.ids[1 + cols]));
        });
        vcx.dispatch_action(SelectLeft);
        for _ in 0..5 {
            vcx.dispatch_action(SelectUp);
        }
        view.update(vcx, |f, _| assert_eq!(f.selection.last_index, Some(0)));
    }

    #[gpui::test]
    fn arrow_keys_follow_the_selected_item_when_the_feed_shifts(cx: &mut TestAppContext) {
        use super::super::super::actions::SelectRight;
        let (view, vcx, env) = feed_window!(cx, 8);
        let clicked = view.update(vcx, |f, cx| {
            let id = f.ids[5].clone();
            f.selection.click(&id, 5, Modifiers::default(), &f.ids);
            cx.notify();
            id
        });
        // A new generation appears at the top of the feed, pushing the clicked item one slot down.
        seed_item(&env.library, vcx, Seed::default());
        vcx.run_until_parked();
        view.update(vcx, |f, _| {
            assert_eq!(f.ids[6], clicked);
            assert_eq!(f.selection.last_index, Some(6), "the anchor followed the item");
        });
        vcx.dispatch_action(SelectRight);
        view.update(vcx, |f, _| assert_eq!(f.selection.single(), Some(&f.ids[7]), "→ moves to the right of the clicked item, not of its old slot"));
    }

    #[gpui::test]
    fn a_library_change_forgets_the_cell_boxes_of_the_old_layout(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 4);
        let (first, second) = view.update(vcx, |f, _| (f.ids[0].clone(), f.ids[1].clone()));
        view.update(vcx, |f, _| assert!(f.cell_bounds(&second).is_some(), "drawn last frame"));
        // Deleted from the detail while it covers the feed: the model changes and the feed picks
        // that up, but the grid is not drawn again before the detail asks where to return to.
        env.library.update(vcx, |m, _| m.lib.delete_generations(std::slice::from_ref(first.media().unwrap())).unwrap());
        view.update(vcx, |f, cx| {
            f.refresh(Change::Library, cx);
            assert_eq!(f.ids[0], second, "moved up a slot");
            assert_eq!(f.land_on(&second, cx), None, "the box recorded for slot 1 must not be handed out for slot 0");
        });
    }

    #[gpui::test]
    fn a_missing_file_is_not_exportable(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 1);
        let missing = seed_item(&env.library, vcx, Seed { status: Status::Missing, ..Seed::default() });
        let item = env.library.read_with(vcx, |m, _| m.lib.get(&missing).cloned().unwrap());
        assert!(item.path.is_some() && item.file().is_none(), "a missing row knows where its file should be but has none to read");
        assert!(Exportable::of_item(&item).is_none(), "no save panel over a file that isn't there");
        vcx.run_until_parked();
        view.update(vcx, |f, cx| {
            f.selection.select_all(&f.ids);
            assert_eq!(f.selected_exportables(cx).len(), 1, "only the seeded image has a file");
        });
    }

    // ----- assets feed -----

    use super::context_menu_tests::{assert_menu, cmd_click};

    fn assets_feed(view: &Entity<FeedView>, vcx: &mut VisualTestContext) {
        view.update(vcx, |f, cx| f.set_filter(FeedFilter::Assets, cx));
        vcx.run_until_parked();
    }

    fn select_entries(view: &Entity<FeedView>, vcx: &mut VisualTestContext, ids: &[EntryId]) {
        view.update(vcx, |feed, cx| {
            feed.selection.clear();
            for id in ids {
                let ix = feed.ids.iter().position(|i| i == id).expect("entry is in the feed");
                feed.cell_mouse_down(ix, id, &cmd_click(), cx);
            }
        });
    }

    fn entries_menu(view: &Entity<FeedView>, vcx: &mut VisualTestContext, ids: &[EntryId]) -> Vec<MenuEntry> {
        select_entries(view, vcx, ids);
        view.update(vcx, |feed, cx| {
            let first = feed.ids.iter().position(|i| i == &ids[0]).expect("entry is in the feed");
            feed.selection.right_click(&ids[0], first);
            let info = feed.menu_info(cx);
            selection_menu_entries(&info)
        })
    }

    #[gpui::test]
    fn assets_feed_lists_imports_and_outputs_newest_first(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 2);
        let import = crate::test_support::seed_asset(&env.library, vcx, MediaType::Image, 9);
        assets_feed(&view, vcx);
        view.update(vcx, |f, cx| {
            assert_eq!(f.title(cx), "Assets");
            assert_eq!(f.ids.len(), 3, "two outputs and the import");
            assert_eq!(f.ids[0], EntryId::Asset(import.clone()), "newest first");
            assert!(f.ids.iter().all(|id| id.asset().is_some()), "nothing but assets");
            assert!(f.selected_items(cx).is_empty());
        });
        let sound = crate::test_support::seed_asset(&env.library, vcx, MediaType::Audio, 1);
        view.update(vcx, |f, cx| {
            assert_eq!(f.ids[0], EntryId::Asset(sound));
            f.media_filter = MediaFilter::Audio;
            f.refresh(Change::Filter, cx);
            assert_eq!(f.ids.len(), 1, "the type filter applies to assets too");
        });
    }

    #[gpui::test]
    fn assets_feed_is_empty_with_its_own_hint(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 0);
        assets_feed(&view, vcx);
        view.update(vcx, |f, cx| {
            assert!(f.ids.is_empty());
            assert_eq!(f.empty_state(cx).1, "No Assets Yet");
        });
    }

    #[gpui::test]
    fn deleting_a_generation_keeps_its_output_in_assets(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 1);
        let (id, path) = first_item_path(&view, vcx, &env);
        env.library.update(vcx, |m, cx| m.delete(std::slice::from_ref(&id), cx));
        view.update(vcx, |f, _| assert!(f.ids.is_empty()));
        assets_feed(&view, vcx);
        view.update(vcx, |f, cx| {
            assert_eq!(f.ids.len(), 1);
            let asset = f.selected_assets(cx);
            assert!(asset.is_empty());
            let entry = f.library.read(cx).lib.entry(&f.ids[0]).and_then(|e| e.file().map(std::path::Path::to_path_buf));
            assert_eq!(entry.as_deref(), Some(path.as_path()), "the same file, now a plain asset");
        });
    }

    #[gpui::test]
    fn asset_menu_offers_exports_and_delete_only(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 0);
        let import = crate::test_support::seed_asset(&env.library, vcx, MediaType::Image, 3);
        assets_feed(&view, vcx);
        let entries = entries_menu(&view, vcx, &[EntryId::Asset(import)]);
        assert_menu(&entries, &["Copy", "Save…", "Delete"], &["Recreate", "Favorite", "Add to Album…", "Open"]);
        let sound = crate::test_support::seed_asset(&env.library, vcx, MediaType::Audio, 1);
        vcx.run_until_parked();
        let entries = entries_menu(&view, vcx, &[EntryId::Asset(sound)]);
        assert_menu(&entries, &["Copy", "Delete"], &["Use Image"]);
    }

    #[gpui::test]
    fn a_referenced_asset_has_no_delete_and_a_mixed_selection_only_exports(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 1);
        let (item, _) = first_item_path(&view, vcx, &env);
        let output = env.library.read_with(vcx, |m, _| m.lib.get(&item).unwrap().output_asset_id.clone().unwrap());
        let import = crate::test_support::seed_asset(&env.library, vcx, MediaType::Image, 5);
        assets_feed(&view, vcx);
        let entries = entries_menu(&view, vcx, &[EntryId::Asset(output.clone())]);
        assert_menu(&entries, &["Copy"], &["Delete"]);
        let entries = entries_menu(&view, vcx, &[EntryId::Asset(output.clone()), EntryId::Asset(import)]);
        assert_menu(&entries, &["Copy"], &["Delete", "Delete Selected"]);
        // Back on the Library feed a generation and an asset can't be selected together, but the
        // menu still has an answer for it.
        view.update(vcx, |f, cx| {
            f.selection.clear();
            f.selection.ids.insert(EntryId::Generation(item.clone()));
            f.selection.ids.insert(EntryId::Asset(output));
            let info = f.menu_info(cx);
            assert_menu(&selection_menu_entries(&info), &["Copy", "Save…"], &["Delete", "Recreate"]);
        });
    }

    #[gpui::test]
    fn delete_asset_confirms_then_trashes_it(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 0);
        let import = crate::test_support::seed_asset(&env.library, vcx, MediaType::Image, 7);
        let path = env.library.read_with(vcx, |m, _| m.lib.asset(&import).unwrap().path.clone());
        assets_feed(&view, vcx);
        select_entries(&view, vcx, &[EntryId::Asset(import.clone())]);
        vcx.dispatch_action(DeleteMedia);
        let (message, detail) = vcx.pending_prompt().expect("delete asks first");
        assert_eq!(message, "Delete this asset?");
        assert!(detail.contains(".majik/trash"), "{detail}");
        vcx.simulate_prompt_answer("Delete");
        vcx.run_until_parked();
        assert!(!path.exists(), "gone from the library folder");
        assert_eq!(trashed_names(&env).len(), 1, "moved to .majik/trash, not erased");
        env.library.read_with(vcx, |m, _| assert!(m.lib.asset(&import).is_none()));
        view.read_with(vcx, |f, _| assert!(f.ids.is_empty()));
    }

    #[gpui::test]
    fn deleting_a_referenced_asset_toasts_instead_of_asking(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 1);
        let output = env.library.read_with(vcx, |m, _| m.lib.generations()[0].output_asset_id.clone().unwrap());
        assets_feed(&view, vcx);
        select_entries(&view, vcx, &[EntryId::Asset(output.clone())]);
        let toasts = vcx.update(|_, cx| crate::ui::toast_generation(cx));
        vcx.dispatch_action(DeleteMedia);
        assert!(vcx.pending_prompt().is_none(), "nothing to confirm");
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), toasts + 1);
        env.library.read_with(vcx, |m, _| assert!(m.lib.asset(&output).is_some()));
    }

    #[gpui::test]
    fn import_paths_adds_assets_and_reports_the_rest(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 0);
        assets_feed(&view, vcx);
        let png = env.dir.path().join("dropped.png");
        std::fs::write(&png, majik_core::images::solid_png(3, 3, [8, 8, 8])).unwrap();
        let text = env.dir.path().join("dropped.txt");
        std::fs::write(&text, b"nope").unwrap();
        let toasts = vcx.update(|_, cx| crate::ui::toast_generation(cx));
        view.update_in(vcx, |f, window, cx| f.import_paths(vec![png.clone()], window, cx));
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), toasts + 1, "told how many were imported");
        view.update(vcx, |f, _| assert_eq!(f.ids.len(), 1));
        view.update_in(vcx, |f, window, cx| f.import_paths(vec![png, text], window, cx));
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), toasts + 2, "told about the file that isn't media");
        view.update(vcx, |f, _| assert_eq!(f.ids.len(), 1, "the same image again is the same asset"));
        env.library.read_with(vcx, |m, _| assert!(m.lib.assets()[0].path.starts_with(m.lib.assets_dir())));
    }

    #[gpui::test]
    fn land_on_selects_and_scrolls_only_when_off_screen(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 8);
        view.update(vcx, |f, cx| {
            let last = f.ids[7].clone();
            f.cell_bounds.borrow_mut().clear();
            assert_eq!(f.land_on(&last, cx), None, "not drawn → not on screen");
            assert_eq!(f.selection.single(), Some(&last));
            assert!(f.scroll.offset().y < px(0.), "scrolled towards the last row");
            let scrolled = f.scroll.offset().y;
            assert_eq!(f.land_on(&EntryId::Generation(GenerationId("missing".into())), cx), None);
            assert_eq!(f.selection.single(), Some(&last), "unknown ids are ignored");

            let first = f.ids[0].clone();
            let cell = Bounds { origin: point(px(240.), px(50.)), size: gpui::size(px(120.), px(120.)) };
            f.cell_bounds.borrow_mut().insert(first.clone(), cell);
            assert_eq!(f.land_on(&first, cx), Some(cell), "on screen → hand the box back");
            assert_eq!(f.selection.single(), Some(&first));
            assert_eq!(f.scroll.offset().y, scrolled, "and don't move the grid under it");
        });
    }

    #[gpui::test]
    fn open_carries_the_cell_origin_when_drawn(cx: &mut TestAppContext) {
        let (view, vcx, _env) = feed_window!(cx, 3);
        let origins: Rc<RefCell<Vec<Option<Bounds<Pixels>>>>> = Rc::new(RefCell::new(Vec::new()));
        let o = origins.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&view, move |_, ev: &FeedEvent, _| {
                if let FeedEvent::Open { origin, .. } = ev {
                    o.borrow_mut().push(*origin);
                }
            })
            .detach();
        });
        let drawn = view.update(vcx, |f, cx| {
            // The headless window lays out and prepaints, so the cell canvas has recorded a box.
            let drawn = f.cell_bounds(&f.ids[1]).expect("cell drawn last frame");
            assert!(drawn.size.width > px(0.));
            f.open_at(1, cx);
            // Scrolled off screen since: nothing to grow out of.
            f.cell_bounds.borrow_mut().clear();
            f.open_at(1, cx);
            drawn
        });
        vcx.run_until_parked();
        assert_eq!(*origins.borrow(), vec![Some(drawn), None]);
    }

    #[gpui::test]
    fn favorite_and_delete_actions_mutate_library(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed_window!(cx, 3);
        let first = view.update(vcx, |f, _| f.ids[0].clone());
        // Favorite the first item.
        view.update(vcx, |f, _cx| {
            f.selection.click(&first, 0, Modifiers::default(), &f.ids.clone());
        });
        vcx.dispatch_action(super::super::super::actions::ToggleFavorite);
        vcx.run_until_parked();
        let fav = env.library.read_with(vcx, |m, _| m.lib.get(first.media().unwrap()).unwrap().is_favorite);
        assert!(fav, "item favorited via action");
        // Favorites feed now has exactly one item.
        let favs = env.library.read_with(vcx, |m, _| m.lib.feed(&FeedFilter::Favorites, MediaFilter::All).len());
        assert_eq!(favs, 1);
    }

    /// An upscaled clip carries both badges, and they are two pills in one strip — before this,
    /// HD and the length were both pinned to the bottom-right corner and drew on top of each other.
    #[test]
    fn an_upscaled_clip_shows_its_length_and_hd_side_by_side() {
        assert_eq!(right_badges(Some(5.0), MediaType::Video, true), vec!["0:05", "HD"], "length first, HD outermost");
        assert_eq!(right_badges(Some(5.0), MediaType::Video, false), vec!["0:05"]);
        assert_eq!(right_badges(None, MediaType::Image, true), vec!["HD"]);
        assert_eq!(right_badges(None, MediaType::Image, false), Vec::<SharedString>::new(), "a plain image gets no strip at all");
        assert_eq!(right_badges(Some(5.0), MediaType::Image, false), Vec::<SharedString>::new(), "a still has no length to state");
        assert_eq!(right_badges(Some(75.0), MediaType::Audio, false), vec!["1:15"]);
    }

    /// The toast names what actually reached the clipboard: the files when the platform carried
    /// them, otherwise the single flavour gpui could hold.
    #[test]
    fn copy_toast_names_what_landed() {
        assert_eq!(copy_toast(1, true, true), "Copied", "the file itself, so no flavour to name");
        assert_eq!(copy_toast(3, false, true), "Copied 3 items");
        assert_eq!(copy_toast(1, true, false), "Copied image", "no file references: the bitmap is what landed");
        assert_eq!(copy_toast(3, true, false), "Copied image", "gpui holds one image, not three");
        assert_eq!(copy_toast(1, false, false), "Copied file path");
        assert_eq!(copy_toast(2, false, false), "Copied 2 file paths", "plural, and honest that they are only paths");
    }

    /// Copying one image puts the bitmap on the clipboard, so a paste into an image editor works on
    /// every platform. (macOS writes the file and its bytes natively first, but that needs the main
    /// thread, so a headless test exercises the portable path every platform shares.)
    #[gpui::test]
    fn copy_puts_the_image_on_the_clipboard(cx: &mut TestAppContext) {
        let (view, vcx, _e) = feed_window!(cx, 2);
        view.update(vcx, |f, _| {
            let id = f.ids[0].clone();
            f.selection.click(&id, 0, Modifiers::default(), &f.ids);
        });
        vcx.dispatch_action(CopyMedia);
        vcx.run_until_parked();
        let item = vcx.read_from_clipboard().expect("something was copied");
        assert!(
            item.entries().iter().any(|entry| matches!(entry, gpui::ClipboardEntry::Image(_))),
            "an image entry, not a path"
        );
    }

    fn select_first(view: &Entity<FeedView>, vcx: &mut VisualTestContext, count: usize) {
        view.update(vcx, |f, _| {
            f.selection.click(&f.ids[0], 0, Modifiers::default(), &f.ids);
            for ix in 1..count {
                f.selection.click(&f.ids[ix], ix, Modifiers { shift: true, ..Modifiers::default() }, &f.ids);
            }
        });
    }

    /// The save panel opens where the last save went, not in the home folder every time.
    #[gpui::test]
    fn save_panel_reopens_in_the_last_saved_folder(cx: &mut TestAppContext) {
        let (view, vcx, _e) = feed_window!(cx, 2);
        let dir = tempfile::tempdir().unwrap();
        let home = directories::BaseDirs::new().unwrap().home_dir().to_path_buf();
        select_first(&view, vcx, 1);
        vcx.dispatch_action(SaveMedia);
        let dest = dir.path().join("first.png");
        vcx.simulate_new_path_selection(move |directory| {
            assert_eq!(directory, home, "nothing remembered yet");
            Some(dest)
        });
        vcx.run_until_parked();
        assert!(dir.path().join("first.png").exists());
        vcx.read(|cx| assert_eq!(cx.global::<Config>().save_directory.as_deref(), Some(dir.path())));

        vcx.dispatch_action(SaveMedia);
        let expected = dir.path().to_path_buf();
        vcx.simulate_new_path_selection(move |directory| {
            assert_eq!(directory, expected, "the panel opens where the last save went");
            None
        });
        vcx.run_until_parked();
    }

    /// A dismissed panel or a failed copy leaves the remembered folder alone.
    #[gpui::test]
    fn cancelled_and_failed_saves_do_not_move_the_save_folder(cx: &mut TestAppContext) {
        let (view, vcx, _e) = feed_window!(cx, 2);
        let dir = tempfile::tempdir().unwrap();
        vcx.update(|_, cx| update_config(cx, |c| c.save_directory = Some(dir.path().to_path_buf())));
        select_first(&view, vcx, 1);
        vcx.dispatch_action(SaveMedia);
        vcx.simulate_new_path_selection(|_| None);
        vcx.run_until_parked();
        vcx.dispatch_action(SaveMedia);
        let elsewhere = tempfile::tempdir().unwrap();
        let dest = elsewhere.path().join("missing").join("out.png");
        vcx.simulate_new_path_selection(move |_| Some(dest));
        vcx.run_until_parked();
        vcx.read(|cx| assert_eq!(cx.global::<Config>().save_directory.as_deref(), Some(dir.path())));
    }

    /// A remembered folder that no longer exists is not offered; the panel goes back to home.
    #[gpui::test]
    fn save_panel_falls_back_to_home_when_the_remembered_folder_is_gone(cx: &mut TestAppContext) {
        let (view, vcx, _e) = feed_window!(cx, 1);
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().to_path_buf();
        drop(dir);
        vcx.update(|_, cx| update_config(cx, |c| c.save_directory = Some(gone)));
        let home = directories::BaseDirs::new().unwrap().home_dir().to_path_buf();
        select_first(&view, vcx, 1);
        vcx.dispatch_action(SaveMedia);
        vcx.simulate_new_path_selection(move |directory| {
            assert_eq!(directory, home);
            None
        });
        vcx.run_until_parked();
    }

    /// Saving several items asks for a folder; that folder is remembered for the next single save.
    #[gpui::test]
    fn saving_several_items_remembers_the_chosen_folder(cx: &mut TestAppContext) {
        let (view, vcx, _e) = feed_window!(cx, 3);
        let dir = tempfile::tempdir().unwrap();
        select_first(&view, vcx, 2);
        vcx.dispatch_action(SaveMedia);
        let chosen = dir.path().to_path_buf();
        vcx.simulate_path_prompt_response(move |options| {
            assert!(options.directories && !options.files);
            Some(vec![chosen])
        });
        vcx.run_until_parked();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "both files copied");
        vcx.read(|cx| assert_eq!(cx.global::<Config>().save_directory.as_deref(), Some(dir.path())));

        select_first(&view, vcx, 1);
        vcx.dispatch_action(SaveMedia);
        let expected = dir.path().to_path_buf();
        vcx.simulate_new_path_selection(move |directory| {
            assert_eq!(directory, expected);
            None
        });
        vcx.run_until_parked();
    }

    /// A video carries no bitmap gpui can hold, so the portable path leaves its path as text.
    #[gpui::test]
    fn copying_a_non_image_falls_back_to_the_path(cx: &mut TestAppContext) {
        let (_view, vcx, _e) = feed_window!(cx, 1);
        let clip = tempfile::tempdir().unwrap();
        let path = clip.path().join("clip.mp4");
        std::fs::write(&path, b"not really an mp4").unwrap();
        let item = Exportable { path: path.clone(), kind: MediaType::Video, name: "clip.mp4".into() };
        vcx.update(|window, cx| copy_items(&[item], window, cx));
        let text = vcx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(text.as_deref(), Some(path.to_string_lossy().as_ref()), "the path, as text");
    }

}

/// One test per selection shape the context menu distinguishes. Each checks that the rows a user
/// can act on are exactly the relevant ones: `present` must be there and enabled, `absent` must be
/// missing or disabled.
#[cfg(test)]
mod context_menu_tests {
    use super::*;
    use crate::test_support::{env, seed_item, Seed, TestEnv};
    use gpui::{Modifiers as GModifiers, MouseButton, MouseDownEvent, Point, TestAppContext, VisualTestContext};

    fn feed(cx: &mut TestAppContext) -> (Entity<FeedView>, &mut VisualTestContext, TestEnv) {
        let env = env(cx, 0, "Mock");
        let (view, vcx) = cx.add_window_view(FeedView::new);
        vcx.run_until_parked();
        (view, vcx, env)
    }

    pub(super) fn cmd_click() -> MouseDownEvent {
        MouseDownEvent {
            button: MouseButton::Left,
            position: Point::default(),
            modifiers: GModifiers { platform: true, ..GModifiers::default() },
            click_count: 1,
            first_mouse: false,
        }
    }

    /// Cmd-click each id into the selection, right-click the first, and build the menu entries.
    fn menu_for(view: &Entity<FeedView>, vcx: &mut VisualTestContext, ids: &[GenerationId]) -> Vec<MenuEntry> {
        vcx.run_until_parked();
        view.update(vcx, |feed, cx| {
            feed.selection.clear();
            for id in ids {
                let entry = EntryId::Generation(id.clone());
                let ix = feed.ids.iter().position(|i| i == &entry).expect("seeded item is in the feed");
                feed.cell_mouse_down(ix, &entry, &cmd_click(), cx);
            }
            let anchor = EntryId::Generation(ids[0].clone());
            let first = feed.ids.iter().position(|i| i == &anchor).expect("seeded item is in the feed");
            feed.selection.right_click(&anchor, first);
            let info = feed.menu_info(cx);
            assert_eq!(info.items.len(), ids.len(), "menu is built for the whole selection");
            selection_menu_entries(&info)
        })
    }

    /// Labels the user can choose, submenu rows as `Parent/Child`.
    fn actionable(entries: &[MenuEntry]) -> Vec<String> {
        let mut out = Vec::new();
        for entry in entries {
            match &entry.kind {
                MenuEntryKind::Separator => {}
                _ if entry.enabled => out.push(entry.label.to_string()),
                _ => {}
            }
        }
        out
    }

    #[track_caller]
    pub(super) fn assert_menu(entries: &[MenuEntry], present: &[&str], absent: &[&str]) {
        let actionable = actionable(entries);
        for label in present {
            assert!(actionable.iter().any(|l| l == label), "expected {label:?} to be actionable; menu = {actionable:?}");
        }
        for label in absent {
            assert!(!actionable.iter().any(|l| l == label), "expected {label:?} to be absent or disabled; menu = {actionable:?}");
        }
    }

    const IMAGE_FULL: &[&str] = &[
        "Open",
        "Copy",
        "Save…",
        "Recreate",
        "Add to Album…",
        "Favorite",
        "Delete",
    ];

    // ----- single, completed ------------------------------------------------

    #[gpui::test]
    fn single_image(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed::default());
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, IMAGE_FULL, &["Remove from Album", "Unfavorite", "Retry", "Cancel Generation", "Delete Selected", "Retry Selected"]);
    }

    #[gpui::test]
    fn single_upscaled_image(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { upscaled: true, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        // An upscale stores its request like any generation, so it recreates (onto the Upscale tab).
        assert_menu(&entries, &["Recreate", "Delete"], &["Retry"]);
    }

    #[gpui::test]
    fn single_imported_image(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { recreatable: false, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Open", "Copy", "Delete"], &["Recreate"]);
    }

    #[gpui::test]
    fn single_video(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { media_type: MediaType::Video, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Open", "Copy", "Save…", "Recreate", "Add to Album…", "Favorite", "Delete"], &["Retry", "Cancel Generation"]);
    }

    #[gpui::test]
    fn single_audio(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { media_type: MediaType::Audio, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Open", "Copy", "Save…", "Recreate", "Add to Album…", "Favorite", "Delete"], &["Retry", "Cancel Generation"]);
    }

    #[gpui::test]
    fn single_favorite_image(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { favorite: true, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Unfavorite"], &["Favorite"]);
    }

    #[gpui::test]
    fn single_image_inside_an_album(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed::default());
        let album = env.library.update(vcx, |m, cx| {
            let album = m.create_album("Trip".into(), cx);
            m.add_to_album(&album, std::slice::from_ref(&id), cx);
            album
        });
        view.update(vcx, |feed, cx| feed.set_filter(FeedFilter::Album(album), cx));
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Add to Album…", "Remove from Album"], &[]);
    }

    #[gpui::test]
    fn single_image_outside_an_album_cannot_be_removed_from_one(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed::default());
        env.library.update(vcx, |m, cx| {
            let album = m.create_album("Trip".into(), cx);
            m.add_to_album(&album, std::slice::from_ref(&id), cx);
        });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Add to Album…"], &["Remove from Album"]);
    }

    // ----- single, not completed --------------------------------------------

    #[gpui::test]
    fn single_failed(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { status: Status::Failed, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(
            &entries,
            &["Retry", "Recreate", "Delete"],
            &["Retry Selected", "Delete Selected", "Open", "Copy", "Cancel Generation", "Favorite", "Add to Album…"],
        );
    }

    #[gpui::test]
    fn single_missing(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { status: Status::Missing, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(
            &entries,
            &["Retry", "Recreate", "Delete"],
            &["Retry Selected", "Delete Selected", "Open", "Copy", "Save…", "Cancel Generation", "Favorite", "Add to Album…"],
        );
    }

    #[gpui::test]
    fn single_missing_without_a_request_cannot_retry(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { status: Status::Missing, recreatable: false, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Delete"], &["Retry", "Recreate"]);
    }

    #[gpui::test]
    fn missing_selected(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let a = seed_item(&env.library, vcx, Seed { status: Status::Missing, ..Seed::default() });
        let b = seed_item(&env.library, vcx, Seed { status: Status::Missing, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(&entries, &["Retry Selected", "Delete Selected"], &["Retry", "Recreate", "Delete", "Open", "Favorite"]);
    }

    #[gpui::test]
    fn completed_and_missing(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let done = seed_item(&env.library, vcx, Seed::default());
        let missing = seed_item(&env.library, vcx, Seed { status: Status::Missing, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[done, missing]);
        assert_menu(&entries, &["Delete Selected"], &["Retry Selected", "Copy", "Open", "Favorite", "Add to Album…"]);
    }

    #[gpui::test]
    fn single_generating(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { status: Status::Generating, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(
            &entries,
            &["Cancel Generation", "Recreate", "Delete"],
            &["Cancel Generations", "Delete Selected", "Retry", "Open", "Copy", "Favorite", "Add to Album…"],
        );
        assert!(entries.iter().find(|e| e.label == "Recreate").unwrap().enabled, "a row the app queued stores its request");
    }

    #[gpui::test]
    fn single_generating_without_a_request_has_recreate_disabled(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let id = seed_item(&env.library, vcx, Seed { status: Status::Generating, recreatable: false, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[id]);
        assert_menu(&entries, &["Cancel Generation", "Delete"], &["Recreate", "Retry"]);
        assert!(entries.iter().any(|e| e.label == "Recreate" && !e.enabled), "the row is shown but greyed: nothing stored to replay");
    }

    // ----- multiple, all completed ------------------------------------------

    #[gpui::test]
    fn two_images(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let a = seed_item(&env.library, vcx, Seed::default());
        let b = seed_item(&env.library, vcx, Seed::default());
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(
            &entries,
            &["Copy", "Save…", "Add to Album…", "Favorite", "Delete Selected"],
            &["Open", "Recreate", "Delete"],
        );
    }

    #[gpui::test]
    fn two_videos(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let video = Seed { media_type: MediaType::Video, ..Seed::default() };
        let a = seed_item(&env.library, vcx, video);
        let b = seed_item(&env.library, vcx, video);
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(
            &entries,
            &["Copy", "Save…", "Add to Album…", "Favorite", "Delete Selected"],
            &["Open", "Recreate", "Delete"],
        );
    }

    #[gpui::test]
    fn image_and_video(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let image = seed_item(&env.library, vcx, Seed::default());
        let video = seed_item(&env.library, vcx, Seed { media_type: MediaType::Video, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[image, video]);
        // A mixed selection still exports, files and deletes together.
        assert_menu(&entries, &["Copy", "Save…", "Add to Album…", "Favorite", "Delete Selected"], &["Open", "Recreate", "Delete"]);
    }

    #[gpui::test]
    fn all_selected_favorited(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let favorite = Seed { favorite: true, ..Seed::default() };
        let a = seed_item(&env.library, vcx, favorite);
        let b = seed_item(&env.library, vcx, favorite);
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(&entries, &["Unfavorite"], &["Favorite"]);
    }

    #[gpui::test]
    fn some_selected_favorited(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let a = seed_item(&env.library, vcx, Seed { favorite: true, ..Seed::default() });
        let b = seed_item(&env.library, vcx, Seed::default());
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(&entries, &["Favorite"], &["Unfavorite"]);
    }

    #[gpui::test]
    fn two_images_inside_an_album(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let a = seed_item(&env.library, vcx, Seed::default());
        let b = seed_item(&env.library, vcx, Seed::default());
        let album = env.library.update(vcx, |m, cx| {
            let album = m.create_album("Trip".into(), cx);
            m.add_to_album(&album, &[a.clone(), b.clone()], cx);
            album
        });
        view.update(vcx, |feed, cx| feed.set_filter(FeedFilter::Album(album), cx));
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(&entries, &["Add to Album…", "Remove from Album", "Delete Selected"], &["Delete"]);
    }

    // ----- multiple, not all completed --------------------------------------

    #[gpui::test]
    fn two_failed(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let failed = Seed { status: Status::Failed, ..Seed::default() };
        let a = seed_item(&env.library, vcx, failed);
        let b = seed_item(&env.library, vcx, failed);
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(&entries, &["Retry Selected", "Delete Selected"], &["Retry", "Recreate", "Delete", "Open", "Cancel Generations", "Favorite"]);
    }

    #[gpui::test]
    fn two_generating(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let generating = Seed { status: Status::Generating, ..Seed::default() };
        let a = seed_item(&env.library, vcx, generating);
        let b = seed_item(&env.library, vcx, generating);
        let entries = menu_for(&view, vcx, &[a, b]);
        assert_menu(&entries, &["Cancel Generations", "Delete Selected"], &["Cancel Generation", "Delete", "Recreate", "Retry Selected", "Open", "Favorite"]);
    }

    #[gpui::test]
    fn completed_and_failed(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let done = seed_item(&env.library, vcx, Seed::default());
        let failed = seed_item(&env.library, vcx, Seed { status: Status::Failed, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[done, failed]);
        assert_menu(
            &entries,
            &["Delete Selected"],
            &["Delete", "Retry", "Retry Selected", "Copy", "Open", "Cancel Generation", "Favorite", "Add to Album…"],
        );
    }

    #[gpui::test]
    fn completed_and_generating(cx: &mut TestAppContext) {
        let (view, vcx, env) = feed(cx);
        let done = seed_item(&env.library, vcx, Seed::default());
        let generating = seed_item(&env.library, vcx, Seed { status: Status::Generating, ..Seed::default() });
        let entries = menu_for(&view, vcx, &[done, generating]);
        assert_menu(
            &entries,
            &["Cancel Generation", "Delete Selected"],
            &["Cancel Generations", "Delete", "Recreate", "Retry", "Copy", "Open", "Favorite", "Add to Album…"],
        );
    }
}
