//! Thumbnail sidecars stored under the blob key `.majik/thumbs/<hash>.<ext>`, where the hash of
//! (path, mtime, size) invalidates automatically when the source changes.

use anyhow::{anyhow, Result};
use image::imageops::FilterType;
use majik_storage::BlobStore;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::model::{Generation, MediaType};
use crate::video;

/// Long edge of stored thumbnails. Enough for a cell up to 400 device pixels, which is every zoom
/// level but the largest on a 2x display.
pub const THUMB_MAX: u32 = 400;

/// Every stored tier, smallest first. The tiers of one source are siblings of a single file: they
/// are rendered, kept and deleted together, so anything walking `.majik/thumbs` must consider all of
/// them (see [`sized_thumb_path`]).
pub const TIERS: [u32; 2] = [THUMB_MAX, THUMB_LARGE];

/// *Short* edge of the second tier, rendered on demand for cells the standard tier can't fill (at
/// the largest zoom a cell reaches ~500 device pixels on a wide window, and up to ~960 on a narrow
/// one). Drawing the standard tier there means stretching it, so the tiles go soft exactly when
/// they are big enough for anyone to notice. It bounds the short edge, unlike the standard tier,
/// because square cells crop to it: a 9:16 picture at 800 px on its long edge is 450 px wide, and
/// a square cell of 700 device px would stretch that by half again. Bounding the short edge covers
/// a square cell up to 800 px whatever the picture's shape, and a letterboxed cell further still.
pub const THUMB_LARGE: u32 = 800;

/// How a cell draws a picture: whole, letterboxed so the long edge spans the cell, or filling it,
/// so the short edge spans the cell and the long edge is cropped. Which edge spans the cell decides
/// how many pixels a tier has to carry to draw sharp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Contain,
    Cover,
}

/// Which edge a tier's size bounds: the standard tier's long edge (the whole picture at 400 px,
/// what the detail's input cards and every letterboxed cell want) and the large tier's short edge
/// (see [`THUMB_LARGE`]).
pub fn tier_fit(tier: u32) -> Fit {
    if tier == THUMB_MAX {
        Fit::Contain
    } else {
        Fit::Cover
    }
}

/// The tier a cell `cell_px` device pixels on a side needs for a picture of `aspect_ratio`
/// (width / height) drawn with `fit`: the standard one while the edge that spans the cell is at
/// least as long as the cell, the large one after. Letterboxed, that edge is the long one, 400 px.
/// Filling a square cell it is the short one, `400 × short / long`, so a portrait picture outgrows
/// the standard tier at a cell a fraction of the size a square one does. A picture whose size is
/// not known yet is treated as square.
pub fn tier_for(cell_px: u32, fit: Fit, aspect_ratio: Option<f32>) -> u32 {
    let spanning = match fit {
        Fit::Contain => 1.,
        Fit::Cover => aspect_ratio.filter(|ratio| ratio.is_finite() && *ratio > 0.).map_or(1., |ratio| if ratio > 1. { 1. / ratio } else { ratio }),
    };
    if cell_px as f32 <= THUMB_MAX as f32 * spanning {
        THUMB_MAX
    } else {
        THUMB_LARGE
    }
}

/// `image` shrunk so the edge `fit` spans is at most `size`; never enlarged. `DynamicImage::resize`
/// would happily blow a 120 px source up to the tier's size, which costs disk and decode time for
/// no detail, and the video poster code has never done it.
fn shrink_to(image: image::DynamicImage, size: u32, fit: Fit) -> image::DynamicImage {
    let (width, height) = (image.width(), image.height());
    let spanning = match fit {
        Fit::Contain => width.max(height),
        Fit::Cover => width.min(height),
    };
    if spanning <= size || spanning == 0 {
        return image;
    }
    let scale = size as f64 / spanning as f64;
    let scaled = |edge: u32| ((edge as f64 * scale).round() as u32).max(1);
    // Triangle: cheap, and a thumbnail is drawn near its own size.
    image.resize_exact(scaled(width), scaled(height), FilterType::Triangle)
}

pub fn thumb_key(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(path.to_string_lossy().as_bytes());
    h.update(mtime.to_le_bytes());
    h.update(meta.len().to_le_bytes());
    Ok(hex::encode(&h.finalize()[..12]))
}

/// Directory (blob-key prefix) every thumbnail lives under.
pub const THUMBS_PREFIX: &str = ".majik/thumbs";

/// Blob key for a thumbnail with the given hash, tier and extension. The standard tier keeps the
/// bare `<hash>.<ext>` it has always had; the large tier is suffixed with what its size bounds
/// (`<hash>@fill800.<ext>`), so the two live side by side and a file from before the large tier
/// bounded the short edge is never mistaken for one.
fn thumb_blob_key(hash: &str, tier: u32, ext: &str) -> String {
    match tier_fit(tier) {
        Fit::Contain => format!("{THUMBS_PREFIX}/{hash}.{ext}"),
        Fit::Cover => format!("{THUMBS_PREFIX}/{hash}@fill{tier}.{ext}"),
    }
}

