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
//! evicted once its decode has landed *and* been charged to the budget. A load in flight is never
//! cancelled, and every decode is charged exactly once — by [`LruImageCache::settle`], from the task
//! that awaits it, rather than the next time the image is asked for. That matters: a tile can
//! scroll off before its decode finishes, and an entry nothing ever asks for again would otherwise
//! stay uncharged, invisible to the budget, forever — which is the leak this cache exists to stop.

use futures::FutureExt as _;
use gpui::{App, AppContext as _, Asset as _, AssetLogger, Entity, ImageAssetLoader, ImageCache, ImageCacheError, ImageCacheItem, RenderImage, Resource, WeakEntity, Window};
use image::imageops::FilterType;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Decoded bytes the feed's thumbnails may occupy before the least recently drawn ones are dropped.
///
/// A thumbnail is 400 px on its long edge (`majik_core::thumbnails::THUMB_MAX`) and decodes to
/// BGRA, so one costs at most 400 × 400 × 4 = 640 KB. The budget has to exceed what a single frame
/// draws, or a frame would evict a tile it is about to ask for again and decode in a loop. At the
/// smallest zoom (`majik_core::feed::ZOOM_LEVELS[0]` = 90 px, a pitch of ~92 px) a W × H viewport
/// draws roughly (W / 92) × (H / 92 + 3) tiles — the `+ 3` being the row of overscan the grid keeps
/// on each side:
///
/// - 2048 × 1121 (the window this was measured on) ≈ 22 × 16 = 352 tiles ≈ 225 MB
/// - 2560 × 1440 ≈ 27 × 19 = 513 tiles ≈ 328 MB
/// - 3008 × 1692 (6K XDR at default scaling) ≈ 32 × 22 = 704 tiles ≈ 450 MB
///
/// 512 MB covers all three, with room to spare in practice: cells fill the width and most
/// thumbnails are not square, so 640 KB is the ceiling rather than the average. Raise it for a
/// bigger display; lowering it below the table above would make the feed decode continuously at the
/// smallest zoom. `budget_covers_the_largest_frame_the_feed_can_draw` guards that.
pub const FEED_IMAGE_BUDGET: usize = 256 * 1024 * 1024;

/// Decoded bytes the detail view may hold in full-size images.
///
/// The stage decodes what `paging::visible_slots` lists — the item plus up to two neighbours on
/// each side, so that paging is instant — and the same rule applies as above: the budget has to
/// exceed one frame's demand. The largest output the catalog can produce is 4K
/// (`ImageResolution::Uhd`, 3840 px on the long edge), i.e. 59 MB decoded for a square one and
/// ~33 MB for 16:9, so five slots ask for at most ~295 MB. 384 MB covers that with room for the
/// before/after compare (two images at once) and the info panel's input cards. Beyond 4K — an
/// imported 8K asset, say — paging back and forth re-decodes instead of holding both, which is the
/// right trade: one image that large is 268 MB on its own.
pub const DETAIL_IMAGE_BUDGET: usize = 192 * 1024 * 1024;

pub struct LruImageCache {
    /// Handed to the task awaiting each decode so it can charge the bytes when they land.
    this: WeakEntity<Self>,
    budget: usize,
    /// Decoded bytes the cache references — what `budget` bounds. Bytes the cache *references*,
    /// not bytes it uniquely owns: if a caller ever stashes an `Arc<RenderImage>` from here,
    /// eviction stops being a release.
    bytes: usize,
    /// Keyed by `gpui::hash(&Resource)`, the key gpui's own caches use.
    entries: HashMap<u64, Entry>,
    /// Bumped on every load; an entry's `last_used` is a stamp from it, which makes "least recently
    /// drawn" a `min` over the map instead of a second collection to keep in sync.
    tick: u64,
    /// Longest edge, in device pixels, to decode at. The feed sets it from the cell it is about to
    /// draw into: a 400 px thumbnail costs 640 KB decoded whatever it is drawn at, and at the
    /// smallest zoom a cell is under 200 device px, so decoding at the file's size spends five
    /// times the memory (and five times the sprite atlas) on pixels nobody sees. `None` decodes the
    /// file as it is — what the detail's stage wants, since it fills the window.
    target: Option<u32>,
}

