//! fal capabilities: supported model lists, capability tables and the
//! endpoint / request-parameter routing tables. Everything is keyed on the model id so the
//! tables stay independent of the catalog structs.
//!
//! Do NOT guess when adding a model — check `fal.ai/models/<id>/api`. Note that fal's newer
//! partner endpoints carry a bare vendor prefix rather than `fal-ai/` (`minimax/h3/text-to-video`,
//! not `fal-ai/minimax/h3/text-to-video`), so the id in the table is the id in the URL.

use serde_json::Value;

use crate::asset::{AssetConstraints, AssetRole};
use crate::models::{
    AspectRatio, AudioModel, AudioModelCapabilities, AudioVoice, ImageModel, ImageResolution, ModelCapabilities, ToolModel, ToolModelCapabilities,
    ToolVariant, VideoAspectRatio, VideoDurationRange, VideoModel, VideoModelCapabilities, VideoReferences, VideoResolution,
};
use crate::references::ReferenceTagStyle;

/// Catalog ids used by the fal routing tables.
pub mod ids {
    // Image models
    pub const GEMINI_3_PRO: &str = "gemini-3-pro";
    pub const GEMINI_31_FLASH: &str = "gemini-3.1-flash";
    pub const GEMINI_25_FLASH: &str = "gemini-2.5-flash";
    pub const GPT_IMAGE_2: &str = "gpt-image-2";
    pub const GPT5: &str = "gpt-5-image";
    pub const GPT5_MINI: &str = "gpt-5-image-mini";
    pub const SEEDREAM_45: &str = "seedream-4.5";
    pub const FLUX_2_MAX: &str = "flux-2-max";
    pub const FLUX_2_PRO: &str = "flux-2-pro";
    pub const FLUX_2_FLEX: &str = "flux-2-flex";
    pub const FLUX_2_KLEIN: &str = "flux-2-klein";
    pub const FLUX_1_DEV: &str = "flux-1-dev";
    pub const FLUX_1_SCHNELL: &str = "flux-1-schnell";
    pub const RECRAFT_V4_PRO: &str = "recraft-4-pro";
    pub const WAN_27_PRO: &str = "wan-2.7-pro";
    pub const MUSE_IMAGE: &str = "muse-image";
    pub const QWEN_IMAGE_3: &str = "qwen-image-3";
    pub const SEEDREAM_5_PRO: &str = "seedream-5-pro";
    pub const SEEDREAM_5_LITE: &str = "seedream-5-lite";
    pub const GROK_IMAGINE_IMAGE_2: &str = "grok-imagine-image-2";

    // Video models
    pub const VEO_31: &str = "veo-3.1";
    pub const VEO_31_FAST: &str = "veo-3.1-fast";
    pub const VEO_31_LITE: &str = "veo-3.1-lite";
    pub const SORA_2_PRO: &str = "sora-2-pro";
    pub const SORA_2: &str = "sora-2";
    pub const KLING_O3_PRO: &str = "kling-o3-pro";
    pub const KLING_30_PRO: &str = "kling-3-pro";
    pub const KLING_30_STANDARD: &str = "kling-3-standard";
    pub const KLING_26_PRO: &str = "kling-2.6-pro";
    pub const KLING_25_TURBO_PRO: &str = "kling-2.5-turbo-pro";
    pub const SEEDANCE_20: &str = "seedance-2";
    pub const SEEDANCE_20_FAST: &str = "seedance-2-fast";
    pub const SEEDANCE_15_PRO: &str = "seedance-1.5-pro";
    pub const HAPPY_HORSE_10: &str = "happyhorse-1.0";
    pub const WAN_27: &str = "wan-2.7";
    pub const PIXVERSE_V6: &str = "pixverse-6";
    pub const GROK_IMAGINE_VIDEO: &str = "grok-imagine-video";
    pub const GROK_IMAGINE_VIDEO_15: &str = "grok-imagine-video-1.5";
    pub const GEMINI_OMNI_FLASH_11: &str = "gemini-omni-flash-1.1";
    pub const SEEDANCE_25: &str = "seedance-2.5";
    pub const MINIMAX_H3: &str = "minimax-h3";
    pub const MINIMAX_H3_MAX: &str = "minimax-h3-max";
    pub const FLUX_3: &str = "flux-3";
    pub const HAPPY_HORSE_11: &str = "happyhorse-1.1";
    pub const WAN_30: &str = "wan-3.0";
    pub const WAN_30_PRIME: &str = "wan-3.0-prime";

    // Audio models
    pub const ELEVEN_LABS_V3: &str = "elevenlabs-v3";
    pub const GEMINI_25_PRO: &str = "gemini-2.5-pro";
}

use ids::*;

// ----- Supported models ---------------------------------------------------------------------------

/// Order matches `FalProvider.supportedImageModels`.
pub const SUPPORTED_IMAGE_MODEL_IDS: &[&str] = &[
    GEMINI_3_PRO,
    GEMINI_31_FLASH,
    GEMINI_25_FLASH,
    GPT_IMAGE_2,
    GPT5,
    GPT5_MINI,
    SEEDREAM_5_PRO,
    SEEDREAM_5_LITE,
    SEEDREAM_45,
    MUSE_IMAGE,
    FLUX_2_MAX,
    FLUX_2_PRO,
    FLUX_2_FLEX,
    FLUX_2_KLEIN,
    FLUX_1_DEV,
    FLUX_1_SCHNELL,
    RECRAFT_V4_PRO,
    QWEN_IMAGE_3,
    WAN_27_PRO,
    GROK_IMAGINE_IMAGE_2,
];

/// Order matches `FalProvider.supportedVideoModels`.
pub const SUPPORTED_VIDEO_MODEL_IDS: &[&str] = &[
    VEO_31,
    VEO_31_FAST,
    VEO_31_LITE,
    GEMINI_OMNI_FLASH_11,
    SORA_2_PRO,
    SORA_2,
    KLING_O3_PRO,
    KLING_30_PRO,
    KLING_30_STANDARD,
    KLING_26_PRO,
    KLING_25_TURBO_PRO,
    SEEDANCE_25,
    SEEDANCE_20,
    SEEDANCE_20_FAST,
    SEEDANCE_15_PRO,
    MINIMAX_H3_MAX,
    MINIMAX_H3,
    FLUX_3,
    HAPPY_HORSE_11,
    HAPPY_HORSE_10,
    WAN_30_PRIME,
    WAN_30,
    WAN_27,
    PIXVERSE_V6,
    GROK_IMAGINE_VIDEO_15,
    GROK_IMAGINE_VIDEO,
];

/// Order matches `FalProvider.supportedAudioModels`.
pub const SUPPORTED_AUDIO_MODEL_IDS: &[&str] = &[ELEVEN_LABS_V3, GEMINI_25_PRO];

// ----- Image capabilities -------------------------------------------------------------------------

const ALL_SEVEN_RATIOS: [AspectRatio; 7] = [
    AspectRatio::Square,
    AspectRatio::Standard,
    AspectRatio::ThreeToFour,
    AspectRatio::Portrait,
    AspectRatio::Landscape,
    AspectRatio::Tall,
    AspectRatio::Wide,
];

