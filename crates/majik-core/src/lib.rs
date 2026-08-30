//! Pure-Rust domain logic for Majik: library model, thumbnails,
//! selection/feed rules and the mock provider. No GPUI here.

pub mod db;
pub mod feed;
pub mod images;
pub mod library;
pub mod model;
pub mod selection;
pub mod thumbnails;
pub mod video;

pub use library::{content_hash, FeedFilter, Library, MediaFilter};
pub use model::{Album, AlbumId, Asset, AssetId, Entry, EntryId, GenerationId, GenerationInput, Generation, MediaType, Status, ToolId};
pub use selection::{Modifiers, Selection};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