struct Entry {
    item: ImageCacheItem,
    /// Decoded bytes charged to the budget: 0 while the decode is in flight, and 0 for good if it
    /// failed — a handful of bytes per broken thumbnail, kept so a failing file isn't re-decoded on
    /// every frame.
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
                for (_, mut entry) in std::mem::take(&mut cache.entries) {
                    if let Some(Ok(image)) = entry.item.get() {
                        cx.drop_image(image, None);
                    }
                }
                cache.bytes = 0;
            })
            .detach();
            Self { this: cx.weak_entity(), budget, bytes: 0, entries: HashMap::new(), tick: 0, target: None }
        })
    }

    /// Charge a decode that has landed against the budget, then evict down to it. Called once per
    /// load, from the task that awaited it.
    fn settle(&mut self, key: u64, window: &mut Window, cx: &mut App) {
        let Some(entry) = self.entries.get_mut(&key) else { return };
        if entry.bytes != 0 {
            return;
        }
        // `get` turns `Loading` into `Loaded`; a failed decode stays at zero bytes and simply never
        // becomes an eviction candidate.
        let Some(Ok(image)) = entry.item.get() else { return };
        let bytes = decoded_bytes(&image);
        entry.bytes = bytes;
        self.bytes = self.bytes.saturating_add(bytes);
        self.evict(window, cx);
    }

    /// Drop least-recently-drawn images until the budget is met.
    fn evict(&mut self, window: &mut Window, cx: &mut App) {
        while self.bytes > self.budget {
            // Only a charged entry is a candidate: evicting one still loading would free nothing
            // and start the same decode again next frame. Skipping them also makes this loop
            // terminate — every iteration removes bytes.
            let victim = self.entries.iter().filter(|(_, entry)| entry.bytes > 0).min_by_key(|(_, entry)| entry.last_used).map(|(key, _)| *key);
            let Some(key) = victim else { return };
            let Some(mut entry) = self.entries.remove(&key) else { return };
            self.bytes = self.bytes.saturating_sub(entry.bytes);
            if let Some(Ok(image)) = entry.item.get() {
                // Frees the decoded bytes *and* the sprite-atlas tile. `App::remove_asset` would
                // only forget the asset and leave the GPU texture behind.
                cx.drop_image(image, Some(window));
            }
        }
    }

    /// Decode into cells `long_edge` device pixels on their longest side from now on. Entries
    /// decoded for a different size are dropped: a zoom step is rare and re-decoding a screenful is
    /// cheaper than keeping two sizes of everything.
    pub fn set_target(&mut self, long_edge: u32, window: &mut Window, cx: &mut App) {
        // Round to a step so that a resize dragging the cells a pixel at a time doesn't re-decode
        // the feed on every frame.
        let long_edge = long_edge.div_ceil(TARGET_STEP) * TARGET_STEP;
        if self.target == Some(long_edge) {
            return;
        }
        self.target = Some(long_edge);
        self.clear(window, cx);
    }

    /// Drop everything, returning the decoded bytes and the atlas tiles.
    fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.bytes = 0;
        for (_, mut entry) in std::mem::take(&mut self.entries) {
            if let Some(Ok(image)) = entry.item.get() {
                cx.drop_image(image, Some(window));
            }
        }
    }

    /// [`Self::set_target`] before anything is cached, so the tests don't need a window to clear.
    #[cfg(test)]
    pub fn set_target_for_test(&mut self, long_edge: u32) {
        self.target = Some(long_edge.div_ceil(TARGET_STEP) * TARGET_STEP);
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

    /// The decoded image for a resource, if the cache holds one.
    #[cfg(test)]
    pub fn loaded(&mut self, resource: &Resource) -> Option<Arc<RenderImage>> {
        self.entries.get_mut(&gpui::hash(resource))?.item.get()?.ok()
    }
}

impl ImageCache for LruImageCache {
    fn load(&mut self, resource: &Resource, window: &mut Window, cx: &mut App) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let key = gpui::hash(resource);
        self.tick += 1;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.tick;
            return entry.item.get();
        }

        // One shared task, held by both the entry and the waiter below, as gpui's own caches do.
        // With a target set we decode the file ourselves so the pixels land at the size they are
        // drawn at; without one, gpui's loader handles every source it knows (SVG, animated GIF,
        // remote URIs), which the detail's stage needs.
        let task = match (self.target, &resource) {
            (Some(long_edge), Resource::Path(path)) => {
                let path = path.clone();
                cx.background_executor().spawn(async move { decode_scaled(&path, long_edge) }).shared()
            }
            _ => {
                let decode = AssetLogger::<ImageAssetLoader>::load(resource.clone(), cx);
                cx.background_executor().spawn(decode).shared()
            }
        };
        self.entries.insert(key, Entry { item: ImageCacheItem::Loading(task.clone()), bytes: 0, last_used: self.tick });

        let view = window.current_view();
        let cache = self.this.clone();
        window
            .spawn(cx, async move |cx| {
                if let Err(e) = task.await {
                    // `AssetLogger` has already logged the cause; the cell keeps its placeholder.
                    tracing::debug!(target: "majik", "thumbnail decode failed: {e}");
                }
                let settled = cx.update(|window, cx| {
                    cache.update(cx, |cache, cx| cache.settle(key, window, cx)).ok();
                });
                if settled.is_err() {
                    // The window went away while the decode ran; there is nothing left to redraw.
                    return;
                }
                // An `img` only picks a decoded image up on its next frame.
                cx.on_next_frame(move |_, cx| cx.notify(view));
            })
            .detach();

        None
    }
}

