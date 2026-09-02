//! Replicate's prices, per model.
//!
//! **Checked 2026-08-29.** Replicate publishes each model's price in the "Run time and cost" panel
//! on its page, which the page renders client-side, but the figures ship in the HTML as a
//! `billingConfig` JSON blob, so `curl https://replicate.com/<slug>` and read
//! `current_tiers[].{criteria, prices}`. Re-check from the slug tables in [`super::capabilities`]
//! and [`super::audio`].
//!
//! Two billing shapes. Official models bill per output: a flat per-image or per-second price,
//! sometimes with a `criteria` tier on resolution and/or audio, and for the FLUX family a per-run
//! fee plus a per-output-megapixel rate. Community models (our two tools) bill compute, and their
//! page publishes only a median run cost, which is what we use.
//!
//! Do NOT guess a price. A model with no published figure belongs in `UNPRICED` in
//! `tests/shared.rs`, not in a made-up row here.

use crate::models::{ImageResolution, VideoResolution};
use crate::pricing::{flat, per_character, per_second, rate, Estimate, PricedJob, Usd};
use crate::settings::{ImageGenerationSettings, VideoGenerationSettings};

use super::capabilities::{image_ids::*, video_ids::*};
use ImageResolution::{Fhd as I2K, Hd as I1K, Sd as I05K, Uhd as I4K};
use VideoResolution::{Fhd as V1080, Hd as V720, Sd as V480};

pub fn pricing(job: &PricedJob<'_>) -> Estimate {
    match job {
        PricedJob::Image(settings) => image(settings),
        PricedJob::Video(settings) => video(settings),
        PricedJob::Audio { settings, characters } => match settings.model.id {
            // $0.10 per thousand input characters.
            "elevenlabs-v3" => per_character(100, *characters),
            _ => Estimate::Unknown,
        },
        // Both tools are community models billed by the second on their own hardware; Replicate
        // publishes a median run cost, which is the only figure to estimate from.
        PricedJob::Tool { settings, .. } => match settings.model.id {
            // philz1337x/clarity-upscaler, A100 (40GB), median $0.015 a run.
            "clarity-upscaler" => flat(15_000),
            // 851-labs/background-remover (catalog name "rembg"), T4, median $0.00054 a run.
            "rembg" => flat(540),
            _ => Estimate::Unknown,
        },
    }
}

// ----- Images -------------------------------------------------------------------------------------

fn image(settings: &ImageGenerationSettings) -> Estimate {
    let resolution = settings.resolution;
    match settings.model.id {
        // Flat per output image.
        GEMINI_25_FLASH => flat(39_000),
        SEEDREAM_45 => flat(40_000),
        RECRAFT_V4_PRO => flat(250_000),
        WAN_27_PRO => flat(30_000),
        QWEN_IMAGE_3 => flat(30_000),
        SEEDREAM_5_LITE => flat(35_000),
        GROK_IMAGINE_IMAGE_2 => flat(40_000),
        FLUX_1_DEV => flat(25_000),
        // "$3 per thousand output images".
        FLUX_1_SCHNELL => flat(3_000),

        // Per output image, tiered on the resolution we ask for.
        GEMINI_3_PRO => by_resolution(&[(I1K, 150_000), (I2K, 150_000), (I4K, 300_000)], resolution),
        GEMINI_31_FLASH => by_resolution(&[(I1K, 67_000), (I2K, 101_000), (I4K, 151_000)], resolution),
        // The GPT slugs tier on `quality`, which our resolution picker maps onto
        // (1K = low, 2K = medium, 4K = high — see `capabilities::api_image_resolution`).
        GPT_5 => by_resolution(&[(I1K, 13_000), (I2K, 50_000), (I4K, 136_000)], resolution),
        GPT_IMAGE_2 => by_resolution(&[(I1K, 12_000), (I2K, 47_000), (I4K, 128_000)], resolution),
        // seedream-5-pro tiers on `size`; its layer-decomposition tiers are a mode we never request.
        SEEDREAM_5_PRO => by_resolution(&[(I1K, 45_000), (I2K, 90_000)], resolution),

        // A per-run fee plus a per-output-megapixel rate. Unlike fal, these slugs take the
        // megapixel count outright, so the resolution picker maps straight onto what is billed.
        FLUX_2_MAX => per_output_megapixels(40_000, 30_000, resolution),
        FLUX_2_PRO => per_output_megapixels(15_000, 15_000, resolution),
        // No run fee: "$0.06 per output image megapixel".
        FLUX_2_FLEX => per_output_megapixels(0, 60_000, resolution),
        // "$1 per thousand output image megapixels".
        FLUX_2_KLEIN => per_output_megapixels(0, 1_000, resolution),
        _ => Estimate::Unknown,
    }
}

