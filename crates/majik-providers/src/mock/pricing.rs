//! Mock's prices. Synthetic — Mock generates nothing and charges nothing — but deterministic, so
//! the app's composer tests can assert real cost arithmetic without pinning a live provider's
//! figures, which would rot the suite the first time fal changes a number.

use crate::catalog;
use crate::pricing::{flat, per_character, per_second, Estimate, PricedJob};

/// $0.01 per image.
const IMAGE: u64 = 10_000;
/// $0.10 per second of video, $0.15 with audio.
const VIDEO_PER_SECOND: u64 = 100_000;
const VIDEO_PER_SECOND_WITH_AUDIO: u64 = 150_000;
/// $0.0001 per spoken character.
const AUDIO_PER_CHARACTER: u64 = 100;
/// $0.02 per tool run.
const TOOL: u64 = 20_000;

/// The one model Mock deliberately reports no price for, so the composer's "no estimate" path is
/// reachable in the running app and in tests — the same idea as the `#fail:` prompt directives.
pub const UNPRICED_MODEL_ID: &str = catalog::image::FLUX_1_SCHNELL.id;

pub fn pricing(job: &PricedJob<'_>) -> Estimate {
    match job {
        PricedJob::Image(settings) if settings.model.id == UNPRICED_MODEL_ID => Estimate::Unknown,
        PricedJob::Image(_) => flat(IMAGE),
        PricedJob::Video(settings) => {
            let rate = if settings.audio_enabled { VIDEO_PER_SECOND_WITH_AUDIO } else { VIDEO_PER_SECOND };
            per_second(rate, settings.duration)
        }
        PricedJob::Audio { characters, .. } => per_character(AUDIO_PER_CHARACTER, *characters),
        PricedJob::Tool(_) => flat(TOOL),
    }
}