const NAMED_SIZE_RATIOS: [AspectRatio; 5] =
    [AspectRatio::Square, AspectRatio::ThreeToFour, AspectRatio::Standard, AspectRatio::Tall, AspectRatio::Landscape];

pub fn image_capabilities(model: &ImageModel) -> Option<ModelCapabilities> {
    use ImageResolution::*;
    let caps = match model.id {
        GEMINI_3_PRO => ModelCapabilities::new(ALL_SEVEN_RATIOS, [Hd, Fhd, Uhd], 14),
        GEMINI_31_FLASH => ModelCapabilities::new(ALL_SEVEN_RATIOS, [Sd, Hd, Fhd, Uhd], 14),
        GEMINI_25_FLASH => ModelCapabilities::new(ALL_SEVEN_RATIOS, [], 14),
        GPT5 => ModelCapabilities::new([AspectRatio::Square], [], 1)
            .with_asset_constraints(AssetConstraints::new([(AssetRole::ReferenceImage, 0..=1), (AssetRole::MaskImage, 0..=1)])),
        GPT5_MINI => ModelCapabilities::new([AspectRatio::Square], [], 1),
        GPT_IMAGE_2 => ModelCapabilities::new(NAMED_SIZE_RATIOS, [Hd, Fhd, Uhd], 10)
            .with_asset_constraints(AssetConstraints::new([(AssetRole::ReferenceImage, 0..=10), (AssetRole::MaskImage, 0..=1)]))
            .with_default_resolution(Fhd),
        SEEDREAM_45 => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 10),
        FLUX_2_MAX => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 1),
        FLUX_2_PRO => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 1),
        FLUX_2_FLEX => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 1),
        FLUX_2_KLEIN => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 4),
        FLUX_1_DEV => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 1),
        FLUX_1_SCHNELL => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 0),
        RECRAFT_V4_PRO => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 0),
        // Muse Image takes `aspect_ratio` directly and has no resolution knob; the edit endpoint
        // accepts up to ten reference images.
        MUSE_IMAGE => ModelCapabilities::new(
            [AspectRatio::Square, AspectRatio::Standard, AspectRatio::ThreeToFour, AspectRatio::Landscape, AspectRatio::Tall, AspectRatio::Wide],
            [],
            10,
        ),
        // Qwen and Seedream 5 use fal's named `image_size` presets, so they inherit that ratio set.
        QWEN_IMAGE_3 => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 6),
        SEEDREAM_5_PRO | SEEDREAM_5_LITE => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 10),
        // Grok Imagine Image 2 sells a 1K / 2K tier and offers neither 21:9 nor 4:5.
        GROK_IMAGINE_IMAGE_2 => ModelCapabilities::new(
            [AspectRatio::Square, AspectRatio::Standard, AspectRatio::ThreeToFour, AspectRatio::Landscape, AspectRatio::Tall],
            [ImageResolution::Hd, ImageResolution::Fhd],
            10,
        ),
        WAN_27_PRO => ModelCapabilities::new(NAMED_SIZE_RATIOS, [], 4),
        _ => return None,
    };
    Some(caps)
}

// ----- Video capabilities -------------------------------------------------------------------------

