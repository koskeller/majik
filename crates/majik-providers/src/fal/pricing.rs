//! fal's prices, per model.
//!
//! **Checked 2026-08-29**, tools re-checked **2026-09-02**, against each model's own page (`fal.ai/models/<endpoint>`); the general
//! pricing page carries only a handful of models. Re-check from the endpoint tables in
//! [`super::capabilities`]. The figures here go out of date whenever fal reprices, which is why
//! every number the app shows is labelled an estimate.
//!
//! Do NOT guess a price. A model with no published figure belongs in `UNPRICED` in
//! `tests/shared.rs`, not in a made-up row here.

use crate::models::{ImageResolution, VideoResolution};
use crate::pricing::{flat, per_character, per_megapixel, per_second, rate, Estimate, PricedJob, ToolInput};
use crate::settings::{ImageGenerationSettings, ToolSettings, VideoGenerationSettings};

use super::capabilities::ids::*;
use super::capabilities::{tool_ids, tool_variant, TOPAZ_STARLIGHT_FAST_2};
use ImageResolution::{Fhd as I2K, Hd as I1K, Sd as I05K, Uhd as I4K};
use VideoResolution::{Fhd as V1080, Hd as V720, Sd as V480, Uhd as V4K};

pub fn pricing(job: &PricedJob<'_>) -> Estimate {
    match job {
        PricedJob::Image(settings) => image(settings),
        PricedJob::Video(settings) => video(settings),
        PricedJob::Audio { settings, characters } => match settings.model.id {
            // $0.10 per 1,000 characters.
            ELEVEN_LABS_V3 => per_character(100, *characters),
            // Gemini TTS bills input *and generated audio* tokens, and the audio's length doesn't
            // follow from the character count — see UNPRICED in tests/shared.rs.
            _ => Estimate::Unknown,
        },
        PricedJob::Tool { settings, input } => tool(settings, *input),
    }
}

// ----- Tools --------------------------------------------------------------------------------------

fn tool(settings: &ToolSettings, input: ToolInput) -> Estimate {
    match settings.model.id {
        // Topaz is tiered by output size: $0.08 up to 24 MP, and no library image comes close.
        tool_ids::TOPAZ_UPSCALE => flat(80_000),
        tool_ids::BRIA_BACKGROUND_REMOVE => flat(18_000),
        tool_ids::TOPAZ_UPSCALE_VIDEO => topaz_video(settings, input),
        _ => Estimate::Unknown,
    }
}

/// Per second of video, at a rate set by the *output* resolution and the Starlight variant: fal
/// quotes 10 s at 30 fps as $1.20 up to 1080p and $2.60 at 4K for Precise 2.6 / HQ / Mini / Sharp,
/// and $0.60 / $1.30 for Fast 2. fal doubles this for 60 fps output, which we never ask for — the
/// client sends no `target_fps`, so there is no interpolation to pay for.
///
/// A clip we have never probed (no dimensions, no duration) prices as unknown rather than as free.
fn topaz_video(settings: &ToolSettings, input: ToolInput) -> Estimate {
    let lines = input.output_lines(settings.upscale_factor);
    if lines == 0 || input.duration_secs == 0 {
        return Estimate::Unknown;
    }
    let fast = tool_variant(&settings.model, settings.variant.as_deref()).is_some_and(|v| v.id == TOPAZ_STARLIGHT_FAST_2);
    let micros_per_second = match (lines <= 1080, fast) {
        (true, false) => 120_000,
        (false, false) => 260_000,
        (true, true) => 60_000,
        (false, true) => 130_000,
    };
    per_second(micros_per_second, input.duration_secs)
}

// ----- Images -------------------------------------------------------------------------------------