/// The path a tier of the thumbnail at `standard` (a [`THUMB_MAX`] one) has, whether or not it has
/// been rendered yet. A pure path operation, so a render pass can ask for it without touching disk.
pub fn sized_thumb_path(standard: &Path, tier: u32) -> Option<PathBuf> {
    if tier_fit(tier) == Fit::Contain {
        return Some(standard.to_path_buf());
    }
    let stem = standard.file_stem()?.to_string_lossy();
    let extension = standard.extension()?.to_string_lossy();
    Some(standard.with_file_name(format!("{stem}@fill{tier}.{extension}")))
}

/// Blob key of a stored thumbnail from the local path the library recorded for it.
pub fn thumb_key_for_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    Some(format!("{THUMBS_PREFIX}/{name}"))
}

/// Generates the thumbnail of an item's output file if missing and returns its local path.
pub fn ensure_thumbnail(item: &Generation, store: &dyn BlobStore) -> Result<PathBuf> {
    let path = item.path.as_ref().ok_or_else(|| anyhow!("item has no file"))?;
    ensure_thumbnail_for(path, item.media_type, store)
}

/// Generates the standard ([`THUMB_MAX`]) thumbnail of a file if missing and returns its local path
/// (materialized from the store). Safe to call from a background thread.
pub fn ensure_thumbnail_for(path: &Path, kind: MediaType, store: &dyn BlobStore) -> Result<PathBuf> {
    ensure_thumbnail_sized(path, kind, THUMB_MAX, store)
}

/// [`ensure_thumbnail_for`] at a given tier ([`THUMB_MAX`] or [`THUMB_LARGE`]). A source smaller
/// than the tier is stored as it is: the tiers are a maximum, never an upscale.
pub fn ensure_thumbnail_sized(path: &Path, kind: MediaType, tier: u32, store: &dyn BlobStore) -> Result<PathBuf> {
    let hash = thumb_key(path)?;
    let fit = tier_fit(tier);
    match kind {
        MediaType::Image => {
            let img = image::open(path)?;
            let ext = if img.color().has_alpha() { "png" } else { "jpg" };
            let key = thumb_blob_key(&hash, tier, ext);
            if store.exists(&key) {
                return store.local_path(&key);
            }
            let bytes = encode_thumbnail(&shrink_to(img, tier, fit), ext)?;
            store.put(&key, &bytes)?;
            store.local_path(&key)
        }
        MediaType::Video => {
            let key = thumb_blob_key(&hash, tier, "jpg");
            if store.exists(&key) {
                return store.local_path(&key);
            }
            // The whole frame, shrunk here like a still picture, so both tiers bound the edge
            // they say they do.
            let poster = video::poster(path, u32::MAX)?;
            let bytes = encode_thumbnail(&shrink_to(image::DynamicImage::ImageRgba8(poster), tier, fit), "jpg")?;
            store.put(&key, &bytes)?;
            store.local_path(&key)
        }
        MediaType::Audio => Err(anyhow!("audio has no thumbnail")),
    }
}

fn encode_thumbnail(thumb: &image::DynamicImage, ext: &str) -> Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    if ext == "png" {
        thumb.to_rgba8().write_to(&mut out, image::ImageFormat::Png)?;
    } else {
        let rgb = thumb.to_rgb8();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
        enc.encode_image(&rgb)?;
    }
    Ok(out.into_inner())
}

