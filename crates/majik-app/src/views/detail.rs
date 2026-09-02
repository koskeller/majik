//! Detail view: carousel with fit/zoom/pan, flarly-style controls floating over a black stage, and
//! an info panel. It fills the Library window and, like Photos, grows out of the feed cell it was
//! opened from and shrinks back into it on close (`morph.rs`).

use gpui::{
    prelude::*, point, px, size, App, Bounds, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, NavigationDirection, ObjectFit, Pixels, Point, PromptLevel, ScrollWheelEvent, SharedString, Task, Window,
};
use gpui_component::button::{ButtonRounded, ButtonVariants as _};
use gpui_component::clipboard::Clipboard;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _};
use std::sync::Arc;
use std::time::Duration;
use majik_core::model::{Asset, AssetId, EntryId, GenerationId, Generation, MediaType, Status, ToolId};
use majik_core::feed;

use crate::actions::*;
use crate::image_cache::{LruImageCache, DETAIL_IMAGE_BUDGET};
use crate::morph::{Direction, Morph};
use crate::paging::{self, Edges, Paging, Step};
use crate::state::{self, LibraryModel, PendingCompose};
use crate::ui::{BoundsSlot, bounds_slot, button, checkerboard, fade_to, format_bytes, format_date, format_duration, icon, measure, now, slot_size, spin};
use crate::views::feed::{copy_items, save_item, Exportable, SaveOutcome};

pub enum DetailEvent {
    /// `Back` was requested on `id`. The owner selects it in the feed and answers with
    /// [`DetailView::close_towards`]; `Close` follows once the morph has played.
    WillClose { id: EntryId },
    /// Drop the view.
    Close,
    /// Hand the current item to the composer panel (Recreate / Use Image); the owner drops the
    /// detail and shows the panel.
    Compose(PendingCompose),
}

/// The save button's state: icon → spinner → ✓ / ✗ → back to the icon after 2 s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveState {
    Idle,
    Saving,
    Saved,
    Failed,
}

const SAVE_STATE_RESET: Duration = Duration::from_secs(2);

/// A control that floats over the stage (flarly's lightbox corners): a translucent pill that
/// swallows the press so the stage doesn't take it for a divider drag or a pan.
fn floating(bg: Hsla, border: Hsla) -> gpui::Div {
    h_flex()
        .p_0p5()
        .gap_0p5()
        .rounded_full()
        .bg(bg.opacity(0.85))
        .border_1()
        .border_color(border)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// A small control in a floating pill, with a circular hover concentric with the pill's ends.
fn floating_button(id: &'static str, icon_name: &'static str) -> gpui_component::button::Button {
    button(id).icon(icon(icon_name)).ghost().small().rounded(ButtonRounded::Size(px(12.)))
}

/// Grab cursor when the image can be panned, closed hand while it is being dragged.
fn stage_cursor(zoomed: bool, dragging: bool) -> Option<CursorStyle> {
    match (zoomed, dragging) {
        (_, true) => Some(CursorStyle::ClosedHand),
        (true, false) => Some(CursorStyle::OpenHand),
        (false, false) => None,
    }
}

/// What the detail shows at an index. An asset is presented as a generation-shaped item (the
/// stage, the chips and the export actions read those fields), with `generation` / `asset` saying
/// what is really there: an output opened from the Assets feed is shown as the generation that made
/// it; a plain input or import has no generation, so no favourite, prompt, model, recreate or
/// retry.
struct Subject {
    item: Generation,
    generation: Option<GenerationId>,
    asset: Option<AssetId>,
}

/// A plain asset as the stage, chips and export actions expect it.
fn item_for_asset(asset: &Asset) -> Generation {
    Generation {
        id: GenerationId(asset.id.0.clone()),
        path: Some(asset.path.clone()),
        media_type: asset.kind,
        status: if asset.missing { Status::Missing } else { Status::Completed },
        created_at_ms: asset.created_at_ms,
        width: asset.width,
        height: asset.height,
        duration_secs: asset.duration_secs,
        file_size: asset.file_size,
        is_favorite: false,
        is_upscaled: false,
        thumbnail: asset.thumbnail.clone(),
        output_asset_id: Some(asset.id.clone()),
        request_json: None,
        model_name: None,
        provider: None,
        error: None,
        error_kind: None,
        tool: None,
        job_id: None,
        poll_url: None,
        queued_at_ms: asset.created_at_ms,
        started_at_ms: None,
        active_job_id: None,
    }
}

pub struct DetailView {
    ids: Vec<EntryId>,
    index: usize,
    save_state: SaveState,
    /// The save in flight and the 2 s reset that follows it (cancel-on-drop).
    save_task: Option<Task<()>>,
    /// 1 Hz re-render while the current item is generating, for the elapsed label.
    elapsed_ticker: Option<Task<()>>,
    /// `None` = fit to view. `Some(z)` = absolute scale where 1.0 is one image pixel per device pixel.
    zoom: Option<f32>,
    pan: Point<Pixels>,
    drag_start: Option<(Point<Pixels>, Point<Pixels>)>,
    show_info: bool,
    /// The current item's input assets for the info panel, loaded once per item so `render_info`
    /// never touches the database.
    info_assets: Option<(GenerationId, Vec<InfoAsset>)>,
    /// For a plain asset: the generations it was an input of (their thumbnails or files), loaded
    /// with `info_assets`.
    info_uses: Vec<std::path::PathBuf>,
    /// The current item's stored request and prompt, parsed once per item so neither `render`
    /// nor the info panel deserialises JSON per frame (playback repaints continuously).
    request: Option<(GenerationId, Option<majik_generation::Request>, Option<String>)>,
    /// For a tool row whose input image is still in the library: that image, shown behind the
    /// result with a divider between them (flarly's split compare). Resolved once per item.
    compare: Option<(GenerationId, Option<std::path::PathBuf>)>,
    /// Where the before/after divider sits, as a fraction of the image width; kept across items.
    divider: f32,
    /// The divider is being dragged (a body drag at fit; when zoomed a drag pans instead).
    divider_drag: bool,
    hover_left: bool,
    hover_right: bool,
    area: BoundsSlot,
    scrub: BoundsSlot,
    focus: FocusHandle,
    library: Entity<LibraryModel>,
    player: Option<majik_video::Player>,
    player_for: Option<GenerationId>,
    audio: Option<majik_audio::Player>,
    audio_for: Option<GenerationId>,
    player_error: Option<String>,
    /// Opening the current video's `Source` off the UI thread (cancel-on-drop).
    player_opening: Option<Task<()>>,
    /// The decode loop: runs while playing, or until the picture for the current position is up
    /// (cancel-on-drop).
    pump: Option<Task<()>>,
    /// The decoded picture on screen, uploaded as a GPUI image.
    frame_image: Option<Arc<gpui::RenderImage>>,
    paging: Paging,
    /// Releases a trackpad gesture that went quiet without an `Ended` (cancel-on-drop).
    gesture_timeout: Option<Task<()>>,
    /// The open or close transition in flight.
    morph: Option<Morph>,
    /// The open morph is pending: it starts on the first frame with a measured stage.
    opening: bool,
    /// The feed cell the view was opened from, in window coordinates.
    origin: Option<Bounds<Pixels>>,
    /// The feed's cache, where the cell thumbnails are already decoded: the travelling box reads
    /// them from there so it can draw on its first frame. Everything else, including the full
    /// image pre-decoded during the open morph, goes through the window's default cache, which is
    /// what the stage reads once the morph finishes.
    thumbnails: Entity<LruImageCache>,
    /// The full-size images the stage, the compare view and the info panel draw. It has its own
    /// cache and its own budget: one 4K image is worth a hundred thumbnails, so sharing the feed's
    /// would flush the grid every time an item is opened. It is dropped with the view, which is
    /// what returns the memory on close. Before this, full-size images went to the window's default
    /// cache, which never evicts anything.
    images: Entity<LruImageCache>,
    /// Emits `Close` when the close morph has played (cancel-on-drop).
    close_task: Option<Task<()>>,
}

impl EventEmitter<DetailEvent> for DetailView {}

/// An input asset as the info panel shows it: its role and the picture that stands for it — the
/// image itself, a video's thumbnail (an `img` of an mp4 draws nothing), or none for audio and for
/// a video whose thumbnail hasn't been rendered yet, which get an icon card instead.
#[derive(Clone, Debug, PartialEq)]
struct InfoAsset {
    role: String,
    kind: MediaType,
    path: std::path::PathBuf,
    picture: Option<std::path::PathBuf>,
}

fn item_pixel_size(item: &Generation) -> Option<(f32, f32)> {
    match (item.width, item.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as f32, h as f32)),
        _ => None,
    }
}

/// Only a cell showing the item's thumbnail can travel; the rest (audio, still generating,
/// failed) crossfade.
fn morphable(item: &Generation) -> bool {
    item.thumbnail.is_some() && item.status == Status::Completed && matches!(item.media_type, MediaType::Image | MediaType::Video)
}

const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 20.0;
/// The stage's zoom row (flarly's `ZOOMS`): multiples of the fitted size, 1× being fit-to-view.
const ZOOM_PRESETS: [f32; 5] = [1.0, 2.0, 4.0, 6.0, 8.0];

impl DetailView {
    /// The item currently shown (the feed reveals it when the detail closes).
    pub fn current_id(&self) -> Option<EntryId> {
        self.ids.get(self.index).cloned()
    }

    #[cfg(test)]
    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// `origin` is the feed cell's box to grow out of (`None`: crossfade in); `thumbnails` the feed's
    /// image cache, see the field.
    pub fn new(ids: Vec<EntryId>, index: usize, origin: Option<Bounds<Pixels>>, thumbnails: Entity<LruImageCache>, cx: &mut Context<Self>) -> Self {
        let library = state::library(cx);
        cx.observe(&library, |this, lib, cx| {
            let lib = lib.read(cx);
            let current = this.ids.get(this.index).cloned();
            this.ids.retain(|id| lib.lib.entry(id).is_some());
            this.info_assets = None;
            this.info_uses.clear();
            this.request = None;
            if this.ids.is_empty() {
                cx.emit(DetailEvent::Close);
            } else {
                // Follow the current item by identity so removing an
                // earlier item doesn't change pages; only if it vanished do we clamp, and then a
                // slide in progress belonged to it.
                match current.and_then(|id| this.ids.iter().position(|candidate| *candidate == id)) {
                    Some(index) => this.index = index,
                    None => {
                        this.index = feed::safe_index(this.index, this.ids.len());
                        this.paging.reset();
                        this.gesture_timeout = None;
                    }
                }
            }
            cx.notify();
        })
        .detach();
        let focus = cx.focus_handle();
        let handle = focus.clone();
        cx.defer(move |cx| {
            if let Some(window) = cx.active_window() {
                window.update(cx, |_, window, cx| handle.focus(window, cx)).ok();
            }
        });
        let index = feed::safe_index(index, ids.len());
        Self {
            ids,
            index,
            compare: None,
            divider: 0.5,
            divider_drag: false,
            hover_left: false,
            hover_right: false,
            zoom: None,
            save_state: SaveState::Idle,
            save_task: None,
            elapsed_ticker: None,
            pan: Point::default(),
            drag_start: None,
            show_info: false,
            info_assets: None,
            info_uses: Vec::new(),
            request: None,
            area: bounds_slot(),
            scrub: bounds_slot(),
            focus,
            library,
            player: None,
            player_for: None,
            audio: None,
            audio_for: None,
            player_error: None,
            player_opening: None,
            pump: None,
            frame_image: None,
            paging: Paging::default(),
            gesture_timeout: None,
            morph: None,
            opening: true,
            origin,
            thumbnails,
            images: LruImageCache::with_budget(DETAIL_IMAGE_BUDGET, cx),
            close_task: None,
        }
    }

    // ----- open / close morph ------------------------------------------------------

    /// The view is growing out of, or shrinking back into, its feed cell; the owner keeps the
    /// feed drawn underneath meanwhile.
    pub fn is_transitioning(&self) -> bool {
        self.morph.is_some() || self.opening
    }

    /// Where the current image sits at rest (fit to view, centred), in window coordinates.
    /// `None` until the stage has been measured.
    fn resting_rect(&self, cx: &App) -> Option<Bounds<Pixels>> {
        let area = *self.area.borrow();
        if area.size.width <= Pixels::ZERO || area.size.height <= Pixels::ZERO {
            return None;
        }
        let pixel_size = self.image_pixel_size(cx);
        let scale = self.fit_scale_for(pixel_size);
        let (iw, ih) = pixel_size.unwrap_or((f32::from(area.size.width) - 24., f32::from(area.size.height) - 24.));
        let (w, h) = (px(iw * scale), px(ih * scale));
        Some(Bounds { origin: point(area.origin.x + (area.size.width - w) / 2., area.origin.y + (area.size.height - h) / 2.), size: size(w, h) })
    }