/// Models billed per output megapixel, as `(first megapixel, each extra)`. Where fal quotes one
/// flat per-megapixel rate, both are the same.
fn image(settings: &ImageGenerationSettings) -> Estimate {
    let (width, height) = output_pixels(settings);
    match settings.model.id {
        // Per-image, no resolution knob.
        GEMINI_25_FLASH => flat(39_000),
        SEEDREAM_45 => flat(40_000),
        RECRAFT_V4_PRO => flat(250_000),
        MUSE_IMAGE => flat(10_000),
        WAN_27_PRO => flat(75_000),
        // The GPT endpoints default to `quality: "high"` at 1024×1024 and we send no quality knob,
        // so that is the tier the request actually uses.
        GPT5 => flat(133_000),
        GPT5_MINI => flat(36_000),

        // Priced per image, but the rate moves with the resolution we ask for.
        // Nano Banana Pro: $0.15, doubled for 4K.
        GEMINI_3_PRO => by_resolution(&[(I1K, 150_000), (I2K, 150_000), (I4K, 300_000)], settings.resolution),
        // Nano Banana 2: $0.08 at 1K, ×0.75 / ×1.5 / ×2 for 0.5K / 2K / 4K.
        GEMINI_31_FLASH => by_resolution(&[(I05K, 60_000), (I1K, 80_000), (I2K, 120_000), (I4K, 160_000)], settings.resolution),
        // gpt-image-2 sells quality tiers, which our resolution picker maps onto (1K = low,
        // 2K = medium, 4K = high — see `capabilities::api_gpt_image_quality`). Figures are the
        // 1024×1024 column; other aspect ratios differ by a few tenths of a cent.
        GPT_IMAGE_2 => by_resolution(&[(I1K, 6_000), (I2K, 53_000), (I4K, 211_000)], settings.resolution),
        // Qwen Image 3: $0.04 at 1K, $0.075 at 2K. We send no resolution knob, so it uses 1K.
        QWEN_IMAGE_3 => flat(40_000),
        // Seedream 5: $0.0675 up to 1536², $0.135 above it. Our 1K tier is the lower band.
        SEEDREAM_5_PRO => by_resolution(&[(I1K, 67_500), (I2K, 135_000)], settings.resolution),
        SEEDREAM_5_LITE => by_resolution(&[(I1K, 33_750), (I2K, 67_500)], settings.resolution),
        // Grok Imagine Image 2 at the `medium` quality we request: $0.06 at 1K, $0.08 at 2K.
        GROK_IMAGINE_IMAGE_2 => by_resolution(&[(I1K, 60_000), (I2K, 80_000)], settings.resolution),

        // Billed per output megapixel, rounded to the nearest with a one-megapixel floor.
        FLUX_2_MAX => per_megapixel(70_000, 30_000, width, height),
        FLUX_2_PRO => per_megapixel(30_000, 15_000, width, height),
        // flux-2-flex bills input megapixels at the same rate; only the output is estimated here.
        FLUX_2_FLEX => per_megapixel(50_000, 50_000, width, height),
        FLUX_2_KLEIN => per_megapixel(5_000, 5_000, width, height),
        FLUX_1_DEV => per_megapixel(25_000, 25_000, width, height),
        FLUX_1_SCHNELL => per_megapixel(3_000, 3_000, width, height),
        _ => Estimate::Unknown,
    }
}

fn by_resolution(table: &[(ImageResolution, u64)], resolution: ImageResolution) -> Estimate {
    match rate(table, resolution) {
        Some(micros) => flat(micros),
        None => Estimate::Unknown,
    }
}

/// The pixel size of the image fal will return, which is what the per-megapixel models bill on.
///
/// The megapixel models take a named `image_size` preset rather than our resolution enum (see
/// [`super::capabilities::api_image_size`]), so the size comes from the preset. An aspect ratio
/// with no preset falls back to fal's own default, `landscape_4_3`.
fn output_pixels(settings: &ImageGenerationSettings) -> (u32, u32) {
    let preset = super::capabilities::api_image_size(&settings.model, settings.aspect_ratio).map(|(_, value)| value);
    preset_pixels(preset.as_deref().unwrap_or("landscape_4_3"))
}

