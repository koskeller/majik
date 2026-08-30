//! Thumbnail sidecars stored under the blob key `.majik/thumbs/<hash>.<ext>`, where the hash of
//! (path, mtime, size) invalidates automatically when the source changes.

use anyhow::{anyhow, Result};
use image::imageops::FilterType;
use majik_storage::BlobStore;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::model::{Generation, MediaType};
use crate::video;

/// Long edge of stored thumbnails. Enough for a cell up
/// to 400 device pixels — every zoom level but the largest, on a 2x display.
pub const THUMB_MAX: u32 = 400;

/// Every stored tier, smallest first. The tiers of one source are siblings of a single file: they
/// are rendered, kept and deleted together, so anything walking `.majik/thumbs` must consider all of
/// them (see [`sized_thumb_path`]).
pub const TIERS: [u32; 2] = [THUMB_MAX, THUMB_LARGE];

/// Long edge of the second tier, rendered on demand for the zoom levels whose cells are bigger
/// than [`THUMB_MAX`] (at the largest zoom a cell reaches ~500 device pixels on a wide window, and
/// up to ~960 on a narrow one). Drawing the standard tier there means stretching it, so the tiles
/// go soft exactly when they are big enough for anyone to notice.
pub const THUMB_LARGE: u32 = 800;

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

/// Blob key for a thumbnail with the given hash, size and extension. The standard tier keeps the
/// bare `<hash>.<ext>` it has always had; other tiers are suffixed, so both live side by side.
fn thumb_blob_key(hash: &str, long_edge: u32, ext: &str) -> String {
    if long_edge == THUMB_MAX {
        format!("{THUMBS_PREFIX}/{hash}.{ext}")
    } else {
        format!("{THUMBS_PREFIX}/{hash}@{long_edge}.{ext}")
    }
}

/// The path a tier of the thumbnail at `standard` (a [`THUMB_MAX`] one) has, whether or not it has
/// been rendered yet. A pure path operation, so a render pass can ask for it without touching disk.
pub fn sized_thumb_path(standard: &Path, long_edge: u32) -> Option<PathBuf> {
    if long_edge == THUMB_MAX {
        return Some(standard.to_path_buf());
    }
    let stem = standard.file_stem()?.to_string_lossy();
    let extension = standard.extension()?.to_string_lossy();
    Some(standard.with_file_name(format!("{stem}@{long_edge}.{extension}")))
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

/// [`ensure_thumbnail_for`] at a given tier. A source smaller than `long_edge` is stored as it is:
/// the tiers are a ceiling, never an upscale.
pub fn ensure_thumbnail_sized(path: &Path, kind: MediaType, long_edge: u32, store: &dyn BlobStore) -> Result<PathBuf> {
    let hash = thumb_key(path)?;
    match kind {
        MediaType::Image => {
            let img = image::open(path)?;
            let ext = if img.color().has_alpha() { "png" } else { "jpg" };
            let key = thumb_blob_key(&hash, long_edge, ext);
            if store.exists(&key) {
                return store.local_path(&key);
            }
            // Only ever shrink. `DynamicImage::resize` would happily blow a 120 px source up to
            // the tier's size, which costs disk and decode time for no detail — and the video
            // poster path has never done it.
            let bytes = if img.width().max(img.height()) > long_edge {
                encode_thumbnail(&img.resize(long_edge, long_edge, FilterType::Triangle), ext)?
            } else {
                encode_thumbnail(&img, ext)?
            };
            store.put(&key, &bytes)?;
            store.local_path(&key)
        }
        MediaType::Video => {
            let key = thumb_blob_key(&hash, long_edge, "jpg");
            if store.exists(&key) {
                return store.local_path(&key);
            }
            let poster = video::poster(path, long_edge)?;
            let bytes = encode_thumbnail(&image::DynamicImage::ImageRgba8(poster), "jpg")?;
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
        // Bigger than both tiers, so each one is a real resize.
        library.complete_generation(&id, &crate::images::gradient_png(1200, 900, 3), false).unwrap();
        let item = library.get(&id).unwrap().clone();
        let path = item.path.clone().unwrap();
        let store = library.blobs();

        let standard = ensure_thumbnail_for(&path, MediaType::Image, store.as_ref()).unwrap();
        let large = ensure_thumbnail_sized(&path, MediaType::Image, THUMB_LARGE, store.as_ref()).unwrap();
        assert_ne!(standard, large, "each tier is its own file");
        assert_eq!(sized_thumb_path(&standard, THUMB_LARGE).as_deref(), Some(large.as_path()), "and the large one's path is derivable from the standard one");
        assert_eq!(sized_thumb_path(&standard, THUMB_MAX).as_deref(), Some(standard.as_path()));
        assert_eq!(image_dimensions(&standard).unwrap().0, THUMB_MAX);
        assert_eq!(image_dimensions(&large).unwrap().0, THUMB_LARGE);
        assert!(standard.exists() && large.exists(), "both stay on disk");

        // A source smaller than the tier is stored as it is.
        let small = library.add_generating(MediaType::Image, None, None, None, None);
        library.complete_generation(&small, &crate::images::gradient_png(120, 90, 4), false).unwrap();
        let small_path = library.get(&small).unwrap().path.clone().unwrap();
        let thumb = ensure_thumbnail_sized(&small_path, MediaType::Image, THUMB_LARGE, store.as_ref()).unwrap();
        assert_eq!(image_dimensions(&thumb).unwrap(), (120, 90), "no upscale");
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
