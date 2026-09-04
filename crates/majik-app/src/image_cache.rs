//! A byte-bounded LRU cache of decoded thumbnails, shared by the feed grid and the detail's morph.
//!
//! gpui's `RetainAllImageCache` never evicts — despite its doc comment it is a bare
//! `HashMap<u64, ImageCacheItem>` — so the feed retained every 400 px thumbnail it had ever drawn:
//! 5.8 GB of resident memory after one scroll through a 10 000-generation library, against 174 MB
//! idle. This keeps the shape gpui expects (an `Entity` handed to `div().image_cache(…)`, see
//! `ImageCacheProvider for Entity<T>`) and drops the least recently drawn images once
//! [`FEED_IMAGE_BUDGET`] decoded bytes are held.
//!
//! The invariant to keep in mind before touching [`LruImageCache::evict`]: an entry is only ever
//! evicted once its decode has finished *and* been charged to the budget. A load in flight is never
//! cancelled, and every decode is charged exactly once, by [`LruImageCache::settle`] from the task
//! that awaits it, rather than the next time the image is asked for. A tile can scroll off before
//! its decode finishes, and an entry nothing asks for again would otherwise stay uncharged and
//! invisible to the budget forever, which is the leak this cache exists to stop.

use futures::FutureExt as _;
use gpui::{App, AppContext as _, Asset as _, AssetLogger, Entity, EntityId, ImageAssetLoader, ImageCache, ImageCacheError, ImageCacheItem, RenderImage, Resource, WeakEntity, Window};
use image::imageops::FilterType;
use majik_core::thumbnails::Fit;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Decoded bytes the feed's thumbnails may occupy before the least recently drawn ones are dropped.
///
/// A thumbnail is 400 px on its long edge (`majik_core::thumbnails::THUMB_MAX`) and decodes to
/// BGRA, so one costs at most 400 × 400 × 4 = 640 KB. The budget has to exceed what a single frame
/// draws, or a frame would evict a tile it is about to ask for again and decode in a loop. At the
/// smallest zoom (`majik_core::feed::ZOOM_LEVELS[0]` = 120 px, a pitch of ~122 px) a W × H viewport
/// draws roughly (W / 122) × (H / 122 + 3) tiles — the `+ 3` being the row of overscan the grid keeps
/// on each side:
///
/// - 2048 × 1121 (the window this was measured on) ≈ 17 × 12 = 204 tiles ≈ 131 MB
/// - 2560 × 1440 ≈ 21 × 15 = 315 tiles ≈ 202 MB
/// - 3008 × 1692 (6K XDR at default scaling) ≈ 25 × 17 = 425 tiles ≈ 272 MB
///
/// 512 MB covers all three, with room to spare in practice: cells fill the width and most
/// thumbnails are not square, so 640 KB is the maximum rather than the average. Raise it for a
/// bigger display; lowering it below the table above would make the feed decode continuously at the
/// smallest zoom. `budget_covers_the_largest_frame_the_feed_can_draw` checks that.
pub const FEED_IMAGE_BUDGET: usize = 256 * 1024 * 1024;

/// Decoded bytes the detail view may hold in full-size images.
///
/// The stage decodes what `paging::visible_slots` lists: the item plus up to two neighbours on
/// each side, so that paging is instant. The same rule applies as above — the budget has to exceed
/// one frame's demand. The largest output the catalog can produce is 4K (`ImageResolution::Uhd`,
/// 3840 px on the long edge), which is 59 MB decoded for a square one and ~33 MB for 16:9, so five
/// slots ask for at most ~295 MB. 384 MB covers that with room for the before/after compare (two
/// images at once) and the info panel's input cards. Above 4K, say an imported 8K asset, paging
/// back and forth re-decodes instead of holding both, which is worth it: one image that large is
/// 268 MB on its own.
pub const DETAIL_IMAGE_BUDGET: usize = 192 * 1024 * 1024;

pub struct LruImageCache {
    /// Handed to the task awaiting each decode so it can charge the bytes when it finishes.
    this: WeakEntity<Self>,
    budget: usize,
    /// Decoded bytes the cache references, which is what `budget` bounds. These are bytes the
    /// cache references, not bytes it uniquely owns: if a caller keeps an `Arc<RenderImage>` from
    /// here, evicting it no longer frees anything.
    bytes: usize,
    /// Keyed by `gpui::hash(&Resource)`, the key gpui's own caches use.
    entries: HashMap<u64, Entry>,
    /// Pictures decoded for an earlier target, kept only to draw until their replacement lands.
    /// A window resize moves the target a step at a time, and dropping everything at each step
    /// painted every tile blank until it was decoded again: a screen-wide blink per step. So a
    /// target change moves what is decoded here instead, a load that finds its picture still
    /// decoding is served the old one, and [`Self::settle`] drops the old one the moment the new
    /// one is charged. Only decoded pictures are kept; a load in flight at the change is dropped,
    /// since its result would be the wrong size as well. Charged like the rest, and the first to go
    /// when the budget is met.
    stale: HashMap<u64, Entry>,
    /// Bumped on every load; an entry's `last_used` is a stamp from it, which makes "least recently
    /// drawn" a `min` over the map instead of a second collection to keep in sync. Also stamps
    /// each load's `id`.
    tick: u64,
    /// The square cell, in device pixels, to decode for, and how the cell draws the picture. The
    /// feed sets it from the cell it is about to draw into: a 400 px thumbnail costs 640 KB decoded
    /// whatever it is drawn at, and at the smallest zoom a cell is under 200 device px, so decoding
    /// at the file's size spends five times the memory (and five times the sprite atlas) on pixels
    /// nobody sees. Letterboxed, the long edge is shrunk to the cell; filled, the short edge is,
    /// and the rest is cropped away as the cell would crop it; in a masonry column the width is,
    /// and the height follows the picture. `None` decodes the file as it is, which is what the
    /// detail's stage wants since it fills the window.
    target: Option<(u32, Fit)>,
}