pub fn video_capabilities(model: &VideoModel) -> Option<VideoModelCapabilities> {
    use VideoAspectRatio::*;
    use VideoResolution::{Fhd, Hd, Sd, Uhd};
    let caps = match model.id {
        VEO_31 => VideoModelCapabilities::new(VideoDurationRange::new(4, 8, Some(vec![4, 6, 8])), [Landscape, Tall], [Hd, Fhd, Uhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_references(VideoReferences::images(3))
            .with_audio(true, false),
        VEO_31_FAST => VideoModelCapabilities::new(VideoDurationRange::new(4, 8, Some(vec![4, 6, 8])), [Landscape, Tall], [Hd, Fhd, Uhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_references(VideoReferences::images(3))
            .with_audio(true, false),
        VEO_31_LITE => VideoModelCapabilities::new(VideoDurationRange::new(4, 8, Some(vec![4, 6, 8])), [Landscape, Tall], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(true, false),
        SORA_2 => VideoModelCapabilities::new(VideoDurationRange::new(4, 20, Some(vec![4, 8, 12, 16, 20])), [Tall, Landscape], [Hd], 1),
        SORA_2_PRO => VideoModelCapabilities::new(VideoDurationRange::new(4, 20, Some(vec![4, 8, 12, 16, 20])), [Tall, Landscape], [Fhd, Hd], 1),
        // Kling O3 (Kuaishou's "Kling Omni"), checked 2026-09-02: the 3.0 Pro shape, and for
        // references its video-to-video endpoint rather than the images-only `reference-to-video`
        // one: a required clip of 3–15 s (720–3840 px, 200 MB) addressed as `@Video1`, plus up to
        // four images as `@Image1`. Its `elements`, `multi_prompt` and `shot_type` inputs have no
        // composer equivalent and are not sent.
        KLING_O3_PRO => VideoModelCapabilities::new(VideoDurationRange::new(3, 15, None), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_references(VideoReferences::images(4).with_videos(1).with_video_secs(3, 15).with_required_video())
            .with_audio(true, false)
            .with_max_prompt_characters(2500),
        KLING_30_PRO => VideoModelCapabilities::new(VideoDurationRange::new(3, 15, None), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(true, false)
            .with_max_prompt_characters(2500),
        KLING_30_STANDARD => VideoModelCapabilities::new(VideoDurationRange::new(3, 15, None), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(true, false)
            .with_max_prompt_characters(2500),
        KLING_25_TURBO_PRO => VideoModelCapabilities::new(VideoDurationRange::new(5, 10, Some(vec![5, 10])), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(false, false)
            .with_max_prompt_characters(2500),
        KLING_26_PRO => VideoModelCapabilities::new(VideoDurationRange::new(5, 10, Some(vec![5, 10])), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(true, false)
            .with_max_prompt_characters(2500),
        SEEDANCE_15_PRO => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 12, None),
            [Wide, Landscape, Standard, Square, Portrait, Tall, Auto],
            [Sd, Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_audio(true, false),
        SEEDANCE_20 => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 15, None),
            [Auto, Wide, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(9).with_videos(3).with_audio(3).with_combined_max(12))
        .with_audio(true, false),
        SEEDANCE_20_FAST => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 15, None),
            [Auto, Wide, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(9).with_videos(3).with_audio(3).with_combined_max(12))
        .with_audio(true, false),
        HAPPY_HORSE_10 => VideoModelCapabilities::new(VideoDurationRange::new(3, 15, None), [Landscape, Tall, Square, Standard, Portrait], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::new([(AssetRole::FirstFrame, 0..=1)]))
            .with_references(VideoReferences::images(9))
            .with_prompt_optional(true)
            .with_audio(true, true),
        WAN_27 => VideoModelCapabilities::new(VideoDurationRange::new(2, 15, None), [Landscape, Tall, Square, Standard, Portrait], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame_and_audio())
            // Its audio slot stays the single conditioning track of the i2v endpoint; the reference
            // endpoint takes images and videos only. fal states no cap on either, so these match
            // Wan 3.0's.
            .with_references(VideoReferences::images(10).with_videos(5)),
        PIXVERSE_V6 => VideoModelCapabilities::new(
            VideoDurationRange::new(1, 15, None),
            [Landscape, Tall, Square, Standard, Portrait, NarrowPortrait, NarrowLandscape, Wide],
            [Hd, Fhd],
            1,
        )
        .with_audio(true, false),
        // Gemini Omni Flash offers only 16:9 / 9:16, and its 360p tier has no enum of ours — the
        // three we can name start at 720p. Its reference clips are at most three seconds each; a
        // longer one fails the whole request with a 422 (`video_duration_too_long`).
        GEMINI_OMNI_FLASH_11 => VideoModelCapabilities::new(VideoDurationRange::new(3, 10, None), [Landscape, Tall], [Hd, Fhd, Uhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_references(VideoReferences::images(10).with_videos(3).with_max_video_secs(3)),
        SEEDANCE_25 => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 30, None),
            [Auto, Wide, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(30).with_videos(10).with_audio(10).with_combined_max(50))
        .with_audio(true, false),
        // H3 has no "let the model decide" ratio, and its tiers are 480P / 768P / 2K / 4K.
        MINIMAX_H3 => VideoModelCapabilities::new(
            VideoDurationRange::new(5, 15, None),
            [Wide, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd, Fhd, Uhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(9).with_videos(3).with_audio(3).with_combined_max(12))
        .with_max_prompt_characters(50_000),
        MINIMAX_H3_MAX => VideoModelCapabilities::new(
            VideoDurationRange::new(5, 15, None),
            [Wide, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(9).with_videos(3).with_audio(3).with_combined_max(12))
        .with_max_prompt_characters(50_000),
        // FLUX 3 also offers 2:1, which we have no enum for.
        FLUX_3 => VideoModelCapabilities::new(
            VideoDurationRange::new(5, 20, None),
            [Auto, Wide, Landscape, Standard, Square, Portrait, Tall],
            [Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_audio(true, false),
        HAPPY_HORSE_11 => VideoModelCapabilities::new(
            VideoDurationRange::new(3, 15, None),
            [Landscape, Tall, Square, Standard, Portrait, Wide],
            [Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::new([(AssetRole::FirstFrame, 0..=1)]))
        .with_references(VideoReferences::images(9))
        .with_prompt_optional(true)
        .with_audio(true, true),
        // Wan 3.0 replaced 2.7's audio-conditioning input with an audio toggle of its own, so it
        // takes both frames but no audio asset.
        WAN_30 | WAN_30_PRIME => VideoModelCapabilities::new(
            VideoDurationRange::new(2, 30, None),
            [Auto, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(10).with_videos(5).with_audio(5))
        .with_audio(true, false)
        .with_max_prompt_characters(5_000),
        GROK_IMAGINE_VIDEO_15 => VideoModelCapabilities::new(
            VideoDurationRange::new(1, 15, None),
            [Landscape, Standard, NarrowLandscape, Square, NarrowPortrait, Portrait, Tall],
            [Sd, Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::new([(AssetRole::FirstFrame, 0..=1)]))
        // Its reference endpoint renders at 720p at most, where text-to-video also sells 1080p.
        .with_references(VideoReferences::images(7).with_resolutions(&[Sd, Hd]))
        .with_max_prompt_characters(4_096),
        GROK_IMAGINE_VIDEO => VideoModelCapabilities::new(
            VideoDurationRange::new(1, 15, None),
            [Landscape, Standard, NarrowLandscape, Square, NarrowPortrait, Portrait, Tall],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(AssetConstraints::new([(AssetRole::FirstFrame, 0..=1)]))
        .with_references(VideoReferences::images(7)),
        _ => return None,
    };
    Some(caps)
}

// ----- Image endpoints ----------------------------------------------------------------------------

/// Text-to-image endpoint.
pub fn endpoint(model: &ImageModel) -> Option<&'static str> {
    Some(match model.id {
        GEMINI_3_PRO => "fal-ai/nano-banana-pro",
        GEMINI_25_FLASH => "fal-ai/gemini-25-flash-image",
        GEMINI_31_FLASH => "fal-ai/nano-banana-2",
        GPT5 => "fal-ai/gpt-image-1.5",
        GPT5_MINI => "fal-ai/gpt-image-1-mini",
        GPT_IMAGE_2 => "openai/gpt-image-2",
        SEEDREAM_45 => "fal-ai/bytedance/seedream/v4.5/text-to-image",
        FLUX_2_MAX => "fal-ai/flux-2-max",
        FLUX_2_PRO => "fal-ai/flux-2-pro",
        FLUX_2_FLEX => "fal-ai/flux-2-flex",
        FLUX_2_KLEIN => "fal-ai/flux-2/klein/4b",
        FLUX_1_DEV => "fal-ai/flux/dev",
        FLUX_1_SCHNELL => "fal-ai/flux/schnell",
        RECRAFT_V4_PRO => "fal-ai/recraft/v4/pro/text-to-image",
        WAN_27_PRO => "fal-ai/wan/v2.7/pro/text-to-image",
        MUSE_IMAGE => "meta/muse-image/text-to-image",
        QWEN_IMAGE_3 => "alibaba/qwen-image-3/text-to-image",
        SEEDREAM_5_PRO => "bytedance/seedream/v5/pro/text-to-image",
        SEEDREAM_5_LITE => "bytedance/seedream/v5/lite/text-to-image",
        GROK_IMAGINE_IMAGE_2 => "xai/grok-imagine-image/v2.0/text-to-image",
        _ => return None,
    })
}

/// Edit endpoint, or `None` if the model has no edit support (or is not on fal at all).
pub fn edit_endpoint(model: &ImageModel) -> Option<&'static str> {
    Some(match model.id {
        GEMINI_3_PRO => "fal-ai/nano-banana-pro/edit",
        GEMINI_25_FLASH => "fal-ai/gemini-25-flash-image/edit",
        GEMINI_31_FLASH => "fal-ai/nano-banana-2/edit",
        GPT5 => "fal-ai/gpt-image-1.5/edit",
        GPT5_MINI => "fal-ai/gpt-image-1-mini/edit",
        GPT_IMAGE_2 => "openai/gpt-image-2/edit",
        SEEDREAM_45 => "fal-ai/bytedance/seedream/v4.5/edit",
        FLUX_2_MAX => "fal-ai/flux-2-max/edit",
        FLUX_2_PRO => "fal-ai/flux-2-pro/edit",
        FLUX_2_FLEX => "fal-ai/flux-2-flex/edit",
        FLUX_2_KLEIN => "fal-ai/flux-2/klein/4b/edit",
        FLUX_1_DEV => "fal-ai/flux/dev/image-to-image",
        FLUX_1_SCHNELL => return None,
        RECRAFT_V4_PRO => return None,
        WAN_27_PRO => "fal-ai/wan/v2.7/pro/edit",
        MUSE_IMAGE => "meta/muse-image/edit",
        QWEN_IMAGE_3 => "alibaba/qwen-image-3/edit",
        SEEDREAM_5_PRO => "bytedance/seedream/v5/pro/edit",
        SEEDREAM_5_LITE => "bytedance/seedream/v5/lite/edit",
        GROK_IMAGINE_IMAGE_2 => "xai/grok-imagine-image/v2.0/edit",
        _ => return None,
    })
}

// ----- Tools --------------------------------------------------------------------------------------
//
// Checked 2026-09-02 against `fal.ai/models/fal-ai/topaz/upscale/{image,video/generative}/api` and
// `fal.ai/models/fal-ai/bria/background/remove/api`.

/// Catalog ids of the tool models fal routes.
pub mod tool_ids {
    pub const TOPAZ_UPSCALE: &str = "topaz-upscale";
    pub const TOPAZ_UPSCALE_VIDEO: &str = "topaz-upscale-video";
    pub const BRIA_BACKGROUND_REMOVE: &str = "bria-background-remove";
}

pub fn tool_endpoint(model: &ToolModel) -> Option<&'static str> {
    Some(match model.id {
        tool_ids::TOPAZ_UPSCALE => "fal-ai/topaz/upscale/image",
        tool_ids::TOPAZ_UPSCALE_VIDEO => "fal-ai/topaz/upscale/video/generative",
        tool_ids::BRIA_BACKGROUND_REMOVE => "fal-ai/bria/background/remove",
        _ => return None,
    })
}

/// Topaz's own enhancement models, as a stable slug plus the display name. The first is the
/// default. A subset of what the endpoint accepts: these are the ones worth choosing between for
/// library media, and every one of them is mapped by [`api_tool_variant`].
const TOPAZ_IMAGE_VARIANTS: &[ToolVariant] = &[
    ToolVariant::new("standard-v2", "Standard V2"),
    ToolVariant::new("high-fidelity-v2", "High Fidelity V2"),
    ToolVariant::new("low-resolution-v2", "Low Resolution V2"),
    ToolVariant::new("cgi", "CGI"),
    ToolVariant::new("text-refine", "Text Refine"),
    ToolVariant::new("recovery-v2", "Recovery V2"),
];

/// Topaz's Starlight family, the diffusion upscalers behind `topaz/upscale/video/generative`.
/// Precise 2.6 is Topaz's own default and the one meant for AI-generated video, which is what a
/// library clip is. Fast 2 is the one that is billed at half the rate.
const TOPAZ_VIDEO_VARIANTS: &[ToolVariant] = &[
    ToolVariant::new("starlight-precise-2.6", "Starlight Precise 2.6"),
    ToolVariant::new("starlight-hq", "Starlight HQ"),
    ToolVariant::new("starlight-mini", "Starlight Mini"),
    ToolVariant::new("starlight-sharp", "Starlight Sharp"),
    ToolVariant::new(TOPAZ_STARLIGHT_FAST_2, "Starlight Fast 2"),
];

/// The one Starlight variant fal prices differently, so `pricing` can tell it apart.
pub const TOPAZ_STARLIGHT_FAST_2: &str = "starlight-fast-2";

pub fn tool_capabilities(model: &ToolModel) -> Option<ToolModelCapabilities> {
    Some(match model.id {
        // The endpoint takes any float; 2× and 4× are the two the composer offers.
        tool_ids::TOPAZ_UPSCALE => ToolModelCapabilities::new(10).with_factors([2, 4]).with_variants(TOPAZ_IMAGE_VARIANTS.to_vec()),
        // One clip per run: a video upscale is minutes of provider time and dollars a second.
        tool_ids::TOPAZ_UPSCALE_VIDEO => ToolModelCapabilities::new(1).with_factors([2, 4]).with_variants(TOPAZ_VIDEO_VARIANTS.to_vec()),
        tool_ids::BRIA_BACKGROUND_REMOVE => ToolModelCapabilities::new(10),
        _ => return None,
    })
}

/// The variant a request will actually run with. Falls back to the model's default variant, so a
/// request naming a variant we have since dropped still runs rather than failing at the provider.
pub fn tool_variant(model: &ToolModel, variant: Option<&str>) -> Option<&'static ToolVariant> {
    let table = match model.id {
        tool_ids::TOPAZ_UPSCALE => TOPAZ_IMAGE_VARIANTS,
        tool_ids::TOPAZ_UPSCALE_VIDEO => TOPAZ_VIDEO_VARIANTS,
        _ => return None,
    };
    let wanted = variant.and_then(|v| table.iter().find(|t| t.id == v));
    wanted.or_else(|| table.first())
}

/// The wire string fal wants for a `ToolVariant::id`, with [`tool_variant`]'s fallback.
pub fn api_tool_variant(model: &ToolModel, variant: Option<&str>) -> Option<&'static str> {
    tool_variant(model, variant).map(|t| t.name)
}

// ----- Video endpoints ----------------------------------------------------------------------------

/// Text-to-video endpoint.
pub fn video_endpoint(model: &VideoModel) -> Option<&'static str> {
    Some(match model.id {
        VEO_31 => "fal-ai/veo3.1",
        VEO_31_FAST => "fal-ai/veo3.1/fast",
        VEO_31_LITE => "fal-ai/veo3.1/lite",
        SORA_2 => "fal-ai/sora-2/text-to-video",
        SORA_2_PRO => "fal-ai/sora-2/text-to-video/pro",
        KLING_O3_PRO => "fal-ai/kling-video/o3/pro/text-to-video",
        KLING_30_PRO => "fal-ai/kling-video/v3/pro/text-to-video",
        KLING_30_STANDARD => "fal-ai/kling-video/v3/standard/text-to-video",
        KLING_25_TURBO_PRO => "fal-ai/kling-video/v2.5-turbo/pro/text-to-video",
        KLING_26_PRO => "fal-ai/kling-video/v2.6/pro/text-to-video",
        SEEDANCE_15_PRO => "fal-ai/bytedance/seedance/v1.5/pro/text-to-video",
        SEEDANCE_20 => "bytedance/seedance-2.0/text-to-video",
        SEEDANCE_20_FAST => "bytedance/seedance-2.0/fast/text-to-video",
        HAPPY_HORSE_10 => "alibaba/happy-horse/text-to-video",
        WAN_27 => "fal-ai/wan/v2.7/text-to-video",
        PIXVERSE_V6 => "fal-ai/pixverse/v6/text-to-video",
        GROK_IMAGINE_VIDEO => "xai/grok-imagine-video/text-to-video",
        GROK_IMAGINE_VIDEO_15 => "xai/grok-imagine-video/v1.5/text-to-video",
        GEMINI_OMNI_FLASH_11 => "google/gemini-omni-flash/v1.1/text-to-video",
        SEEDANCE_25 => "bytedance/seedance-2.5/text-to-video",
        MINIMAX_H3 => "minimax/h3/text-to-video",
        MINIMAX_H3_MAX => "minimax/h3-max/text-to-video",
        FLUX_3 => "blackforestlabs/flux-3/text-to-video",
        HAPPY_HORSE_11 => "alibaba/happy-horse/v1.1/text-to-video",
        WAN_30 => "alibaba/wan-3.0/text-to-video",
        WAN_30_PRIME => "alibaba/wan-3.0-prime/text-to-video",
        _ => return None,
    })
}

/// Image-to-video endpoint.
pub fn video_i2v_endpoint(model: &VideoModel) -> Option<&'static str> {
    Some(match model.id {
        VEO_31 => "fal-ai/veo3.1/image-to-video",
        VEO_31_FAST => "fal-ai/veo3.1/fast/image-to-video",
        VEO_31_LITE => "fal-ai/veo3.1/lite/image-to-video",
        SORA_2 => "fal-ai/sora-2/image-to-video",
        SORA_2_PRO => "fal-ai/sora-2/image-to-video/pro",
        KLING_O3_PRO => "fal-ai/kling-video/o3/pro/image-to-video",
        KLING_30_PRO => "fal-ai/kling-video/v3/pro/image-to-video",
        KLING_30_STANDARD => "fal-ai/kling-video/v3/standard/image-to-video",
        KLING_25_TURBO_PRO => "fal-ai/kling-video/v2.5-turbo/pro/image-to-video",
        KLING_26_PRO => "fal-ai/kling-video/v2.6/pro/image-to-video",
        SEEDANCE_15_PRO => "fal-ai/bytedance/seedance/v1.5/pro/image-to-video",
        SEEDANCE_20 => "bytedance/seedance-2.0/image-to-video",
        SEEDANCE_20_FAST => "bytedance/seedance-2.0/fast/image-to-video",
        HAPPY_HORSE_10 => "alibaba/happy-horse/image-to-video",
        WAN_27 => "fal-ai/wan/v2.7/image-to-video",
        PIXVERSE_V6 => "fal-ai/pixverse/v6/image-to-video",
        GROK_IMAGINE_VIDEO => "xai/grok-imagine-video/image-to-video",
        GROK_IMAGINE_VIDEO_15 => "xai/grok-imagine-video/v1.5/image-to-video",
        GEMINI_OMNI_FLASH_11 => "google/gemini-omni-flash/v1.1/image-to-video",
        SEEDANCE_25 => "bytedance/seedance-2.5/image-to-video",
        MINIMAX_H3 => "minimax/h3/image-to-video",
        MINIMAX_H3_MAX => "minimax/h3-max/image-to-video",
        FLUX_3 => "blackforestlabs/flux-3/image-to-video",
        HAPPY_HORSE_11 => "alibaba/happy-horse/v1.1/image-to-video",
        WAN_30 => "alibaba/wan-3.0/image-to-video",
        WAN_30_PRIME => "alibaba/wan-3.0-prime/image-to-video",
        _ => return None,
    })
}

/// Endpoint that takes a required first frame AND a required last frame. Distinct from the i2v
/// endpoint because (a) only some models expose it and (b) the request param keys differ
/// (`first_frame_url` / `last_frame_url`, not `image_url` / `end_image_url`). For models where the
/// i2v endpoint already accepts both keys (Kling, Seedance, WAN) this returns `None` and the caller
/// stays on i2v.
pub fn video_first_last_frame_endpoint(model: &VideoModel) -> Option<&'static str> {
    Some(match model.id {
        VEO_31 => "fal-ai/veo3.1/first-last-frame-to-video",
        VEO_31_FAST => "fal-ai/veo3.1/fast/first-last-frame-to-video",
        VEO_31_LITE => "fal-ai/veo3.1/lite/first-last-frame-to-video",
        FLUX_3 => "blackforestlabs/flux-3/first-last-frame-to-video",
        _ => return None,
    })
}

/// Endpoint that takes lists of reference media the prompt addresses by handle. Distinct from i2v:
/// it has no frame parameter at all, which is why references and frames are mutually exclusive.
pub fn video_reference_endpoint(model: &VideoModel) -> Option<&'static str> {
    Some(match model.id {
        VEO_31 => "fal-ai/veo3.1/reference-to-video",
        VEO_31_FAST => "fal-ai/veo3.1/fast/reference-to-video",
        KLING_O3_PRO => "fal-ai/kling-video/o3/pro/video-to-video/reference",
        SEEDANCE_20 => "bytedance/seedance-2.0/reference-to-video",
        SEEDANCE_20_FAST => "bytedance/seedance-2.0/fast/reference-to-video",
        SEEDANCE_25 => "bytedance/seedance-2.5/reference-to-video",
        HAPPY_HORSE_10 => "alibaba/happy-horse/reference-to-video",
        HAPPY_HORSE_11 => "alibaba/happy-horse/v1.1/reference-to-video",
        WAN_27 => "fal-ai/wan/v2.7/reference-to-video",
        WAN_30 => "alibaba/wan-3.0/reference-to-video",
        WAN_30_PRIME => "alibaba/wan-3.0-prime/reference-to-video",
        GROK_IMAGINE_VIDEO => "xai/grok-imagine-video/reference-to-video",
        GROK_IMAGINE_VIDEO_15 => "xai/grok-imagine-video/v1.5/reference-to-video",
        GEMINI_OMNI_FLASH_11 => "google/gemini-omni-flash/v1.1/reference-to-video",
        MINIMAX_H3 => "minimax/h3/reference-to-video",
        MINIMAX_H3_MAX => "minimax/h3-max/reference-to-video",
        _ => return None,
    })
}

/// The request keys and prompt dialect of a model's reference endpoint. Read off each endpoint's
/// own `openapi.json` on 2026-08-29. No two families agree, and like the price tables these change,
/// so re-check the schema before adding a model here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceParams {
    pub images: Option<&'static str>,
    pub videos: Option<&'static str>,
    pub audio: Option<&'static str>,
    pub style: ReferenceTagStyle,
    /// `videos` is one URL string rather than a list (Kling O3 Pro's `video_url`).
    pub single_video: bool,
    /// The key the reference endpoint spells the audio switch under, where it differs from the
    /// model's other endpoints; it replaces `api_audio_param`'s key in the reference body.
    pub audio_toggle: Option<&'static str>,
}

impl ReferenceParams {
    const fn new(images: &'static str, style: ReferenceTagStyle) -> Self {
        Self { images: Some(images), videos: None, audio: None, style, single_video: false, audio_toggle: None }
    }

    const fn with_videos(mut self, videos: &'static str) -> Self {
        self.videos = Some(videos);
        self
    }

    const fn with_single_video(mut self, video: &'static str) -> Self {
        self.videos = Some(video);
        self.single_video = true;
        self
    }

    const fn with_audio_toggle(mut self, audio_toggle: &'static str) -> Self {
        self.audio_toggle = Some(audio_toggle);
        self
    }

    const fn with_audio(mut self, audio: &'static str) -> Self {
        self.audio = Some(audio);
        self
    }

    pub fn param_for(&self, role: crate::AssetRole) -> Option<&'static str> {
        match role {
            crate::AssetRole::ReferenceImage => self.images,
            crate::AssetRole::ReferenceVideo => self.videos,
            crate::AssetRole::Audio => self.audio,
            _ => None,
        }
    }
}

pub fn video_reference_params(model: &VideoModel) -> Option<ReferenceParams> {
    use ReferenceTagStyle::*;
    Some(match model.id {
        VEO_31 | VEO_31_FAST => ReferenceParams::new("image_urls", Prose),
        // Checked 2026-09-02; the video-to-video endpoint takes one clip under a singular key and
        // spells its audio switch `keep_audio` (the reference clip's own track) where the
        // family's other endpoints say `generate_audio`.
        KLING_O3_PRO => ReferenceParams::new("image_urls", At).with_single_video("video_url").with_audio_toggle("keep_audio"),
        SEEDANCE_20 | SEEDANCE_20_FAST | SEEDANCE_25 => ReferenceParams::new("image_urls", At).with_videos("video_urls").with_audio("audio_urls"),
        HAPPY_HORSE_10 | HAPPY_HORSE_11 => ReferenceParams::new("image_urls", Character),
        WAN_27 => ReferenceParams::new("reference_image_urls", Prose).with_videos("reference_video_urls"),
        WAN_30 | WAN_30_PRIME => ReferenceParams::new("reference_image_urls", Prose)
            .with_videos("reference_video_urls")
            .with_audio("reference_audio_urls"),
        GROK_IMAGINE_VIDEO => ReferenceParams::new("reference_image_urls", At),
        GROK_IMAGINE_VIDEO_15 => ReferenceParams::new("reference_image_urls", AngleZeroBased),
        GEMINI_OMNI_FLASH_11 => ReferenceParams::new("image_urls", Prose).with_videos("reference_video_urls"),
        MINIMAX_H3 | MINIMAX_H3_MAX => ReferenceParams::new("reference_image_urls", Prose)
            .with_videos("reference_video_urls")
            .with_audio("reference_audio_urls"),
        _ => return None,
    })
}

/// Which fal endpoint variant a request is targeting. The frame-param helpers return different keys
/// depending on the variant — e.g. veo3.1 uses `image_url` on i2v but `first_frame_url` on the
/// first-last endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEndpointVariant {
    T2v,
    I2v,
    FirstLast,
    /// The reference endpoint: lists of media, no frames.
    Reference,
}

// ----- Image API mappings -------------------------------------------------------------------------

/// Maps an `AspectRatio` to the fal size parameter for a given image model.
/// Gemini models use `aspect_ratio` with raw ratio strings; FLUX/Wan/Seedream/Recraft use
/// `image_size` with named presets; GPT uses `image_size` with pixel dimensions.
///
/// Returns `None` when the model isn't on fal, or when the requested aspect ratio isn't supported
/// by the model's size family; callers then skip the key.
pub fn api_image_size(model: &ImageModel, aspect_ratio: AspectRatio) -> Option<(&'static str, String)> {
    match model.id {
        GEMINI_3_PRO | GEMINI_31_FLASH | GEMINI_25_FLASH => Some(("aspect_ratio", aspect_ratio.raw().to_string())),
        GPT5 | GPT5_MINI => api_gpt_image_size(aspect_ratio).map(|v| ("image_size", v.to_string())),
        GPT_IMAGE_2 | SEEDREAM_45 | FLUX_2_MAX | FLUX_2_PRO | FLUX_2_FLEX | FLUX_2_KLEIN | FLUX_1_DEV | FLUX_1_SCHNELL | RECRAFT_V4_PRO
        | WAN_27_PRO | QWEN_IMAGE_3 | SEEDREAM_5_PRO | SEEDREAM_5_LITE => api_named_image_size(aspect_ratio).map(|v| ("image_size", v.to_string())),
        MUSE_IMAGE | GROK_IMAGINE_IMAGE_2 => Some(("aspect_ratio", aspect_ratio.raw().to_string())),
        _ => None,
    }
}

pub fn api_image_resolution(model: &ImageModel, resolution: ImageResolution) -> Option<(&'static str, String)> {
    match model.id {
        GEMINI_3_PRO | GEMINI_31_FLASH => Some(("resolution", resolution.raw().to_string())),
        GPT_IMAGE_2 => api_gpt_image_quality(resolution).map(|v| ("quality", v.to_string())),
        // Grok Imagine Image 2 sells 1K / 2K tiers under `resolution`, spelled lowercase.
        GROK_IMAGINE_IMAGE_2 => match resolution {
            ImageResolution::Hd => Some(("resolution", "1k".to_string())),
            ImageResolution::Fhd => Some(("resolution", "2k".to_string())),
            ImageResolution::Sd | ImageResolution::Uhd => None,
        },
        GEMINI_25_FLASH | GPT5 | GPT5_MINI | SEEDREAM_45 | FLUX_2_MAX | FLUX_2_PRO | FLUX_2_FLEX | FLUX_2_KLEIN | FLUX_1_DEV | FLUX_1_SCHNELL
        | RECRAFT_V4_PRO | WAN_27_PRO | MUSE_IMAGE | QWEN_IMAGE_3 | SEEDREAM_5_PRO | SEEDREAM_5_LITE => None,
        _ => None,
    }
}

pub fn api_edit_image_param(model: &ImageModel) -> Option<&'static str> {
    Some(match model.id {
        GEMINI_3_PRO => "image_urls",
        GEMINI_31_FLASH => "image_urls",
        GEMINI_25_FLASH => "image_urls",
        GPT5 => "image_urls",
        GPT5_MINI => "image_urls",
        GPT_IMAGE_2 => "image_urls",
        SEEDREAM_45 => "image_urls",
        FLUX_2_MAX => "image_urls",
        FLUX_2_PRO => "image_urls",
        FLUX_2_FLEX => "image_urls",
        FLUX_2_KLEIN => "image_urls",
        FLUX_1_DEV => "image_url",
        FLUX_1_SCHNELL => "image_urls",
        RECRAFT_V4_PRO => "image_urls",
        WAN_27_PRO => "image_urls",
        MUSE_IMAGE => "image_urls",
        QWEN_IMAGE_3 => "image_urls",
        SEEDREAM_5_PRO => "image_urls",
        SEEDREAM_5_LITE => "image_urls",
        GROK_IMAGINE_IMAGE_2 => "image_urls",
        _ => return None,
    })
}

pub fn api_supports_output_format(model: &ImageModel) -> Option<bool> {
    Some(match model.id {
        GEMINI_3_PRO => true,
        GEMINI_31_FLASH => true,
        GEMINI_25_FLASH => true,
        GPT5 => true,
        GPT5_MINI => true,
        GPT_IMAGE_2 => true,
        SEEDREAM_45 => false,
        FLUX_2_MAX => true,
        FLUX_2_PRO => true,
        FLUX_2_FLEX => true,
        FLUX_2_KLEIN => true,
        FLUX_1_DEV => true,
        FLUX_1_SCHNELL => true,
        RECRAFT_V4_PRO => false,
        WAN_27_PRO => false,
        MUSE_IMAGE => true,
        QWEN_IMAGE_3 => true,
        SEEDREAM_5_PRO => true,
        SEEDREAM_5_LITE => true,
        GROK_IMAGINE_IMAGE_2 => true,
        _ => return None,
    })
}

/// API parameter key for an inpainting mask. `None` means the model has no native mask field on its
/// fal /edit endpoint, in which case callers must reject mask assets rather than silently dropping
/// them. As of the 2026-05-04 spec sweep only the two GPT image models expose a mask field — and
/// they use different keys (`mask_image_url` vs `mask_url`).
pub fn api_mask_param(model: &ImageModel) -> Option<&'static str> {
    match model.id {
        GPT5 => Some("mask_image_url"),
        GPT_IMAGE_2 => Some("mask_url"),
        _ => None,
    }
}

/// gpt-image-2's `quality` knob maps onto our resolution picker: 1K = low, 2K = medium, 4K = high.
/// `Sd` (0.5K) has no fal mapping and isn't in `supported_resolutions`.
fn api_gpt_image_quality(resolution: ImageResolution) -> Option<&'static str> {
    match resolution {
        ImageResolution::Sd => None,
        ImageResolution::Hd => Some("low"),
        ImageResolution::Fhd => Some("medium"),
        ImageResolution::Uhd => Some("high"),
    }
}

/// GPT image sizes only cover square. Other aspect ratios return `None` — the caller omits the key
/// and fal uses its default.
fn api_gpt_image_size(aspect_ratio: AspectRatio) -> Option<&'static str> {
    match aspect_ratio {
        AspectRatio::Square => Some("1024x1024"),
        AspectRatio::ThreeToFour | AspectRatio::Standard | AspectRatio::Portrait | AspectRatio::Tall | AspectRatio::Landscape | AspectRatio::Wide => None,
    }
}

/// Fal named sizes don't have a mapping for `Portrait` (4:5) or `Wide` (21:9); those return `None`.
fn api_named_image_size(aspect_ratio: AspectRatio) -> Option<&'static str> {
    match aspect_ratio {
        AspectRatio::Square => Some("square_hd"),
        AspectRatio::ThreeToFour => Some("portrait_4_3"),
        AspectRatio::Standard => Some("landscape_4_3"),
        AspectRatio::Tall => Some("portrait_16_9"),
        AspectRatio::Landscape => Some("landscape_16_9"),
        AspectRatio::Portrait | AspectRatio::Wide => None,
    }
}