fn by_resolution(table: &[(ImageResolution, u64)], resolution: ImageResolution) -> Estimate {
    match rate(table, resolution) {
        Some(micros) => flat(micros),
        None => Estimate::Unknown,
    }
}

/// A run fee plus `per_megapixel` × the megapixels the request asks for. Replicate's FLUX slugs take
/// the count as an input (`resolution: "0.5 MP"…`, `output_megapixels: "0.5"…`), so a half-megapixel
/// request really is billed at half, with no rounding up the way fal does it.
fn per_output_megapixels(run_fee: u64, per_megapixel: u64, resolution: ImageResolution) -> Estimate {
    // Tenths of a megapixel, matching `capabilities::api_image_resolution`.
    let tenths = match resolution {
        I05K => 5,
        I1K => 10,
        I2K => 20,
        I4K => 40,
    };
    Estimate::Exact(Usd(run_fee + per_megapixel * tenths / 10))
}

// ----- Video --------------------------------------------------------------------------------------

/// Per-second rates keyed on `(resolution, audio on)`. `None` is the resolution for a model whose
/// capability table declares none: the Kling family and Replicate's Sora 2 slug.
type VideoRates = &'static [((Option<VideoResolution>, bool), u64)];

fn video(settings: &VideoGenerationSettings) -> Estimate {
    let table: VideoRates = match settings.model.id {
        // Replicate tiers Veo on audio alone. Unlike fal it publishes no resolution tier, so
        // 720p and 1080p bill the same.
        VEO_31 => &[
            ((Some(V720), false), 200_000),
            ((Some(V720), true), 400_000),
            ((Some(V1080), false), 200_000),
            ((Some(V1080), true), 400_000),
        ],
        VEO_31_FAST => &[
            ((Some(V720), false), 100_000),
            ((Some(V720), true), 150_000),
            ((Some(V1080), false), 100_000),
            ((Some(V1080), true), 150_000),
        ],
        // Lite is the other way round: resolution tiers, no audio (its capability table has the
        // audio toggle off).
        VEO_31_LITE => &[((Some(V720), false), 50_000), ((Some(V1080), false), 80_000)],
        // Replicate's sora-2 slug exposes no resolution at all.
        SORA_2 => &[((None, false), 100_000), ((None, true), 100_000)],
        // Pro tiers on `resolution: standard | high`, which our 720p / 1080p map onto.
        SORA_2_PRO => &[
            ((Some(V720), false), 300_000),
            ((Some(V720), true), 300_000),
            ((Some(V1080), false), 500_000),
            ((Some(V1080), true), 500_000),
        ],
        // kling-v3-video tiers on `mode`, which we never send — its schema default is "pro", the
        // tier this catalog model names.
        KLING_30_PRO => &[((None, false), 224_000), ((None, true), 336_000)],
        KLING_25_TURBO_PRO => &[((None, false), 70_000), ((None, true), 70_000)],
        KLING_26_PRO => &[((None, false), 70_000), ((None, true), 140_000)],
        SEEDANCE_15_PRO => &[
            ((Some(V480), false), 13_000),
            ((Some(V480), true), 25_000),
            ((Some(V720), false), 26_000),
            ((Some(V720), true), 52_000),
            ((Some(V1080), false), 60_000),
            ((Some(V1080), true), 120_000),
        ],
        // Seedance 2 charges more when a video is fed in, which we never do; these are its
        // `non_video_in` rates, and audio is included in them.
        SEEDANCE_20 => &[
            ((Some(V480), false), 80_000),
            ((Some(V480), true), 80_000),
            ((Some(V720), false), 180_000),
            ((Some(V720), true), 180_000),
            ((Some(V1080), false), 450_000),
            ((Some(V1080), true), 450_000),
        ],
        SEEDANCE_20_FAST => &[
            ((Some(V480), false), 70_000),
            ((Some(V480), true), 70_000),
            ((Some(V720), false), 150_000),
            ((Some(V720), true), 150_000),
        ],
        // Audio is always on for Happy Horse and included in the rate.
        HAPPY_HORSE_10 => &[
            ((Some(V720), true), 140_000),
            ((Some(V720), false), 140_000),
            ((Some(V1080), true), 280_000),
            ((Some(V1080), false), 280_000),
        ],
        // Replicate charges one rate for wan-2.7 whatever the resolution.
        WAN_27 => &[
            ((Some(V720), false), 100_000),
            ((Some(V720), true), 100_000),
            ((Some(V1080), false), 100_000),
            ((Some(V1080), true), 100_000),
        ],
        PIXVERSE_V6 => &[
            ((Some(V720), false), 90_000),
            ((Some(V720), true), 120_000),
            ((Some(V1080), false), 180_000),
            ((Some(V1080), true), 230_000),
        ],
        GROK_IMAGINE_VIDEO => &[
            ((Some(V480), false), 50_000),
            ((Some(V480), true), 50_000),
            ((Some(V720), false), 50_000),
            ((Some(V720), true), 50_000),
        ],
        // Replicate quotes one flat rate for grok-imagine-video-1.5, with no resolution tier.
        GROK_IMAGINE_VIDEO_15 => &[
            ((Some(V480), false), 80_000),
            ((Some(V480), true), 80_000),
            ((Some(V720), false), 80_000),
            ((Some(V720), true), 80_000),
        ],
        // Replicate tiers seedance-2.5 on resolution *and* on whether a reference video was sent.
        // We never send one, so these are the `non_video_in` rates.
        SEEDANCE_25 => &[
            ((Some(V480), false), 102_800),
            ((Some(V480), true), 102_800),
            ((Some(V720), false), 231_200),
            ((Some(V720), true), 231_200),
        ],
        // minimax/h3's two tiers are 768P and 2K, which our Hd and Fhd map onto.
        MINIMAX_H3 => &[
            ((Some(V720), false), 80_000),
            ((Some(V720), true), 80_000),
            ((Some(V1080), false), 130_000),
            ((Some(V1080), true), 130_000),
        ],
        // flux-3's `t2v_i2v` tier. The draft and video-to-video tiers are modes we never request.
        FLUX_3 => &[
            ((Some(V720), false), 170_000),
            ((Some(V720), true), 170_000),
            ((Some(V1080), false), 290_000),
            ((Some(V1080), true), 290_000),
        ],
        HAPPY_HORSE_11 => &[
            ((Some(V720), false), 140_000),
            ((Some(V720), true), 140_000),
            ((Some(V1080), false), 180_000),
            ((Some(V1080), true), 180_000),
        ],
        WAN_30 => &[
            ((Some(V480), false), 25_000),
            ((Some(V480), true), 25_000),
            ((Some(V720), false), 50_000),
            ((Some(V720), true), 50_000),
            ((Some(V1080), false), 100_000),
            ((Some(V1080), true), 100_000),
        ],
        WAN_30_PRIME => &[
            ((Some(V480), false), 68_000),
            ((Some(V480), true), 68_000),
            ((Some(V720), false), 140_000),
            ((Some(V720), true), 140_000),
            ((Some(V1080), false), 280_000),
            ((Some(V1080), true), 280_000),
        ],
        _ => return Estimate::Unknown,
    };
    match rate(table, (settings.resolution, settings.audio_enabled)) {
        Some(micros) => per_second(micros, settings.duration),
        None => Estimate::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::ToolInput;
    use crate::settings::ToolSettings;
    use crate::catalog;
    use crate::models::AspectRatio;
    use crate::settings::AudioGenerationSettings;
    use crate::voices::elevenlabs;

    /// Every figure below is one Replicate publishes in the model's own `billingConfig`, so a
    /// re-sweep that moves a price fails here with the model that moved.
    fn image_price(id: &str, resolution: ImageResolution) -> Estimate {
        let model = catalog::image::model(id).expect("catalog model").clone();
        image(&ImageGenerationSettings { model, aspect_ratio: AspectRatio::Square, resolution })
    }

    fn video_price(id: &str, resolution: Option<VideoResolution>, duration: u32, audio_enabled: bool) -> Estimate {
        let model = catalog::video::model(id).expect("catalog model").clone();
        video(&VideoGenerationSettings { model, aspect_ratio: None, resolution, duration, audio_enabled })
    }

    fn dollars(estimate: Estimate) -> String {
        estimate.amount().expect("priced").to_string()
    }

    #[test]
    fn flat_per_image_models() {
        assert_eq!(dollars(image_price("gemini-2.5-flash", I1K)), "$0.039");
        assert_eq!(dollars(image_price("seedream-4.5", I2K)), "$0.04");
        assert_eq!(dollars(image_price("recraft-4-pro", I1K)), "$0.25");
        assert_eq!(dollars(image_price("flux-1-dev", I1K)), "$0.025");
        assert_eq!(dollars(image_price("flux-1-schnell", I1K)), "$0.003", "$3 per thousand images");
    }

    #[test]
    fn resolution_tiers_move_the_per_image_price() {
        assert_eq!(dollars(image_price("gemini-3-pro", I1K)), "$0.15");
        assert_eq!(dollars(image_price("gemini-3-pro", I2K)), "$0.15", "2K bills as 1K");
        assert_eq!(dollars(image_price("gemini-3-pro", I4K)), "$0.30");
        assert_eq!(dollars(image_price("gemini-3.1-flash", I1K)), "$0.067");
        assert_eq!(dollars(image_price("gemini-3.1-flash", I4K)), "$0.15", "$0.151, shown to the cent above a dime");
        // The GPT slugs tier on quality, which our resolution picker maps onto.
        assert_eq!(dollars(image_price("gpt-image-2", I1K)), "$0.012");
        assert_eq!(dollars(image_price("gpt-image-2", I2K)), "$0.047");
        assert_eq!(dollars(image_price("gpt-image-2", I4K)), "$0.13");
        assert_eq!(dollars(image_price("gpt-5-image", I4K)), "$0.14");
    }

    #[test]
    fn a_resolution_the_slug_never_offers_has_no_price() {
        // Replicate's nano-banana slugs start at 1K, and so do their capability tables.
        assert_eq!(image_price("gemini-3-pro", I05K), Estimate::Unknown);
    }

    #[test]
    fn megapixel_models_scale_with_the_megapixels_requested() {
        // flux-2-pro is $0.015 a run plus $0.015 an output megapixel, and the slug takes the
        // megapixel count outright, so half a megapixel really is billed at half.
        assert_eq!(dollars(image_price("flux-2-pro", I05K)), "$0.023");
        assert_eq!(dollars(image_price("flux-2-pro", I1K)), "$0.03");
        assert_eq!(dollars(image_price("flux-2-pro", I2K)), "$0.045");
        assert_eq!(dollars(image_price("flux-2-pro", I4K)), "$0.075");
        // flux-2-max: $0.04 a run plus $0.03 a megapixel.
        assert_eq!(dollars(image_price("flux-2-max", I1K)), "$0.07");
        assert_eq!(dollars(image_price("flux-2-max", I4K)), "$0.16");
        // No run fee on these two.
        assert_eq!(dollars(image_price("flux-2-flex", I1K)), "$0.06");
        assert_eq!(dollars(image_price("flux-2-flex", I2K)), "$0.12");
        assert_eq!(dollars(image_price("flux-2-klein", I1K)), "$0.001");
        assert_eq!(dollars(image_price("flux-2-klein", I4K)), "$0.004");
    }

    #[test]
    fn the_same_model_costs_a_different_amount_here_than_on_fal() {
        // The whole reason pricing hangs off the descriptor rather than the catalog.
        let fal = crate::fal::descriptor();
        let square = |id: &str| {
            let model = catalog::image::model(id).expect("catalog model").clone();
            ImageGenerationSettings { model, aspect_ratio: AspectRatio::Square, resolution: I1K }
        };
        let wan = square("wan-2.7-pro");
        assert_eq!(dollars(image(&wan)), "$0.03");
        assert_eq!(dollars(fal.price(&PricedJob::Image(&wan))), "$0.075");
        let gemini = square("gemini-3.1-flash");
        assert_eq!(dollars(image(&gemini)), "$0.067");
        assert_eq!(dollars(fal.price(&PricedJob::Image(&gemini))), "$0.08");
    }

    #[test]
    fn veo_tiers_on_audio_here_and_lite_tiers_on_resolution() {
        // Replicate publishes no resolution tier for veo-3.1, unlike fal.
        assert_eq!(dollars(video_price("veo-3.1", Some(V720), 8, true)), "$3.20");
        assert_eq!(dollars(video_price("veo-3.1", Some(V1080), 8, true)), "$3.20");
        assert_eq!(dollars(video_price("veo-3.1", Some(V1080), 8, false)), "$1.60");
        assert_eq!(dollars(video_price("veo-3.1-fast", Some(V1080), 5, true)), "$0.75");
        // Lite is the other way round: resolution tiers, and its capability table has no audio.
        assert_eq!(dollars(video_price("veo-3.1-lite", Some(V720), 8, false)), "$0.40");
        assert_eq!(dollars(video_price("veo-3.1-lite", Some(V1080), 8, false)), "$0.64");
    }

    #[test]
    fn models_without_a_resolution_knob_price_on_none() {
        assert_eq!(dollars(video_price("kling-3-pro", None, 5, false)), "$1.12", "mode defaults to pro");
        assert_eq!(dollars(video_price("kling-3-pro", None, 5, true)), "$1.68");
        assert_eq!(dollars(video_price("kling-2.6-pro", None, 10, true)), "$1.40");
        assert_eq!(dollars(video_price("kling-2.5-turbo-pro", None, 5, false)), "$0.35");
        // Replicate's sora-2 slug exposes no resolution at all.
        assert_eq!(dollars(video_price("sora-2", None, 8, false)), "$0.80");
    }

    #[test]
    fn the_remaining_video_models_price_per_second() {
        assert_eq!(dollars(video_price("sora-2-pro", Some(V1080), 8, false)), "$4.00");
        assert_eq!(dollars(video_price("sora-2-pro", Some(V720), 8, false)), "$2.40");
        assert_eq!(dollars(video_price("seedance-1.5-pro", Some(V720), 5, true)), "$0.26");
        assert_eq!(dollars(video_price("seedance-2", Some(V720), 5, true)), "$0.90");
        assert_eq!(dollars(video_price("seedance-2-fast", Some(V720), 5, true)), "$0.75");
        assert_eq!(dollars(video_price("happyhorse-1.0", Some(V1080), 10, true)), "$2.80");
        assert_eq!(dollars(video_price("wan-2.7", Some(V1080), 10, false)), "$1.00", "one rate whatever the resolution");
        assert_eq!(dollars(video_price("pixverse-6", Some(V1080), 5, true)), "$1.15");
        assert_eq!(dollars(video_price("grok-imagine-video", Some(V480), 6, false)), "$0.30");
    }

    #[test]
    fn elevenlabs_bills_per_thousand_characters() {
        let model = catalog::audio::model("elevenlabs-v3").expect("catalog model").clone();
        let voice = elevenlabs::all().first().expect("a voice").clone();
        let settings = AudioGenerationSettings { model, speaker1: voice, speaker2: None };
        assert_eq!(dollars(pricing(&PricedJob::Audio { settings: &settings, characters: 1_000 })), "$0.10");
    }

    #[test]
    fn the_compute_billed_tools_use_their_published_median_run() {
        assert_eq!(dollars(pricing(&PricedJob::Tool { settings: &ToolSettings::new(catalog::tool::CLARITY_UPSCALER.clone()), input: ToolInput::default() })), "$0.015");
        assert_eq!(dollars(pricing(&PricedJob::Tool { settings: &ToolSettings::new(catalog::tool::REMBG.clone()), input: ToolInput::default() })), "$0.001");
        assert_eq!(pricing(&PricedJob::Tool { settings: &ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone()), input: ToolInput::default() }), Estimate::Unknown, "fal's tool, not Replicate's");
    }
}