struct Entry {
    item: ImageCacheItem,
    /// Which load this is: the task that awaits a decode charges only the entry it started, so a
    /// decode outlived by a target change cannot charge the load that replaced it.
    id: u64,
    /// Decoded bytes charged to the budget: 0 while the decode is in flight, and 0 permanently if
    /// it failed. That costs a handful of bytes per broken thumbnail, kept so a failing file isn't
    /// re-decoded on every frame.
    bytes: usize,
    last_used: u64,
}

impl LruImageCache {
    /// The feed's thumbnail cache, at [`FEED_IMAGE_BUDGET`].
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::with_budget(FEED_IMAGE_BUDGET, cx)
    }

    pub fn with_budget(budget: usize, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            // The sprite atlas outlives this entity, so the tiles have to go back explicitly. No
            // window is passed: `observe_release_in` would re-enter the window update that is
            // dropping these views, and gpui's own caches pass `None` here for the same reason.
            cx.on_release(|cache: &mut Self, cx| {
                for (_, mut entry) in std::mem::take(&mut cache.entries).into_iter().chain(std::mem::take(&mut cache.stale)) {
                    if let Some(Ok(image)) = entry.item.get() {
                        cx.drop_image(image, None);
                    }
                }
                cache.bytes = 0;
            })
            .detach();
            Self { this: cx.weak_entity(), budget, bytes: 0, entries: HashMap::new(), stale: HashMap::new(), tick: 0, target: None }
        })
    }

    /// Charge a finished decode against the budget, then evict down to it. Called once per load,
    /// from the task that awaited it; `id` is that load's, so a decode dropped by a target change
    /// finds nothing to charge.
    fn settle(&mut self, key: u64, id: u64, window: &mut Window, cx: &mut App) {
        let Some(entry) = self.entries.get_mut(&key).filter(|entry| entry.id == id) else { return };
        if entry.bytes != 0 {
            return;
        }
        // `get` turns `Loading` into `Loaded`; a failed decode stays at zero bytes and simply never
        // becomes an eviction candidate.
        let Some(result) = entry.item.get() else { return };
        if let Ok(image) = &result {
            let bytes = decoded_bytes(image);
            entry.bytes = bytes;
            self.bytes = self.bytes.saturating_add(bytes);
        }
        // The picture this one replaces has stood in long enough, whether or not the
        // replacement decoded: a failure is drawn as nothing either way.
        if let Some(stale) = self.stale.remove(&key) {
            self.drop_entry(stale, window, cx);
        }
        self.evict(window, cx);
    }

    /// Drop least-recently-drawn images until the budget is met: what is left over from an earlier
    /// target first, then the current pictures.
    fn evict(&mut self, window: &mut Window, cx: &mut App) {
        while self.bytes > self.budget {
            if let Some(key) = self.stale.keys().next().copied() {
                let Some(entry) = self.stale.remove(&key) else { return };
                self.drop_entry(entry, window, cx);
                continue;
            }
            // Only a charged entry is a candidate: evicting one still loading would free nothing
            // and start the same decode again next frame. Skipping them also makes this loop
            // terminate — every iteration removes bytes.
            let victim = self.entries.iter().filter(|(_, entry)| entry.bytes > 0).min_by_key(|(_, entry)| entry.last_used).map(|(key, _)| *key);
            let Some(key) = victim else { return };
            let Some(entry) = self.entries.remove(&key) else { return };
            self.drop_entry(entry, window, cx);
        }
    }

    /// Give an entry's bytes back to the budget and its tile to the atlas.
    fn drop_entry(&mut self, mut entry: Entry, window: &mut Window, cx: &mut App) {
        self.bytes = self.bytes.saturating_sub(entry.bytes);
        if let Some(Ok(image)) = entry.item.get() {
            // Frees the decoded bytes *and* the sprite-atlas tile. `App::remove_asset` would
            // only forget the asset and leave the GPU texture behind.
            cx.drop_image(image, Some(window));
        }
    }

    /// Decode for square cells `cell` device pixels on a side, drawn with `fit`, from now on.
    /// What is decoded for the old size or fit stays on screen until its replacement lands (see
    /// `stale`), so a resize or a zoom step never paints the grid blank; a load still in flight
    /// is dropped, and a stale picture the previous step left is kept only where the new one did
    /// not land in time to replace it.
    pub fn set_target(&mut self, cell: u32, fit: Fit, window: &mut Window, cx: &mut App) {
        // Round to a step so that a resize dragging the cells a pixel at a time doesn't re-decode
        // the feed on every frame.
        let cell = cell.div_ceil(TARGET_STEP) * TARGET_STEP;
        if self.target == Some((cell, fit)) {
            return;
        }
        self.target = Some((cell, fit));
        for (key, mut entry) in std::mem::take(&mut self.entries) {
            match entry.item.get() {
                Some(Ok(image)) => {
                    // A decode that finished but whose task has not charged it yet is charged
                    // here, since that task will no longer find it.
                    if entry.bytes == 0 {
                        entry.bytes = decoded_bytes(&image);
                        self.bytes = self.bytes.saturating_add(entry.bytes);
                    }
                    if let Some(older) = self.stale.insert(key, entry) {
                        self.drop_entry(older, window, cx);
                    }
                }
                _ => self.drop_entry(entry, window, cx),
            }
        }
    }

    /// [`Self::set_target`] before anything is cached, so the tests don't need a window to clear.
    #[cfg(test)]
    pub fn set_target_for_test(&mut self, cell: u32) {
        self.target = Some((cell.div_ceil(TARGET_STEP) * TARGET_STEP, Fit::Contain));
    }

    #[cfg(test)]
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
    }

    #[cfg(test)]
    pub fn budget(&self) -> usize {
        self.budget
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Pictures left over from an earlier target, still standing in.
    #[cfg(test)]
    pub fn stale_len(&self) -> usize {
        self.stale.len()
    }

    /// The decoded image for a resource, if the cache holds one.
    #[cfg(test)]
    pub fn loaded(&mut self, resource: &Resource) -> Option<Arc<RenderImage>> {
        self.entries.get_mut(&gpui::hash(resource))?.item.get()?.ok()
    }

    /// Whether the cache has an entry for a resource at all: decoding, decoded, or failed. The
    /// feed asks this about the picture a cell is drawing, and [`Self::warm`] about the one it
    /// wants instead, and switches only when the first is false or the second is true — so a cell
    /// never goes back to drawing nothing while a sharper tier decodes. Unlike a load, this is
    /// not a use: it leaves the entry's recency alone.
    pub fn holds(&self, resource: &Resource) -> bool {
        let key = gpui::hash(resource);
        self.entries.contains_key(&key) || self.stale.contains_key(&key)
    }

    /// Whether a resource is decoded and drawable this frame. If it is not, the decode is started
    /// (or is already in flight) and `view` is notified on the frame it lands; a decode that
    /// already failed is remembered and reported, so the caller can draw something else instead
    /// of waiting for a picture that will never come. `view` is the entity to redraw, which lets
    /// a view ask from its own render without going through gpui's rendered-view stack.
    pub fn warm(&mut self, resource: &Resource, view: EntityId, window: &mut Window, cx: &mut App) -> Warmth {
        match self.load_for(resource, view, window, cx) {
            Some(Ok(_)) => Warmth::Ready,
            Some(Err(_)) => Warmth::Failed,
            None => Warmth::Decoding,
        }
    }

    /// [`ImageCache::load`] on behalf of `view`, the entity to notify when the decode lands.
    fn load_for(&mut self, resource: &Resource, view: EntityId, window: &mut Window, cx: &mut App) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let key = gpui::hash(resource);
        self.tick += 1;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.tick;
            return match entry.item.get() {
                // Still decoding for the new target: the old target's picture stands in.
                None => self.stale.get_mut(&key).and_then(|stale| stale.item.get()).filter(Result::is_ok),
                loaded => loaded,
            };
        }

        // One shared task, held by both the entry and the waiter below, as gpui's own caches do.
        // With a target set we decode the file ourselves so the pixels come out at the size they
        // are drawn at; without one, gpui's loader handles every source it knows (SVG, animated
        // GIF, remote URIs), which the detail's stage needs.
        let task = match (self.target, &resource) {
            (Some((cell, fit)), Resource::Path(path)) => {
                let path = path.clone();
                cx.background_executor().spawn(async move { decode_scaled(&path, cell, fit) }).shared()
            }
            _ => {
                let decode = AssetLogger::<ImageAssetLoader>::load(resource.clone(), cx);
                cx.background_executor().spawn(decode).shared()
            }
        };
        self.start(key, task, view, window, cx);
        self.stale.get_mut(&key).and_then(|stale| stale.item.get()).filter(Result::is_ok)
    }

    /// An entry whose decode finishes only when the test sends its image, so a test can see the
    /// frames drawn while a picture is still decoding; the real executor finishes every decode
    /// inside `run_until_parked`. Lands through the same path as a real decode.
    #[cfg(test)]
    pub fn hold_pending_for_test(&mut self, resource: &Resource, view: EntityId, window: &mut Window, cx: &mut App) -> futures::channel::oneshot::Sender<Arc<RenderImage>> {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let task = cx.background_executor().spawn(async move { receiver.await.map_err(|_| ImageCacheError::Io(Arc::new(std::io::Error::other("the test dropped the decode")))) }).shared();
        self.start(gpui::hash(resource), task, view, window, cx);
        sender
    }

    /// Hold a decode in flight and, when it finishes, charge it and redraw `view`.
    fn start(&mut self, key: u64, task: gpui::ImageLoadingTask, view: EntityId, window: &mut Window, cx: &mut App) {
        let id = self.tick;
        self.entries.insert(key, Entry { item: ImageCacheItem::Loading(task.clone()), id, bytes: 0, last_used: self.tick });

        let cache = self.this.clone();
        window
            .spawn(cx, async move |cx| {
                if let Err(e) = task.await {
                    // `AssetLogger` has already logged the cause; the cell keeps its placeholder.
                    tracing::debug!(target: "majik", "thumbnail decode failed: {e}");
                }
                let settled = cx.update(|window, cx| {
                    cache.update(cx, |cache, cx| cache.settle(key, id, window, cx)).ok();
                });
                if settled.is_err() {
                    // The window went away while the decode ran; there is nothing left to redraw.
                    return;
                }
                // An `img` only picks a decoded image up on its next frame.
                cx.on_next_frame(move |_, cx| cx.notify(view));
            })
            .detach();
    }
}

