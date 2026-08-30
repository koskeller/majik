//! Generates deterministic solid-colour MP4s for the mock provider: one H.264 keyframe per second,
//! `yuv420p`, sized from the requested aspect ratio and resolution, coloured from a hash of the
//! prompt. Encoding happens in process (`majik_core::video::encode_solid_clip`), so no ffmpeg is
//! involved and the clip is exactly what was asked for.

use crate::error::GenerationError;
use crate::models::{VideoAspectRatio, VideoModel, VideoResolution};
use crate::ProviderId;

use super::image_renderer::{self, fit_longest_edge, parse_ratio, Color};

pub async fn render(
    provider: &ProviderId,
    model: &VideoModel,
    clean_prompt: &str,
    duration: u32,
    aspect_ratio: Option<VideoAspectRatio>,
    resolution: Option<VideoResolution>,
) -> Result<Vec<u8>, GenerationError> {
    let (width, height) = pixel_size(aspect_ratio, resolution);
    let color = image_renderer::color(provider, model.id, clean_prompt, width, height);
    tokio::task::spawn_blocking(move || render_blocking(width, height, duration, color))
        .await
        .map_err(|e| GenerationError::ProviderFailed(format!("mock video: {e}")))?
}

/// The synchronous core of [`render`]: a `seconds`-long clip of one colour.
pub fn render_blocking(width: u32, height: u32, seconds: u32, color: Color) -> Result<Vec<u8>, GenerationError> {
    majik_core::video::encode_solid_clip(width, height, seconds.max(1), color).map_err(|e| GenerationError::ProviderFailed(format!("mock video: {e}")))
}

/// Keep mock videos small so tests stay fast: 480 / 720 / 1080 / 1920 longest edge.
pub fn pixel_size(aspect_ratio: Option<VideoAspectRatio>, resolution: Option<VideoResolution>) -> (u32, u32) {
    let long_edge = match resolution.unwrap_or(VideoResolution::Sd) {
        VideoResolution::Sd => 480,
        VideoResolution::Hd => 720,
        VideoResolution::Fhd => 1080,
        VideoResolution::Uhd => 1920,
    };
    let ratio = aspect_ratio.unwrap_or(VideoAspectRatio::Landscape);
    // `Auto` has no intrinsic shape; default it to 16:9 for sizing.
    let raw = if ratio == VideoAspectRatio::Auto { "16:9" } else { ratio.raw() };
    let (num, denom) = parse_ratio(raw, (16, 9));
    fit_longest_edge(long_edge, num, denom)
}
