//! Multi-representation clipboard writes (port of `ClipboardService.copyMedia`): each item is placed
//! on the pasteboard as both a file URL (Finder / Mail get the file) and its raw bytes under the
//! native type (image / audio editors).
//!
//! There is no portable way to do this. gpui's clipboard carries a `ClipboardEntry::ExternalPaths`
//! variant, but every backend drops it on write (`gpui_macos::pasteboard` and
//! `gpui_windows::clipboard` both match it to `{}`), so gpui can put an image or a string on the
//! clipboard, never a file. The multi-entry case is not portable either: Windows writes each entry
//! (an image *and* a string), while macOS concatenates the strings and discards the image. So the
//! portable code writes one flavour and this module adds the files themselves where it can: macOS
//! writes the file URLs and the bytes together in [`copy_media`], Windows adds a `CF_HDROP` list
//! beside whatever was written with [`add_file_references`]. Linux can do neither yet: its clipboard
//! is owned by the display-server connection gpui holds, so a second owner in the same process would
//! conflict with it. That needs `ExternalPaths` support upstream in gpui.

use std::path::PathBuf;

/// One media file to copy.
#[derive(Clone, Debug)]
pub struct ClipboardMedia {
    pub path: PathBuf,
    /// UTType-ish content type of the raw bytes (`image/png`, `video/mp4`, `audio/mpeg`).
    pub content_type: String,
}

/// Whether [`copy_media`] writes the files and their bytes together on this platform.
pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// Whether [`add_file_references`] puts the files themselves on the clipboard on this platform.
/// macOS needs no second step ([`copy_media`] already wrote them); Linux has no way to yet.
pub const ADDS_FILE_REFERENCES: bool = cfg!(target_os = "windows");

/// Add `paths` to the clipboard as the files themselves, keeping what is already there, so a paste
/// into Explorer or a mail composer yields the files while a paste into an image editor still gets
/// the bitmap the portable code wrote. Opening the clipboard does not empty it, so the `CF_HDROP`
/// list sits beside the existing formats rather than replacing them.
#[cfg(target_os = "windows")]
pub fn add_file_references(paths: &[PathBuf]) -> anyhow::Result<()> {
    use anyhow::anyhow;
    use clipboard_win::{formats, Clipboard, Setter as _};

    if paths.is_empty() {
        return Ok(());
    }
    let paths: Vec<String> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
    // The clipboard is a single global resource; another process may hold it for a moment.
    let _clipboard = Clipboard::new_attempts(10).map_err(|e| anyhow!("opening the clipboard: {e}"))?;
    formats::FileList.write_clipboard(&paths).map_err(|e| anyhow!("writing the file list: {e}"))?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn add_file_references(_paths: &[PathBuf]) -> anyhow::Result<()> {
    anyhow::bail!("file references are not implemented on this platform yet")
}

#[cfg(target_os = "macos")]
#[allow(unused_unsafe)]
pub fn copy_media(items: &[ClipboardMedia]) -> anyhow::Result<()> {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardWriting};
    use objc2_foundation::{ns_string, NSArray, NSData, NSString, NSURL};

    MainThreadMarker::new().ok_or_else(|| anyhow::anyhow!("copy_media must run on the main thread"))?;
    if items.is_empty() {
        return Ok(());
    }
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();

    let mut pb_items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = Vec::new();
    for item in items {
        let Some(path) = item.path.to_str() else { continue };
        let url = unsafe { NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), false) };  // safe wrapper varies by version
        let pb_item = NSPasteboardItem::new();
        if let Some(url_str) = url.absoluteString() {
            let url_data = NSData::with_bytes(url_str.to_string().as_bytes());
            unsafe { pb_item.setData_forType(&url_data, NSPasteboardTypeFileURL) };
        }
        if let Ok(bytes) = std::fs::read(&item.path) {
            let ty: &NSString = match item.content_type.as_str() {
                "image/png" => ns_string!("public.png"),
                "image/jpeg" => ns_string!("public.jpeg"),
                "video/mp4" | "video/mpeg" => ns_string!("public.mpeg-4"),
                "audio/mpeg" | "audio/mp3" => ns_string!("public.mp3"),
                "audio/wav" => ns_string!("com.microsoft.waveform-audio"),
                other => &NSString::from_str(other),
            };
            let data = NSData::with_bytes(&bytes);
            let ty: &NSPasteboardType = ty;
            unsafe { pb_item.setData_forType(&data, ty) };
        }
        pb_items.push(ProtocolObject::from_retained(pb_item));
    }
    let array = NSArray::from_retained_slice(&pb_items);
    pb.writeObjects(&array);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn copy_media(_items: &[ClipboardMedia]) -> anyhow::Result<()> {
    anyhow::bail!("multi-representation clipboard is not implemented on this platform yet")
}