// ----- Video API mappings -------------------------------------------------------------------------

/// Maps our `VideoAspectRatio` to the endpoint's aspect-ratio field, as `(key, value)`.
/// Almost every fal endpoint takes `aspect_ratio` with our raw value; the exceptions are models
/// that spell "let the model decide" differently, or that have no aspect-ratio field at all.
pub fn api_video_aspect_ratio(model: &VideoModel, aspect_ratio: VideoAspectRatio) -> Option<(&'static str, &'static str)> {
    use VideoAspectRatio::*;
    match model.id {
        // Wan 3.0 calls `.auto` `adaptive`.
        WAN_30 | WAN_30_PRIME => match aspect_ratio {
            Auto => Some(("aspect_ratio", "adaptive")),
            _ => Some(("aspect_ratio", aspect_ratio.raw())),
        },
        // H3 has no `auto`; the text-to-video endpoint rejects anything outside its six ratios.
        MINIMAX_H3 | MINIMAX_H3_MAX => match aspect_ratio {
            Auto | NarrowLandscape | NarrowPortrait => None,
            _ => Some(("aspect_ratio", aspect_ratio.raw())),
        },
        _ => Some(("aspect_ratio", aspect_ratio.raw())),
    }
}

/// Maps our `VideoResolution` to the endpoint's resolution field, as `(key, value)`.
/// Every endpoint but MiniMax's takes our raw value under `resolution`; H3 spells the tiers
/// `480P` / `768P` / `2K` / `4K`, with an uppercase P and two names we have no enum for.
pub fn api_video_resolution(model: &VideoModel, resolution: VideoResolution) -> Option<(&'static str, &'static str)> {
    use VideoResolution::*;
    match model.id {
        MINIMAX_H3 => Some((
            "resolution",
            match resolution {
                Sd => "480P",
                Hd => "768P",
                Fhd => "2K",
                Uhd => "4K",
            },
        )),
        // H3 Max only sells the two lower tiers.
        MINIMAX_H3_MAX => match resolution {
            Sd => Some(("resolution", "480P")),
            Hd => Some(("resolution", "768P")),
            Fhd | Uhd => None,
        },
        _ => Some(("resolution", resolution.raw())),
    }
}