    /// Second half of `Back`: the owner answers `WillClose` with the feed cell to shrink into
    /// (`None` when it is off screen — then the view fades out), and `Close` is emitted once the
    /// morph has played.
    pub fn close_towards(&mut self, cell: Option<Bounds<Pixels>>, cx: &mut Context<Self>) {
        if self.morph.is_some() {
            return;
        }
        self.opening = false;
        if cx.reduce_motion() {
            cx.emit(DetailEvent::Close);
            return;
        }
        // A zoomed image would overflow the stage on its way down: shrink from its fitted rect.
        self.zoom = None;
        self.pan = Point::default();
        self.paging.reset();
        self.gesture_timeout = None;
        let cell = cell.filter(|_| self.item(cx).as_ref().is_some_and(morphable));
        let morph = Morph::new(Direction::Close, cell, self.resting_rect(cx), now(cx));
        let duration = morph.duration();
        self.morph = Some(morph);
        self.close_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            this.update(cx, |_, cx| cx.emit(DetailEvent::Close)).ok();
        }));
        cx.notify();
    }

    /// Start the open morph once the stage is measured (or right away when there is no cell to
    /// travel from) and retire it when it finishes; returns whether another frame is needed.
    /// Called from `render`; the notifies let the owner follow [`Self::is_transitioning`].
    fn tick_morph(&mut self, item: &Generation, clock: std::time::Instant, cx: &mut Context<Self>) -> bool {
        if let Some(morph) = &self.morph {
            if morph.is_done(clock) && morph.direction() == Direction::Open {
                self.morph = None;
                cx.notify();
                return false;
            }
            return true;
        }
        if !self.opening {
            return false;
        }
        let stage = self.resting_rect(cx);
        if stage.is_none() && self.origin.is_some() {
            return false;
        }
        self.opening = false;
        cx.notify();
        if cx.reduce_motion() {
            return false;
        }
        let cell = self.origin.take().filter(|_| morphable(item));
        self.morph = Some(Morph::new(Direction::Open, cell, stage, clock));
        true
    }

    // ----- video ---------------------------------------------------------------

    /// Create / drop the player so it always matches the current item. Opening reads the whole
    /// sample table, so it happens off the UI thread; the poster stays up until the first frame.
    fn sync_player(&mut self, item: &Generation, window: &mut Window, cx: &mut Context<Self>) {
        // Mid-slide the leaving item keeps its player (and so its picture) and the arriving one
        // waits: opening a source and dropping a decoder are work the animation's frames can't
        // spare, and the strip would otherwise snap from the last frame back to the thumbnail.
        if self.paging.is_animating() {
            return;
        }
        self.sync_audio(item);
        let wants = item.media_type == MediaType::Video && item.status == Status::Completed && item.path.is_some();
        if !wants {
            self.drop_player(window);
            return;
        }
        if self.player_for.as_ref() == Some(&item.id) {
            return;
        }
        self.drop_player(window);
        self.player_for = Some(item.id.clone());
        self.player_error = None;
        let Some(path) = item.path.clone() else { return };
        let id = item.id.clone();
        self.player_opening = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = cx
                .background_spawn({
                    let path = path.clone();
                    async move { majik_video::Source::open(&path) }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.player_for.as_ref() != Some(&id) {
                    return;
                }
                match opened {
                    Ok(source) => {
                        let executor = cx.background_executor().clone();
                        let now: majik_video::Now = Arc::new(move || executor.now());
                        let mut player = majik_video::Player::new(source, &path, now);
                        player.set_looping(true);
                        if std::env::var_os("MAJIK_AUTOPLAY").is_some() {
                            player.play();
                        }
                        this.player = Some(player);
                        this.ensure_pump(window, cx);
                    }
                    Err(e) => this.player_error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn drop_player(&mut self, window: &mut Window) {
        self.player = None;
        self.player_for = None;
        self.pump = None;
        self.player_opening = None;
        if let Some(image) = self.frame_image.take() {
            if let Err(e) = window.drop_image(image) {
                tracing::warn!(target: "majik", "releasing video frame: {e:#}");
            }
        }
    }

    /// Start the decode loop if the player wants frames and none is running. Each turn asks the
    /// player for a job, runs it on the background executor, applies the result, and waits half a
    /// frame interval; it ends when the player is paused with the right picture up.
    fn ensure_pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pump.is_some() || !self.player.as_ref().is_some_and(|p| p.wants_frames()) {
            return;
        }
        self.pump = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let next = this.update(cx, |this, _| {
                    let job = this.player.as_mut().and_then(|p| p.decode_job().map(|job| (job, p.frame_interval())));
                    if job.is_none() {
                        this.pump = None;
                    }
                    job
                });
                let Ok(Some((job, interval))) = next else { break };
                let result = cx.background_spawn(async move { job.run() }).await;
                let keep = this.update_in(cx, |this, window, cx| {
                    let Some(player) = this.player.as_mut() else { return false };
                    if player.apply(result) {
                        this.upload_frame(window);
                    }
                    if let Some(e) = this.player.as_ref().and_then(|p| p.error()) {
                        this.player_error = Some(e.to_string());
                    }
                    cx.notify();
                    this.player.as_ref().is_some_and(|p| p.wants_frames())
                });
                if !matches!(keep, Ok(true)) {
                    this.update(cx, |this, _| this.pump = None).ok();
                    break;
                }
                cx.background_executor().timer((interval / 2).max(Duration::from_millis(4))).await;
            }
        }));
    }

    /// Hand the player's current frame to GPUI (its bytes are already BGRA) and release the previous one.
    fn upload_frame(&mut self, window: &mut Window) {
        let Some(frame) = self.player.as_ref().and_then(|p| p.frame()) else { return };
        let Some(image) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra.clone()) else {
            tracing::warn!(target: "majik", "video frame has {} bytes for {}x{}", frame.bgra.len(), frame.width, frame.height);
            return;
        };
        let render = Arc::new(gpui::RenderImage::new([image::Frame::new(image)]));
        if let Some(previous) = self.frame_image.replace(render) {
            if let Err(e) = window.drop_image(previous) {
                tracing::warn!(target: "majik", "releasing video frame: {e:#}");
            }
        }
    }

    /// Port of `AudioPlayerView`: one `majik_audio::Player` per audio item, hard-stopped on page change.
    fn sync_audio(&mut self, item: &Generation) {
        let wants = item.media_type == MediaType::Audio && item.status == Status::Completed && item.path.is_some();
        if !wants {
            self.audio = None;
            self.audio_for = None;
            return;
        }
        if self.audio_for.as_ref() == Some(&item.id) {
            return;
        }
        self.audio = None;
        self.audio_for = Some(item.id.clone());
        self.player_error = None;
        match majik_audio::Player::open(item.path.as_ref().unwrap()) {
            Ok(p) => self.audio = Some(p),
            Err(e) => self.player_error = Some(e.to_string()),
        }
    }

    fn toggle_playback(&mut self, _: &TogglePlayback, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.player {
            p.toggle();
            self.ensure_pump(window, cx);
            cx.notify();
        } else if let Some(a) = &mut self.audio {
            a.toggle();
            cx.notify();
        }
    }

    fn seek_fraction(&mut self, frac: f32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.player {
            let d = p.duration();
            if d > 0.0 {
                p.seek((frac.clamp(0.0, 1.0) as f64) * d);
                self.ensure_pump(window, cx);
                cx.notify();
            }
        } else if let Some(a) = &mut self.audio {
            let d = a.duration();
            if d > 0.0 {
                a.seek((frac.clamp(0.0, 1.0) as f64) * d);
                cx.notify();
            }
        }
    }

    /// (position, duration, playing) of whichever player is active.
    #[cfg(test)]
    fn transport(&self) -> Option<(f64, f64, bool)> {
        if let Some(p) = &self.player {
            return Some((p.position(), p.duration(), p.is_playing()));
        }
        if let Some(a) = &self.audio {
            return Some((a.position(), a.duration(), a.is_playing() && !a.finished()));
        }
        None
    }

    /// The transport bar's (position, duration, playing) for `item`: its player's once that is
    /// open, and until then — while the source opens, or mid-slide while the leaving item still
    /// holds the player — a paused bar at zero over the stored duration. The bar keeps its place
    /// either way: were it to vanish meanwhile, the stage would grow and shrink by its height and
    /// the picture jump with it. `None` for anything that doesn't play.
    fn transport_for(&self, item: &Generation) -> Option<(f64, f64, bool)> {
        if let Some(p) = self.player.as_ref().filter(|_| self.player_for.as_ref() == Some(&item.id)) {
            return Some((p.position(), p.duration(), p.is_playing()));
        }
        if let Some(a) = self.audio.as_ref().filter(|_| self.audio_for.as_ref() == Some(&item.id)) {
            return Some((a.position(), a.duration(), a.is_playing() && !a.finished()));
        }
        let plays = matches!(item.media_type, MediaType::Video | MediaType::Audio) && item.status == Status::Completed && item.file().is_some();
        plays.then(|| (0.0, item.duration_secs.unwrap_or(0.0), false))
    }

    /// Keep the elapsed label of a generating item moving (1 Hz), like the feed's generating cells.
    fn sync_elapsed_ticker(&mut self, item: &Generation, cx: &mut Context<Self>) {
        if item.status != Status::Generating {
            self.elapsed_ticker = None;
        } else if self.elapsed_ticker.is_none() {
            self.elapsed_ticker = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }));
        }
    }

    fn subject_at(&self, index: usize, cx: &App) -> Option<Subject> {
        let lib = &self.library.read(cx).lib;
        match self.ids.get(index)? {
            EntryId::Generation(id) => lib.get(id).cloned().map(|item| Subject { generation: Some(item.id.clone()), asset: item.output_asset_id.clone(), item }),
            EntryId::Asset(id) => {
                let asset = lib.asset(id)?;
                Some(match lib.generation_producing(id).and_then(|m| lib.get(&m)).cloned() {
                    Some(item) => Subject { generation: Some(item.id.clone()), asset: Some(id.clone()), item },
                    None => Subject { item: item_for_asset(asset), generation: None, asset: Some(id.clone()) },
                })
            }
        }
    }

    fn subject(&self, cx: &App) -> Option<Subject> {
        self.subject_at(self.index, cx)
    }

    /// The item shown (see [`Subject`]).
    fn item(&self, cx: &App) -> Option<Generation> {
        self.subject(cx).map(|s| s.item)
    }

    /// The generation shown, if the subject is one.
    fn generation(&self, cx: &App) -> Option<Generation> {
        self.subject(cx).filter(|s| s.generation.is_some()).map(|s| s.item)
    }

    fn go(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.ids.len() as isize;
        if n == 0 || self.morph.is_some() {
            return;
        }
        let next = (self.index as isize + delta).clamp(0, n - 1) as usize;
        if next != self.index {
            let step = if next > self.index { Step::Next } else { Step::Prev };
            self.index = next;
            self.zoom = None;
            self.pan = Point::default();
            self.gesture_timeout = None;
            let width = f32::from(slot_size(&self.area).width);
            // An unmeasured stage (headless tests) has nothing to slide, so it jumps — as does
            // reduce motion.
            if width > 0.0 && !cx.reduce_motion() {
                self.paging.navigate(step, width, now(cx));
            }
            cx.notify();
        }
    }

    fn edges(&self) -> Edges {
        Edges { width: f32::from(slot_size(&self.area).width), at_start: self.index == 0, at_end: self.index + 1 >= self.ids.len() }
    }

    /// Apply a page change decided by a gesture. The strip is already re-based, so unlike `go()`
    /// this must not restart a slide.
    fn apply_step(&mut self, step: Option<Step>, cx: &mut Context<Self>) {
        match step {
            Some(Step::Next) if self.index + 1 < self.ids.len() => self.index += 1,
            Some(Step::Prev) if self.index > 0 => self.index -= 1,
            _ => return,
        }
        self.zoom = None;
        self.pan = Point::default();
        cx.notify();
    }

    /// Some backends send no phases at all, and macOS delivers a cancelled gesture as a plain
    /// `Moved`, so an open gesture that goes quiet is released by this timer (see
    /// [`Paging::quiet_timeout`] for how long "quiet" is).
    fn arm_gesture_timeout(&mut self, cx: &mut Context<Self>) {
        if !self.paging.has_gesture() {
            self.gesture_timeout = None;
            return;
        }
        let timeout = self.paging.quiet_timeout();
        self.gesture_timeout = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(timeout).await;
            this.update(cx, |this, cx| this.finish_gesture(cx)).ok();
        }));
    }

    fn finish_gesture(&mut self, cx: &mut Context<Self>) {
        self.gesture_timeout = None;
        let step = self.paging.finish(self.edges(), now(cx));
        self.apply_step(step, cx);
        cx.notify();
    }

    // ----- zoom / pan ---------------------------------------------------------

    fn image_pixel_size(&self, cx: &App) -> Option<(f32, f32)> {
        let item = self.item(cx)?;
        let player = self.player.as_ref().filter(|_| self.player_for.as_ref() == Some(&item.id));
        if let Some((w, h)) = player.and_then(|p| p.size()) {
            return Some((w as f32, h as f32));
        }
        item_pixel_size(&item)
    }

    /// Scale that fits an image of `size` pixels inside the viewing area (logical px per image px).
    fn fit_scale_for(&self, size: Option<(f32, f32)>) -> f32 {
        let area = slot_size(&self.area);
        let Some((iw, ih)) = size else { return 1.0 };
        let pad = 24.0;
        let aw = (f32::from(area.width) - pad).max(1.0);
        let ah = (f32::from(area.height) - pad).max(1.0);
        (aw / iw).min(ah / ih)
    }

    fn current_scale(&self, window: &Window, cx: &App) -> f32 {
        match self.zoom {
            Some(z) => z / window.scale_factor(),
            None => self.fit_scale_for(self.image_pixel_size(cx)),
        }
    }

    /// The zoom that shows the image at its fitted size.
    fn fit_zoom(&self, window: &Window, cx: &App) -> f32 {
        self.fit_scale_for(self.image_pixel_size(cx)) * window.scale_factor()
    }

    /// Absolute zoom is capped at `ZOOM_MAX`, but the largest preset must stay reachable for a
    /// small image whose fitted size is already many device pixels per image pixel.
    fn max_zoom(&self, window: &Window, cx: &App) -> f32 {
        ZOOM_MAX.max(self.fit_zoom(window, cx) * ZOOM_PRESETS[ZOOM_PRESETS.len() - 1])
    }

    /// Zoom to `factor` × the fitted size; 1× is fit-to-view itself (`zoom = None`), as the
    /// presets in the stage's zoom row.
    fn zoom_preset(&mut self, factor: f32, window: &Window, cx: &mut Context<Self>) {
        if factor <= 1.0 {
            self.zoom = None;
            self.pan = Point::default();
            cx.notify();
        } else {
            let zoom = self.fit_zoom(window, cx) * factor;
            self.set_zoom_around(zoom, None, window, cx);
        }
    }

    /// The preset the current zoom matches, if any: fit is 1×, and a wheel or pinch zoom that
    /// ends between presets highlights none.
    fn active_zoom_preset(&self, window: &Window, cx: &App) -> Option<f32> {
        let Some(zoom) = self.zoom else { return Some(1.0) };
        let factor = zoom / self.fit_zoom(window, cx);
        ZOOM_PRESETS.into_iter().find(|preset| (factor - preset).abs() < 0.01)
    }

    fn set_zoom_around(&mut self, new_zoom: f32, anchor: Option<Point<Pixels>>, window: &Window, cx: &mut Context<Self>) {
        let new_zoom = new_zoom.clamp(ZOOM_MIN, self.max_zoom(window, cx));
        let old_scale = self.current_scale(window, cx);
        let new_scale = new_zoom / window.scale_factor();
        let area = slot_size(&self.area);
        let center = point(area.width / 2., area.height / 2.);
        if let Some(p) = anchor {
            // Keep the image point under the cursor fixed.
            let k = new_scale / old_scale;
            let v = point(p.x - center.x - self.pan.x, p.y - center.y - self.pan.y);
            self.pan = point(p.x - center.x - v.x * k, p.y - center.y - v.y * k);
        }
        self.zoom = Some(new_zoom);
        self.clamp_pan(new_scale, cx);
        cx.notify();
    }

    fn clamp_pan(&mut self, scale: f32, cx: &App) {
        let Some((iw, ih)) = self.image_pixel_size(cx) else { return };
        let area = slot_size(&self.area);
        let (w, h) = (iw * scale, ih * scale);
        let max_x = ((w - f32::from(area.width)) / 2.).max(0.);
        let max_y = ((h - f32::from(area.height)) / 2.).max(0.);
        self.pan = point(px(f32::from(self.pan.x).clamp(-max_x, max_x)), px(f32::from(self.pan.y).clamp(-max_y, max_y)));
    }

    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        let z = self.current_scale(window, cx) * window.scale_factor() * ZOOM_STEP;
        self.set_zoom_around(z, None, window, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        let z = self.current_scale(window, cx) * window.scale_factor() / ZOOM_STEP;
        self.set_zoom_around(z, None, window, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, window: &mut Window, cx: &mut Context<Self>) {
        // Toggle between "actual size" and "fit", like Preview.
        if self.zoom.is_some() {
            self.zoom = None;
            self.pan = Point::default();
            cx.notify();
        } else {
            self.set_zoom_around(1.0, None, window, cx);
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, area_origin: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.morph.is_some() {
            return;
        }
        let delta = ev.delta.pixel_delta(px(20.));
        if ev.modifiers.platform || ev.modifiers.control {
            let factor = (1.0 - f32::from(delta.y) / 200.0).clamp(0.5, 2.0);
            let z = self.current_scale(window, cx) * window.scale_factor() * factor;
            let anchor = point(ev.position.x - area_origin.x, ev.position.y - area_origin.y);
            self.set_zoom_around(z, Some(anchor), window, cx);
        } else if self.zoom.is_some() {
            self.pan = point(self.pan.x + delta.x, self.pan.y + delta.y);
            let scale = self.current_scale(window, cx);
            self.clamp_pan(scale, cx);
            cx.notify();
        } else if ev.delta.precise() {
            // Trackpad / Magic Mouse paging. Wheel mice (`Lines`) stay inert, like Photos.
            let edges = self.edges();
            if edges.width <= 0.0 {
                return;
            }
            let step = self.paging.scroll(ev.touch_phase, point(f32::from(delta.x), f32::from(delta.y)), edges, now(cx));
            self.apply_step(step, cx);
            self.arm_gesture_timeout(cx);
            cx.notify();
        }
    }

    // ----- actions ------------------------------------------------------------

    fn next(&mut self, _: &NextItem, _: &mut Window, cx: &mut Context<Self>) {
        self.go(1, cx);
    }

    fn prev(&mut self, _: &PrevItem, _: &mut Window, cx: &mut Context<Self>) {
        self.go(-1, cx);
    }

    pub(crate) fn back(&mut self, _: &Back, _: &mut Window, cx: &mut Context<Self>) {
        if self.morph.is_some() {
            return;
        }
        match self.current_id() {
            Some(id) => cx.emit(DetailEvent::WillClose { id }),
            None => cx.emit(DetailEvent::Close),
        }
    }

    fn show_info(&mut self, _: &ShowInfo, _: &mut Window, cx: &mut Context<Self>) {
        if self.morph.is_some() {
            return;
        }
        self.show_info = !self.show_info;
        cx.notify();
    }

    /// Parse the current item's request once (called from `render`, like `sync_player`).
    fn sync_request(&mut self, item: &Generation) {
        if self.request.as_ref().is_some_and(|(id, ..)| *id == item.id) {
            return;
        }
        self.request = Some((item.id.clone(), item.request_json.as_deref().and_then(majik_generation::Request::from_json), item.prompt()));
    }

    fn request(&self) -> Option<&majik_generation::Request> {
        self.request.as_ref().and_then(|(_, request, _)| request.as_ref())
    }

    /// Resolve the before/after image of the current item once (called from `render`, like
    /// `sync_request`): a completed tool row's reference image, while its file is still there.
    fn sync_compare(&mut self, item: &Generation, cx: &App) {
        if self.compare.as_ref().is_some_and(|(id, _)| *id == item.id) {
            return;
        }
        let original = (item.tool.is_some() && item.status == Status::Completed && item.media_type == MediaType::Image)
            .then(|| {
                let lib = &self.library.read(cx).lib;
                lib.inputs(&item.id).into_iter().find(|(link, _)| link.role == majik_providers::AssetRole::ReferenceImage.raw()).and_then(|(_, asset)| asset.file().map(std::path::Path::to_path_buf))
            })
            .flatten();
        self.compare = Some((item.id.clone(), original));
    }

    /// The image the current item is compared against, when there is one.
    fn compare_original(&self) -> Option<&std::path::Path> {
        self.compare.as_ref().and_then(|(_, original)| original.as_deref())
    }

    /// Put the before/after divider under `x` (window coordinates), clamped to the image.
    fn drag_divider_to(&mut self, x: Pixels, window: &Window, cx: &mut Context<Self>) {
        let area = *self.area.borrow();
        let Some((iw, _)) = self.image_pixel_size(cx) else { return };
        let w = iw * self.current_scale(window, cx);
        if w <= 0. {
            return;
        }
        let left = (f32::from(area.size.width) - w) / 2. + f32::from(self.pan.x);
        self.divider = ((f32::from(x) - f32::from(area.origin.x) - left) / w).clamp(0., 1.);
        cx.notify();
    }

    fn prompt(&self) -> Option<String> {
        self.request.as_ref().and_then(|(_, _, prompt)| prompt.clone())
    }

    /// Keep the info panel's input-asset strip matched to the current item (called from `render`,
    /// like `sync_player`); a no-op while the panel is closed.
    fn sync_info_assets(&mut self, item: &Generation, cx: &App) {
        if !self.show_info || self.info_assets.as_ref().is_some_and(|(id, _)| *id == item.id) {
            return;
        }
        let lib = &self.library.read(cx).lib;
        let assets = lib
            .inputs(&item.id)
            .into_iter()
            .map(|(link, asset)| {
                let picture = match asset.kind {
                    MediaType::Image => Some(asset.path.clone()),
                    MediaType::Video => asset.thumbnail.clone(),
                    MediaType::Audio => None,
                };
                InfoAsset { role: link.role, kind: asset.kind, path: asset.path, picture }
            })
            .collect();
        self.info_assets = Some((item.id.clone(), assets));
        self.info_uses = match self.subject(cx) {
            Some(Subject { generation: None, asset: Some(asset), .. }) => {
                lib.generations_using(&asset).iter().filter_map(|id| lib.get(id)).filter_map(|used| used.thumbnail.clone().or_else(|| used.file().map(std::path::Path::to_path_buf))).collect()
            }
            _ => Vec::new(),
        };
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.generation(cx) {
            self.library.update(cx, |m, cx| m.set_favorite(std::slice::from_ref(&item.id), !item.is_favorite, cx));
        }
    }

    /// Delete the generation (its files stay as assets), or trash the asset when that is what is
    /// shown. Trashing is refused while a live generation references the asset, as in the grid.
    fn delete(&mut self, _: &DeleteMedia, window: &mut Window, cx: &mut Context<Self>) {
        let Some(subject) = self.subject(cx) else { return };
        let library = self.library.clone();
        match (subject.generation, subject.asset) {
            (Some(id), _) => {
                let label = subject.item.media_type.label().to_lowercase();
                let answer = window.prompt(PromptLevel::Warning, &format!("Delete this {label}?"), Some("The files stay in the library as assets."), &["Delete", "Cancel"], cx);
                cx.spawn(async move |_, cx| {
                    if answer.await == Ok(0) {
                        cx.update(|cx| library.update(cx, |m, cx| m.delete(std::slice::from_ref(&id), cx)));
                    }
                })
                .detach();
            }
            (None, Some(asset)) => {
                if self.library.read(cx).lib.is_referenced(&asset) {
                    crate::ui::toast(window, format!("{} is used by a generation and can't be deleted.", subject.item.file_name()), cx);
                    return;
                }
                let answer = window.prompt(PromptLevel::Warning, "Delete this asset?", Some("The file is moved to the library's .majik/trash folder."), &["Delete", "Cancel"], cx);
                cx.spawn_in(window, async move |_, cx| {
                    if answer.await == Ok(0) {
                        cx.update(|window, cx| {
                            if let Err(e) = library.update(cx, |m, cx| m.delete_assets(std::slice::from_ref(&asset), cx)) {
                                crate::ui::toast(window, format!("Couldn't delete: {e:#}"), cx);
                            }
                        })
                        .ok();
                    }
                })
                .detach();
            }
            (None, None) => {}
        }
    }


    fn copy(&mut self, _: &CopyMedia, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(exportable) = self.item(cx).as_ref().and_then(Exportable::of_item) {
            copy_items(&[exportable], window, cx);
        }
    }

    fn save(&mut self, _: &SaveMedia, window: &mut Window, cx: &mut Context<Self>) {
        if self.save_state != SaveState::Idle {
            return;
        }
        let Some(exportable) = self.item(cx).filter(|i| i.status == Status::Completed).as_ref().and_then(Exportable::of_item) else { return };
        self.save_state = SaveState::Saving;
        let task = save_item(exportable, window, cx);
        self.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            let settled = this.update_in(cx, |v, window, cx| {
                v.save_state = match &outcome {
                    SaveOutcome::Saved => SaveState::Saved,
                    SaveOutcome::Failed(e) => {
                        crate::ui::toast(window, format!("Save failed: {e}"), cx);
                        SaveState::Failed
                    }
                    SaveOutcome::Cancelled => SaveState::Idle,
                };
                cx.notify();
                v.save_state != SaveState::Idle
            });
            if !matches!(settled, Ok(true)) {
                return;
            }
            cx.background_executor().timer(SAVE_STATE_RESET).await;
            this.update(cx, |v, cx| {
                v.save_state = SaveState::Idle;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn recreate(&mut self, _: &Recreate, window: &mut Window, cx: &mut Context<Self>) {
        match self.generation(cx) {
            Some(item) if item.can_recreate() => {
                cx.emit(DetailEvent::Compose(PendingCompose { recreate: Some(item.id.clone()) }))
            }
            _ => crate::ui::toast(window, "This item can't be recreated.", cx),
        }
    }

    fn retry(&mut self, _: &Retry, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.generation(cx) {
            self.library.update(cx, |m, cx| m.retry(std::slice::from_ref(&item.id), cx));
        }
    }

    // ----- rendering ----------------------------------------------------------

    /// One viewport-wide slot of the strip at horizontal position `x`. Only the current slot carries
    /// zoom/pan, the live player and the interactive controls; neighbours are fit-scaled previews,
    /// rendered (clipped) even at rest so their images are decoded before a swipe reveals them.
    fn render_slot(&self, k: usize, item: &Generation, is_current: bool, x: Pixels, window: &Window, cx: &Context<Self>) -> gpui::AnyElement {
        let area = slot_size(&self.area);
        let theme = cx.theme();
        let (muted, muted_fg, danger) = (theme.muted, theme.muted_foreground, theme.danger);
        let size = if is_current { self.image_pixel_size(cx) } else { item_pixel_size(item) };
        let scale = if is_current { self.current_scale(window, cx) } else { self.fit_scale_for(size) };
        let (iw, ih) = size.unwrap_or((f32::from(area.width).max(1.) - 24., f32::from(area.height).max(1.) - 24.));
        let (w, h) = (px(iw * scale), px(ih * scale));
        let pan = if is_current { self.pan } else { Point::default() };
        let left = (area.width - w) / 2. + pan.x;
        let top = (area.height - h) / 2. + pan.y;

        let media: gpui::AnyElement = match (item.status, &item.path, item.media_type) {
            (Status::Generating, _, _) => {
                // `GenerationLoadingView(style: .regular)`: a larger spinner over the elapsed time
                // of the attempt (a retry counts from when it was asked for).
                let elapsed = majik_core::now_ms().saturating_sub(item.queued_at_ms) / 1000;
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(spin(icon("loader-circle").size_10().text_color(muted_fg)))
                    .child(gpui::div().text_color(muted_fg).child(format!("Generating {}  ·  {}", item.media_type.label().to_lowercase(), format_duration(elapsed as f64))))
                    .into_any_element()
            }
            (Status::Failed, _, _) => {
                let provider = item.provider.clone().map(majik_providers::ProviderId);
                let action = majik_generation::recovery::recovery_action(item.error_kind.as_deref(), item.error.as_deref(), provider.as_ref());
                let secondary: Option<gpui::AnyElement> = match &action {
                    majik_generation::RecoveryAction::Retry => None,
                    majik_generation::RecoveryAction::OpenProviderSettings => Some(
                        button(("recover-settings", k)).label(action.title()).icon(icon(action.icon())).outline().on_click(|_, _, cx| crate::windows::open_settings(crate::views::settings::SettingsTarget::providers(), cx)).into_any_element(),
                    ),
                    majik_generation::RecoveryAction::CheckCredits(url) => {
                        let url = url.clone();
                        Some(button(("recover-credits", k)).label(action.title()).icon(icon(action.icon())).outline().on_click(move |_, _, cx| cx.open_url(&url)).into_any_element())
                    }
                };
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(icon("circle-alert").size_8().text_color(danger))
                    .child("Something went wrong")
                    .child(gpui::div().text_sm().text_color(muted_fg).max_w(px(480.)).text_center().child(item.error.clone().unwrap_or_default()))
                    .when(is_current, |d| {
                        d.child(h_flex().gap_2().child(button(("retry", k)).label("Try Again").icon(icon("refresh-cw")).primary().on_click(cx.listener(|this, _, w, cx| this.retry(&Retry, w, cx)))).children(secondary))
                    })
                    .into_any_element()
            }
            (Status::Missing, _, _) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(icon("file-x").size_8().text_color(danger))
                .child("File is missing")
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(muted_fg)
                        .max_w(px(480.))
                        .text_center()
                        .child(format!("{} is no longer in the library folder. It may have been moved or deleted outside Majik.", item.file_name())),
                )
                .when(is_current && item.can_retry(), |d| {
                    d.child(button(("regenerate", k)).label("Generate Again").icon(icon("refresh-cw")).primary().on_click(cx.listener(|this, _, w, cx| this.retry(&Retry, w, cx))))
                })
                .into_any_element(),
            (_, Some(path), MediaType::Image) => match self.compare_original().filter(|_| is_current) {
                // Before/after: the input underneath, the result clipped to the right of the divider.
                Some(original) => {
                    let cut = w * self.divider;
                    let (line, ink) = (gpui::white(), gpui::black().opacity(0.55));
                    let label = |text: &'static str| gpui::div().px_2().py_0p5().rounded_full().bg(ink).text_xs().text_color(line).child(text);
                    gpui::div()
                        .id(("compare", k))
                        .absolute()
                        .inset_0()
                        .child(gpui::img(original.to_path_buf()).image_cache(&self.images).absolute().left(left).top(top).w(w).h(h).object_fit(ObjectFit::Fill))
                        .child(gpui::div().absolute().left(left + cut).top(top).w(w - cut).h(h).overflow_hidden().child(gpui::img(path.clone()).image_cache(&self.images).absolute().left(-cut).top(px(0.)).w(w).h(h).object_fit(ObjectFit::Fill)))
                        .child(gpui::div().absolute().left(left + cut - px(1.)).top(top).w(px(2.)).h(h).bg(line).shadow_md())
                        .child(
                            gpui::div()
                                .absolute()
                                .left(left + cut - px(14.))
                                .top(top + h / 2. - px(14.))
                                .size(px(28.))
                                .rounded_full()
                                .bg(ink)
                                .border_1()
                                .border_color(line)
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap_1()
                                .child(gpui::div().w(px(2.)).h(px(10.)).rounded_sm().bg(line))
                                .child(gpui::div().w(px(2.)).h(px(10.)).rounded_sm().bg(line)),
                        )
                        .child(gpui::div().absolute().left(left + px(8.)).top(top + px(8.)).child(label("Before")))
                        .child(gpui::div().absolute().right(area.width - left - w + px(8.)).top(top + px(8.)).child(label("After")))
                        .into_any_element()
                }
                None => gpui::img(path.clone()).image_cache(&self.images).absolute().left(left).top(top).w(w).h(h).object_fit(ObjectFit::Fill).into_any_element(),
            },
            (_, Some(_), MediaType::Video) => {
                // The picture follows the player, which mid-slide still belongs to the leaving item.
                let owns_player = self.player_for.as_ref() == Some(&item.id);
                let player = if owns_player { self.player.as_ref() } else { None };
                let playing = player.map(|p| p.is_playing()).unwrap_or(false);
                let frame = if owns_player { self.frame_image.clone() } else { None };
                let surface_el: gpui::AnyElement = match frame {
                    Some(f) => gpui::img(f).absolute().left(left).top(top).w(w).h(h).object_fit(ObjectFit::Fill).into_any_element(),
                    None => item
                        .thumbnail
                        .clone()
                        .map(|t| gpui::img(t).image_cache(&self.thumbnails).absolute().left(left).top(top).w(w).h(h).object_fit(ObjectFit::Fill).into_any_element())
                        .unwrap_or_else(|| gpui::div().absolute().left(left).top(top).w(w).h(h).bg(muted).into_any_element()),
                };
                gpui::div()
                    .id(("video-stage", k))
                    .absolute()
                    .inset_0()
                    .child(surface_el)
                    .when(!playing, |d| {
                        d.child(
                            v_flex().absolute().inset_0().items_center().justify_center().gap_2().child(
                                gpui::div().p_4().rounded_full().bg(gpui::black().opacity(0.5)).child(icon("play").size_8().text_color(gpui::white())),
                            ),
                        )
                    })
                    .when(is_current, |d| {
                        d.when_some(self.player_error.clone(), |d, err| {
                            d.child(gpui::div().absolute().bottom_4().left_4().px_2().py_1().rounded_md().bg(gpui::black().opacity(0.6)).text_xs().text_color(gpui::white()).child(err))
                        })
                        .on_click(cx.listener(|this, _, w, cx| this.toggle_playback(&TogglePlayback, w, cx)))
                    })
                    .into_any_element()
            }
            _ => {
                let playing = is_current && self.audio.as_ref().map(|a| a.is_playing() && !a.finished()).unwrap_or(false);
                v_flex()
                    .id(("audio-stage", k))
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(gpui::div().p_6().rounded_full().bg(muted).child(icon(if playing { "pause" } else { "audio-lines" }).size_12().text_color(muted_fg)))
                    .child(gpui::div().text_sm().text_color(muted_fg).child(item.file_name()))
                    .when(is_current, |d| {
                        d.when_some(self.player_error.clone(), |d, e| d.child(gpui::div().text_xs().text_color(danger).child(e)))
                            .on_click(cx.listener(|this, _, w, cx| this.toggle_playback(&TogglePlayback, w, cx)))
                    })
                    .into_any_element()
            }
        };
        gpui::div().id(("slot", k)).absolute().top_0().left(x).w(area.width).h(area.height).child(media).into_any_element()
    }

    fn render_info(&self, item: &Generation, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let chip = |label: &'static str, value: String| {
            v_flex()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.muted)
                .child(gpui::div().text_xs().text_color(theme.muted_foreground).child(label))
                .child(gpui::div().text_sm().child(value))
        };
        let mut chips: Vec<gpui::AnyElement> = vec![chip("Type", item.media_type.label().into()).into_any_element()];
        if let Some(m) = &item.model_name {
            chips.push(chip("Model", m.clone()).into_any_element());
        }
        if let (Some(w), Some(h)) = (item.width, item.height) {
            chips.push(chip("Dimensions", format!("{w} × {h}")).into_any_element());
        }
        if let Some(d) = item.duration_secs {
            chips.push(chip("Duration", format_duration(d)).into_any_element());
        }
        if let Some(s) = item.file_size {
            chips.push(chip("Size", format_bytes(s)).into_any_element());
        }
        if let Some(req) = self.request() {
            chips.push(chip("Provider", req.provider.to_string()).into_any_element());
            match &req.generation_type {
                majik_generation::GenerationType::Image(s) => {
                    chips.push(chip("Aspect", s.aspect_ratio.raw().into()).into_any_element());
                    chips.push(chip("Resolution", s.resolution.raw().into()).into_any_element());
                }
                majik_generation::GenerationType::Video(s) => {
                    if let Some(a) = s.aspect_ratio {
                        chips.push(chip("Aspect", a.raw().into()).into_any_element());
                    }
                    if let Some(r) = s.resolution {
                        chips.push(chip("Resolution", r.display_name().into()).into_any_element());
                    }
                    chips.push(chip("Length", format!("{}s", s.duration)).into_any_element());
                    chips.push(chip("Audio", if s.audio_enabled { "On".into() } else { "Off".into() }).into_any_element());
                }
                majik_generation::GenerationType::Audio(s) => {
                    chips.push(chip("Voice 1", s.speaker1.display_name.clone()).into_any_element());
                    if let Some(v2) = &s.speaker2 {
                        chips.push(chip("Voice 2", v2.display_name.clone()).into_any_element());
                    }
                }
                // Tools have no settings beyond the model, shown by name above.
                majik_generation::GenerationType::Upscale(_) | majik_generation::GenerationType::RemoveBackground(_) => {}
            }
        }
        if item.is_upscaled {
            chips.push(chip("Upscaled", "Yes".into()).into_any_element());
        }
        chips.push(chip("Created", format_date(item.created_at_ms)).into_any_element());

        // The request's input assets: images and video thumbnails as pictures, audio (no player
        // in the sheet) and a video still without a thumbnail as an icon card.
        let assets: Vec<gpui::AnyElement> = self
            .info_assets
            .as_ref()
            .filter(|(id, _)| *id == item.id)
            .map(|(_, assets)| assets.as_slice())
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(k, asset)| {
                let role = majik_providers::AssetRole::from_raw(&asset.role);
                let caption: SharedString = match role {
                    Some(role) => role.display_name().into(),
                    None => asset.path.extension().and_then(|e| e.to_str()).unwrap_or("file").to_uppercase().into(),
                };
                let card = gpui::div().id(("info-asset", k)).relative().flex_none().size(px(96.)).rounded_lg().overflow_hidden().bg(theme.muted);
                let card = match &asset.picture {
                    Some(picture) => card.child(crate::ui::cover_image(picture.clone()).image_cache(&self.images).rounded_lg()),
                    None => {
                        let glyph = match asset.kind {
                            MediaType::Audio => "audio-lines",
                            MediaType::Video => "film",
                            MediaType::Image => "image",
                        };
                        card.child(v_flex().size_full().items_center().justify_center().child(icon(glyph).size_5()))
                    }
                };
                card.child(gpui::div().absolute().bottom_0p5().left_0p5().px_1().rounded_sm().bg(gpui::black().opacity(0.5)).text_xs().text_color(gpui::white()).child(caption)).into_any_element()
            })
            .collect();

        v_flex()
            .w(px(300.))
            .h_full()
            .p_3()
            .gap_3()
            .border_l_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(gpui::div().font_weight(gpui::FontWeight::SEMIBOLD).child("Info"))
            .child(h_flex().flex_wrap().gap_1p5().children(chips))
            .when(!assets.is_empty(), |d| {
                d.child(
                    v_flex()
                        .gap_1()
                        .child(gpui::div().text_xs().text_color(theme.muted_foreground).child("Input"))
                        .child(h_flex().id("info-assets").gap_2().overflow_x_scroll().children(assets)),
                )
            })
            .when(!self.info_uses.is_empty(), |d| {
                // A plain asset: the generations it went into, as their thumbnails.
                let uses = self.info_uses.iter().enumerate().map(|(k, path)| {
                    gpui::div().id(("info-use", k)).relative().flex_none().size(px(96.)).rounded_lg().overflow_hidden().bg(theme.muted).child(crate::ui::cover_image(path.clone()).image_cache(&self.images).rounded_lg()).into_any_element()
                });
                d.child(
                    v_flex()
                        .gap_1()
                        .child(gpui::div().text_xs().text_color(theme.muted_foreground).child(format!("Used in {} generation(s)", self.info_uses.len())))
                        .child(h_flex().id("info-uses").gap_2().overflow_x_scroll().children(uses)),
                )
            })
            .when_some(self.prompt(), |d, prompt| {
                let prompt2 = prompt.clone();
                d.child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(gpui::div().text_xs().text_color(theme.muted_foreground).child("Prompt"))
                                .child(gpui::div().debug_selector(|| "copy-prompt".into()).child(Clipboard::new("copy-prompt").value(prompt2).tooltip("Copy prompt").on_copied(|_, window, cx| crate::ui::toast(window, "Prompt copied", cx)))),
                        )
                        .child(gpui::div().text_sm().child(prompt)),
                )
            })
            .when_some(item.error.clone(), |d, err| d.child(gpui::div().text_sm().text_color(theme.danger).child(err)))
            .child(gpui::div().text_xs().text_color(theme.muted_foreground).child(item.file_name()))
    }
}