/// Header-only dimension read for images.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    image::ImageReader::open(path).ok()?.into_dimensions().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;

    #[test]
    fn video_poster_is_stored_as_jpeg_under_thumbs() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        let id = library.add_generating(MediaType::Video, None, Some("mock".into()), Some("Mock".into()), None);
        let clip = video::encode_solid_clip(96, 64, 2, [200, 100, 50]).unwrap();
        library.complete_generation(&id, &clip, false).unwrap();
        let item = library.get(&id).unwrap().clone();

        let path = ensure_thumbnail(&item, library.blobs().as_ref()).unwrap();
        assert!(path.starts_with(dir.path().join(THUMBS_PREFIX)), "{}", path.display());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("jpg"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "not a JPEG");
        let poster = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!((poster.width(), poster.height()), (96, 64));
        let px = poster.get_pixel(48, 32).0;
        assert!(px.iter().zip([200u8, 100, 50]).all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= 10), "{px:?}");

        assert_eq!(ensure_thumbnail(&item, library.blobs().as_ref()).unwrap(), path, "second call is a cache hit");
    }

    #[test]
    fn tiers_live_side_by_side_and_never_upscale() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        let id = library.add_generating(MediaType::Image, None, None, None, None);
        // Bigger than both tiers on every edge, so each one is a real resize.
        library.complete_generation(&id, &crate::images::gradient_png(1200, 900, 3), false).unwrap();
        let item = library.get(&id).unwrap().clone();
        let path = item.path.clone().unwrap();
        let store = library.blobs();

        let standard = ensure_thumbnail_for(&path, MediaType::Image, store.as_ref()).unwrap();
        let large = ensure_thumbnail_sized(&path, MediaType::Image, THUMB_LARGE, store.as_ref()).unwrap();
        assert_ne!(standard, large, "each tier is its own file");
        assert_eq!(sized_thumb_path(&standard, THUMB_LARGE).as_deref(), Some(large.as_path()), "and the large one's path is derivable from the standard one");
        assert_eq!(sized_thumb_path(&standard, THUMB_MAX).as_deref(), Some(standard.as_path()));
        assert!(large.to_string_lossy().ends_with("@fill800.jpg"), "{}", large.display());
        assert_eq!(image_dimensions(&standard).unwrap(), (THUMB_MAX, 300), "the standard tier bounds the long edge");
        assert_eq!(image_dimensions(&large).unwrap(), (1067, THUMB_LARGE), "the large tier bounds the short edge");
        assert!(standard.exists() && large.exists(), "both stay on disk");

        // A source smaller than the tier is stored as it is.
        let small = library.add_generating(MediaType::Image, None, None, None, None);
        library.complete_generation(&small, &crate::images::gradient_png(120, 90, 4), false).unwrap();
        let small_path = library.get(&small).unwrap().path.clone().unwrap();
        let thumb = ensure_thumbnail_sized(&small_path, MediaType::Image, THUMB_LARGE, store.as_ref()).unwrap();
        assert_eq!(image_dimensions(&thumb).unwrap(), (120, 90), "no upscale");
    }

    /// A portrait picture in a square cell is cropped to its short edge, so that is the edge the
    /// large tier has to carry: at 800 px on the long edge a 9:16 picture is 450 px wide, and a
    /// 700 px cell stretches it by half again. A clip's poster is bounded the same way.
    #[test]
    fn large_tier_of_a_portrait_picture_is_800_wide_and_taller() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        let store = library.blobs();
        let id = library.add_generating(MediaType::Image, None, None, None, None);
        library.complete_generation(&id, &crate::images::gradient_png(900, 1800, 5), false).unwrap();
        let path = library.get(&id).unwrap().path.clone().unwrap();
        let standard = ensure_thumbnail_for(&path, MediaType::Image, store.as_ref()).unwrap();
        let large = ensure_thumbnail_sized(&path, MediaType::Image, THUMB_LARGE, store.as_ref()).unwrap();
        assert_eq!(image_dimensions(&standard).unwrap(), (200, THUMB_MAX));
        assert_eq!(image_dimensions(&large).unwrap(), (THUMB_LARGE, 1600));

        let clip = library.add_generating(MediaType::Video, None, None, None, None);
        library.complete_generation(&clip, &video::encode_solid_clip(96, 64, 2, [200, 100, 50]).unwrap(), false).unwrap();
        let clip_path = library.get(&clip).unwrap().path.clone().unwrap();
        let poster = ensure_thumbnail_sized(&clip_path, MediaType::Video, THUMB_LARGE, store.as_ref()).unwrap();
        assert_eq!(image_dimensions(&poster).unwrap(), (96, 64), "a small clip's poster is the frame as it is");
        assert!(poster.to_string_lossy().ends_with("@fill800.jpg"));
    }

    /// The tier a cell needs follows the edge that spans it, and a letterboxed cell only ever
    /// spans the long one.
    #[test]
    fn tier_for_a_cell_follows_the_edge_that_spans_it() {
        let portrait = Some(608. / 1088.);
        // A square cell at the largest zoom on a 2x display, filled: the standard tier's 224 px
        // short edge would be stretched three times over.
        assert_eq!(tier_for(724, Fit::Cover, portrait), THUMB_LARGE);
        // The same cell, letterboxed, is still past what 400 px can fill...
        assert_eq!(tier_for(724, Fit::Contain, portrait), THUMB_LARGE);
        // ...but a cell the standard tier's long edge spans is fine letterboxed, and not filled.
        assert_eq!(tier_for(300, Fit::Contain, portrait), THUMB_MAX);
        assert_eq!(tier_for(300, Fit::Cover, portrait), THUMB_LARGE);
        assert_eq!(tier_for(300, Fit::Cover, Some(1088. / 608.)), THUMB_LARGE, "landscape is the same story");
        assert_eq!(tier_for(223, Fit::Cover, portrait), THUMB_MAX, "until the cell is inside the short edge");
        // A square picture spans a square cell with its whole edge, and an unknown size counts as one.
        assert_eq!(tier_for(400, Fit::Cover, Some(1.)), THUMB_MAX);
        assert_eq!(tier_for(400, Fit::Cover, None), THUMB_MAX);
        assert_eq!(tier_for(401, Fit::Cover, None), THUMB_LARGE);
    }

    #[test]
    fn video_thumbnail_reports_undecodable_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        let id = library.add_generating(MediaType::Video, None, None, None, None);
        library.complete_generation(&id, b"not really media", false).unwrap();
        let item = library.get(&id).unwrap().clone();
        assert!(ensure_thumbnail(&item, library.blobs().as_ref()).is_err());
    }
}