/// Maps duration to the JSON value each video model's API expects (some take a string, some an int).
pub fn api_duration(model: &VideoModel, duration: u32) -> Option<Value> {
    Some(match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => Value::String(format!("{duration}s")),
        SORA_2 | SORA_2_PRO => Value::from(duration),
        KLING_O3_PRO | KLING_30_PRO | KLING_30_STANDARD | KLING_25_TURBO_PRO | KLING_26_PRO => Value::String(duration.to_string()),
        SEEDANCE_15_PRO | SEEDANCE_20 | SEEDANCE_20_FAST | SEEDANCE_25 => Value::String(duration.to_string()),
        // FLUX 3's duration enum mixes the string "auto" with integers; a number goes as a number.
        FLUX_3 => Value::from(duration),
        GEMINI_OMNI_FLASH_11 | MINIMAX_H3 | MINIMAX_H3_MAX | WAN_30 | WAN_30_PRIME => Value::from(duration),
        HAPPY_HORSE_11 => Value::from(duration),
        GROK_IMAGINE_VIDEO_15 => Value::from(duration),
        HAPPY_HORSE_10 | WAN_27 | PIXVERSE_V6 | GROK_IMAGINE_VIDEO => Value::from(duration),
        _ => return None,
    })
}

pub fn api_start_frame_param(model: &VideoModel, variant: VideoEndpointVariant) -> Option<&'static str> {
    match variant {
        VideoEndpointVariant::T2v | VideoEndpointVariant::Reference => None,
        VideoEndpointVariant::I2v => api_i2v_start_frame_param(model),
        VideoEndpointVariant::FirstLast => api_first_last_frame_params(model).map(|(start, _)| start),
    }
}