impl Render for DetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(subject) = self.subject(cx) else {
            return gpui::div().size_full().into_any_element();
        };
        let is_generation = subject.generation.is_some();
        let subject_asset = subject.asset.clone();
        let item = subject.item;
        // The slide advances before the players sync, so the frame it settles on is the one that
        // hands the player to the item it landed on (`sync_player` waits while it runs).
        let clock = now(cx);
        let mut keep_pumping = false;
        if self.paging.is_animating() {
            self.paging.tick(clock);
            keep_pumping |= self.paging.is_animating();
        }
        self.sync_player(&item, window, cx);
        self.sync_elapsed_ticker(&item, cx);
        self.sync_request(&item);
        self.sync_compare(&item, cx);
        self.sync_info_assets(&item, cx);
        if let Some(a) = &self.audio {
            if a.is_playing() && !a.finished() {
                keep_pumping = true;
            }
        }
        keep_pumping |= self.tick_morph(&item, clock, cx);
        if keep_pumping {
            window.request_animation_frame();
        }
        // How far open the view is: chrome and background fade with it while the travelling
        // box (`frame`) stands in for the image.
        let (progress, frame) = match &self.morph {
            Some(morph) => (morph.progress(clock), morph.frame(clock)),
            None if self.opening => (0.0, self.origin.filter(|_| morphable(&item))),
            None => (1.0, None),
        };
        let morphing = self.morph.is_some() || self.opening;
        let theme = cx.theme();
        let (bg, fg, muted_fg, border) = (theme.background, theme.foreground, theme.muted_foreground, theme.border);
        let (muted, primary) = (theme.muted, theme.primary);
        // The stage is black (white in a light theme), as Photos and most viewers show media; the
        // transparency checkerboard is only for a background removal, where the alpha is the point.
        let (check_a, check_b) = if theme.mode.is_dark() { (gpui::rgb(0x202226).into(), gpui::rgb(0x2b2d32).into()) } else { (gpui::rgb(0xf3f3f5).into(), gpui::rgb(0xe2e2e6).into()) };
        let stage_bg = match item.media_type {
            MediaType::Audio => bg,
            _ if theme.mode.is_dark() => gpui::black(),
            _ => gpui::white(),
        };
        let transparent_result = item.tool == Some(ToolId::RemoveBackground) && item.media_type == MediaType::Image;
        let n = self.ids.len();

        // --- strip of slots: neighbours sit one width to either side, all shifted by the paging offset ---
        // While the box travels it stands in for the strip.
        let area = slot_size(&self.area);
        let width = f32::from(area.width);
        let offset = self.paging.offset();
        let mut slots: Vec<gpui::AnyElement> = Vec::new();
        if frame.is_none() {
            for k in paging::visible_slots(self.index, n, offset, width) {
                let slot_item = if k == self.index { Some(item.clone()) } else { self.subject_at(k, cx).map(|s| s.item) };
                if let Some(slot_item) = slot_item {
                    let x = px((k as f32 - self.index as f32) * width + offset);
                    slots.push(self.render_slot(k, &slot_item, k == self.index, x, window, cx));
                }
            }
        }
        let media = gpui::div().absolute().inset_0().children(slots);

        // The travelling box: the cell's cover-cropped thumbnail, opening up to the image's aspect
        // as the box reaches the stage. The full-size image is decoded meanwhile (invisibly) so it
        // is ready the moment the box arrives. `frame` is in window coordinates (the cell and the
        // stage both are), and the view sits under the window's title bar, so the box is anchored
        // to the window rather than positioned within the view.
        let travelling = frame.and_then(|frame| {
            let thumbnail = item.thumbnail.clone()?;
            let radius = px(4. * (1.0 - progress));
            let prewarm = item.file().map(std::path::Path::to_path_buf).filter(|_| item.media_type == MediaType::Image);
            let card = gpui::div()
                .relative()
                .w(frame.size.width)
                .h(frame.size.height)
                .overflow_hidden()
                .rounded(radius)
                .bg(muted)
                .child(crate::ui::cover_image(thumbnail).image_cache(&self.thumbnails))
                .when_some(prewarm, |d, path| d.child(gpui::img(path).image_cache(&self.images).absolute().top_0().left_0().w(px(1.)).h(px(1.)).opacity(0.)));
            Some(gpui::anchored().snap_to_window().position(frame.origin).child(card))
        });

        // 80 pt hover zones with a 44 pt chevron badge, fading in/out over 120 ms.
        let arrow = |side: &'static str, visible: bool, enabled: bool, window: &mut Window, cx: &mut Context<Self>| {
            let opacity = fade_to(("chevron", side), if visible && enabled { 1.0 } else { 0.0 }, Duration::from_millis(120), window, cx);
            gpui::div()
                .absolute()
                .top_0()
                .bottom_0()
                .w(px(80.))
                .when(side == "left", |d| d.left_0())
                .when(side == "right", |d| d.right_0())
                .when(enabled, |d| d.cursor_pointer())
                .flex()
                .items_center()
                .justify_center()
                .when(opacity > 0.0, |d| {
                    d.child(
                        gpui::div()
                            .debug_selector(|| format!("chevron-{side}"))
                            .size(px(44.))
                            .rounded_full()
                            .bg(bg.opacity(0.65))
                            .border_1()
                            .border_color(border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .opacity(opacity)
                            .child(icon(if side == "left" { "circle-arrow-left" } else { "circle-arrow-right" }).size_6().text_color(fg)),
                    )
                })
        };

        // The item's controls and, apart from them, the close button float at the stage's
        // top-right, each in its own pill; the model and date live in the info panel.
        let image_controls = floating(bg, border)
            .debug_selector(|| "image-controls".into())
            .when(is_generation, |t| {
                t.child(floating_button("fav", if item.is_favorite { "heart-filled" } else { "heart" }).tooltip_with_action("Favorite", &ToggleFavorite, Some("Detail")).on_click(cx.listener(|this, _, w, cx| this.toggle_favorite(&ToggleFavorite, w, cx))))
            })
            .child(
                floating_button(
                    "save",
                    match self.save_state {
                        SaveState::Saved => "check",
                        SaveState::Failed => "x",
                        SaveState::Idle | SaveState::Saving => "download",
                    },
                )
                    .loading(self.save_state == SaveState::Saving)
                    .loading_icon(icon("loader-circle"))
                    .disabled(self.save_state != SaveState::Idle || item.status != Status::Completed)
                    .tooltip_with_action("Save…", &SaveMedia, Some("Detail"))
                    .on_click(cx.listener(|this, _, w, cx| this.save(&SaveMedia, w, cx))),
            )
            .child(floating_button("info", "info").selected(self.show_info).tooltip_with_action("Info", &ShowInfo, Some("Detail")).on_click(cx.listener(|this, _, w, cx| this.show_info(&ShowInfo, w, cx))))
            .when(is_generation, |t| {
                t.child(floating_button("recreate", "refresh-cw").tooltip_with_action("Recreate", &Recreate, Some("Detail")).on_click(cx.listener(|this, _, w, cx| this.recreate(&Recreate, w, cx))))
            })
            .child(floating_button("more", "ellipsis").dropdown_menu({
                let this = cx.weak_entity();
                let can_recreate = item.can_recreate();
                let has_prompt = self.prompt().is_some();
                let has_file = item.file().is_some();
                let deletable = is_generation || subject_asset.as_ref().is_some_and(|a| !self.library.read(cx).lib.is_referenced(a));
                move |menu, _, _| {
                    let mk = |label: &'static str, f: fn(&mut DetailView, &mut Window, &mut Context<DetailView>), this: gpui::WeakEntity<DetailView>, enabled: bool| {
                        PopupMenuItem::new(label).disabled(!enabled).on_click(move |_, window, cx| {
                            this.update(cx, |v, cx| f(v, window, cx)).ok();
                        })
                    };
                    // Action-backed entries render their key binding; Copy Prompt has none. A plain
                    // asset gets the file actions only: it is dragged into the composer, not "used".
                    let menu = menu.menu_with_disabled("Copy", Box::new(CopyMedia), !has_file);
                    let menu = if is_generation {
                        menu.item(mk("Copy Prompt", |v, w, cx| {
                            if let Some(p) = v.item(cx).and_then(|i| i.prompt()) {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(p));
                                crate::ui::toast(w, "Prompt copied", cx);
                            }
                        }, this.clone(), has_prompt))
                            .menu_with_disabled("Recreate", Box::new(Recreate), !can_recreate)
                    } else {
                        menu
                    };
                    menu.separator().menu_with_disabled("Delete", Box::new(DeleteMedia), !deletable)
                }
            }));
        let close_control = floating(bg, border)
            .debug_selector(|| "close-control".into())
            .child(floating_button("close", "close").tooltip_with_action("Close", &Back, Some("Detail")).on_click(cx.listener(|this, _, w, cx| this.back(&Back, w, cx))));
        let top_right = (!morphing).then(|| h_flex().absolute().top_3().right_3().gap_2().child(image_controls).child(close_control));

        let area_slot = self.area.clone();
        let can_prev = self.index > 0;
        let can_next = self.index + 1 < n;
        let arrows: Vec<gpui::AnyElement> = if morphing {
            Vec::new()
        } else {
            vec![
                arrow("left", self.hover_left, can_prev, window, cx)
                    .id("prev-zone")
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.hover_left = *hovered;
                        cx.notify();
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.go(-1, cx)))
                    .into_any_element(),
                arrow("right", self.hover_right, can_next, window, cx)
                    .id("next-zone")
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.hover_right = *hovered;
                        cx.notify();
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.go(1, cx)))
                    .into_any_element(),
            ]
        };
        // flarly's bottom-left zoom row: fit-relative presets over the stage, for media with a
        // known pixel size.
        let zoom_row = (!morphing && item.status == Status::Completed && self.image_pixel_size(cx).is_some()).then(|| {
            let active = self.active_zoom_preset(window, cx);
            floating(bg, border)
                .debug_selector(|| "zoom-presets".into())
                .absolute()
                .bottom_3()
                .left_3()
                // The glyph takes a control's slot so the pill matches the top-right ones.
                .child(gpui::div().size_6().flex().items_center().justify_center().child(icon("zoom-in").size_4().text_color(muted_fg)))
                .children(ZOOM_PRESETS.into_iter().enumerate().map(|(i, preset)| {
                    let label: SharedString = format!("{preset}×").into();
                    gpui::div().debug_selector(move || format!("zoom-{preset}")).child(
                        button(("zoom-preset", i))
                            .label(label)
                            .ghost()
                            .small()
                            .rounded(ButtonRounded::Size(px(12.)))
                            .selected(active == Some(preset))
                            .on_click(cx.listener(move |this, _, window, cx| this.zoom_preset(preset, window, cx))),
                    )
                }))
        });

        let stage = gpui::div()
            .id("detail-stage")
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .overflow_hidden()
            .bg(stage_bg)
            .when(transparent_result, |d| d.child(gpui::div().debug_selector(|| "checkerboard".into()).absolute().inset_0().child(checkerboard(check_a, check_b))))
            .child(gpui::div().absolute().inset_0().child(measure(area_slot, cx.weak_entity())))
            .child(media)
            .when_some(
                if self.zoom.is_none() && self.compare_original().is_some() { Some(CursorStyle::ResizeLeftRight) } else { stage_cursor(self.zoom.is_some(), self.drag_start.is_some()) },
                |d, cursor| d.cursor(cursor),
            )
            .children(arrows)
            .when_some(zoom_row, |d, row| d.child(row))
            .when_some(top_right, |d, controls| d.child(controls))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, window, cx| {
                let origin = this.area.borrow().origin;
                this.on_scroll(ev, origin, window, cx);
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                if this.divider_drag {
                    this.drag_divider_to(ev.position.x, window, cx);
                } else if let Some((start, pan0)) = this.drag_start {
                    this.pan = point(pan0.x + ev.position.x - start.x, pan0.y + ev.position.y - start.y);
                    let scale = this.current_scale(window, cx);
                    this.clamp_pan(scale, cx);
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    // At fit a body drag moves the before/after divider; zoomed in, it pans.
                    if this.zoom.is_some() {
                        this.drag_start = Some((ev.position, this.pan));
                    } else if this.compare_original().is_some() {
                        this.divider_drag = true;
                        this.drag_divider_to(ev.position.x, window, cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.drag_start = None;
                    this.divider_drag = false;
                }),
            )
            .on_mouse_down(MouseButton::Navigate(NavigationDirection::Back), cx.listener(|this, _: &MouseDownEvent, _, cx| this.go(-1, cx)))
            .on_mouse_down(MouseButton::Navigate(NavigationDirection::Forward), cx.listener(|this, _: &MouseDownEvent, _, cx| this.go(1, cx)));

        let controls = self.transport_for(&item).map(|(pos, dur, playing)| {
            let frac = if dur > 0.0 { (pos / dur) as f32 } else { 0.0 };
            h_flex()
                .h(px(40.))
                .px_3()
                .gap_3()
                .items_center()
                .border_t_1()
                .border_color(border)
                .child(button("play").icon(icon(if playing { "pause" } else { "play" })).ghost().small().tooltip_with_action("Play / Pause", &TogglePlayback, Some("Detail")).on_click(cx.listener(|this, _, w, cx| this.toggle_playback(&TogglePlayback, w, cx))))
                .child(gpui::div().text_xs().text_color(muted_fg).w(px(44.)).child(format_duration(pos)))
                .child(
                    gpui::div()
                        .id("scrub")
                        .flex_1()
                        .h(px(16.))
                        .relative()
                        .cursor_pointer()
                        .child(gpui::div().absolute().inset_0().child(measure(self.scrub.clone(), cx.weak_entity())))
                        .child(gpui::div().absolute().left_0().right_0().top(px(6.)).h(px(4.)).rounded_full().bg(muted))
                        .child(gpui::div().absolute().left_0().top(px(6.)).h(px(4.)).w(gpui::relative(frac.clamp(0.0, 1.0))).rounded_full().bg(primary))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                let b = *this.scrub.borrow();
                                if b.size.width > px(0.) {
                                    let frac = f32::from(ev.position.x - b.origin.x) / f32::from(b.size.width);
                                    this.seek_fraction(frac, window, cx);
                                }
                            }),
                        ),
                )
                .child(gpui::div().text_xs().text_color(muted_fg).w(px(44.)).child(format_duration(dur)))
        });
        let stage_column = gpui::div().flex().flex_col().flex_1().min_w_0().h_full().child(stage).children(controls);
        let body = gpui::div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .w_full()
            .opacity(progress)
            .child(stage_column)
            .when(self.show_info && !morphing, |d| d.child(self.render_info(&item, cx)));

        gpui::div()
            .id("detail")
            .key_context("Detail")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(bg.opacity(progress))
            .text_color(fg)
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::prev))
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::show_info))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::recreate))
            .on_action(cx.listener(Self::retry))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::toggle_playback))
            .child(body)
            .children(travelling)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::morph::{self, Shape};
    use crate::test_support::{env, seed_asset, seed_item, seed_request, Seed};
    use gpui::TestAppContext;
    use majik_generation::{GenerationType, Request};
    use majik_providers::{catalog, AspectRatio, ImageGenerationSettings, ImageResolution, ProviderId};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn all_ids(env: &crate::test_support::TestEnv, cx: &mut gpui::VisualTestContext) -> Vec<GenerationId> {
        env.library.read_with(cx, |m, _| m.lib.feed(&majik_core::FeedFilter::Library, majik_core::MediaFilter::All))
    }

    macro_rules! detail_window {
        ($cx:ident, $n:expr, $index:expr) => {{
            let env = env($cx, $n, "Mock");
            let (_view, vcx) = $cx.add_window_view(|window, cx| {
                let _ = window;
                crate::views::feed::FeedView::new(window, cx)
            });
            vcx.run_until_parked();
            let ids = all_ids(&env, vcx);
            let thumbnails = _view.read_with(vcx, |feed, _| feed.image_cache());
            let entries: Vec<EntryId> = ids.iter().cloned().map(EntryId::Generation).collect();
            let detail = vcx.new(|cx| DetailView::new(entries, $index, None, thumbnails, cx));
            vcx.run_until_parked();
            (detail, vcx, env, ids)
        }};
    }

    /// A detail that is the window's own root view, so `render` actually runs and the stage decodes
    /// its images. The `detail_window!` above drives view logic with the feed as the root, which
    /// never paints the detail.
    fn drawn_detail(cx: &mut TestAppContext, items: usize) -> (Entity<DetailView>, &mut gpui::VisualTestContext, crate::test_support::TestEnv) {
        let env = env(cx, items, "Mock");
        let ids = env.library.read_with(cx, |m, _| m.lib.feed(&majik_core::FeedFilter::Library, majik_core::MediaFilter::All));
        let entries: Vec<EntryId> = ids.into_iter().map(EntryId::Generation).collect();
        let (detail, vcx) = cx.add_window_view(|_window, cx| {
            let thumbnails = LruImageCache::new(cx);
            DetailView::new(entries, 0, None, thumbnails, cx)
        });
        vcx.run_until_parked();
        (detail, vcx, env)
    }

    /// Paging never grew a bound before: full-size images went to gpui's global cache, which evicts
    /// nothing, so every image paged past stayed decoded (a 4K one is ~59 MB) until the app exited.
    #[gpui::test]
    fn paging_through_images_keeps_the_full_size_cache_bounded(cx: &mut TestAppContext) {
        let (detail, vcx, _env) = drawn_detail(cx, 12);
        let images = detail.read_with(vcx, |d, _| d.images.clone());
        // The shipping budget holds five 4K images; the seeded ones are 64 px, so ask for three.
        let budget = 3 * 64 * 64 * 4;
        images.update(vcx, |cache, _| cache.set_budget(budget));

        for _ in 0..11 {
            detail.update_in(vcx, |d, window, cx| d.next(&crate::actions::NextItem, window, cx));
            vcx.run_until_parked();
            vcx.update(|window, cx| window.simulate_next_frame(cx));
            vcx.run_until_parked();
            let (bytes, budget) = images.read_with(vcx, |cache, _| (cache.bytes(), cache.budget()));
            assert!(bytes <= budget, "{bytes} bytes held against a {budget} budget while paging");
        }

        let (bytes, held) = images.read_with(vcx, |cache, _| (cache.bytes(), cache.len()));
        assert!(bytes > 0, "the stage did decode the images it drew");
        assert!(held < 12, "paging the whole set kept only the recent images, not all 12 ({held})");
    }

    /// The stage's images and the feed's thumbnails are separate caches on purpose: one 4K image is
    /// worth a hundred thumbnails, so sharing one would flush the grid every time an item is opened.
    #[gpui::test]
    fn the_stage_does_not_spend_the_feeds_thumbnail_budget(cx: &mut TestAppContext) {
        let (detail, vcx, _env) = drawn_detail(cx, 6);
        for _ in 0..5 {
            detail.update_in(vcx, |d, window, cx| d.next(&crate::actions::NextItem, window, cx));
            vcx.run_until_parked();
            vcx.update(|window, cx| window.simulate_next_frame(cx));
            vcx.run_until_parked();
        }
        let (images, thumbnails) = detail.read_with(vcx, |d, _| (d.images.clone(), d.thumbnails.clone()));
        assert!(images.read_with(vcx, |cache, _| cache.bytes()) > 0, "the stage filled its own cache");
        assert_eq!(thumbnails.read_with(vcx, |cache, _| cache.bytes()), 0, "and left the thumbnail cache alone");
    }

    /// A detail opened on a freshly seeded video, with the player synced the way `render` does it
    /// and the first frame decoded.
    macro_rules! video_detail {
        ($cx:ident, $seed:expr) => {{
            let (detail, vcx, env, _ids) = detail_window!($cx, 1, 0);
            let video = seed_item(&env.library, vcx, Seed { media_type: MediaType::Video, ..$seed });
            vcx.run_until_parked();
            let ids = all_ids(&env, vcx);
            let index = ids.iter().position(|i| i == &video).expect("video is in the feed");
            let thumbnails = detail.read_with(vcx, |d, _| d.thumbnails.clone());
            let detail = vcx.new(|cx| DetailView::new(ids.into_iter().map(EntryId::Generation).collect(), index, None, thumbnails, cx));
            sync_player(&detail, vcx);
            (detail, vcx, env, video)
        }};
    }

    /// What `render` does on every frame: keep the player matched to the current item.
    fn sync_player(detail: &Entity<DetailView>, vcx: &mut gpui::VisualTestContext) {
        detail.update_in(vcx, |d, window, cx| {
            let item = d.item(cx).unwrap();
            d.sync_player(&item, window, cx);
        });
        vcx.run_until_parked();
    }

    fn shown_pts(detail: &Entity<DetailView>, vcx: &mut gpui::VisualTestContext) -> Option<f64> {
        detail.read_with(vcx, |d, _| d.player.as_ref().and_then(|p| p.frame()).map(|f| f.pts_secs))
    }

    #[gpui::test]
    fn video_opens_paused_with_a_ready_player_and_first_frame(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _video) = video_detail!(cx, Seed::default());
        detail.read_with(vcx, |d, cx| {
            let player = d.player.as_ref().expect("player opened");
            assert!(!player.is_playing(), "never autoplays");
            assert_eq!(d.transport(), Some((0.0, 2.0, false)));
            assert!(d.frame_image.is_some(), "first frame is up while paused");
            assert!(d.pump.is_none(), "nothing left to decode while paused");
            assert!(d.player_error.is_none());
            assert_eq!(d.image_pixel_size(cx), Some((64.0, 64.0)), "zoom uses the decoded size");
        });
        assert_eq!(shown_pts(&detail, vcx), Some(0.0));
    }

    #[gpui::test(iterations = 5)]
    fn space_plays_and_frames_advance_with_the_clock(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _video) = video_detail!(cx, Seed::default());
        detail.update_in(vcx, |d, window, cx| d.toggle_playback(&TogglePlayback, window, cx));
        vcx.run_until_parked();
        assert!(detail.read_with(vcx, |d, _| d.player.as_ref().unwrap().is_playing() && d.pump.is_some()));

        vcx.background_executor.advance_clock(Duration::from_millis(1100));
        vcx.run_until_parked();
        assert_eq!(shown_pts(&detail, vcx), Some(1.0));
        let position = detail.read_with(vcx, |d, _| d.transport().unwrap().0);
        assert!((position - 1.1).abs() < 1e-6, "{position}");

        detail.update_in(vcx, |d, window, cx| d.toggle_playback(&TogglePlayback, window, cx));
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(Duration::from_secs(5));
        vcx.run_until_parked();
        detail.read_with(vcx, |d, _| {
            assert_eq!(d.transport(), Some((1.1, 2.0, false)), "paused: position holds");
            assert!(d.pump.is_none(), "pump ends once paused with the right frame up");
        });
        assert_eq!(shown_pts(&detail, vcx), Some(1.0));
    }

    #[gpui::test]
    fn scrubber_seeks_while_paused_and_shows_that_frame(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _video) = video_detail!(cx, Seed::default());
        detail.update_in(vcx, |d, window, cx| d.seek_fraction(0.5, window, cx));
        assert_eq!(detail.read_with(vcx, |d, _| d.transport()), Some((1.0, 2.0, false)));
        vcx.run_until_parked();
        assert_eq!(shown_pts(&detail, vcx), Some(1.0), "the sought frame is decoded without playing");
        detail.update_in(vcx, |d, window, cx| d.seek_fraction(2.0, window, cx));
        assert_eq!(detail.read_with(vcx, |d, _| d.transport()), Some((2.0, 2.0, false)), "clamped");
    }

    #[gpui::test]
    fn playback_loops_back_to_zero(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _video) = video_detail!(cx, Seed::default());
        detail.update_in(vcx, |d, window, cx| d.toggle_playback(&TogglePlayback, window, cx));
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(Duration::from_millis(2100));
        vcx.run_until_parked();
        detail.read_with(vcx, |d, _| {
            let (position, _, playing) = d.transport().unwrap();
            assert!(playing);
            assert!(position < 1.0, "wrapped, got {position}");
        });
        assert_eq!(shown_pts(&detail, vcx), Some(0.0));
    }

    #[gpui::test]
    fn leaving_the_item_drops_player_pump_and_frame(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _video) = video_detail!(cx, Seed::default());
        detail.update_in(vcx, |d, window, cx| d.toggle_playback(&TogglePlayback, window, cx));
        vcx.run_until_parked();
        detail.update(vcx, |d, cx| {
            let other = d.ids.iter().position(|id| id != &EntryId::Generation(_video.clone())).expect("the seeded image");
            d.go(other as isize - d.index as isize, cx);
        });
        sync_player(&detail, vcx);
        detail.read_with(vcx, |d, cx| {
            assert_eq!(d.item(cx).unwrap().media_type, MediaType::Image);
            assert!(d.player.is_none() && d.pump.is_none() && d.frame_image.is_none() && d.player_for.is_none());
        });
    }

    #[gpui::test]
    fn transport_bar_is_there_before_the_player_is_open(cx: &mut TestAppContext) {
        let (detail, vcx, env, _ids) = detail_window!(cx, 1, 0);
        let video = seed_item(&env.library, vcx, Seed { media_type: MediaType::Video, ..Seed::default() });
        // What the model's completion probe fills in for a real generation.
        env.library.update(vcx, |m, _| m.lib.set_media_info(&video, Some(64), Some(64), Some(2.0)));
        vcx.run_until_parked();
        let ids = all_ids(&env, vcx);
        let index = ids.iter().position(|i| i == &video).expect("video is in the feed");
        let thumbnails = detail.read_with(vcx, |d, _| d.thumbnails.clone());
        let detail = vcx.new(|cx| DetailView::new(ids.into_iter().map(EntryId::Generation).collect(), index, None, thumbnails, cx));
        detail.update_in(vcx, |d, window, cx| {
            let item = d.item(cx).unwrap();
            d.sync_player(&item, window, cx);
            assert!(d.player.is_none(), "the source is still opening");
            assert_eq!(d.transport_for(&item), Some((0.0, 2.0, false)), "a paused bar over the stored duration meanwhile");
        });
        vcx.run_until_parked();
        detail.read_with(vcx, |d, cx| {
            assert!(d.player.is_some());
            assert_eq!(d.transport_for(&d.item(cx).unwrap()), Some((0.0, 2.0, false)), "then the player's own");
        });
        detail.update(vcx, |d, cx| {
            let other = d.ids.iter().position(|id| id != &EntryId::Generation(video.clone())).expect("the seeded image");
            d.go(other as isize - d.index as isize, cx);
            assert_eq!(d.transport_for(&d.item(cx).unwrap()), None, "an image has no bar, whoever holds the player");
        });
    }

    #[gpui::test]
    fn a_slide_keeps_the_leaving_videos_player_until_it_settles(cx: &mut TestAppContext) {
        // A 32 px image beside the 64 px clip, so the fitted size tells whose it is.
        let (detail, vcx, env, _ids) = detail_window!(cx, 0, 0);
        let image = seed_item(&env.library, vcx, Seed::default());
        let video = seed_item(&env.library, vcx, Seed { media_type: MediaType::Video, ..Seed::default() });
        vcx.run_until_parked();
        let ids = all_ids(&env, vcx);
        let index = ids.iter().position(|i| i == &video).expect("video is in the feed");
        let thumbnails = detail.read_with(vcx, |d, _| d.thumbnails.clone());
        let detail = vcx.new(|cx| DetailView::new(ids.into_iter().map(EntryId::Generation).collect(), index, None, thumbnails, cx));
        sync_player(&detail, vcx);
        let other = detail.read_with(vcx, |d, _| d.ids.iter().position(|id| *id == EntryId::Generation(image.clone())).expect("the seeded image"));
        detail.update_in(vcx, |d, window, cx| {
            let delta = other as isize - d.index as isize;
            d.go(delta, cx);
            // An unmeasured (headless) stage jumps, so start the slide a measured one would have.
            d.paging.navigate(if delta > 0 { paging::Step::Next } else { paging::Step::Prev }, 800.0, now(cx));
            let item = d.item(cx).unwrap();
            assert_eq!(item.media_type, MediaType::Image);
            d.sync_player(&item, window, cx);
            assert_eq!(d.player_for, Some(video.clone()), "mid-slide the leaving video keeps its player");
            assert!(d.player.is_some() && d.frame_image.is_some(), "… and its picture");
            assert_eq!(d.image_pixel_size(cx), Some((32.0, 32.0)), "the arriving image is fitted by its own size, not the player's");
            assert_eq!(d.transport_for(&item), None);
        });
        detail.update_in(vcx, |d, window, cx| {
            // Settle the slide the way the render loop does, a frame at a time.
            let mut clock = now(cx);
            while d.paging.is_animating() {
                clock += Duration::from_millis(16);
                d.paging.tick(clock);
            }
            let item = d.item(cx).unwrap();
            d.sync_player(&item, window, cx);
            assert!(d.player.is_none() && d.frame_image.is_none() && d.player_for.is_none(), "dropped once the slide has settled");
        });
    }

    #[gpui::test]
    fn unsupported_video_shows_error_chip_and_no_player(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _video) = video_detail!(cx, Seed { bytes: Some(crate::test_support::unsupported_clip()), ..Seed::default() });
        detail.read_with(vcx, |d, _| {
            assert!(d.player.is_none());
            assert_eq!(d.player_error.as_deref(), Some("unsupported codec (zvc9)"));
        });
    }

    #[gpui::test]
    fn corrupt_video_reports_an_error_instead_of_a_player(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _video) = video_detail!(cx, Seed { bytes: Some(b"not really media"), ..Seed::default() });
        detail.read_with(vcx, |d, _| {
            assert!(d.player.is_none());
            assert!(d.player_error.as_deref().is_some_and(|e| e.starts_with("invalid video file")), "{:?}", d.player_error);
        });
    }

    /// Record every event the detail emits, as names.
    fn record_events(detail: &Entity<DetailView>, vcx: &mut gpui::VisualTestContext) -> Rc<RefCell<Vec<&'static str>>> {
        let events: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        vcx.update(|_, cx| {
            cx.subscribe(detail, move |_, ev: &DetailEvent, _| {
                sink.borrow_mut().push(match ev {
                    DetailEvent::WillClose { .. } => "will_close",
                    DetailEvent::Close => "close",
                    DetailEvent::Compose(_) => "compose",
                });
            })
            .detach();
        });
        events
    }

    #[gpui::test]
    fn save_button_cycles_saving_saved_idle(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 2, 0);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.png");
        detail.update_in(vcx, |d, w, cx| d.save(&SaveMedia, w, cx));
        detail.update(vcx, |d, _| assert_eq!(d.save_state, SaveState::Saving));
        // A second press while the panel is up is ignored (the button is disabled anyway).
        detail.update_in(vcx, |d, w, cx| d.save(&SaveMedia, w, cx));
        let chosen = dest.clone();
        let home = directories::BaseDirs::new().unwrap().home_dir().to_path_buf();
        vcx.simulate_new_path_selection(move |directory| {
            assert_eq!(directory, home, "nothing saved yet, so the panel opens in the home folder");
            assert!(directory.is_absolute(), "resolved on this platform, not from $HOME");
            Some(chosen)
        });
        vcx.run_until_parked();
        detail.update(vcx, |d, _| assert_eq!(d.save_state, SaveState::Saved));
        assert!(dest.exists(), "file copied");
        vcx.background_executor.advance_clock(SAVE_STATE_RESET);
        vcx.run_until_parked();
        detail.update(vcx, |d, _| assert_eq!(d.save_state, SaveState::Idle));
    }

    #[gpui::test]
    fn save_failure_shows_x_then_reverts_and_cancel_is_idle(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 2, 0);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("missing").join("out.png");
        detail.update_in(vcx, |d, w, cx| d.save(&SaveMedia, w, cx));
        vcx.simulate_new_path_selection(move |_| Some(dest));
        vcx.run_until_parked();
        detail.update(vcx, |d, _| assert_eq!(d.save_state, SaveState::Failed));
        vcx.background_executor.advance_clock(SAVE_STATE_RESET);
        vcx.run_until_parked();
        detail.update(vcx, |d, _| assert_eq!(d.save_state, SaveState::Idle));
        // Dismissing the panel goes straight back to idle.
        detail.update_in(vcx, |d, w, cx| d.save(&SaveMedia, w, cx));
        vcx.simulate_new_path_selection(|_| None);
        vcx.run_until_parked();
        detail.update(vcx, |d, _| assert_eq!(d.save_state, SaveState::Idle));
    }

    #[gpui::test]
    fn generating_item_runs_elapsed_ticker(cx: &mut TestAppContext) {
        let (detail, vcx, env, _ids) = detail_window!(cx, 1, 0);
        let generating = env.library.update(vcx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Image, None, None, None, None);
            m.changed(cx);
            id
        });
        let (done, generating_item) = env.library.read_with(vcx, |m, _| (m.lib.get(&_ids[0]).cloned().unwrap(), m.lib.get(&generating).cloned().unwrap()));
        detail.update(vcx, |d, cx| {
            d.sync_elapsed_ticker(&generating_item, cx);
            assert!(d.elapsed_ticker.is_some());
            d.sync_elapsed_ticker(&done, cx);
            assert!(d.elapsed_ticker.is_none(), "dropped once the item is no longer generating");
        });
    }

    #[gpui::test]
    fn missing_item_shows_no_player_and_regenerates_in_place(cx: &mut TestAppContext) {
        let (detail, vcx, env, _ids) = detail_window!(cx, 1, 0);
        let missing = seed_item(&env.library, vcx, Seed { status: Status::Missing, ..Seed::default() });
        vcx.run_until_parked();
        let ids = all_ids(&env, vcx);
        let index = ids.iter().position(|i| i == &missing).expect("missing item stays in the feed");
        let thumbnails = detail.read_with(vcx, |d, _| d.thumbnails.clone());
        let detail = vcx.new(|cx| DetailView::new(ids.into_iter().map(EntryId::Generation).collect(), index, None, thumbnails, cx));
        vcx.run_until_parked();
        detail.update_in(vcx, |d, window, cx| {
            let item = d.item(cx).unwrap();
            assert_eq!(item.status, Status::Missing);
            d.sync_player(&item, window, cx);
            assert!(d.player.is_none() && d.audio.is_none() && d.player_error.is_none());
        });
        detail.update_in(vcx, |d, w, cx| d.retry(&Retry, w, cx));
        vcx.run_until_parked();
        env.library.read_with(vcx, |m, _| assert_eq!(m.lib.get(&missing).unwrap().status, Status::Generating, "regenerated under the same id"));
    }

    fn attach_png(env: &crate::test_support::TestEnv, vcx: &mut gpui::VisualTestContext, id: &GenerationId, role: &str, seed: usize) {
        let png = majik_core::images::solid_png(8, 8, [seed as u8 * 40, 80, 160]);
        env.library.update(vcx, |m, _| {
            let asset = m.lib.import_asset("image/png", &png).expect("asset stored");
            m.lib.attach_inputs(id, &[(asset, role)]).expect("input linked");
        });
    }

    /// What `render` does on every frame for the info panel: load the current item's assets.
    fn info_assets(detail: &Entity<DetailView>, vcx: &mut gpui::VisualTestContext) -> Vec<InfoAsset> {
        detail.update(vcx, |d, cx| {
            let item = d.item(cx).unwrap();
            d.sync_info_assets(&item, cx);
            d.info_assets.as_ref().filter(|(id, _)| *id == item.id).map(|(_, a)| a.clone()).unwrap_or_default()
        })
    }

    #[gpui::test]
    fn info_panel_lists_the_items_input_assets_in_order(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 1, 0);
        attach_png(&env, vcx, &ids[0], "last_frame", 1);
        attach_png(&env, vcx, &ids[0], "first_frame", 0);
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        let assets = info_assets(&detail, vcx);
        let roles: Vec<&str> = assets.iter().map(|a| a.role.as_str()).collect();
        assert_eq!(roles, ["first_frame", "last_frame"], "role order, not attach order");
        assert!(assets.iter().all(|a| a.picture.as_ref().is_some_and(|p| p.is_file())), "every strip image is a stored asset file");
    }

    #[gpui::test]
    fn info_panel_shows_a_video_input_by_its_thumbnail(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 1, 0);
        let clip = seed_asset(&env.library, vcx, MediaType::Video, 1);
        env.library.update(vcx, |m, _| m.lib.attach_inputs(&ids[0], &[(clip.clone(), "reference_video")]).expect("input linked"));
        vcx.run_until_parked();
        let asset = env.library.read_with(vcx, |m, _| m.lib.asset(&clip).cloned().unwrap());
        let thumbnail = asset.thumbnail.clone().expect("the import rendered a thumbnail");
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        let assets = info_assets(&detail, vcx);
        assert_eq!(assets.len(), 1);
        assert_eq!((assets[0].kind, assets[0].picture.as_deref()), (MediaType::Video, Some(thumbnail.as_path())), "the card draws the thumbnail, never the mp4");
        assert_eq!(assets[0].path, asset.path, "the caption still knows the file");
    }

    #[gpui::test]
    fn info_panel_shows_an_audio_input_as_an_icon_card(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 1, 0);
        let sound = seed_asset(&env.library, vcx, MediaType::Audio, 1);
        env.library.update(vcx, |m, _| m.lib.attach_inputs(&ids[0], &[(sound, "audio")]).expect("input linked"));
        vcx.run_until_parked();
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        let assets = info_assets(&detail, vcx);
        assert_eq!(assets.iter().map(|a| (a.kind, a.picture.is_none())).collect::<Vec<_>>(), [(MediaType::Audio, true)]);
    }

    #[gpui::test]
    fn info_panel_shows_no_strip_without_assets(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 1, 0);
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        assert!(info_assets(&detail, vcx).is_empty());
    }

    #[gpui::test]
    fn info_assets_are_not_loaded_while_the_panel_is_closed(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 1, 0);
        attach_png(&env, vcx, &ids[0], "reference_image", 0);
        assert!(info_assets(&detail, vcx).is_empty(), "closed panel: nothing read from the database");
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        assert_eq!(info_assets(&detail, vcx).len(), 1);
    }

    #[gpui::test]
    fn info_assets_follow_the_current_item(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 2, 0);
        attach_png(&env, vcx, &ids[1], "reference_image", 0);
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        assert!(info_assets(&detail, vcx).is_empty(), "first item has no inputs");
        detail.update(vcx, |d, cx| d.go(1, cx));
        assert_eq!(info_assets(&detail, vcx).len(), 1, "second item's strip replaces the cached first");
        detail.update(vcx, |d, cx| d.go(-1, cx));
        assert!(info_assets(&detail, vcx).is_empty());
    }

    #[gpui::test]
    fn info_assets_reload_when_the_library_changes(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 1, 0);
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        assert!(info_assets(&detail, vcx).is_empty());
        attach_png(&env, vcx, &ids[0], "reference_image", 0);
        env.library.update(vcx, |m, cx| m.changed(cx));
        vcx.run_until_parked();
        assert_eq!(info_assets(&detail, vcx).len(), 1, "the library observer drops the stale cache");
    }

    // ----- assets as subjects -----

    /// A detail over `entries` as the window's root, drawn and past its open morph (while it plays,
    /// paging and the info panel wait).
    fn detail_over(cx: &mut TestAppContext, entries: Vec<EntryId>, index: usize) -> (Entity<DetailView>, &mut gpui::VisualTestContext) {
        let (detail, vcx) = cx.add_window_view(|_, cx| {
            let thumbnails = LruImageCache::new(cx);
            DetailView::new(entries, index, None, thumbnails, cx)
        });
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(morph::FADE_DURATION + Duration::from_millis(50));
        redraw(&detail, vcx);
        (detail, vcx)
    }

    #[gpui::test]
    fn an_import_shows_file_facts_and_where_it_was_used_without_generation_chrome(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let import = crate::test_support::seed_asset(&env.library, cx, MediaType::Image, 4);
        // A generation that used it as a reference (its bytes never leave the library).
        let generated = env.library.update(cx, |m, cx| {
            let request = Request::new(ProviderId::mock(), GenerationType::Image(ImageGenerationSettings { model: catalog::image::ALL[0].clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd }), "from ref", vec![]);
            let ids = m.generate(vec![request], &[(import.clone(), majik_providers::AssetRole::ReferenceImage)], None, cx);
            m.lib.complete_generation(&ids[0], &majik_core::images::solid_png(2, 2, [9, 9, 9]), false).unwrap();
            ids[0].clone()
        });
        let (detail, vcx) = detail_over(cx, vec![EntryId::Asset(import.clone())], 0);
        detail.update_in(vcx, |d, w, cx| {
            let subject = d.subject(cx).unwrap();
            assert!(subject.generation.is_none() && subject.asset == Some(import.clone()));
            let item = subject.item;
            assert_eq!(item.status, Status::Completed);
            assert!(item.model_name.is_none() && item.request_json.is_none() && !item.can_recreate() && !item.can_retry());
            assert_eq!((item.width, item.height), (Some(5), Some(5)), "file facts come from the asset");
            assert!(item.file().is_some(), "the stage and the export actions have a file");
            d.show_info(&ShowInfo, w, cx);
            d.sync_info_assets(&item, cx);
            assert!(d.info_assets.as_ref().unwrap().1.is_empty(), "an import has no inputs of its own");
            assert_eq!(d.info_uses.len(), 1, "… but it went into one generation");
            // Generation-only actions are inert.
            d.toggle_favorite(&ToggleFavorite, w, cx);
            d.retry(&Retry, w, cx);
        });
        env.library.read_with(vcx, |m, _| assert!(!m.lib.get(&generated).unwrap().is_favorite, "favouriting a plain asset touches nothing"));
        let before = vcx.update(|_, cx| crate::ui::toast_generation(cx));
        detail.update_in(vcx, |d, w, cx| d.recreate(&Recreate, w, cx));
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), before + 1, "recreate has nothing to replay");
    }

    #[gpui::test]
    fn recreate_hands_the_rows_id_to_the_composer_tools_included(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let id = seed_item(&env.library, cx, Seed { upscaled: true, ..Seed::default() });
        let (detail, vcx) = detail_over(cx, vec![EntryId::Generation(id.clone())], 0);
        let handed: Rc<RefCell<Vec<PendingCompose>>> = Default::default();
        let h = handed.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&detail, move |_, ev: &DetailEvent, _| {
                if let DetailEvent::Compose(pending) = ev {
                    h.borrow_mut().push(pending.clone());
                }
            })
            .detach();
        });
        detail.update(vcx, |d, cx| assert!(d.item(cx).unwrap().can_recreate(), "an upscale row recreates too"));
        detail.update_in(vcx, |d, w, cx| d.recreate(&Recreate, w, cx));
        vcx.run_until_parked();
        assert_eq!(handed.borrow().iter().map(|p| p.recreate.clone()).collect::<Vec<_>>(), vec![Some(id)]);
    }

    /// An upscale row over an imported image, as the composer's Upscale tab leaves it.
    fn upscaled(env: &crate::test_support::TestEnv, cx: &mut TestAppContext) -> (GenerationId, majik_core::model::AssetId) {
        let input = seed_asset(&env.library, cx, MediaType::Image, 7);
        let request = Request::tool(ProviderId::mock(), majik_providers::ToolSettings::new(catalog::tool::MOCK_UPSCALE.clone()), majik_generation::AssetInput::new(majik_providers::AssetRole::ReferenceImage, "image/png", vec![]));
        let id = seed_request(&env.library, cx, &request, &[("reference_image", input.clone())]);
        (id, input)
    }

    #[gpui::test]
    fn an_upscale_with_its_input_shows_a_before_after_split(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let (id, input) = upscaled(&env, cx);
        let plain = seed_item(&env.library, cx, Seed::default());
        let input_path = env.library.read_with(cx, |m, _| m.lib.asset(&input).unwrap().path.clone());
        let (detail, vcx) = detail_over(cx, vec![EntryId::Generation(id.clone()), EntryId::Generation(plain)], 0);
        detail.update(vcx, |d, _| {
            assert_eq!(d.compare_original(), Some(input_path.as_path()), "the image the tool ran over");
            assert_eq!(d.divider, 0.5, "halfway to begin with");
        });
        // A generation without an input has nothing to compare against.
        detail.update(vcx, |d, cx| d.go(1, cx));
        redraw(&detail, vcx);
        detail.update(vcx, |d, _| assert!(d.compare_original().is_none()));
        // The input's file going missing takes the comparison away, not the result.
        std::fs::remove_file(&input_path).unwrap();
        env.library.update(vcx, |m, cx| {
            m.lib.reload().unwrap();
            m.changed(cx);
        });
        detail.update(vcx, |d, cx| d.go(-1, cx));
        redraw(&detail, vcx);
        detail.update(vcx, |d, cx| {
            assert!(d.compare_original().is_none());
            assert!(d.item(cx).unwrap().file().is_some(), "the upscale itself is still shown");
        });
    }

    #[gpui::test]
    fn dragging_moves_the_divider_at_fit_and_pans_when_zoomed(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let (id, _) = upscaled(&env, cx);
        let (detail, vcx) = detail_over(cx, vec![EntryId::Generation(id)], 0);
        // A 32×32 result fitted into an 800×600 stage: 24 px of padding, scale 18, so the image
        // is 576 px wide starting at x = 112.
        detail.update(vcx, |d, _| *d.area.borrow_mut() = Bounds { origin: point(px(0.), px(0.)), size: size(px(800.), px(600.)) });
        detail.update_in(vcx, |d, window, cx| {
            d.drag_divider_to(px(112. + 144.), window, cx);
            assert!((d.divider - 0.25).abs() < 0.01, "{}", d.divider);
            d.drag_divider_to(px(5.), window, cx);
            assert_eq!(d.divider, 0., "clamped to the image's left edge");
            d.drag_divider_to(px(2000.), window, cx);
            assert_eq!(d.divider, 1., "… and its right edge");
            d.drag_divider_to(px(112. + 288.), window, cx);
        });
        // Zoomed in, a drag pans and the divider keeps its place, which is also kept across items.
        detail.update_in(vcx, |d, window, cx| {
            d.set_zoom_around(2.0, None, window, cx);
            assert!(d.zoom.is_some());
            assert!((d.divider - 0.5).abs() < 0.01);
            d.go(0, cx);
            assert!((d.divider - 0.5).abs() < 0.01, "the divider is kept across items");
        });
    }

    #[gpui::test]
    fn an_output_opened_from_assets_is_shown_as_its_generation(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let id = seed_item(&env.library, cx, Seed::default());
        let output = env.library.read_with(cx, |m, _| m.lib.get(&id).unwrap().output_asset_id.clone().unwrap());
        let (detail, vcx) = detail_over(cx, vec![EntryId::Asset(output.clone())], 0);
        detail.update(vcx, |d, cx| {
            let subject = d.subject(cx).unwrap();
            assert_eq!(subject.generation, Some(id.clone()));
            assert_eq!(subject.asset, Some(output.clone()));
            assert!(subject.item.can_recreate(), "the generation's request is there");
            assert_eq!(subject.item.prompt().as_deref(), Some("seeded"));
        });
        detail.update_in(vcx, |d, w, cx| d.toggle_favorite(&ToggleFavorite, w, cx));
        env.library.read_with(vcx, |m, _| assert!(m.lib.get(&id).unwrap().is_favorite, "favourite lands on the generation"));
    }

    #[gpui::test]
    fn deleting_an_unreferenced_asset_from_the_detail_trashes_it_and_closes(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let import = crate::test_support::seed_asset(&env.library, cx, MediaType::Image, 6);
        let path = env.library.read_with(cx, |m, _| m.lib.asset(&import).unwrap().path.clone());
        let (detail, vcx) = detail_over(cx, vec![EntryId::Asset(import.clone())], 0);
        let closed = Rc::new(Cell::new(false));
        let flag = closed.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&detail, move |_, ev: &DetailEvent, _| {
                if matches!(ev, DetailEvent::Close) {
                    flag.set(true);
                }
            })
            .detach();
        });
        detail.update_in(vcx, |d, w, cx| d.delete(&DeleteMedia, w, cx));
        let (message, detail_text) = vcx.pending_prompt().expect("delete asks first");
        assert_eq!(message, "Delete this asset?");
        assert!(detail_text.contains(".majik/trash"));
        vcx.simulate_prompt_answer("Delete");
        vcx.run_until_parked();
        assert!(!path.exists());
        env.library.read_with(vcx, |m, _| assert!(m.lib.asset(&import).is_none()));
        assert!(closed.get(), "nothing left to show");
    }

    #[gpui::test]
    fn deleting_a_referenced_asset_from_the_detail_toasts(cx: &mut TestAppContext) {
        let env = env(cx, 1, "Mock");
        let (item, output) = env.library.read_with(cx, |m, _| (m.lib.generations()[0].id.clone(), m.lib.generations()[0].output_asset_id.clone().unwrap()));
        // Referenced as an input by a second generation, and shown as a plain asset because its
        // own generation is gone.
        env.library.update(cx, |m, cx| {
            let next = m.lib.add_generating(MediaType::Video, None, None, Some("Mock".into()), None);
            m.lib.attach_inputs(&next, &[(output.clone(), "first_frame")]).unwrap();
            m.delete(std::slice::from_ref(&item), cx);
        });
        let (detail, vcx) = detail_over(cx, vec![EntryId::Asset(output.clone())], 0);
        detail.update(vcx, |d, cx| assert!(d.subject(cx).unwrap().generation.is_none(), "shown as a plain asset"));
        let before = vcx.update(|_, cx| crate::ui::toast_generation(cx));
        detail.update_in(vcx, |d, w, cx| d.delete(&DeleteMedia, w, cx));
        assert!(vcx.pending_prompt().is_none(), "nothing to confirm");
        assert_eq!(vcx.update(|_, cx| crate::ui::toast_generation(cx)), before + 1);
        env.library.read_with(vcx, |m, _| assert!(m.lib.asset(&output).is_some()));
    }

    #[gpui::test]
    fn paging_walks_a_mixed_list_and_drops_a_deleted_asset(cx: &mut TestAppContext) {
        let env = env(cx, 1, "Mock");
        let import = crate::test_support::seed_asset(&env.library, cx, MediaType::Audio, 2);
        let item = env.library.read_with(cx, |m, _| m.lib.generations()[0].id.clone());
        let (detail, vcx) = detail_over(cx, vec![EntryId::Generation(item.clone()), EntryId::Asset(import.clone())], 0);
        detail.update(vcx, |d, cx| {
            assert_eq!(d.subject(cx).unwrap().generation, Some(item.clone()));
            d.go(1, cx);
            let subject = d.subject(cx).unwrap();
            assert_eq!(subject.asset, Some(import.clone()));
            assert_eq!(subject.item.media_type, MediaType::Audio);
            assert_eq!(d.current_id(), Some(EntryId::Asset(import.clone())));
        });
        env.library.update(vcx, |m, cx| m.delete_assets(std::slice::from_ref(&import), cx).unwrap());
        vcx.run_until_parked();
        detail.update(vcx, |d, _| {
            assert_eq!(d.ids, vec![EntryId::Generation(item.clone())], "the trashed asset left the list");
            assert_eq!(d.index, 0);
        });
    }

    /// A detail over `id` as the window's root, drawn and past its open morph (which hides the
    /// chevrons and the info panel), so `debug_bounds` and clicks work.
    fn rendered_detail_on<'a>(cx: &'a mut TestAppContext, env: &crate::test_support::TestEnv, id: &GenerationId) -> (Entity<DetailView>, &'a mut gpui::VisualTestContext) {
        let ids = env.library.read_with(cx, |m, _| m.lib.feed(&majik_core::FeedFilter::Library, majik_core::MediaFilter::All));
        let index = ids.iter().position(|i| i == id).expect("item is in the feed");
        let (detail, vcx) = cx.add_window_view(|_, cx| {
            let thumbnails = LruImageCache::new(cx);
            DetailView::new(ids.into_iter().map(EntryId::Generation).collect(), index, None, thumbnails, cx)
        });
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(morph::FADE_DURATION + Duration::from_millis(50));
        redraw(&detail, vcx);
        (detail, vcx)
    }

    fn first_id(env: &crate::test_support::TestEnv, cx: &mut TestAppContext) -> GenerationId {
        env.library.read_with(cx, |m, _| m.lib.generations()[0].id.clone())
    }

    fn redraw(detail: &Entity<DetailView>, vcx: &mut gpui::VisualTestContext) {
        detail.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
    }

    fn chevron_shown(vcx: &mut gpui::VisualTestContext, side: &'static str) -> bool {
        vcx.debug_bounds(if side == "left" { "chevron-left" } else { "chevron-right" }).is_some()
    }

    #[gpui::test]
    fn chevrons_fade_in_on_hover_and_out_over_120ms(cx: &mut TestAppContext) {
        let env = env(cx, 2, "Mock");
        let first = first_id(&env, cx);
        let (detail, vcx) = rendered_detail_on(cx, &env, &first);
        assert!(!chevron_shown(vcx, "right"), "hidden until hovered");
        detail.update(vcx, |d, cx| {
            d.hover_right = true;
            cx.notify();
        });
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(Duration::from_millis(60));
        redraw(&detail, vcx);
        assert!(chevron_shown(vcx, "right"), "fading in");
        vcx.background_executor.advance_clock(Duration::from_millis(120));
        redraw(&detail, vcx);
        assert!(chevron_shown(vcx, "right"), "fully shown");
        detail.update(vcx, |d, cx| {
            d.hover_right = false;
            cx.notify();
        });
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(Duration::from_millis(60));
        redraw(&detail, vcx);
        assert!(chevron_shown(vcx, "right"), "still fading out halfway through the 120 ms");
        vcx.background_executor.advance_clock(Duration::from_millis(120));
        redraw(&detail, vcx);
        assert!(!chevron_shown(vcx, "right"), "gone once the fade-out has played");
    }

    #[gpui::test]
    fn chevron_stays_hidden_at_the_end_of_the_strip(cx: &mut TestAppContext) {
        let env = env(cx, 2, "Mock");
        let first = first_id(&env, cx);
        let (detail, vcx) = rendered_detail_on(cx, &env, &first);
        detail.update(vcx, |d, cx| {
            d.hover_left = true;
            cx.notify();
        });
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(Duration::from_millis(200));
        redraw(&detail, vcx);
        assert!(!chevron_shown(vcx, "left"), "no previous item: hovering the zone shows nothing");
    }

    #[gpui::test]
    fn controls_float_over_the_stage_with_close_apart(cx: &mut TestAppContext) {
        let env = env(cx, 1, "Mock");
        let id = first_id(&env, cx);
        let (detail, vcx) = rendered_detail_on(cx, &env, &id);
        let stage = detail.read_with(vcx, |d, _| *d.area.borrow());
        assert!(vcx.debug_bounds("caption").is_none(), "no caption over the stage: the model and date are in the info panel");
        let controls = vcx.debug_bounds("image-controls").expect("the item's controls float over the stage");
        let close = vcx.debug_bounds("close-control").expect("the close button has its own panel");
        assert!(stage.contains(&controls.center()) && stage.contains(&close.center()), "both sit inside the stage: {stage:?} {controls:?} {close:?}");
        assert!(close.origin.x > controls.origin.x + controls.size.width, "close sits apart, to the right of the controls");
        let closing = Rc::new(Cell::new(false));
        let c = closing.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&detail, move |_, ev: &DetailEvent, _| {
                if let DetailEvent::WillClose { .. } = ev {
                    c.set(true);
                }
            })
            .detach();
        });
        vcx.simulate_click(close.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(closing.get(), "the floating close button closes the detail");
    }

    #[gpui::test]
    fn checkerboard_backs_only_background_removals(cx: &mut TestAppContext) {
        let env = env(cx, 1, "Mock");
        let image = first_id(&env, cx);
        let (_detail, vcx) = rendered_detail_on(cx, &env, &image);
        assert!(vcx.debug_bounds("checkerboard").is_none(), "a plain image sits on the black stage");
        let input = seed_asset(&env.library, cx, MediaType::Image, 7);
        let request = Request::tool(ProviderId::mock(), majik_providers::ToolSettings::new(catalog::tool::MOCK_REMOVE_BACKGROUND.clone()), majik_generation::AssetInput::new(majik_providers::AssetRole::ReferenceImage, "image/png", vec![]));
        let cutout = seed_request(&env.library, cx, &request, &[("reference_image", input)]);
        let (_detail, vcx) = rendered_detail_on(cx, &env, &cutout);
        assert!(vcx.debug_bounds("checkerboard").is_some(), "a background removal shows its transparency");
        let audio = seed_item(&env.library, cx, Seed { media_type: MediaType::Audio, ..Seed::default() });
        let (_detail, vcx) = rendered_detail_on(cx, &env, &audio);
        assert!(vcx.debug_bounds("checkerboard").is_none(), "the audio hero has no transparency to show");
    }

    #[gpui::test]
    fn zoom_presets_are_multiples_of_the_fitted_size(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 1, 0);
        detail.update(vcx, |d, _| measure_stage(d));
        detail.update_in(vcx, |d, w, cx| {
            let fit = d.fit_zoom(w, cx);
            assert_eq!(d.active_zoom_preset(w, cx), Some(1.0), "fit to view is 1×");
            d.zoom_preset(4.0, w, cx);
            assert!((d.zoom.unwrap() - fit * 4.0).abs() < 1e-3, "{:?} vs {fit} × 4", d.zoom);
            assert_eq!(d.active_zoom_preset(w, cx), Some(4.0));
            d.zoom_in(&crate::actions::ZoomIn, w, cx);
            assert_eq!(d.active_zoom_preset(w, cx), None, "a step between presets highlights none");
            d.zoom_preset(8.0, w, cx);
            assert!((d.zoom.unwrap() - fit * 8.0).abs() < 1e-3, "the largest preset is reachable past ZOOM_MAX for a small image");
            d.zoom_preset(1.0, w, cx);
            assert!(d.zoom.is_none() && d.pan == Point::default(), "1× is fit to view, recentred");
        });
    }

    #[gpui::test]
    fn zoom_row_shows_for_completed_media_and_a_click_picks_a_preset(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let generating = seed_item(&env.library, cx, Seed { status: Status::Generating, ..Seed::default() });
        let (_detail, vcx) = rendered_detail_on(cx, &env, &generating);
        assert!(vcx.debug_bounds("zoom-presets").is_none(), "nothing to zoom while generating");
        let done = seed_item(&env.library, cx, Seed::default());
        let (detail, vcx) = rendered_detail_on(cx, &env, &done);
        let two = vcx.debug_bounds("zoom-2").expect("the zoom row floats over a completed image");
        vcx.simulate_click(two.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        detail.update_in(vcx, |d, w, cx| assert_eq!(d.active_zoom_preset(w, cx), Some(2.0)));
    }

    #[gpui::test]
    fn a_zoom_preset_click_leaves_the_divider_alone(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let (id, _) = upscaled(&env, cx);
        let (detail, vcx) = detail_over(cx, vec![EntryId::Generation(id)], 0);
        // The row sits at the stage's left edge: a press that reached the stage would drag the
        // divider to the far left.
        let one = vcx.debug_bounds("zoom-1").expect("zoom row over an upscale");
        vcx.simulate_click(one.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        detail.update(vcx, |d, _| assert!((d.divider - 0.5).abs() < 0.01, "the row swallowed the press: {}", d.divider));
    }

    #[gpui::test]
    fn copy_prompt_writes_the_clipboard_and_toasts(cx: &mut TestAppContext) {
        let env = env(cx, 0, "Mock");
        let id = seed_item(&env.library, cx, Seed::default());
        let (detail, vcx) = rendered_detail_on(cx, &env, &id);
        let prompt = detail.read_with(vcx, |d, cx| d.item(cx).unwrap().prompt().expect("seeded item has a prompt"));
        assert!(vcx.debug_bounds("copy-prompt").is_none(), "no info panel, no copy button");
        detail.update_in(vcx, |d, w, cx| d.show_info(&ShowInfo, w, cx));
        vcx.run_until_parked();
        let button = vcx.debug_bounds("copy-prompt").expect("copy button next to the prompt");
        vcx.simulate_click(button.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        assert_eq!(vcx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(prompt.as_str()));
        let toast = vcx.update(|window, cx| crate::ui::current_toast(window, cx));
        assert_eq!(toast.map(|(message, _)| message.to_string()).as_deref(), Some("Prompt copied"));
        // The ✓ reverts to the copy icon after 2 s.
        vcx.background_executor.advance_clock(Duration::from_millis(2100));
        redraw(&detail, vcx);
        assert!(vcx.debug_bounds("copy-prompt").is_some());
    }

    /// The hero and transport bar over a real WAV. `majik_audio` needs an output device, so on a
    /// machine without one the same seed must show the error instead of a player.
    #[gpui::test]
    fn audio_item_gets_a_player_and_transport_or_a_visible_error(cx: &mut TestAppContext) {
        let env = env(cx, 1, "Mock");
        let wav: &'static [u8] = Box::leak(majik_providers::mock::MockClient::silent_wav(500).into_boxed_slice());
        let audio = seed_item(&env.library, cx, Seed { media_type: MediaType::Audio, bytes: Some(wav), ..Seed::default() });
        let (detail, vcx) = rendered_detail_on(cx, &env, &audio);
        if !majik_audio::output_device_available() {
            detail.read_with(vcx, |d, _| {
                assert!(d.audio.is_none() && d.transport().is_none());
                assert!(d.player_error.is_some(), "the hero shows why there is no player");
            });
            return;
        }
        detail.update_in(vcx, |d, window, cx| {
            assert!(d.player.is_none(), "audio never gets the video player");
            assert!(d.player_error.is_none());
            let (position, duration, playing) = d.transport().expect("audio transport");
            assert_eq!(position, 0.0);
            assert!((duration - 0.5).abs() < 0.05, "500 ms clip, got {duration}");
            assert!(!playing, "opens paused");
            d.toggle_playback(&TogglePlayback, window, cx);
            assert!(d.transport().unwrap().2, "space plays");
            d.toggle_playback(&TogglePlayback, window, cx);
            assert!(!d.transport().unwrap().2, "space again pauses");
            d.seek_fraction(0.5, window, cx);
            let position = d.transport().unwrap().0;
            assert!((position - 0.25).abs() < 0.05, "scrubber seeks to the middle, got {position}");
        });
        // The feed is newest-first: the seeded image is the next page. The drawn stage slides
        // there, and the player goes once the slide has settled.
        detail.update(vcx, |d, cx| d.go(1, cx));
        vcx.run_until_parked();
        vcx.background_executor.advance_clock(Duration::from_millis(700));
        redraw(&detail, vcx);
        detail.read_with(vcx, |d, _| assert!(d.audio.is_none() && d.audio_for.is_none(), "leaving the item hard-stops its player"));
    }

    #[test]
    fn stage_cursor_reflects_zoom_and_drag() {
        assert_eq!(stage_cursor(false, false), None);
        assert_eq!(stage_cursor(true, false), Some(CursorStyle::OpenHand));
        assert_eq!(stage_cursor(true, true), Some(CursorStyle::ClosedHand));
    }

    #[gpui::test]
    fn reduce_motion_jumps_instead_of_sliding(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 5, 2);
        vcx.update(|_, cx| cx.set_reduce_motion(true));
        detail.update(vcx, |d, _| measure_stage(d));
        detail.update_in(vcx, |d, w, cx| d.next(&crate::actions::NextItem, w, cx));
        detail.update(vcx, |d, _| {
            assert_eq!(d.index, 3);
            assert!(!d.paging.is_animating(), "key navigation jump-cuts");
        });
    }

    #[gpui::test]
    fn navigation_clamps_and_wraps_at_ends(cx: &mut TestAppContext) {
        let (detail, vcx, _env, ids) = detail_window!(cx, 5, 2);
        detail.update(vcx, |d, _| assert_eq!(d.index, 2));
        detail.update_in(vcx, |d, w, cx| d.next(&crate::actions::NextItem, w, cx));
        detail.update(vcx, |d, _| assert_eq!(d.index, 3));
        detail.update_in(vcx, |d, w, cx| d.prev(&crate::actions::PrevItem, w, cx));
        detail.update_in(vcx, |d, w, cx| d.prev(&crate::actions::PrevItem, w, cx));
        detail.update(vcx, |d, _| assert_eq!(d.index, 1));
        // Can't go below 0.
        for _ in 0..5 {
            detail.update_in(vcx, |d, w, cx| d.prev(&crate::actions::PrevItem, w, cx));
        }
        detail.update(vcx, |d, _| assert_eq!(d.index, 0));
        // Can't exceed the last index.
        for _ in 0..20 {
            detail.update_in(vcx, |d, w, cx| d.next(&crate::actions::NextItem, w, cx));
        }
        detail.update(vcx, |d, _| assert_eq!(d.index, ids.len() - 1));
    }

    #[gpui::test]
    fn back_asks_then_closes_after_the_morph(cx: &mut TestAppContext) {
        let (detail, vcx, _env, ids) = detail_window!(cx, 3, 1);
        let events = record_events(&detail, vcx);
        detail.update_in(vcx, |d, w, cx| d.back(&crate::actions::Back, w, cx));
        vcx.run_until_parked();
        assert_eq!(*events.borrow(), vec!["will_close"], "the owner gets to pick the cell first");

        // Unmeasured stage and no cell: a fade.
        detail.update(vcx, |d, cx| d.close_towards(None, cx));
        detail.update_in(vcx, |d, w, cx| {
            assert!(d.is_transitioning());
            assert_eq!(d.morph.map(|m| m.shape()), Some(Shape::Fade));
            // Mid-morph input is ignored: no second close, no paging.
            d.back(&crate::actions::Back, w, cx);
            d.go(1, cx);
            d.show_info(&crate::actions::ShowInfo, w, cx);
            assert_eq!(d.index, 1);
            assert!(!d.show_info);
        });
        vcx.run_until_parked();
        assert_eq!(*events.borrow(), vec!["will_close"]);

        vcx.background_executor.advance_clock(morph::FADE_DURATION);
        vcx.run_until_parked();
        assert_eq!(*events.borrow(), vec!["will_close", "close"]);
        assert_eq!(detail.read_with(vcx, |d, _| d.current_id()), Some(EntryId::Generation(ids[1].clone())));
    }

    #[gpui::test]
    fn close_shrinks_into_the_cell_from_the_fitted_rect(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 3, 0);
        let cell = gpui::Bounds { origin: point(px(300.), px(100.)), size: gpui::size(px(120.), px(120.)) };
        give_thumbnail(&env, &ids[0], vcx);
        detail.update_in(vcx, |d, w, cx| {
            measure_stage(d);
            d.zoom_in(&crate::actions::ZoomIn, w, cx);
            assert!(d.zoom.is_some());
            d.close_towards(Some(cell), cx);
            assert!(d.zoom.is_none(), "a zoomed image shrinks from its fitted rect");
            let morph = d.morph.expect("close morph");
            assert_eq!(morph.direction(), Direction::Close);
            let stage = d.resting_rect(cx).expect("measured");
            assert_eq!(morph.shape(), Shape::Geometry { cell, stage });
            assert_eq!(morph.frame(now(cx)), Some(stage), "starts where the image rests");
            assert!(stage.size.width <= px(STAGE_W) && stage.origin.x >= px(0.));
        });
        vcx.background_executor.advance_clock(morph::DURATION);
        detail.update(vcx, |d, cx| assert_eq!(d.morph.expect("still set until Close").frame(now(cx)), Some(cell), "lands on the cell"));
    }

    #[gpui::test]
    fn open_grows_from_the_cell_once_the_stage_is_measured(cx: &mut TestAppContext) {
        let env = env(cx, 3, "Mock");
        let (feed, vcx) = cx.add_window_view(crate::views::feed::FeedView::new);
        vcx.run_until_parked();
        let ids = all_ids(&env, vcx);
        give_thumbnail(&env, &ids[0], vcx);
        let cell = gpui::Bounds { origin: point(px(10.), px(60.)), size: gpui::size(px(90.), px(90.)) };
        let thumbnails = feed.read_with(vcx, |feed, _| feed.image_cache());
        let detail = vcx.new(|cx| DetailView::new(ids.iter().cloned().map(EntryId::Generation).collect(), 0, Some(cell), thumbnails, cx));
        vcx.run_until_parked();
        detail.update(vcx, |d, cx| {
            assert!(d.is_transitioning(), "pending until the stage is measured");
            let item = d.item(cx).unwrap();
            assert!(!d.tick_morph(&item, now(cx), cx));
            assert!(d.morph.is_none(), "nowhere to grow to yet");
            measure_stage(d);
            assert!(d.tick_morph(&item, now(cx), cx), "wants frames");
            let morph = d.morph.expect("open morph");
            assert_eq!(morph.direction(), Direction::Open);
            assert_eq!(morph.frame(now(cx)), Some(cell), "starts at the cell");
            assert!(d.origin.is_none(), "consumed");
        });
        vcx.background_executor.advance_clock(morph::DURATION);
        detail.update(vcx, |d, cx| {
            let item = d.item(cx).unwrap();
            assert_eq!(d.morph.unwrap().frame(now(cx)), d.resting_rect(cx), "lands on the resting rect");
            assert!(!d.tick_morph(&item, now(cx), cx));
            assert!(!d.is_transitioning(), "settled");
        });
    }

    #[gpui::test]
    fn no_thumbnail_or_reduce_motion_skips_the_geometry(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 2, 0);
        let cell = gpui::Bounds { origin: point(px(10.), px(60.)), size: gpui::size(px(90.), px(90.)) };
        detail.update(vcx, |d, cx| {
            measure_stage(d);
            d.origin = Some(cell);
            let item = d.item(cx).unwrap();
            assert!(item.thumbnail.is_none(), "inert test libraries have no thumbnails");
            d.tick_morph(&item, now(cx), cx);
            assert_eq!(d.morph.map(|m| m.shape()), Some(Shape::Fade), "nothing to travel with");
        });
        vcx.update(|_, cx| cx.set_reduce_motion(true));
        let events = record_events(&detail, vcx);
        detail.update(vcx, |d, cx| {
            d.morph = None;
            d.close_towards(Some(cell), cx);
            assert!(!d.is_transitioning());
        });
        vcx.run_until_parked();
        assert_eq!(*events.borrow(), vec!["close"], "closes on the spot");
    }

    /// Inert test libraries never render thumbnails; point the item at its own file so the cell
    /// has something to travel with.
    fn give_thumbnail(env: &crate::test_support::TestEnv, id: &GenerationId, vcx: &mut gpui::VisualTestContext) {
        env.library.update(vcx, |m, cx| {
            let path = m.lib.get(id).and_then(|i| i.path.clone()).expect("seeded image has a file");
            m.lib.set_thumbnail(id, path);
            m.changed(cx);
        });
    }

    #[gpui::test]
    fn reset_zoom_toggles_fit_and_actual_size(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 2, 0);
        detail.update(vcx, |d, _| assert!(d.zoom.is_none(), "starts fit-to-view"));
        detail.update_in(vcx, |d, w, cx| d.reset_zoom(&crate::actions::ResetZoom, w, cx));
        detail.update(vcx, |d, _| assert_eq!(d.zoom, Some(1.0), "→ actual size"));
        detail.update_in(vcx, |d, w, cx| d.reset_zoom(&crate::actions::ResetZoom, w, cx));
        detail.update(vcx, |d, _| assert!(d.zoom.is_none(), "→ back to fit"));
    }

    #[gpui::test]
    fn deleting_current_item_closes_when_empty(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 1, 0);
        let closed = Rc::new(Cell::new(false));
        let c = closed.clone();
        vcx.update(|_, cx| {
            cx.subscribe(&detail, move |_, ev: &DetailEvent, _| {
                if let DetailEvent::Close = ev {
                    c.set(true);
                }
            })
            .detach();
        });
        // Delete the only item directly through the library; the detail observer prunes + closes.
        env.library.update(vcx, |m, cx| m.delete(&[ids[0].clone()], cx));
        vcx.run_until_parked();
        assert!(closed.get(), "detail closes when its last item is gone");
    }

    const STAGE_W: f32 = 800.0;

    /// Pretend the stage has been laid out; without a width there is nothing to slide.
    fn measure_stage(d: &DetailView) {
        *d.area.borrow_mut() = gpui::Bounds { origin: Point::default(), size: gpui::size(px(STAGE_W), px(600.)) };
    }

    fn scroll(dx: f32, phase: gpui::TouchPhase) -> ScrollWheelEvent {
        ScrollWheelEvent { position: Point::default(), delta: gpui::ScrollDelta::Pixels(point(px(dx), px(0.))), modifiers: gpui::Modifiers::default(), touch_phase: phase }
    }

    #[gpui::test]
    fn next_slides_in_when_measured(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 5, 2);
        detail.update(vcx, |d, _| measure_stage(d));
        detail.update_in(vcx, |d, w, cx| d.next(&crate::actions::NextItem, w, cx));
        detail.update(vcx, |d, _| {
            assert_eq!(d.index, 3, "index moves synchronously");
            assert!(d.paging.is_animating());
            assert_eq!(d.paging.offset(), STAGE_W, "the old item starts where it was");
        });
        vcx.executor().advance_clock(std::time::Duration::from_millis(700));
        detail.update(vcx, |d, cx| {
            d.paging.tick(now(cx));
            assert!(!d.paging.is_animating());
            assert_eq!(d.paging.offset(), 0.0);
        });
    }

    #[gpui::test]
    fn trackpad_swipe_pages_only_on_end(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 5, 2);
        detail.update(vcx, |d, _| measure_stage(d));
        detail.update_in(vcx, |d, w, cx| d.on_scroll(&scroll(0., gpui::TouchPhase::Started), Point::default(), w, cx));
        detail.update_in(vcx, |d, w, cx| d.on_scroll(&scroll(-0.5 * STAGE_W, gpui::TouchPhase::Moved), Point::default(), w, cx));
        detail.update(vcx, |d, _| {
            assert_eq!(d.index, 2, "dragging alone never pages");
            assert!(d.paging.is_dragging());
            assert!(d.gesture_timeout.is_some());
        });
        detail.update_in(vcx, |d, w, cx| d.on_scroll(&scroll(0., gpui::TouchPhase::Ended), Point::default(), w, cx));
        detail.update(vcx, |d, _| {
            assert_eq!(d.index, 3);
            assert!(d.zoom.is_none());
            assert!(d.gesture_timeout.is_none());
            assert!(d.paging.is_animating());
        });
        // Momentum after the release is ignored.
        for _ in 0..5 {
            detail.update_in(vcx, |d, w, cx| d.on_scroll(&scroll(-100., gpui::TouchPhase::Moved), Point::default(), w, cx));
        }
        detail.update(vcx, |d, _| assert_eq!(d.index, 3));
        // Wheel mice send line deltas and never page.
        let lines = ScrollWheelEvent { delta: gpui::ScrollDelta::Lines(point(-30., 0.)), ..scroll(0., gpui::TouchPhase::Moved) };
        detail.update_in(vcx, |d, w, cx| d.on_scroll(&lines, Point::default(), w, cx));
        detail.update(vcx, |d, _| assert_eq!(d.index, 3));
    }

    #[gpui::test]
    fn command_scroll_still_zooms(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 3, 1);
        detail.update(vcx, |d, _| measure_stage(d));
        let ev = ScrollWheelEvent { modifiers: gpui::Modifiers { platform: true, ..Default::default() }, ..scroll(0., gpui::TouchPhase::Moved) };
        let ev = ScrollWheelEvent { delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-40.))), ..ev };
        detail.update_in(vcx, |d, w, cx| d.on_scroll(&ev, Point::default(), w, cx));
        detail.update(vcx, |d, _| {
            assert!(d.zoom.is_some());
            assert_eq!(d.index, 1);
            assert!(!d.paging.has_gesture());
        });
    }

    #[gpui::test]
    fn gesture_timeout_commits(cx: &mut TestAppContext) {
        let (detail, vcx, _env, _ids) = detail_window!(cx, 5, 2);
        detail.update(vcx, |d, _| measure_stage(d));
        detail.update_in(vcx, |d, w, cx| d.on_scroll(&scroll(0., gpui::TouchPhase::Started), Point::default(), w, cx));
        detail.update_in(vcx, |d, w, cx| d.on_scroll(&scroll(-0.5 * STAGE_W, gpui::TouchPhase::Moved), Point::default(), w, cx));
        vcx.executor().advance_clock(paging::GESTURE_TIMEOUT * 2);
        vcx.run_until_parked();
        detail.update(vcx, |d, _| assert_eq!(d.index, 2, "resting fingers on a trackpad that sends phases is not a release"));
        vcx.executor().advance_clock(paging::STALE_GESTURE_TIMEOUT);
        vcx.run_until_parked();
        detail.update(vcx, |d, _| {
            assert_eq!(d.index, 3, "a gesture that stays quiet for too long is released");
            assert!(!d.paging.has_gesture());
            assert!(d.gesture_timeout.is_none());
        });
    }

    #[gpui::test]
    fn removing_current_item_mid_slide_resets_paging(cx: &mut TestAppContext) {
        let (detail, vcx, env, ids) = detail_window!(cx, 4, 1);
        detail.update(vcx, |d, _| measure_stage(d));
        detail.update_in(vcx, |d, w, cx| d.next(&crate::actions::NextItem, w, cx));
        detail.update(vcx, |d, _| assert!(d.paging.is_animating()));
        env.library.update(vcx, |m, cx| m.delete(&[ids[2].clone()], cx));
        vcx.run_until_parked();
        detail.update(vcx, |d, _| {
            assert_eq!(d.ids.len(), 3);
            assert_eq!(d.index, 2, "clamped onto the next item");
            assert!(!d.paging.is_animating());
            assert_eq!(d.paging.offset(), 0.0);
        });
        // Removing an earlier item keeps the same current item (index shifts) and leaves the slide alone.
        detail.update_in(vcx, |d, w, cx| d.prev(&crate::actions::PrevItem, w, cx));
        env.library.update(vcx, |m, cx| m.delete(&[ids[0].clone()], cx));
        vcx.run_until_parked();
        detail.update(vcx, |d, _| {
            assert_eq!(d.ids, vec![EntryId::Generation(ids[1].clone()), EntryId::Generation(ids[3].clone())]);
            assert_eq!(d.index, 0);
            assert!(d.paging.is_animating());
        });
    }
}