/// Cell sizes are rounded up to a multiple of this before they become a decode size, so that
/// dragging a window edge doesn't re-decode the feed on every frame.
const TARGET_STEP: u32 = 32;

/// Decodes an image file, shrunk so its longest edge is at most `long_edge`. Never enlarges: a
/// thumbnail drawn bigger than it was stored is stretched by the GPU, exactly as before, rather
/// than costing memory for pixels that carry no detail.
///
/// Mirrors what gpui's own loader produces — a `RenderImage` of BGRA frames — for the still images
/// the feed draws. Anything with more than one frame or a source gpui has to fetch goes through
/// its loader instead (see the caller).
fn decode_scaled(path: &Path, long_edge: u32) -> Result<Arc<RenderImage>, ImageCacheError> {
    let bytes = std::fs::read(path).map_err(|e| ImageCacheError::Io(Arc::new(e)))?;
    let image = image::load_from_memory(&bytes).map_err(|e| ImageCacheError::Image(Arc::new(e)))?;
    let longest = image.width().max(image.height());
    let image = if longest > long_edge && long_edge > 0 {
        // Triangle: the thumbnails are already close to their drawn size, so a cheaper filter is
        // enough and this runs on the scroll path.
        image.resize(long_edge, long_edge, FilterType::Triangle)
    } else {
        image
    };
    let mut frame = image.to_rgba8();
    // gpui renders BGRA.
    for pixel in frame.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(SmallVec::from_elem(image::Frame::new(frame), 1))))
}

/// Decoded bytes an image holds. `RenderImage` keeps one 4-byte-per-pixel buffer per frame and
/// `as_bytes` hands back exactly that buffer, so this is the real cost of the `Arc` — animated
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

    /// Draw `images`, let the decodes land, and deliver the frame their completion asks for.
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

        // Eviction is driven by decodes landing, not by a timer or by a frame: memory can only grow
        // when a new image arrives, so that is the moment the budget is applied. A cache that is
        // over budget and draws nothing new therefore stays put until the next decode — which is
        // why a feed that has stopped scrolling never churns.
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

    #[gpui::test]
    fn changing_the_target_drops_what_was_decoded_for_the_old_one(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        cache.update(vcx, |cache, _| cache.set_target_for_test(IMAGE));
        draw(&probe, vcx, &[&a]);
        let large = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("a decoded");

        // A zoom step: everything held is the wrong size now.
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(IMAGE / 2, window, cx)));
        cache.read_with(vcx, |cache, _| {
            assert_eq!(cache.len(), 0, "the old sizes went");
            assert_eq!(cache.bytes(), 0);
        });
        vcx.update(|window, _| assert!(!window.has_image_atlas_entry(&large), "and gave their atlas tiles back"));

        draw(&probe, vcx, &[&a]);
        assert_eq!(cache.read_with(vcx, |cache, _| cache.bytes()), (IMAGE / 2 * IMAGE / 2 * 4) as usize, "redecoded at the new size");
    }

    /// Nudging a window edge must not re-decode the feed on every frame.
    #[gpui::test]
    fn a_target_within_the_same_step_is_not_a_change(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let a = image_file(dir.path(), "a.png", [200, 30, 30]);
        let (probe, vcx, cache) = probe(cx, 8 * IMAGE_BYTES);
        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(100, window, cx)));
        draw(&probe, vcx, &[&a]);
        let before = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("a decoded");

        vcx.update(|window, cx| cache.update(cx, |cache, cx| cache.set_target(101, window, cx)));

        let after = cache.update(vcx, |cache, _| cache.loaded(&resource(&a))).expect("still there");
        assert_eq!(before.id, after.id, "the same decode was kept");
    }

    /// The budget is only safe while it exceeds what one frame can ask for; below that the feed
    /// would evict tiles it is about to redraw. Now that cells decode at the size they are drawn,
    /// a screenful costs about the window's own area in device pixels — near enough the same at
    /// every zoom, since smaller tiles mean proportionally more of them — so this checks the
    /// largest window Majik plausibly runs in, at every zoom level, on a 2x display.
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
            }
        }
    }
}