pub fn api_end_frame_param(model: &VideoModel, variant: VideoEndpointVariant) -> Option<&'static str> {
    match variant {
        VideoEndpointVariant::T2v | VideoEndpointVariant::Reference => None,
        VideoEndpointVariant::I2v => api_i2v_end_frame_param(model),
        VideoEndpointVariant::FirstLast => api_first_last_frame_params(model).map(|(_, end)| end),
    }
}

/// The `(start, end)` keys the model's first-last-frame endpoint takes. The endpoints don't agree
/// on them — veo3.1 takes `first_frame_url` / `last_frame_url`, FLUX 3 `start_image_url` /
/// `end_image_url` — so this is a per-model table rather than a constant derived from the endpoint
/// existing. Every model in [`video_first_last_frame_endpoint`] must appear here; the
/// `every_first_last_endpoint_has_frame_params` test in `tests/fal.rs` fails if one doesn't.
fn api_first_last_frame_params(model: &VideoModel) -> Option<(&'static str, &'static str)> {
    Some(match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => ("first_frame_url", "last_frame_url"),
        FLUX_3 => ("start_image_url", "end_image_url"),
        _ => return None,
    })
}

fn api_i2v_start_frame_param(model: &VideoModel) -> Option<&'static str> {
    Some(match model.id {
        VEO_31 => "image_url",
        VEO_31_FAST => "image_url",
        VEO_31_LITE => "image_url",
        SORA_2 => "image_url",
        SORA_2_PRO => "image_url",
        // O3's image-to-video endpoint went back to `image_url`, where 3.0 says `start_image_url`.
        KLING_O3_PRO => "image_url",
        KLING_30_PRO => "start_image_url",
        KLING_30_STANDARD => "start_image_url",
        KLING_25_TURBO_PRO => "image_url",
        KLING_26_PRO => "start_image_url",
        SEEDANCE_15_PRO => "image_url",
        SEEDANCE_20 => "image_url",
        SEEDANCE_20_FAST => "image_url",
        HAPPY_HORSE_10 => "image_url",
        WAN_27 => "image_url",
        PIXVERSE_V6 => "image_url",
        GROK_IMAGINE_VIDEO => "image_url",
        GROK_IMAGINE_VIDEO_15 => "image_url",
        GEMINI_OMNI_FLASH_11 => "image_url",
        SEEDANCE_25 => "image_url",
        MINIMAX_H3 | MINIMAX_H3_MAX => "image_url",
        FLUX_3 => "image_url",
        HAPPY_HORSE_11 => "image_url",
        // Wan 3.0's image-to-video endpoint requires `start_image_url`, not `image_url`.
        WAN_30 | WAN_30_PRIME => "start_image_url",
        _ => return None,
    })
}