/// What [`LruImageCache::warm`] found: the picture can be drawn now, is still decoding, or its
/// decode failed and drawing it would paint nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warmth {
    Ready,
    Decoding,
    Failed,
}

impl ImageCache for LruImageCache {
    fn load(&mut self, resource: &Resource, window: &mut Window, cx: &mut App) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let view = window.current_view();
        self.load_for(resource, view, window, cx)
    }
}

/// Cell sizes are rounded up to a multiple of this before they become a decode size, so that
/// dragging a window edge doesn't re-decode the feed on every frame.
const TARGET_STEP: u32 = 32;

/// Decodes an image file for a cell `cell` pixels wide. Letterboxed (`Fit::Contain`), the image
/// is shrunk so its longest edge is at most `cell`. Filled (`Fit::Cover`), the cell shows the
/// centred square of the image's short edge and nothing else, so that is what is decoded: the
/// short edge shrunk to at most `cell`, and the overhang cropped away. Either way a tile costs at
/// most `cell²` pixels. In a masonry column (`Fit::Width`) the width is shrunk to at most `cell`
/// and the height follows, up to the tallest cell the column allows
/// (`majik_core::feed::MASONRY_RATIO_RANGE`), beyond which the middle is kept as the cell would
/// crop it: at most `3 · cell²` pixels. The budget arithmetic assumes both bounds. Never enlarges:
/// a thumbnail drawn bigger than it was stored is stretched by the GPU, exactly as before, rather
/// than costing memory for pixels that carry no detail.
///
/// Produces the same thing as gpui's own loader, a `RenderImage` of BGRA frames, for the still
/// images the feed draws. Anything with more than one frame or a source gpui has to fetch goes
/// through its loader instead (see the caller).
fn decode_scaled(path: &Path, cell: u32, fit: Fit) -> Result<Arc<RenderImage>, ImageCacheError> {
    let bytes = std::fs::read(path).map_err(|e| ImageCacheError::Io(Arc::new(e)))?;
    let image = image::load_from_memory(&bytes).map_err(|e| ImageCacheError::Image(Arc::new(e)))?;
    let (width, height) = (image.width(), image.height());
    // Triangle: the thumbnails are already close to their drawn size, so a cheaper filter is
    // enough and this runs on the scroll path.
    let image = match fit {
        Fit::Contain if width.max(height) > cell && cell > 0 => image.resize(cell, cell, FilterType::Triangle),
        Fit::Contain => image,
        Fit::Cover => {
            let short = width.min(height);
            let image = if short > cell && cell > 0 {
                let scale = cell as f64 / short as f64;
                let scaled = |edge: u32| ((edge as f64 * scale).round() as u32).max(1);
                image.resize_exact(scaled(width), scaled(height), FilterType::Triangle)
            } else {
                image
            };
            let side = image.width().min(image.height());
            image.crop_imm((image.width() - side) / 2, (image.height() - side) / 2, side, side)
        }
        Fit::Width => {
            let image = if width > cell && cell > 0 {
                let scale = cell as f64 / width as f64;
                let scaled = |edge: u32| ((edge as f64 * scale).round() as u32).max(1);
                image.resize_exact(scaled(width), scaled(height), FilterType::Triangle)
            } else {
                image
            };
            let tallest = ((image.width() as f32 / *majik_core::feed::MASONRY_RATIO_RANGE.start()).round() as u32).max(1);
            if image.height() > tallest {
                image.crop_imm(0, (image.height() - tallest) / 2, image.width(), tallest)
            } else {
                image
            }
        }
    };
    let mut frame = image.to_rgba8();
    // gpui renders BGRA.
    for pixel in frame.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(SmallVec::from_elem(image::Frame::new(frame), 1))))
}