/// fal's named `image_size` presets. Every one is under a megapixel, so today they all bill the
/// first-megapixel rate; the table is here so a larger preset prices itself correctly.
fn preset_pixels(preset: &str) -> (u32, u32) {
    match preset {
        "square_hd" => (1024, 1024),
        "square" => (512, 512),
        "portrait_4_3" => (768, 1024),
        "portrait_16_9" => (576, 1024),
        "landscape_16_9" => (1024, 576),
        // `landscape_4_3`, and anything fal adds later, until this table catches up.
        _ => (1024, 768),
    }
}

// ----- Video --------------------------------------------------------------------------------------

/// Per-second rates keyed on `(resolution, audio on)`. `None` is the resolution for a model with no
/// resolution knob, which is the Kling family, whose capability tables declare no resolutions.
type VideoRates = &'static [((Option<VideoResolution>, bool), u64)];

fn video(settings: &VideoGenerationSettings) -> Estimate {
    let table: VideoRates = match settings.model.id {
        VEO_31 => &[
            ((Some(V720), false), 200_000),
            ((Some(V720), true), 400_000),
            ((Some(V1080), false), 200_000),
            ((Some(V1080), true), 400_000),
            ((Some(V4K), false), 400_000),
            ((Some(V4K), true), 600_000),
        ],
        VEO_31_FAST => &[
            ((Some(V720), false), 100_000),
            ((Some(V720), true), 150_000),
            ((Some(V1080), false), 100_000),
            ((Some(V1080), true), 150_000),
            ((Some(V4K), false), 300_000),
            ((Some(V4K), true), 350_000),
        ],
        VEO_31_LITE => &[
            ((Some(V720), false), 30_000),
            ((Some(V720), true), 50_000),
            ((Some(V1080), false), 50_000),
            ((Some(V1080), true), 80_000),
        ],
        // Sora 2 quotes one rate; Pro charges by resolution ($0.70/s is true 1920×1080).
        SORA_2 => &[((Some(V720), false), 100_000), ((Some(V720), true), 100_000)],
        SORA_2_PRO => &[
            ((Some(V720), false), 300_000),
            ((Some(V720), true), 300_000),
            ((Some(V1080), false), 700_000),
            ((Some(V1080), true), 700_000),
        ],
        // The Kling family declares no resolutions; audio roughly doubles the rate. (Kling also
        // sells a dearer "voice control" tier, which we never request.)
        // Kling O3 Pro, checked 2026-09-02: $0.112/s, $0.14/s with audio ("a 5 second video with
        // audio costs $0.70"). Its reference path (the video-to-video endpoint) bills a flat
        // $0.168/s, so a reference run is under-estimated; the table has no endpoint dimension,
        // as with H3 Max.
        KLING_O3_PRO => &[((None, false), 112_000), ((None, true), 140_000)],
        KLING_30_PRO => &[((None, false), 112_000), ((None, true), 168_000)],
        KLING_30_STANDARD => &[((None, false), 84_000), ((None, true), 126_000)],
        KLING_26_PRO => &[((None, false), 70_000), ((None, true), 140_000)],
        KLING_25_TURBO_PRO => &[((None, false), 70_000), ((None, true), 70_000)],
        // Seedance bills video tokens (`width × height × fps × seconds / 1024`), so the
        // per-second rates below are that formula at 24 fps for each resolution fal offers.
        // Seedance 2 is $0.014 per 1,000 tokens and includes audio in the rate; the derived 720p
        // and 1080p figures reproduce fal's published $0.3034/s and $0.682/s.
        SEEDANCE_20 => &[((Some(V480), false), 134_000), ((Some(V480), true), 134_000), ((Some(V720), false), 303_400), ((Some(V720), true), 303_400)],
        SEEDANCE_20_FAST => &[((Some(V480), false), 107_600), ((Some(V480), true), 107_600), ((Some(V720), false), 241_900), ((Some(V720), true), 241_900)],
        // Seedance 1.5 Pro is $2.4 per million tokens with audio, $1.2 without. The 720p-with-audio
        // row reproduces fal's "a 720p 5 second video with audio costs roughly $0.26".
        SEEDANCE_15_PRO => &[
            ((Some(V480), false), 11_529),
            ((Some(V480), true), 23_058),
            ((Some(V720), false), 25_920),
            ((Some(V720), true), 51_840),
            ((Some(V1080), false), 58_320),
            ((Some(V1080), true), 116_640),
        ],
        // Audio is always on for Happy Horse and included in the rate.
        HAPPY_HORSE_10 => &[((Some(V720), true), 140_000), ((Some(V720), false), 140_000), ((Some(V1080), true), 280_000), ((Some(V1080), false), 280_000)],
        WAN_27 => &[((Some(V720), false), 100_000), ((Some(V720), true), 100_000), ((Some(V1080), false), 150_000), ((Some(V1080), true), 150_000)],
        PIXVERSE_V6 => &[
            ((Some(V720), false), 45_000),
            ((Some(V720), true), 60_000),
            ((Some(V1080), false), 90_000),
            ((Some(V1080), true), 115_000),
        ],
        GROK_IMAGINE_VIDEO => &[((Some(V480), false), 50_000), ((Some(V480), true), 50_000), ((Some(V720), false), 70_000), ((Some(V720), true), 70_000)],
        GROK_IMAGINE_VIDEO_15 => &[
            ((Some(V480), false), 80_000),
            ((Some(V480), true), 80_000),
            ((Some(V720), false), 140_000),
            ((Some(V720), true), 140_000),
            ((Some(V1080), false), 250_000),
            ((Some(V1080), true), 250_000),
        ],
        // Gemini Omni Flash bills per second by tier; our lowest offered tier is 720p.
        GEMINI_OMNI_FLASH_11 => &[
            ((Some(V720), false), 100_000),
            ((Some(V720), true), 100_000),
            ((Some(V1080), false), 150_000),
            ((Some(V1080), true), 150_000),
            ((Some(V4K), false), 300_000),
            ((Some(V4K), true), 300_000),
        ],
        // Seedance 2.5 bills video tokens like the rest of the family; these are fal's own
        // per-second figures for a 16:9 clip with audio.
        SEEDANCE_25 => &[
            ((Some(V480), false), 220_500),
            ((Some(V480), true), 220_500),
            ((Some(V720), false), 473_000),
            ((Some(V720), true), 473_000),
            ((Some(V1080), false), 1_164_000),
            ((Some(V1080), true), 1_164_000),
        ],
        // H3's tiers are 480P / 768P / 2K / 4K, which our four resolutions map onto in order.
        MINIMAX_H3 => &[
            ((Some(V480), false), 50_000),
            ((Some(V480), true), 50_000),
            ((Some(V720), false), 60_000),
            ((Some(V720), true), 60_000),
            ((Some(V1080), false), 130_000),
            ((Some(V1080), true), 130_000),
            ((Some(V4K), false), 160_000),
            ((Some(V4K), true), 160_000),
        ],
        // H3 Max's list price, now that fal's half-rate launch promo has ended. The reference
        // endpoint bills a flat $0.08/s at both tiers, so a 480p reference run is under-estimated;
        // the table has no endpoint dimension and t2v/i2v are the common path. Its per-reference
        // token charge above fal's included 4096 tokens is not encoded, as with H3 and Seedance.
        MINIMAX_H3_MAX => &[
            ((Some(V480), false), 50_000),
            ((Some(V480), true), 50_000),
            ((Some(V720), false), 80_000),
            ((Some(V720), true), 80_000),
        ],
        FLUX_3 => &[
            ((Some(V720), false), 170_000),
            ((Some(V720), true), 170_000),
            ((Some(V1080), false), 290_000),
            ((Some(V1080), true), 290_000),
        ],
        // Audio is always on for Happy Horse and included in the rate.
        HAPPY_HORSE_11 => &[
            ((Some(V720), false), 140_000),
            ((Some(V720), true), 140_000),
            ((Some(V1080), false), 180_000),
            ((Some(V1080), true), 180_000),
        ],
        WAN_30 => &[
            ((Some(V480), false), 50_000),
            ((Some(V480), true), 50_000),
            ((Some(V720), false), 100_000),
            ((Some(V720), true), 100_000),
            ((Some(V1080), false), 200_000),
            ((Some(V1080), true), 200_000),
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

    /// Every assertion below is a worked example fal publishes on the model's own page, so a
    /// re-sweep that changes a number fails here with the model that moved.
    fn image_price(id: &str, aspect_ratio: AspectRatio, resolution: ImageResolution) -> Estimate {
        let model = catalog::image::model(id).expect("catalog model").clone();
        image(&ImageGenerationSettings { model, aspect_ratio, resolution })
    }

    fn video_price(id: &str, resolution: Option<VideoResolution>, duration: u32, audio_enabled: bool) -> Estimate {
        let model = catalog::video::model(id).expect("catalog model").clone();
        video(&VideoGenerationSettings { model, aspect_ratio: None, resolution, duration, audio_enabled })
    }

    fn dollars(estimate: Estimate) -> String {
        estimate.amount().expect("priced").to_string()
    }

    #[test]
    fn flat_per_image_models_ignore_the_settings() {
        assert_eq!(dollars(image_price("seedream-4.5", AspectRatio::Square, I1K)), "$0.04");
        assert_eq!(dollars(image_price("seedream-4.5", AspectRatio::Landscape, I4K)), "$0.04", "no resolution knob to move it");
        assert_eq!(dollars(image_price("recraft-4-pro", AspectRatio::Square, I1K)), "$0.25");
        assert_eq!(dollars(image_price("gemini-2.5-flash", AspectRatio::Square, I1K)), "$0.039");
    }

    #[test]
    fn resolution_tiers_move_the_per_image_price() {
        // Nano Banana Pro: $0.15, doubled for 4K.
        assert_eq!(dollars(image_price("gemini-3-pro", AspectRatio::Square, I1K)), "$0.15");
        assert_eq!(dollars(image_price("gemini-3-pro", AspectRatio::Square, I4K)), "$0.30");
        // Nano Banana 2 charges four different rates.
        assert_eq!(dollars(image_price("gemini-3.1-flash", AspectRatio::Square, I05K)), "$0.06");
        assert_eq!(dollars(image_price("gemini-3.1-flash", AspectRatio::Square, I1K)), "$0.08");
        assert_eq!(dollars(image_price("gemini-3.1-flash", AspectRatio::Square, I2K)), "$0.12");
        assert_eq!(dollars(image_price("gemini-3.1-flash", AspectRatio::Square, I4K)), "$0.16");
    }

    #[test]
    fn a_resolution_the_model_never_offers_has_no_price() {
        // Nano Banana Pro's capability table starts at 1K, so 0.5K is a row we never wrote.
        assert_eq!(image_price("gemini-3-pro", AspectRatio::Square, I05K), Estimate::Unknown);
    }

    #[test]
    fn megapixel_models_bill_the_preset_they_are_sent() {
        // Every named preset we send is under a megapixel, so all of them pay the first-megapixel
        // rate: $0.03 for flux-2-pro, $0.05 for flex, $0.003 for schnell.
        assert_eq!(dollars(image_price("flux-2-pro", AspectRatio::Square, I1K)), "$0.03");
        assert_eq!(dollars(image_price("flux-2-pro", AspectRatio::Landscape, I1K)), "$0.03");
        assert_eq!(dollars(image_price("flux-2-flex", AspectRatio::Square, I1K)), "$0.05");
        assert_eq!(dollars(image_price("flux-2-max", AspectRatio::Square, I1K)), "$0.07");
        assert_eq!(dollars(image_price("flux-1-schnell", AspectRatio::Square, I1K)), "$0.003");
        assert_eq!(dollars(image_price("flux-1-dev", AspectRatio::Square, I1K)), "$0.025");
        assert_eq!(dollars(image_price("flux-2-klein", AspectRatio::Square, I1K)), "$0.005");
    }

    #[test]
    fn veo_charges_for_audio_and_for_resolution() {
        // fal's own example: 5 s at 1080p with audio on veo-3.1-fast costs $0.75.
        assert_eq!(dollars(video_price("veo-3.1-fast", Some(V1080), 5, true)), "$0.75");
        assert_eq!(dollars(video_price("veo-3.1-fast", Some(V1080), 5, false)), "$0.50");
        assert_eq!(dollars(video_price("veo-3.1", Some(V1080), 8, true)), "$3.20");
        assert_eq!(dollars(video_price("veo-3.1", Some(V4K), 8, true)), "$4.80", "4K costs half again as much");
        assert_eq!(dollars(video_price("veo-3.1-lite", Some(V720), 8, false)), "$0.24");
    }

    #[test]
    fn kling_prices_without_a_resolution_knob() {
        // Kling declares no resolutions, so the settings carry `None` and the table must key on it.
        assert_eq!(dollars(video_price("kling-2.6-pro", None, 5, false)), "$0.35");
        assert_eq!(dollars(video_price("kling-2.6-pro", None, 10, true)), "$1.40");
        assert_eq!(dollars(video_price("kling-3-pro", None, 5, false)), "$0.56");
        assert_eq!(dollars(video_price("kling-o3-pro", None, 5, false)), "$0.56");
        assert_eq!(dollars(video_price("kling-o3-pro", None, 5, true)), "$0.70", "fal's own worked example");
        assert_eq!(dollars(video_price("kling-2.5-turbo-pro", None, 5, false)), "$0.35");
    }

    #[test]
    fn token_billed_video_matches_fals_worked_examples() {
        // "a 720p 5 second video with audio costs roughly $0.26": the token formula at 24 fps.
        assert_eq!(dollars(video_price("seedance-1.5-pro", Some(V720), 5, true)), "$0.26");
        assert_eq!(dollars(video_price("seedance-1.5-pro", Some(V720), 5, false)), "$0.13", "audio doubles the token rate");
        // Published per-second rates for Seedance 2.
        assert_eq!(dollars(video_price("seedance-2", Some(V720), 1, true)), "$0.30");
        assert_eq!(dollars(video_price("seedance-2-fast", Some(V720), 1, true)), "$0.24");
    }

    #[test]
    fn the_remaining_video_models_price_per_second_by_resolution() {
        assert_eq!(dollars(video_price("happyhorse-1.0", Some(V1080), 10, true)), "$2.80");
        assert_eq!(dollars(video_price("wan-2.7", Some(V720), 10, false)), "$1.00");
        assert_eq!(dollars(video_price("pixverse-6", Some(V1080), 5, true)), "$0.58");
        // fal's example: "For 6s 480p video your request will cost $0.3".
        assert_eq!(dollars(video_price("grok-imagine-video", Some(V480), 6, false)), "$0.30");
        assert_eq!(dollars(video_price("sora-2", Some(V720), 10, false)), "$1.00");
        assert_eq!(dollars(video_price("sora-2-pro", Some(V1080), 4, false)), "$2.80");
    }

    #[test]
    fn elevenlabs_bills_per_thousand_characters() {
        let model = catalog::audio::model("elevenlabs-v3").expect("catalog model").clone();
        let voice = elevenlabs::all().first().expect("a voice").clone();
        let settings = AudioGenerationSettings { model, speaker1: voice, speaker2: None };
        assert_eq!(dollars(pricing(&PricedJob::Audio { settings: &settings, characters: 500 })), "$0.05");
        assert_eq!(dollars(pricing(&PricedJob::Audio { settings: &settings, characters: 10_000 })), "$1.00");
    }

    #[test]
    fn gemini_tts_has_no_price_we_can_stand_behind() {
        let model = catalog::audio::model("gemini-2.5-pro").expect("catalog model").clone();
        let voice = crate::voices::gemini::all().first().expect("a voice").clone();
        let settings = AudioGenerationSettings { model, speaker1: voice, speaker2: None };
        assert_eq!(pricing(&PricedJob::Audio { settings: &settings, characters: 1_000 }), Estimate::Unknown);
    }

    fn tool(model: &crate::models::ToolModel, factor: u32, input: ToolInput) -> Estimate {
        let settings = ToolSettings::new(model.clone()).with_factor(factor);
        pricing(&PricedJob::Tool { settings: &settings, input })
    }

    /// Per second, at the rate the *output* resolution falls in — so the factor moves a clip
    /// between tiers. A 720p clip at 2× is 1440 lines, which is the 4K tier, not the 1080p one.
    #[test]
    fn topaz_video_bills_per_second_of_output_resolution() {
        let video = &catalog::tool::TOPAZ_UPSCALE_VIDEO;
        // 540p → 1080p at 2×: $0.12 a second, fal's "$1.20 per 10 seconds up to 1080p".
        assert_eq!(dollars(tool(video, 2, ToolInput::video(960, 540, 10))), "$1.20");
        // 1080p → 2160p at 2×: $0.26 a second.
        assert_eq!(dollars(tool(video, 2, ToolInput::video(1920, 1080, 10))), "$2.60");
        // The factor is what moves a clip between tiers: the same 360p source is 720 lines at 2×
        // but 1440 at 4×, which is billed as 4K.
        assert_eq!(dollars(tool(video, 2, ToolInput::video(640, 360, 5))), "$0.60");
        assert_eq!(dollars(tool(video, 4, ToolInput::video(640, 360, 5))), "$1.30");
    }

    /// Starlight Fast 2 is billed at half the rate of the other Starlight models; an unknown
    /// variant runs (and is priced) as the default, which is a quality model.
    #[test]
    fn topaz_video_fast_2_costs_half() {
        let video = &catalog::tool::TOPAZ_UPSCALE_VIDEO;
        let priced = |variant: &str, input| {
            let settings = ToolSettings::new(video.clone()).with_factor(2).with_variant(variant);
            dollars(pricing(&PricedJob::Tool { settings: &settings, input }))
        };
        assert_eq!(priced("starlight-fast-2", ToolInput::video(960, 540, 10)), "$0.60");
        assert_eq!(priced("starlight-fast-2", ToolInput::video(1920, 1080, 10)), "$1.30");
        assert_eq!(priced("starlight-hq", ToolInput::video(960, 540, 10)), "$1.20");
        assert_eq!(priced("proteus", ToolInput::video(960, 540, 10)), "$1.20", "a dropped variant falls back to the default");
    }

    /// A clip the library never probed has no duration to bill, and a made-up number would be worse
    /// than saying so.
    #[test]
    fn topaz_video_without_a_probed_clip_is_unknown() {
        let video = &catalog::tool::TOPAZ_UPSCALE_VIDEO;
        assert_eq!(tool(video, 2, ToolInput::default()), Estimate::Unknown);
        assert_eq!(tool(video, 2, ToolInput::video(1920, 1080, 0)), Estimate::Unknown, "no duration");
        assert_eq!(tool(video, 2, ToolInput::video(0, 0, 5)), Estimate::Unknown, "no dimensions");
    }

    #[test]
    fn tools_are_flat_per_image() {
        assert_eq!(dollars(pricing(&PricedJob::Tool { settings: &ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone()), input: ToolInput::default() })), "$0.08");
        assert_eq!(dollars(pricing(&PricedJob::Tool { settings: &ToolSettings::new(catalog::tool::BRIA_BACKGROUND_REMOVE.clone()), input: ToolInput::default() })), "$0.018");
        assert_eq!(pricing(&PricedJob::Tool { settings: &ToolSettings::new(catalog::tool::CLARITY_UPSCALER.clone()), input: ToolInput::default() }), Estimate::Unknown, "Replicate's tool, not fal's");
    }
}