fn api_i2v_end_frame_param(model: &VideoModel) -> Option<&'static str> {
    match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => None,
        SORA_2 | SORA_2_PRO => None,
        KLING_O3_PRO => Some("end_image_url"),
        KLING_30_PRO => Some("end_image_url"),
        KLING_30_STANDARD => Some("end_image_url"),
        KLING_25_TURBO_PRO => Some("tail_image_url"),
        KLING_26_PRO => Some("end_image_url"),
        SEEDANCE_15_PRO => Some("end_image_url"),
        SEEDANCE_20 => Some("end_image_url"),
        SEEDANCE_20_FAST => Some("end_image_url"),
        HAPPY_HORSE_10 => None,
        WAN_27 => Some("end_image_url"),
        PIXVERSE_V6 => None,
        GROK_IMAGINE_VIDEO => None,
        GROK_IMAGINE_VIDEO_15 => None,
        GEMINI_OMNI_FLASH_11 => Some("end_image_url"),
        SEEDANCE_25 => Some("end_image_url"),
        MINIMAX_H3 | MINIMAX_H3_MAX => Some("end_image_url"),
        // FLUX 3's i2v endpoint takes an opening frame only; both frames go to its own endpoint.
        FLUX_3 => None,
        HAPPY_HORSE_11 => None,
        WAN_30 | WAN_30_PRIME => Some("end_image_url"),
        _ => None,
    }
}