/// Decoded bytes an image holds. `RenderImage` keeps one 4-byte-per-pixel buffer per frame and
/// `as_bytes` returns exactly that buffer, so this is the real cost of the `Arc`, animated
/// thumbnails included.
fn decoded_bytes(image: &RenderImage) -> usize {
    (0..image.frame_count()).filter_map(|frame| image.as_bytes(frame)).map(<[u8]>::len).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext};
    use majik_core::images::solid_png;
    use std::path::{Path, PathBuf};

    /// Every test image is this size, so a budget can be expressed in whole images.
    const IMAGE: u32 = 64;
    const IMAGE_BYTES: usize = (IMAGE * IMAGE * 4) as usize;

    /// Draws whatever images it is handed, through the cache under test. `ImageCache::load` may
    /// only run inside a paint pass (it calls `Window::current_view`), so the tests drive a real
    /// view rather than calling the cache directly.
    struct Probe {
        cache: Entity<LruImageCache>,
        images: Vec<PathBuf>,
    }

    impl Render for Probe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
                .image_cache(self.cache.clone())
                .children(self.images.iter().map(|path| gpui::img(path.clone()).w(px(10.)).h(px(10.))))
        }
    }

    fn image_file(dir: &Path, name: &str, rgb: [u8; 3]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, solid_png(IMAGE, IMAGE, rgb)).unwrap();
        path
    }

    fn resource(path: &Path) -> Resource {
        Resource::Path(path.to_path_buf().into())
    }

    /// A probe over a cache with `budget` bytes, showing nothing yet.
    fn probe(cx: &mut TestAppContext, budget: usize) -> (Entity<Probe>, &mut VisualTestContext, Entity<LruImageCache>) {
        let cache = cx.update(|cx| LruImageCache::with_budget(budget, cx));
        let (probe, vcx) = cx.add_window_view({
            let cache = cache.clone();
            |_window, _cx| Probe { cache, images: Vec::new() }
        });
        (probe, vcx, cache)
    }

    /// Draw `images`, let the decodes finish, and deliver the frame their completion asks for.
    fn draw(probe: &Entity<Probe>, vcx: &mut VisualTestContext, images: &[&PathBuf]) {
        probe.update(vcx, |probe, cx| {
            probe.images = images.iter().map(|path| (*path).clone()).collect();
            cx.notify();
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn bytes_track_the_decoded_size(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);

        draw(&probe, vcx, &[&a]);

        let (bytes, len) = cache.read_with(vcx, |cache, _| (cache.bytes(), cache.len()));
        assert_eq!(bytes, IMAGE_BYTES, "a 64x64 BGRA thumbnail");
        assert_eq!(len, 1);
    }

    #[gpui::test]
    fn the_least_recently_drawn_image_goes_first(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (image_file(dir.path(), "a.png", [200, 30, 30]), image_file(dir.path(), "b.png", [30, 200, 30]), image_file(dir.path(), "c.png", [30, 30, 200]));
        let (probe, vcx, cache) = probe(cx, 2 * IMAGE_BYTES);

        draw(&probe, vcx, &[&a, &b]);
        // `a` is drawn again here, so `b` becomes the least recently used.
        draw(&probe, vcx, &[&a, &c]);

        cache.update(vcx, |cache, _| {
            assert!(cache.loaded(&resource(&a)).is_some(), "the image still on screen was kept");
            assert!(cache.loaded(&resource(&c)).is_some(), "the newest image was kept");
            assert!(cache.loaded(&resource(&b)).is_none(), "the least recently drawn image was evicted");
            assert!(cache.bytes() <= cache.budget(), "{} bytes against a {} budget", cache.bytes(), cache.budget());
        });
    }

    #[gpui::test]
    fn drawing_an_image_again_renews_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (image_file(dir.path(), "a.png", [200, 30, 30]), image_file(dir.path(), "b.png", [30, 200, 30]), image_file(dir.path(), "c.png", [30, 30, 200]));
        let (probe, vcx, cache) = probe(cx, 2 * IMAGE_BYTES);

        draw(&probe, vcx, &[&a, &b]);
        draw(&probe, vcx, &[&a]);
        draw(&probe, vcx, &[&c]);

        cache.update(vcx, |cache, _| {
            assert!(cache.loaded(&resource(&a)).is_some(), "the touched image survived");
            assert!(cache.loaded(&resource(&b)).is_none(), "the untouched one did not");
        });
    }

    #[gpui::test]
    fn the_same_image_twice_is_charged_once(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);

        draw(&probe, vcx, &[&a, &a, &a]);

        let (bytes, len) = cache.read_with(vcx, |cache, _| (cache.bytes(), cache.len()));
        assert_eq!(len, 1, "one entry per resource");
        assert_eq!(bytes, IMAGE_BYTES, "charged once");
    }

    #[gpui::test]
    fn a_load_in_flight_is_never_evicted(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (image_file(dir.path(), "a.png", [200, 30, 30]), image_file(dir.path(), "b.png", [30, 200, 30]), image_file(dir.path(), "c.png", [30, 30, 200]));
        let (probe, vcx, cache) = probe(cx, IMAGE_BYTES);

        // All three start decoding in the same frame: nothing is charged yet, so nothing is dropped
        // mid-frame and no decode is cancelled.
        probe.update(vcx, |probe, cx| {
            probe.images = vec![a.clone(), b.clone(), c.clone()];
            cx.notify();
        });
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        cache.read_with(vcx, |cache, _| {
            assert_eq!(cache.len(), 3, "every load is kept while it is in flight");
            assert_eq!(cache.bytes(), 0, "and none of them is charged yet");
        });

        vcx.run_until_parked();
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        vcx.run_until_parked();

        let (bytes, budget, len) = cache.read_with(vcx, |cache, _| (cache.bytes(), cache.budget(), cache.len()));
        assert!(bytes <= budget, "{bytes} bytes against a {budget} budget once the decodes landed");
        assert_eq!(len, 1, "the rest were evicted");
    }

    /// Asks the cache for one picture from its own render, the way the feed asks for the tier a
    /// cell wants, and keeps what it was told each frame.
    struct Warmer {
        cache: Entity<LruImageCache>,
        path: PathBuf,
        answers: Vec<bool>,
    }

    impl Render for Warmer {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let view = cx.entity_id();
            let ready = self.cache.update(cx, |cache, cx| cache.warm(&resource(&self.path), view, window, cx));
            self.answers.push(ready == Warmth::Ready);
            gpui::div()
        }
    }

    #[gpui::test]
    fn warm_starts_the_decode_and_reports_ready_only_once_it_has_finished(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let cache = cx.update(|cx| LruImageCache::with_budget(8 * IMAGE_BYTES, cx));
        let (warmer, vcx) = cx.add_window_view({
            let cache = cache.clone();
            let path = a.clone();
            |_window, _cx| Warmer { cache, path, answers: Vec::new() }
        });

        // The first frame asks, is told no, and has started the decode.
        warmer.read_with(vcx, |warmer, _| assert_eq!(warmer.answers, vec![false], "not decoded on the frame that asked"));
        cache.read_with(vcx, |cache, _| assert!(cache.holds(&resource(&a)), "and the decode was started"));

        // The decode lands, the view is redrawn, and the same question is answered yes.
        vcx.run_until_parked();
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        vcx.run_until_parked();
        warmer.read_with(vcx, |warmer, _| assert_eq!(warmer.answers.last(), Some(&true), "ready on the frame after the decode landed: {:?}", warmer.answers));
        cache.read_with(vcx, |cache, _| assert_eq!(cache.bytes(), IMAGE_BYTES, "charged like any other decode"));
    }

    /// Asking whether a picture is held is not drawing it, so it must not keep that picture alive
    /// over one that is actually drawn.
    #[gpui::test]
    fn holds_reports_an_entry_without_renewing_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (image_file(dir.path(), "a.png", [200, 30, 30]), image_file(dir.path(), "b.png", [30, 200, 30]), image_file(dir.path(), "c.png", [30, 30, 200]));
        let (probe, vcx, cache) = probe(cx, 2 * IMAGE_BYTES);

        draw(&probe, vcx, &[&a, &b]);
        cache.read_with(vcx, |cache, _| {
            assert!(cache.holds(&resource(&a)));
            assert!(cache.holds(&resource(&a)), "asked twice, the way a cell asks every frame");
            assert!(!cache.holds(&resource(&c)), "never asked for");
        });

        draw(&probe, vcx, &[&c]);

        cache.update(vcx, |cache, _| {
            assert!(cache.loaded(&resource(&a)).is_none(), "`holds` did not count as a use: `a` was still the least recently drawn");
            assert!(cache.loaded(&resource(&b)).is_some());
            assert!(cache.loaded(&resource(&c)).is_some());
        });
    }

    #[gpui::test]
    fn an_evicted_image_leaves_the_sprite_atlas(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (image_file(dir.path(), "a.png", [200, 30, 30]), image_file(dir.path(), "b.png", [30, 200, 30]));
        let (probe, vcx, cache) = probe(cx, IMAGE_BYTES);

        draw(&probe, vcx, &[&a]);
        let image = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("a decoded");
        vcx.update(|window, _| assert!(window.has_image_atlas_entry(&image), "the tile was uploaded"));

        draw(&probe, vcx, &[&b]);

        vcx.update(|window, _| assert!(!window.has_image_atlas_entry(&image), "eviction gave the GPU tile back"));
        cache.read_with(vcx, |cache, _| assert_eq!(cache.bytes(), IMAGE_BYTES, "only the survivor is charged"));
    }

    #[gpui::test]
    fn the_budget_is_enforced_as_new_decodes_land(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);

        let (b, c) = (image_file(dir.path(), "b.png", [30, 200, 30]), image_file(dir.path(), "c.png", [30, 30, 200]));
        draw(&probe, vcx, &[&a, &b]);
        cache.read_with(vcx, |cache, _| assert_eq!(cache.bytes(), 2 * IMAGE_BYTES));
        let image = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("a decoded");

        // Eviction is driven by decodes finishing, not by a timer or by a frame: memory can only
        // grow when a new image arrives, so that is when the budget is applied. A cache that is
        // over budget and draws nothing new stays as it is until the next decode, so a feed that
        // has stopped scrolling never churns.
        cache.update(vcx, |cache, _| cache.set_budget(IMAGE_BYTES));
        draw(&probe, vcx, &[&a, &b]);
        cache.read_with(vcx, |cache, _| assert_eq!(cache.bytes(), 2 * IMAGE_BYTES, "nothing new decoded, nothing evicted"));

        draw(&probe, vcx, &[&c]);

        cache.read_with(vcx, |cache, _| {
            assert_eq!(cache.bytes(), IMAGE_BYTES, "the new decode brought the cache back to budget");
            assert_eq!(cache.len(), 1);
        });
        assert_eq!(Arc::strong_count(&image), 1, "and the evicted image is this test's alone now");
    }

    #[gpui::test]
    fn a_target_decodes_the_image_at_the_size_it_is_drawn(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        // A cell half the stored thumbnail's edge: a quarter of the pixels.
        cache.update(vcx, |cache, _| cache.set_target_for_test(IMAGE / 2));

        draw(&probe, vcx, &[&a]);

        let bytes = cache.read_with(vcx, |cache, _| cache.bytes());
        assert_eq!(bytes, (IMAGE / 2 * IMAGE / 2 * 4) as usize, "decoded at the drawn size, not the stored one");
        let image = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("a decoded");
        assert_eq!(image.size(0).width.0 as u32, IMAGE / 2);
    }

    #[gpui::test]
    fn a_target_never_enlarges_an_image(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        // Drawn twice the size it was stored: stretching costs the GPU nothing, and the pixels
        // an upscale would invent cost memory for no detail.
        cache.update(vcx, |cache, _| cache.set_target_for_test(IMAGE * 2));

        draw(&probe, vcx, &[&a]);

        assert_eq!(cache.read_with(vcx, |cache, _| cache.bytes()), IMAGE_BYTES, "kept at its stored size");
    }

    /// A window resize moves the target a step at a time, and dropping everything at each step
    /// painted every tile blank until it was decoded again: a screen-wide blink per step. The old
    /// picture stands in until the new one lands, and goes the moment it does.
    #[gpui::test]
    fn changing_the_target_keeps_the_old_picture_until_the_new_one_lands(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        cache.update(vcx, |cache, _| cache.set_target_for_test(IMAGE));
        draw(&probe, vcx, &[&a]);
        let large = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("a decoded");

        // A resize step: everything held is the wrong size now, but it is all there is to draw.
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE / 2, Fit::Contain, window, cx)));
        cache.read_with(vcx, |cache, _| {
            assert_eq!((cache.len(), cache.stale_len()), (0, 1), "the old size is kept aside");
            assert_eq!(cache.bytes(), IMAGE_BYTES, "and still charged");
            assert!(cache.holds(&resource(&a)), "so a cell keeps drawing it");
        });
        vcx.update(|window, _| assert!(window.has_image_atlas_entry(&large), "its tile stays on the GPU"));

        // The next frame asks for the picture: the new decode starts and the old one is served
        // meanwhile, so the frame draws something.
        let view = probe.entity_id();
        let served = vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.warm(&resource(&a), view, window, cx)));
        assert_eq!(served, Warmth::Ready, "drawable this frame");
        cache.read_with(vcx, |cache, _| assert_eq!((cache.len(), cache.stale_len()), (1, 1), "the new decode is in flight beside the old picture"));

        draw(&probe, vcx, &[&a]);
        cache.update(vcx, |cache, _| {
            assert_eq!(cache.stale_len(), 0, "the old picture went the moment the new one landed");
            assert_eq!(cache.bytes(), (IMAGE / 2 * IMAGE / 2 * 4) as usize, "only the new size is charged");
            assert_eq!(cache.loaded(&resource(&a)).expect("redecoded").size(0).width.0 as u32, IMAGE / 2);
        });
        vcx.update(|window, _| assert!(!window.has_image_atlas_entry(&large), "and its tile went back"));
    }

    /// A fast drag moves the target twice before the first step's decode lands. The picture from
    /// before the first step is kept for the second, and the decode the first started is dropped
    /// without charging anything, so nothing goes blank in between and nothing leaks.
    #[gpui::test]
    fn a_second_target_change_keeps_the_last_decoded_picture(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        // Three distinct decode steps need a picture larger than the step: 128 → 64 → 32.
        let a = dir.path().join("big.png");
        std::fs::write(&a, solid_png(2 * IMAGE, 2 * IMAGE, [200, 30, 30])).unwrap();
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        cache.update(vcx, |cache, _| cache.set_target_for_test(2 * IMAGE));
        draw(&probe, vcx, &[&a]);
        let view = probe.entity_id();

        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE, Fit::Contain, window, cx)));
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.warm(&resource(&a), view, window, cx)));
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE / 2, Fit::Contain, window, cx)));
        cache.read_with(vcx, |cache, _| {
            assert_eq!((cache.len(), cache.stale_len()), (0, 1), "the first step's decode was dropped, the picture before it kept");
            assert_eq!(cache.bytes(), 4 * IMAGE_BYTES);
        });
        let served = vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.warm(&resource(&a), view, window, cx)));
        assert_eq!(served, Warmth::Ready, "still drawable");

        draw(&probe, vcx, &[&a]);
        cache.update(vcx, |cache, _| {
            assert_eq!(cache.stale_len(), 0);
            assert_eq!(cache.bytes(), (IMAGE / 2 * IMAGE / 2 * 4) as usize, "the dropped decode charged nothing when it finished");
            assert_eq!(cache.loaded(&resource(&a)).expect("redecoded").size(0).width.0 as u32, IMAGE / 2);
        });
    }

    /// A filled square cell shows the centred square of a picture's short edge, so that is all
    /// that gets decoded: a 64 × 128 picture drawn into a 64 px cell decodes to the middle
    /// 64 × 64, not the 32 × 64 that shrinking the long edge to the cell would leave it.
    #[gpui::test]
    fn a_filled_cell_decodes_the_centred_square_of_the_short_edge(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tall.png");
        // Red above, blue below: the crop is the seam.
        let mut tall = image::RgbaImage::new(IMAGE, 2 * IMAGE);
        for (_, y, pixel) in tall.enumerate_pixels_mut() {
            *pixel = image::Rgba(if y < IMAGE { [200, 30, 30, 255] } else { [30, 30, 200, 255] });
        }
        tall.save(&path).unwrap();
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE, Fit::Cover, window, cx)));
        draw(&probe, vcx, &[&path]);

        let decoded = cache.update(vcx, |cache, _| cache.loaded(&resource(&path))).expect("decoded");
        assert_eq!(decoded.size(0), gpui::size(gpui::DevicePixels(IMAGE as i32), gpui::DevicePixels(IMAGE as i32)));
        let pixels = decoded.as_bytes(0).unwrap();
        let (top, bottom) = (&pixels[..4], &pixels[pixels.len() - 4..]);
        assert_eq!((top, bottom), (&[30, 30, 200, 255][..], &[200, 30, 30, 255][..]), "BGRA: the seam sits in the middle, red on top");
        assert_eq!(cache.read_with(vcx, |cache, _| cache.bytes()), IMAGE_BYTES, "a tile costs the cell, not the whole picture");

        // Half the cell: still square, shrunk with the short edge.
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE / 2, Fit::Cover, window, cx)));
        draw(&probe, vcx, &[&path]);
        let decoded = cache.update(vcx, |cache, _| cache.loaded(&resource(&path))).expect("redecoded");
        assert_eq!(decoded.size(0), gpui::size(gpui::DevicePixels(IMAGE as i32 / 2), gpui::DevicePixels(IMAGE as i32 / 2)));
    }

    /// Red above, blue below, `IMAGE` wide and `height` tall: any crop shows at the seam.
    fn seam_picture(dir: &Path, height: u32) -> PathBuf {
        let path = dir.join("tall.png");
        let mut tall = image::RgbaImage::new(IMAGE, height);
        for (_, y, pixel) in tall.enumerate_pixels_mut() {
            *pixel = image::Rgba(if y < height / 2 { [200, 30, 30, 255] } else { [30, 30, 200, 255] });
        }
        tall.save(&path).unwrap();
        path
    }

    /// A masonry cell is as tall as its picture, so the whole picture is decoded, at the column's
    /// width: a 64 × 128 picture in a 32 px column decodes to 32 × 64, nothing cropped.
    #[gpui::test]
    fn a_width_fitted_cell_decodes_the_whole_picture_at_the_column_width(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = seam_picture(dir.path(), 2 * IMAGE);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE / 2, Fit::Width, window, cx)));
        draw(&probe, vcx, &[&path]);

        let decoded = cache.update(vcx, |cache, _| cache.loaded(&resource(&path))).expect("decoded");
        assert_eq!(decoded.size(0), gpui::size(gpui::DevicePixels(IMAGE as i32 / 2), gpui::DevicePixels(IMAGE as i32)));
        let pixels = decoded.as_bytes(0).unwrap();
        let (top, bottom) = (&pixels[..4], &pixels[pixels.len() - 4..]);
        assert_eq!((top, bottom), (&[30, 30, 200, 255][..], &[200, 30, 30, 255][..]), "BGRA: red top edge and blue bottom edge both survive");
        assert_eq!(cache.read_with(vcx, |cache, _| cache.bytes()), IMAGE_BYTES / 2, "half the width, the full height");

        // A column wider than the picture never enlarges it.
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(4 * IMAGE, Fit::Width, window, cx)));
        draw(&probe, vcx, &[&path]);
        let decoded = cache.update(vcx, |cache, _| cache.loaded(&resource(&path))).expect("redecoded");
        assert_eq!(decoded.size(0), gpui::size(gpui::DevicePixels(IMAGE as i32), gpui::DevicePixels(2 * IMAGE as i32)));
    }

    /// The column crops a picture taller than the masonry range allows (1:3), and so does the
    /// decode: a 64 × 256 picture in a 64 px column decodes to the middle 64 × 192.
    #[gpui::test]
    fn a_width_fitted_cell_crops_a_picture_taller_than_the_masonry_range(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = seam_picture(dir.path(), 4 * IMAGE);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE, Fit::Width, window, cx)));
        draw(&probe, vcx, &[&path]);

        let decoded = cache.update(vcx, |cache, _| cache.loaded(&resource(&path))).expect("decoded");
        assert_eq!(decoded.size(0), gpui::size(gpui::DevicePixels(IMAGE as i32), gpui::DevicePixels(3 * IMAGE as i32)));
        let pixels = decoded.as_bytes(0).unwrap();
        let (top, middle, bottom) = (&pixels[..4], &pixels[pixels.len() / 2..pixels.len() / 2 + 4], &pixels[pixels.len() - 4..]);
        assert_eq!((top, bottom), (&[30, 30, 200, 255][..], &[200, 30, 30, 255][..]), "cropped equally at both ends");
        assert_eq!(middle, &[200, 30, 30, 255][..], "the seam is still in the middle, so the crop was centred");
        assert_eq!(cache.read_with(vcx, |cache, _| cache.bytes()), 3 * IMAGE_BYTES, "at most three square tiles");
    }

    /// Nudging a window edge must not re-decode the feed on every frame.
    #[gpui::test]
    fn a_target_within_the_same_step_is_not_a_change(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(100, Fit::Contain, window, cx)));
        draw(&probe, vcx, &[&a]);
        let before = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("a decoded");

        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(101, Fit::Contain, window, cx)));

        let after = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("still there");
        assert_eq!(before.id, after.id, "the same decode was kept");
    }

    /// The budget is only safe while it exceeds what one frame can ask for; below that the feed
    /// would evict tiles it is about to redraw. Now that cells decode at the size they are drawn,
    /// a screenful costs about the window's own area in device pixels, which is roughly the same at
    /// every zoom since smaller tiles mean proportionally more of them. So this checks the largest
    /// window Majik plausibly runs in, at every zoom level, on a 2x display.
    /// See [`FEED_IMAGE_BUDGET`].
    #[test]
    fn budget_covers_the_largest_frame_the_feed_can_draw() {
        // A 6K XDR display at its default scaling, and a 5K 27".
        for (width, height) in [(3008., 1692.), (2560., 1440.)] {
            for zoom in majik_core::feed::ZOOM_LEVELS {
                let columns = majik_core::feed::columns_for(width, zoom) as f32;
                // Cells fill the width, so they run from the zoom level up to nearly twice it.
                let cell = (width - majik_core::feed::GRID_GAP * (columns - 1.)) / columns;
                // The grid keeps a row of overscan on each side of the viewport.
                let rows = (height / (cell + majik_core::feed::GRID_GAP)).ceil() + 3.;
                let decoded = (cell * 2.).ceil() as usize; // 2x display
                let frame = (columns * rows) as usize * decoded * decoded * 4;
                assert!(
                    frame <= FEED_IMAGE_BUDGET,
                    "a {width}x{height} window at zoom {zoom} draws {frame} B in one frame, over the {FEED_IMAGE_BUDGET} B budget"
                );
                // Masonry: each column is a strip of tiles at the column's width; the strip covers
                // the viewport plus a cell's width of overscan on each side, and at each end a
                // cell as tall as the range allows (3 × the width) may straddle it.
                let strip = ((height + 2. * cell + 2. * 3. * cell) * 2.).ceil() as usize;
                let masonry = columns as usize * decoded * strip * 4;
                assert!(
                    masonry <= FEED_IMAGE_BUDGET,
                    "a {width}x{height} window at zoom {zoom} draws {masonry} B of masonry in one frame, over the {FEED_IMAGE_BUDGET} B budget"
                );
            }
        }
    }
}
