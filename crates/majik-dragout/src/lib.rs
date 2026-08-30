//! Native drag-out of files from the app to Finder / other apps, with real bitmap previews and both a
//! file-URL and raw-bytes representation per item.
//!
//! On macOS this starts an `NSDraggingSession` on the window's content view. Because Majik's media are
//! plain files on disk, we drag the file paths directly (no temp staging). Other platforms are stubs
//! until an OLE / XDND backend is added (see the migration plan, Phase 7/8).

use raw_window_handle::RawWindowHandle;
use std::path::PathBuf;

pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// One file to drag out, with an optional preview bitmap (PNG/JPEG bytes) shown under the cursor.
#[derive(Clone)]
pub struct DragItem {
    pub path: PathBuf,
    /// Encoded image bytes (PNG/JPEG) for the drag preview; falls back to the file's Finder icon.
    pub preview: Option<Vec<u8>>,
}

impl DragItem {
    pub fn new(path: PathBuf) -> Self {
        Self { path, preview: None }
    }
    pub fn with_preview(path: PathBuf, preview: Option<Vec<u8>>) -> Self {
        Self { path, preview }
    }
}

/// Begin a copy drag of `items` from the given window. Must be called on the UI thread during the
/// mouse-down/drag that initiated it (AppKit requires the current event to be a left-mouse drag).
#[cfg(target_os = "macos")]
#[allow(unused_unsafe)]
pub fn begin_drag(window: RawWindowHandle, items: &[DragItem], cell_size: f64) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{
        NSApplication, NSDraggingImageComponent, NSDraggingImageComponentIconKey, NSDraggingItem, NSImage, NSPasteboardItem,
        NSPasteboardTypeFileURL, NSView, NSWorkspace,
    };
    use objc2_foundation::{NSArray, NSData, NSPoint, NSRect, NSSize, NSString, NSURL};

    let RawWindowHandle::AppKit(h) = window else { return Err(anyhow!("not an AppKit window")) };
    let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("begin_drag must run on the main thread"))?;
    if items.is_empty() {
        return Err(anyhow!("nothing to drag"));
    }
    let view: &NSView = unsafe { &*(h.ns_view.as_ptr() as *const NSView) };
    let app = NSApplication::sharedApplication(mtm);
    let event = app.currentEvent().ok_or_else(|| anyhow!("no current event; begin_drag must be called during a mouse drag"))?;

    let source = source::make_source(mtm);
    let source_proto: &ProtocolObject<dyn objc2_app_kit::NSDraggingSource> = ProtocolObject::from_ref(&*source);

    let size = NSSize::new(cell_size, cell_size);
    // Anchor the drag image on the cursor: convert the current event's window location into the
    // view's (flipped) coordinate space, then centre the preview there. Without this the image
    // starts at the view origin (a corner) instead of under the pointer.
    let cursor = view.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
    let mut dragging_items: Vec<Retained<NSDraggingItem>> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let path = item.path.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?;
        let url = unsafe { NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), false) };  // objc2 marks this unsafe on some versions

        let pb_item = NSPasteboardItem::new();
        // File URL so Finder / Mail get the real file.
        let url_string = unsafe { url.absoluteString() }.ok_or_else(|| anyhow!("no url string"))?;
        let url_data = NSData::with_bytes(url_string.to_string().as_bytes());
        unsafe { pb_item.setData_forType(&url_data, NSPasteboardTypeFileURL) };

        let dragging_item = unsafe { NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*pb_item)) };

        // Centre the preview on the cursor, cascading each extra item by 8pt so a multi-item drag fans out.
        let offset = i as f64 * 8.0;
        let origin = NSPoint::new(cursor.x - cell_size / 2.0 + offset, cursor.y - cell_size / 2.0 + offset);
        dragging_item.setDraggingFrame(NSRect::new(origin, size));

        // Preview: the actual bitmap when we have one, else the file's type icon.
        let preview: Option<Retained<NSImage>> = item
            .preview
            .as_ref()
            .and_then(|bytes| {
                let data = NSData::with_bytes(bytes);
                unsafe { NSImage::initWithData(NSImage::alloc(), &data) }
            })
            .or_else(|| {
                let workspace = unsafe { NSWorkspace::sharedWorkspace() };
                Some(unsafe { workspace.iconForFile(&NSString::from_str(path)) })
            });
        if let Some(image) = preview {
            let comp = unsafe { NSDraggingImageComponent::draggingImageComponentWithKey(NSDraggingImageComponentIconKey) };
            unsafe { comp.setContents(Some(&image)) };
            comp.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
            // Build the components array once and hold it inside the block so it stays alive for
            // every invocation (AppKit calls this lazily). Returning a pointer into a temporary
            // `Retained` that dropped at the end of the block would be a use-after-free.
            let components = NSArray::from_retained_slice(std::slice::from_ref(&comp));
            let provider = block2::RcBlock::new(move || std::ptr::NonNull::from(&*components));
            unsafe { dragging_item.setImageComponentsProvider(Some(&provider)) };
        }
        dragging_items.push(dragging_item);
    }

    let array = NSArray::from_retained_slice(&dragging_items);
    let session = view.beginDraggingSessionWithItems_event_source(&array, &event, source_proto);
    // Session runs on AppKit's run loop; keep the source alive until it ends.
    source::retain_until_end(&session, source);
    let _ = &session;
    Ok::<(), anyhow::Error>(()).context("begin drag")
}

#[cfg(not(target_os = "macos"))]
pub fn begin_drag(_window: RawWindowHandle, _items: &[DragItem], _cell_size: f64) -> anyhow::Result<()> {
    anyhow::bail!("drag-out is not implemented on this platform yet")
}

#[cfg(target_os = "macos")]
mod source {
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol};
    use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSDragOperation, NSDraggingContext, NSDraggingSession, NSDraggingSource};
    use std::cell::RefCell;

    define_class!(
        // SAFETY: superclass NSObject has no subclassing requirements; no Drop; main-thread only
        // because NSDraggingSource requires it.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "MajikDragSource"]
        pub struct DragSource;

        unsafe impl NSObjectProtocol for DragSource {}

        unsafe impl NSDraggingSource for DragSource {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn operation_mask(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
                // NSDragOperationCopy
                NSDragOperation(1)
            }

            #[unsafe(method(draggingSession:endedAtPoint:operation:))]
            fn ended(&self, _session: &NSDraggingSession, _point: objc2_foundation::NSPoint, _op: NSDragOperation) {
                // Drop *this* source (the one whose drag just ended) from the keep-alive set.
                let me = self as *const DragSource;
                ACTIVE.with(|a| a.borrow_mut().retain(|s| !std::ptr::eq(Retained::as_ptr(s), me)));
            }
        }
    );

    thread_local! {
        // Sources are retained here for the life of their session so AppKit's async callbacks stay valid.
        static ACTIVE: RefCell<Vec<Retained<DragSource>>> = const { RefCell::new(Vec::new()) };
    }

    pub fn make_source(mtm: MainThreadMarker) -> Retained<DragSource> {
        let _ = mtm;
        unsafe { msg_send![DragSource::alloc(mtm), init] }
    }

    pub fn retain_until_end(_session: &NSDraggingSession, source: Retained<DragSource>) {
        ACTIVE.with(|a| a.borrow_mut().push(source));
    }

    // `DefinedClass` is derived by define_class!; referenced to keep the import used.
    #[allow(unused_imports)]
    use DefinedClass as _;
}