/// API parameter key for the audio toggle. `None` means either the model has no audio parameter or
/// fal does not support the model; callers omit the key.
pub fn api_audio_param(model: &VideoModel) -> Option<&'static str> {
    match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => Some("generate_audio"),
        SORA_2 | SORA_2_PRO => None,
        KLING_O3_PRO | KLING_30_PRO | KLING_30_STANDARD => Some("generate_audio"),
        KLING_25_TURBO_PRO => None,
        KLING_26_PRO => Some("generate_audio"),
        SEEDANCE_15_PRO | SEEDANCE_20 | SEEDANCE_20_FAST => Some("generate_audio"),
        HAPPY_HORSE_10 => None,
        WAN_27 => None,
        PIXVERSE_V6 => Some("generate_audio_switch"),
        GROK_IMAGINE_VIDEO | GROK_IMAGINE_VIDEO_15 => None,
        GEMINI_OMNI_FLASH_11 => None,
        SEEDANCE_25 => Some("generate_audio"),
        MINIMAX_H3 | MINIMAX_H3_MAX => None,
        FLUX_3 => Some("generate_audio"),
        // Happy Horse always renders audio; there is no toggle to send.
        HAPPY_HORSE_11 => None,
        // Wan 3.0 spells the toggle `audio`, not `generate_audio`.
        WAN_30 | WAN_30_PRIME => Some("audio"),
        _ => None,
    }
}

/// API parameter key for user-supplied audio conditioning.
pub fn api_audio_input_param(model: &VideoModel) -> Option<&'static str> {
    match model.id {
        // Wan 2.7 conditions on a supplied track; Wan 3.0 dropped the input and kept only the
        // `audio` toggle, so it takes no audio asset.
        WAN_27 => Some("audio_url"),
        _ => None,
    }
}

/// Fields a model's schema marks required but that the composer has no setting for, sent at the
/// value the endpoint documents as its default. H3 Max is the only one: its schema requires
/// `prompt_expansion_mode` where plain H3 defaults it, and a missing required field is a 422.
pub fn api_required_defaults(model: &VideoModel) -> &'static [(&'static str, &'static str)] {
    match model.id {
        MINIMAX_H3_MAX => &[("prompt_expansion_mode", "balanced")],
        _ => &[],
    }
}

// ----- Audio models + voices ----------------------------------------------------------------------

/// fal's ElevenLabs v3 endpoints accept a different voice list from Replicate's `elevenlabs/v3`
/// schema, so this stays provider-specific.
pub fn eleven_labs_v3_voices() -> &'static [AudioVoice] {
    crate::voices::elevenlabs::fal_voices()
}

/// Gemini 30-voice set from the Google Cloud Gemini-TTS docs.
pub fn gemini_voices() -> &'static [AudioVoice] {
    crate::voices::gemini::all()
}

pub fn audio_capabilities(model: &AudioModel) -> Option<AudioModelCapabilities> {
    match model.id {
        ELEVEN_LABS_V3 => {
            let voices = eleven_labs_v3_voices();
            Some(AudioModelCapabilities {
                supported_voices: voices.to_vec(),
                supports_two_speakers: true,
                max_characters_monologue: 5000,
                max_characters_dialogue: 2000,
                default_voice: voices.iter().find(|v| v.id == "Rachel").cloned(),
                secondary_default_voice: voices.iter().find(|v| v.id == "Roger").cloned(),
            })
        }
        GEMINI_25_PRO => {
            let voices = gemini_voices();
            Some(AudioModelCapabilities {
                supported_voices: voices.to_vec(),
                supports_two_speakers: true,
                max_characters_monologue: 50000,
                max_characters_dialogue: 50000,
                default_voice: voices.iter().find(|v| v.id == "Kore").cloned(),
                secondary_default_voice: voices.iter().find(|v| v.id == "Puck").cloned(),
            })
        }
        _ => None,
    }
}
