//! Replicate capabilities: supported model lists, per-model capabilities, slugs and
//! the per-slug request parameter mappings.
//!
//! Source of truth for every per-model entry below: the latest version's `openapi_schema` for each
//! Replicate slug, fetched 2026-05-05, and 2026-08-29 for the slugs added in that sweep (seedance
//! 2.5, minimax/h3, flux-3, happyhorse-1.1, wan-3, wan-3-prime, grok-imagine-video-1.5,
//! qwen-image-3, seedream-5-pro, seedream-5-lite, grok-imagine-image-2). When Replicate ships a
//! schema change to a slug we use, re-sweep that slug and update the affected entries here.

use serde_json::{json, Value};

use crate::asset::{AssetConstraints, AssetRole};
use crate::models::{
    AspectRatio, ImageModel, ImageResolution, ModelCapabilities, ToolModel, ToolModelCapabilities, VideoAspectRatio, VideoDurationRange, VideoModel,
    VideoModelCapabilities, VideoReferences, VideoResolution,
};
use crate::references::ReferenceTagStyle;
use crate::replicate::error::{ReplicateError, ReplicateResult};

// ----- catalog ids ------------------------------------------------------------------------------

pub mod image_ids {
    pub const GEMINI_3_PRO: &str = "gemini-3-pro";
    pub const GEMINI_31_FLASH: &str = "gemini-3.1-flash";
    pub const GEMINI_25_FLASH: &str = "gemini-2.5-flash";
    pub const GPT_IMAGE_2: &str = "gpt-image-2";
    pub const GPT_5: &str = "gpt-5-image";
    pub const SEEDREAM_45: &str = "seedream-4.5";
    pub const FLUX_2_MAX: &str = "flux-2-max";
    pub const FLUX_2_PRO: &str = "flux-2-pro";
    pub const FLUX_2_FLEX: &str = "flux-2-flex";
    pub const FLUX_2_KLEIN: &str = "flux-2-klein";
    pub const FLUX_1_DEV: &str = "flux-1-dev";
    pub const FLUX_1_SCHNELL: &str = "flux-1-schnell";
    pub const RECRAFT_V4_PRO: &str = "recraft-4-pro";
    pub const WAN_27_PRO: &str = "wan-2.7-pro";
    pub const QWEN_IMAGE_3: &str = "qwen-image-3";
    pub const SEEDREAM_5_PRO: &str = "seedream-5-pro";
    pub const SEEDREAM_5_LITE: &str = "seedream-5-lite";
    pub const GROK_IMAGINE_IMAGE_2: &str = "grok-imagine-image-2";
}

pub mod video_ids {
    pub const VEO_31: &str = "veo-3.1";
    pub const VEO_31_FAST: &str = "veo-3.1-fast";
    pub const VEO_31_LITE: &str = "veo-3.1-lite";
    pub const SORA_2_PRO: &str = "sora-2-pro";
    pub const SORA_2: &str = "sora-2";
    pub const KLING_30_PRO: &str = "kling-3-pro";
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
    pub const SEEDANCE_25: &str = "seedance-2.5";
    pub const MINIMAX_H3: &str = "minimax-h3";
    pub const FLUX_3: &str = "flux-3";
    pub const HAPPY_HORSE_11: &str = "happyhorse-1.1";
    pub const WAN_30: &str = "wan-3.0";
    pub const WAN_30_PRIME: &str = "wan-3.0-prime";
}

use image_ids::*;
use video_ids::*;

// ----- supported models -------------------------------------------------------------------------

/// Catalog ids of the image models Replicate supports, in display order.
pub const SUPPORTED_IMAGE_MODEL_IDS: &[&str] = &[
    GEMINI_3_PRO,
    GEMINI_31_FLASH,
    GEMINI_25_FLASH,
    GPT_IMAGE_2,
    GPT_5,
    // gpt5Mini intentionally excluded — Replicate's openai/gpt-image-1-mini
    // slug has openai_api_key as a *required* input (BYOK / pass-through
    // billing), unlike the gpt-image-1.5 and gpt-image-2 slugs which are
    // billed through Replicate. Our keychain holds one Replicate token
    // per user; we can't hold a second OpenAI sk-key to pass through.
    SEEDREAM_5_PRO,
    SEEDREAM_5_LITE,
    SEEDREAM_45,
    // museImage intentionally excluded — Meta ships Muse Image on fal and OpenRouter only.
    FLUX_2_MAX,
    FLUX_2_PRO,
    FLUX_2_FLEX,
    FLUX_2_KLEIN,
    FLUX_1_DEV,
    FLUX_1_SCHNELL,
    RECRAFT_V4_PRO,
    QWEN_IMAGE_3,
    // qwenImage3Pro intentionally excluded — the Pro tier is an OpenRouter-only listing;
    // Replicate publishes no `qwen-image-3-pro` slug.
    WAN_27_PRO,
    GROK_IMAGINE_IMAGE_2,
];

/// Catalog ids of the video models Replicate supports, in display order.
pub const SUPPORTED_VIDEO_MODEL_IDS: &[&str] = &[
    VEO_31,
    VEO_31_FAST,
    VEO_31_LITE,
    // geminiOmniFlash11 intentionally excluded — fal-only; Replicate has no Gemini Omni slug.
    SORA_2_PRO,
    SORA_2,
    // klingO3Pro intentionally excluded for now — Replicate's `kwaivgi/kling-v3-omni-video` slug
    // picks its tier with a `mode` field, addresses references as `<<<image_1>>>`, takes one
    // reference clip of 3–10 s, and publishes no price we could check; wiring it is its own change.
    KLING_30_PRO,
    // kling30Standard intentionally excluded — Replicate has no equivalent SKU.
    KLING_26_PRO,
    KLING_25_TURBO_PRO,
    SEEDANCE_25,
    SEEDANCE_20,
    SEEDANCE_20_FAST,
    SEEDANCE_15_PRO,
    MINIMAX_H3,
    // minimaxH3Max and minimaxH3MaxTurbo intentionally excluded — Replicate publishes no
    // `minimax/h3-max` or `minimax/h3-max-turbo` slug (checked 2026-09-03).
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

pub fn supported_image_models() -> Vec<ImageModel> {
    SUPPORTED_IMAGE_MODEL_IDS
        .iter()
        .map(|id| crate::catalog::image::model(id).unwrap_or_else(|| panic!("image model '{id}' missing from catalog")).clone())
        .collect()
}

pub fn supported_video_models() -> Vec<VideoModel> {
    SUPPORTED_VIDEO_MODEL_IDS
        .iter()
        .map(|id| crate::catalog::video::model(id).unwrap_or_else(|| panic!("video model '{id}' missing from catalog")).clone())
        .collect()
}

// ----- image capabilities -----------------------------------------------------------------------

pub fn image_capabilities(model: &ImageModel) -> Option<ModelCapabilities> {
    use AspectRatio::*;
    use ImageResolution::*;
    let caps = match model.id {
        GEMINI_3_PRO => ModelCapabilities::new([Square, ThreeToFour, Standard, Portrait, Tall, Landscape, Wide], [Hd, Fhd, Uhd], 14),
        GEMINI_31_FLASH => ModelCapabilities::new([Square, ThreeToFour, Standard, Portrait, Tall, Landscape, Wide], [Hd, Fhd, Uhd], 14),
        GEMINI_25_FLASH => ModelCapabilities::new([Square, ThreeToFour, Standard, Portrait, Tall, Landscape, Wide], [], 14),
        GPT_5 => ModelCapabilities::new([Square], [Hd, Fhd, Uhd], 1),
        GPT_IMAGE_2 => ModelCapabilities::new([Square], [Hd, Fhd, Uhd], 10).with_default_resolution(Fhd),
        SEEDREAM_45 => ModelCapabilities::new([Square, Standard, ThreeToFour, Landscape, Tall], [Fhd, Uhd], 10),
        FLUX_2_MAX => ModelCapabilities::new([Square, Landscape, Portrait, Tall, ThreeToFour, Standard], [Sd, Hd, Fhd, Uhd], 8),
        FLUX_2_PRO => ModelCapabilities::new([Square, Landscape, Portrait, Tall, ThreeToFour, Standard], [Sd, Hd, Fhd, Uhd], 8),
        FLUX_2_FLEX => ModelCapabilities::new([Square, Landscape, Portrait, Tall, ThreeToFour, Standard], [Sd, Hd, Fhd, Uhd], 10),
        FLUX_2_KLEIN => ModelCapabilities::new([Square, Landscape, Tall, Standard, ThreeToFour, Portrait, Wide], [Sd, Hd, Fhd, Uhd], 4),
        FLUX_1_DEV => ModelCapabilities::new([Square, Landscape, Wide, Portrait, ThreeToFour, Standard, Tall], [Sd, Hd], 1),
        FLUX_1_SCHNELL => ModelCapabilities::new([Square, Landscape, Wide, Portrait, ThreeToFour, Standard, Tall], [Sd, Hd], 0),
        RECRAFT_V4_PRO => ModelCapabilities::new([Square, Standard, ThreeToFour, Landscape, Tall, Portrait], [], 0),
        // wan-2.7-image-pro doesn't expose `aspect_ratio` — uses combined `size`.
        // We expose only resolution; aspect ratio picker hidden.
        WAN_27_PRO => ModelCapabilities::new([], [Hd, Fhd, Uhd], 4),
        // Seedream 5 sells `size` tiers 1K / 1.5K / 2K / auto; we map our three onto them.
        SEEDREAM_5_PRO | SEEDREAM_5_LITE => ModelCapabilities::new([Square, Standard, ThreeToFour, Landscape, Tall, Wide], [Hd, Fhd], 10),
        // Replicate's qwen-image-3 takes a plain `aspect_ratio` enum and no resolution knob. It also
        // offers 3:2 / 2:3 / 2:1 / 1:2, which our image `AspectRatio` has no members for.
        QWEN_IMAGE_3 => ModelCapabilities::new([Square, Standard, ThreeToFour, Landscape, Tall], [], 1),
        // Grok Imagine Image 2 defaults to 2K on Replicate; both tiers are offered.
        GROK_IMAGINE_IMAGE_2 => ModelCapabilities::new([Square, Standard, ThreeToFour, Landscape, Tall], [Hd, Fhd], 1).with_default_resolution(Fhd),
        _ => return None,
    };
    Some(caps)
}

// ----- video capabilities -----------------------------------------------------------------------

fn first_frame_only() -> AssetConstraints {
    AssetConstraints::new([(AssetRole::FirstFrame, 0..=1)])
}

pub fn video_capabilities(model: &VideoModel) -> Option<VideoModelCapabilities> {
    use VideoAspectRatio::*;
    use VideoResolution::*;
    let caps = match model.id {
        VEO_31 => VideoModelCapabilities::new(VideoDurationRange::new(4, 8, Some(vec![4, 6, 8])), [Landscape, Tall], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_references(VideoReferences::images(3))
            .with_audio(true, false),
        VEO_31_FAST => VideoModelCapabilities::new(VideoDurationRange::new(4, 8, Some(vec![4, 6, 8])), [Landscape, Tall], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(true, false),
        VEO_31_LITE => VideoModelCapabilities::new(VideoDurationRange::new(4, 8, Some(vec![4, 6, 8])), [Landscape, Tall], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(false, false),
        // Replicate's openai/sora-2 slug exposes neither image input nor
        // duration nor explicit resolution — only prompt + aspect_ratio.
        // maxInputImages == 0 → asset constraints default to none.
        SORA_2 => VideoModelCapabilities::new(VideoDurationRange::new(8, 8, Some(vec![8])), [Tall, Landscape], [], 0),
        SORA_2_PRO => VideoModelCapabilities::new(VideoDurationRange::new(8, 8, Some(vec![8])), [Tall, Landscape], [Hd, Fhd], 0),
        // Replicate's kwaivgi/kling-v3-video has start_image + end_image,
        // and optional generate_audio. No resolution param.
        KLING_30_PRO => VideoModelCapabilities::new(VideoDurationRange::new(3, 15, None), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(true, false),
        KLING_25_TURBO_PRO => VideoModelCapabilities::new(VideoDurationRange::new(5, 10, Some(vec![5, 10])), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(false, false),
        // kwaivgi/kling-v2.6 has only start_image (no end_image in schema).
        KLING_26_PRO => VideoModelCapabilities::new(VideoDurationRange::new(5, 10, Some(vec![5, 10])), [Landscape, Tall, Square], [], 1)
            .with_asset_constraints(first_frame_only())
            .with_audio(true, false),
        SEEDANCE_15_PRO => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 12, None),
            [Landscape, Standard, Square, Portrait, Tall, Wide],
            [Sd, Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_audio(true, false),
        SEEDANCE_20 => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 15, None),
            [Auto, Landscape, Standard, Square, Portrait, Tall, Wide],
            [Sd, Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_audio(true, false),
        SEEDANCE_20_FAST => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 15, None),
            [Auto, Landscape, Standard, Square, Portrait, Tall, Wide],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_audio(true, false),
        HAPPY_HORSE_10 => VideoModelCapabilities::new(VideoDurationRange::new(3, 15, None), [Landscape, Tall, Square, Standard, Portrait], [Hd, Fhd], 1)
            .with_asset_constraints(first_frame_only())
            .with_prompt_optional(true)
            .with_audio(true, true),
        // wan-2.7-t2v has aspect_ratio and resolution; wan-2.7-i2v has resolution
        // but no aspect_ratio. We expose t2v's aspect ratios; the body builder
        // omits aspect_ratio when targeting the i2v slug.
        WAN_27 => VideoModelCapabilities::new(VideoDurationRange::new(2, 15, None), [Landscape, Tall, Square, Standard, Portrait], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame_and_audio())
            .with_audio(false, false),
        // pixverse-v6 uses `quality` enum (360p/540p/720p/1080p) and
        // `generate_audio_switch` toggle. Only 720p/1080p map to our enum.
        PIXVERSE_V6 => VideoModelCapabilities::new(VideoDurationRange::new(5, 15, Some(vec![5, 8, 10, 15])), [Landscape, Tall, Square], [Hd, Fhd], 1)
            .with_asset_constraints(AssetConstraints::first_last_frame())
            .with_audio(true, false),
        // xai/grok-imagine-video has only `image` (no last_frame).
        GROK_IMAGINE_VIDEO => VideoModelCapabilities::new(
            VideoDurationRange::new(1, 15, None),
            [Auto, Landscape, Standard, Square, Tall, Portrait, NarrowLandscape, NarrowPortrait],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(first_frame_only())
        .with_audio(false, false),
// Replicate's seedance-2.5 stops at 720p, unlike fal's 1080p tier.
        SEEDANCE_25 => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 30, None),
            [Auto, Wide, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(30).with_videos(10).with_audio(10))
        .with_audio(true, false),
        // Replicate's minimax/h3 sells only the 768P and 2K tiers, and names the ratio field `ratio`.
        MINIMAX_H3 => VideoModelCapabilities::new(
            VideoDurationRange::new(4, 15, Some(vec![4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])),
            [Auto, Wide, Landscape, Standard, Square, Portrait, Tall],
            [Hd, Fhd],
            1,
        )
        .with_asset_constraints(AssetConstraints::first_last_frame())
        .with_references(VideoReferences::images(9).with_videos(3).with_audio(3).with_combined_max(12)),
        FLUX_3 => VideoModelCapabilities::new(
            VideoDurationRange::new(5, 20, None),
            [Auto, Wide, Landscape, Standard, Square, Portrait, Tall],
            [Hd, Fhd],
            1,
        )
        .with_asset_constraints(first_frame_only())
        .with_audio(true, false),
        HAPPY_HORSE_11 => VideoModelCapabilities::new(VideoDurationRange::new(3, 15, None), [Landscape, Tall, Square, Standard, Portrait], [Hd, Fhd], 1)
            .with_asset_constraints(first_frame_only())
            .with_references(VideoReferences::images(9))
            .with_prompt_optional(true)
            .with_audio(true, true),
        // Replicate's wan-3 slugs take a first frame only and expose no audio toggle.
        WAN_30 | WAN_30_PRIME => VideoModelCapabilities::new(
            VideoDurationRange::new(2, 30, None),
            [Auto, Landscape, Standard, Square, Portrait, Tall],
            [Sd, Hd, Fhd],
            1,
        )
        .with_asset_constraints(first_frame_only()),
        // Replicate's grok-imagine-video-1.5 stops at 720p, unlike fal's 1080p tier.
        GROK_IMAGINE_VIDEO_15 => VideoModelCapabilities::new(
            VideoDurationRange::new(1, 15, None),
            [Auto, Landscape, Standard, NarrowLandscape, Square, NarrowPortrait, Portrait, Tall],
            [Sd, Hd],
            1,
        )
        .with_asset_constraints(first_frame_only()),
                _ => return None,
    };
    Some(caps)
}

// ----- image endpoints (slugs) ------------------------------------------------------------------

pub fn endpoint(model: &ImageModel) -> Option<&'static str> {
    Some(match model.id {
        GEMINI_3_PRO => "google/nano-banana-pro",
        GEMINI_31_FLASH => "google/nano-banana-2",
        GEMINI_25_FLASH => "google/gemini-2.5-flash-image",
        GPT_5 => "openai/gpt-image-1.5",
        GPT_IMAGE_2 => "openai/gpt-image-2",
        SEEDREAM_45 => "bytedance/seedream-4.5",
        FLUX_2_MAX => "black-forest-labs/flux-2-max",
        FLUX_2_PRO => "black-forest-labs/flux-2-pro",
        FLUX_2_FLEX => "black-forest-labs/flux-2-flex",
        FLUX_2_KLEIN => "black-forest-labs/flux-2-klein-4b",
        FLUX_1_DEV => "black-forest-labs/flux-dev",
        FLUX_1_SCHNELL => "black-forest-labs/flux-schnell",
        RECRAFT_V4_PRO => "recraft-ai/recraft-v4-pro",
        WAN_27_PRO => "wan-video/wan-2.7-image-pro",
        QWEN_IMAGE_3 => "alibaba/qwen-image-3",
        SEEDREAM_5_PRO => "bytedance/seedream-5-pro",
        SEEDREAM_5_LITE => "bytedance/seedream-5-lite",
        GROK_IMAGINE_IMAGE_2 => "xai/grok-imagine-image-2",
        _ => return None,
    })
}

/// Replicate slugs that accept reference images use the same slug as t2i —
/// the OpenAPI's array input field switches the call to edit/img2img mode.
/// Models that have no image input (flux-schnell, recraft-v4-pro) return `None`.
pub fn edit_endpoint(model: &ImageModel) -> Option<&'static str> {
    Some(match model.id {
        GEMINI_3_PRO => "google/nano-banana-pro",
        GEMINI_31_FLASH => "google/nano-banana-2",
        GEMINI_25_FLASH => "google/gemini-2.5-flash-image",
        GPT_5 => "openai/gpt-image-1.5",
        GPT_IMAGE_2 => "openai/gpt-image-2",
        SEEDREAM_45 => "bytedance/seedream-4.5",
        FLUX_2_MAX => "black-forest-labs/flux-2-max",
        FLUX_2_PRO => "black-forest-labs/flux-2-pro",
        FLUX_2_FLEX => "black-forest-labs/flux-2-flex",
        FLUX_2_KLEIN => "black-forest-labs/flux-2-klein-4b",
        FLUX_1_DEV => "black-forest-labs/flux-dev",
        FLUX_1_SCHNELL => return None,
        RECRAFT_V4_PRO => return None,
        WAN_27_PRO => "wan-video/wan-2.7-image-pro",
        QWEN_IMAGE_3 => "alibaba/qwen-image-3",
        SEEDREAM_5_PRO => "bytedance/seedream-5-pro",
        SEEDREAM_5_LITE => "bytedance/seedream-5-lite",
        GROK_IMAGINE_IMAGE_2 => "xai/grok-imagine-image-2",
        _ => return None,
    })
}

// ----- video endpoints (slugs) ------------------------------------------------------------------

pub fn video_endpoint(model: &VideoModel) -> Option<&'static str> {
    Some(match model.id {
        VEO_31 => "google/veo-3.1",
        VEO_31_FAST => "google/veo-3.1-fast",
        VEO_31_LITE => "google/veo-3.1-lite",
        SORA_2 => "openai/sora-2",
        SORA_2_PRO => "openai/sora-2-pro",
        KLING_30_PRO => "kwaivgi/kling-v3-video",
        KLING_25_TURBO_PRO => "kwaivgi/kling-v2.5-turbo-pro",
        KLING_26_PRO => "kwaivgi/kling-v2.6",
        SEEDANCE_15_PRO => "bytedance/seedance-1.5-pro",
        SEEDANCE_20 => "bytedance/seedance-2.0",
        SEEDANCE_20_FAST => "bytedance/seedance-2.0-fast",
        HAPPY_HORSE_10 => "alibaba/happyhorse-1.0",
        WAN_27 => "wan-video/wan-2.7-t2v",
        PIXVERSE_V6 => "pixverse/pixverse-v6",
        GROK_IMAGINE_VIDEO => "xai/grok-imagine-video",
        GROK_IMAGINE_VIDEO_15 => "xai/grok-imagine-video-1.5",
        SEEDANCE_25 => "bytedance/seedance-2.5",
        MINIMAX_H3 => "minimax/h3",
        FLUX_3 => "black-forest-labs/flux-3",
        HAPPY_HORSE_11 => "alibaba/happyhorse-1.1",
        WAN_30 => "alibaba/wan-3",
        WAN_30_PRIME => "alibaba/wan-3-prime",
        _ => return None,
    })
}

pub fn video_i2v_endpoint(model: &VideoModel) -> Option<&'static str> {
    Some(match model.id {
        VEO_31 => "google/veo-3.1",
        VEO_31_FAST => "google/veo-3.1-fast",
        VEO_31_LITE => "google/veo-3.1-lite",
        SORA_2 => return None,
        SORA_2_PRO => return None,
        KLING_30_PRO => "kwaivgi/kling-v3-video",
        KLING_25_TURBO_PRO => "kwaivgi/kling-v2.5-turbo-pro",
        KLING_26_PRO => "kwaivgi/kling-v2.6",
        SEEDANCE_15_PRO => "bytedance/seedance-1.5-pro",
        SEEDANCE_20 => "bytedance/seedance-2.0",
        SEEDANCE_20_FAST => "bytedance/seedance-2.0-fast",
        HAPPY_HORSE_10 => "alibaba/happyhorse-1.0",
        WAN_27 => "wan-video/wan-2.7-i2v",
        PIXVERSE_V6 => "pixverse/pixverse-v6",
        GROK_IMAGINE_VIDEO => "xai/grok-imagine-video",
        GROK_IMAGINE_VIDEO_15 => "xai/grok-imagine-video-1.5",
        SEEDANCE_25 => "bytedance/seedance-2.5",
        MINIMAX_H3 => "minimax/h3",
        FLUX_3 => "black-forest-labs/flux-3",
        HAPPY_HORSE_11 => "alibaba/happyhorse-1.1",
        WAN_30 => "alibaba/wan-3",
        WAN_30_PRIME => "alibaba/wan-3-prime",
        _ => return None,
    })
}

/// Replicate doesn't currently expose dedicated first-last-frame endpoints
/// for the models we mirror — the i2v slug accepts both frames where
/// supported. Returns `None` for everything; the resolver falls through to
/// the i2v slug.
pub fn video_first_last_frame_endpoint(_model: &VideoModel) -> Option<&'static str> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoEndpointVariant {
    T2v,
    I2v,
    FirstLast,
    /// Reference-to-video: the same slug, with the reference arrays instead of a frame.
    Reference,
}

/// The request keys and prompt dialect for a slug's reference arrays. Read off each model's own
/// input schema on replicate.com on 2026-08-29; they disagree with fal's for the same model, which
/// is why the tables are per provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceParams {
    pub images: Option<&'static str>,
    pub videos: Option<&'static str>,
    pub audio: Option<&'static str>,
    pub style: ReferenceTagStyle,
}

impl ReferenceParams {
    const fn new(images: &'static str, style: ReferenceTagStyle) -> Self {
        Self { images: Some(images), videos: None, audio: None, style }
    }

    const fn with_videos(mut self, videos: &'static str) -> Self {
        self.videos = Some(videos);
        self
    }

    const fn with_audio(mut self, audio: &'static str) -> Self {
        self.audio = Some(audio);
        self
    }

    pub fn param_for(&self, role: AssetRole) -> Option<&'static str> {
        match role {
            AssetRole::ReferenceImage => self.images,
            AssetRole::ReferenceVideo => self.videos,
            AssetRole::Audio => self.audio,
            _ => None,
        }
    }
}

pub fn video_reference_params(model: &VideoModel) -> Option<ReferenceParams> {
    use ReferenceTagStyle::*;
    Some(match model.id {
        SEEDANCE_25 => ReferenceParams::new("reference_images", Bracketed).with_videos("reference_videos").with_audio("reference_audios"),
        MINIMAX_H3 => ReferenceParams::new("reference_image_urls", Prose).with_videos("reference_video_urls").with_audio("reference_audio_urls"),
        VEO_31 => ReferenceParams::new("reference_images", Prose),
        // happyhorse-1.1 has one `images` array: one entry is a start frame, two or more are
        // references. The variant decides what it means, not the key.
        HAPPY_HORSE_11 => ReferenceParams::new("images", BracketedSpaced),
        _ => return None,
    })
}

pub fn resolve_video_endpoint(model: &VideoModel, has_first_frame: bool, has_last_frame: bool) -> ReplicateResult<(&'static str, VideoEndpointVariant)> {
    if has_first_frame && has_last_frame {
        if let Some(first_last) = video_first_last_frame_endpoint(model) {
            return Ok((first_last, VideoEndpointVariant::FirstLast));
        }
    }
    if has_first_frame {
        return video_i2v_endpoint(model)
            .map(|slug| (slug, VideoEndpointVariant::I2v))
            .ok_or_else(|| ReplicateError::UnsupportedModel(model.id.to_string()));
    }
    video_endpoint(model)
        .map(|slug| (slug, VideoEndpointVariant::T2v))
        .ok_or_else(|| ReplicateError::UnsupportedModel(model.id.to_string()))
}

// ----- image API mappings -----------------------------------------------------------------------

/// Maps our `AspectRatio` to the slug's `aspect_ratio` enum string.
/// Returns `None` when the model has no aspect ratio param at all
/// (e.g. wan-2.7-image-pro uses combined `size`).
pub fn api_image_size(model: &ImageModel, aspect_ratio: AspectRatio) -> Option<(&'static str, &'static str)> {
    match model.id {
        // No aspect_ratio field on this slug.
        WAN_27_PRO => None,
        // GPT image models accept only 1:1, 3:2, 2:3 — we expose only square.
        GPT_5 | GPT_IMAGE_2 => (aspect_ratio == AspectRatio::Square).then_some(("aspect_ratio", "1:1")),
        // Most slugs accept our AspectRatio raw value verbatim.
        _ => Some(("aspect_ratio", aspect_ratio.raw())),
    }
}

/// Maps our `ImageResolution` to the slug's resolution-like field. The
/// field key varies per slug (`resolution`, `megapixels`, `output_megapixels`,
/// `size`, `quality`). Returns `None` when the model has no resolution param
/// or the requested resolution isn't in the slug's enum.
pub fn api_image_resolution(model: &ImageModel, resolution: ImageResolution) -> Option<(&'static str, &'static str)> {
    use ImageResolution::*;
    match model.id {
        // resolution: 1K | 2K | 4K
        GEMINI_3_PRO | GEMINI_31_FLASH => match resolution {
            Sd => None,
            Hd => Some(("resolution", "1K")),
            Fhd => Some(("resolution", "2K")),
            Uhd => Some(("resolution", "4K")),
        },
        // No resolution field on this slug.
        GEMINI_25_FLASH => None,
        // quality: low | medium | high | auto. Our hd→low, fhd→medium, uhd→high.
        GPT_5 | GPT_IMAGE_2 => match resolution {
            Sd => None,
            Hd => Some(("quality", "low")),
            Fhd => Some(("quality", "medium")),
            Uhd => Some(("quality", "high")),
        },
        // size: 2K | 4K
        SEEDREAM_45 => match resolution {
            Sd | Hd => None,
            Fhd => Some(("size", "2K")),
            Uhd => Some(("size", "4K")),
        },
        // resolution: "0.5 MP" | "1 MP" | "2 MP" | "4 MP"
        FLUX_2_MAX | FLUX_2_PRO | FLUX_2_FLEX => match resolution {
            Sd => Some(("resolution", "0.5 MP")),
            Hd => Some(("resolution", "1 MP")),
            Fhd => Some(("resolution", "2 MP")),
            Uhd => Some(("resolution", "4 MP")),
        },
        // output_megapixels: "0.25" | "0.5" | "1" | "2" | "4"
        FLUX_2_KLEIN => match resolution {
            Sd => Some(("output_megapixels", "0.5")),
            Hd => Some(("output_megapixels", "1")),
            Fhd => Some(("output_megapixels", "2")),
            Uhd => Some(("output_megapixels", "4")),
        },
        // megapixels: "1" | "0.25"
        FLUX_1_DEV | FLUX_1_SCHNELL => match resolution {
            Sd => Some(("megapixels", "0.25")),
            Hd => Some(("megapixels", "1")),
            Fhd | Uhd => None,
        },
        // size enum is explicit pixel dims, not MP/quality. Skip.
        RECRAFT_V4_PRO => None,
        // size accepts "1K"/"2K"/"4K".
        WAN_27_PRO => match resolution {
            Sd => None,
            Hd => Some(("size", "1K")),
            Fhd => Some(("size", "2K")),
            Uhd => Some(("size", "4K")),
        },
        // Seedream 5's `size` enum is "1K" | "1.5K" | "2K" | "auto".
        SEEDREAM_5_PRO | SEEDREAM_5_LITE => match resolution {
            Hd => Some(("size", "1K")),
            Fhd => Some(("size", "2K")),
            Sd | Uhd => None,
        },
        // Grok Imagine Image 2 spells its two tiers lowercase.
        GROK_IMAGINE_IMAGE_2 => match resolution {
            Hd => Some(("resolution", "1k")),
            Fhd => Some(("resolution", "2k")),
            Sd | Uhd => None,
        },
        // qwen-image-3 has no resolution knob.
        QWEN_IMAGE_3 => None,
        _ => None,
    }
}

/// Replicate slugs use varying field names for reference images and
/// varying types (single string vs array of strings). Returns the key
/// name and an `is_array` flag so the body builder can encode correctly.
pub fn api_edit_image_param(model: &ImageModel) -> Option<(&'static str, bool)> {
    match model.id {
        GEMINI_3_PRO => Some(("image_input", true)),
        GEMINI_31_FLASH => Some(("image_input", true)),
        GEMINI_25_FLASH => Some(("image_input", true)),
        GPT_5 => Some(("input_images", true)),
        GPT_IMAGE_2 => Some(("input_images", true)),
        SEEDREAM_45 => Some(("image_input", true)),
        FLUX_2_MAX => Some(("input_images", true)),
        FLUX_2_PRO => Some(("input_images", true)),
        FLUX_2_FLEX => Some(("input_images", true)),
        FLUX_2_KLEIN => Some(("images", true)),
        FLUX_1_DEV => Some(("image", false)),
        FLUX_1_SCHNELL => None,
        RECRAFT_V4_PRO => None,
        WAN_27_PRO => Some(("images", true)),
        SEEDREAM_5_PRO | SEEDREAM_5_LITE => Some(("image_input", true)),
        // Both of these take a single reference image, not an array.
        QWEN_IMAGE_3 => Some(("image", false)),
        GROK_IMAGINE_IMAGE_2 => Some(("image", false)),
        _ => None,
    }
}

/// Replicate slugs that have an `output_format` enum including "png".
/// Returns `Some(true)` when we can request PNG natively (skipping the transcode).
pub fn api_supports_output_format(model: &ImageModel) -> Option<bool> {
    match model.id {
        GEMINI_3_PRO => Some(true),
        GEMINI_31_FLASH => Some(true),
        GEMINI_25_FLASH => Some(true),
        GPT_5 => Some(true),
        GPT_IMAGE_2 => Some(true),
        SEEDREAM_45 => Some(false),
        FLUX_2_MAX => Some(true),
        FLUX_2_PRO => Some(true),
        FLUX_2_FLEX => Some(true),
        FLUX_2_KLEIN => Some(true),
        FLUX_1_DEV => Some(true),
        FLUX_1_SCHNELL => Some(true),
        RECRAFT_V4_PRO => Some(false),
        WAN_27_PRO => Some(false),
        SEEDREAM_5_PRO | SEEDREAM_5_LITE => Some(true),
        QWEN_IMAGE_3 => Some(false),
        GROK_IMAGINE_IMAGE_2 => Some(false),
        _ => None,
    }
}

/// None of the Replicate slugs in our catalog expose a mask input field
/// (verified against each slug's openapi_schema 2026-05-05). Mask support
/// on Replicate is fal-only for the GPT image models. Returns `None` for
/// every model so mask assets are rejected at the constraint layer.
pub fn api_mask_param(_model: &ImageModel) -> Option<&'static str> {
    None
}

/// Per-model overrides that lower content moderation to its most permissive
/// setting. The exact field varies — `disable_safety_checker` on FLUX 1/Klein
/// and Seedream, `safety_tolerance` (1-5, 5 = most permissive) on FLUX 2 Pro/
/// Max/Flex, `moderation: low` on the OpenAI gpt-image slugs, and
/// `safety_filter_level: block_only_high` on nano-banana-pro. Models with
/// no exposed safety knob (nano-banana-2, gemini-2.5-flash-image,
/// recraft-v4-pro, wan-2.7-image-pro) return `None` — moderation is enforced
/// server-side and there's no caller-side override.
pub fn api_safety_override(model: &ImageModel) -> Option<Vec<(&'static str, Value)>> {
    match model.id {
        FLUX_1_SCHNELL | FLUX_1_DEV | FLUX_2_KLEIN | SEEDREAM_45 => Some(vec![("disable_safety_checker", json!(true))]),
        FLUX_2_MAX | FLUX_2_PRO | FLUX_2_FLEX => Some(vec![("safety_tolerance", json!(5))]),
        GPT_5 | GPT_IMAGE_2 => Some(vec![("moderation", json!("low"))]),
        // nano-banana-pro: default is already block_only_high, but set
        // explicitly so we don't silently diverge if Replicate changes it.
        GEMINI_3_PRO => Some(vec![("safety_filter_level", json!("block_only_high"))]),
        GEMINI_31_FLASH | GEMINI_25_FLASH | RECRAFT_V4_PRO | WAN_27_PRO => None,
        _ => None,
    }
}

// ----- video API mappings -----------------------------------------------------------------------

/// Maps our `VideoAspectRatio` to the slug's aspect-ratio-like field, as `(key, value)`.
/// The key is `"aspect_ratio"` for every slug but `minimax/h3`, which calls it `ratio`.
/// Variant-aware: wan-2.7-i2v has no aspect_ratio field, so returns `None`
/// for that variant even though wan-2.7-t2v supports several ratios.
pub fn api_video_aspect_ratio(model: &VideoModel, aspect_ratio: VideoAspectRatio, variant: VideoEndpointVariant) -> Option<(&'static str, &'static str)> {
    use VideoAspectRatio::*;
    match model.id {
        // Sora uses semantic strings, not aspect ratios.
        SORA_2 | SORA_2_PRO => match aspect_ratio {
            Tall => Some(("aspect_ratio", "portrait")),
            Landscape => Some(("aspect_ratio", "landscape")),
            Auto | Square | NarrowLandscape | NarrowPortrait | Standard | Portrait | Wide => None,
        },
        // wan-2.7-i2v has no aspect_ratio field.
        WAN_27 if variant == VideoEndpointVariant::I2v => None,
        // Replicate derives image-to-video aspect ratio from the input image.
        HAPPY_HORSE_10 | HAPPY_HORSE_11 if variant == VideoEndpointVariant::I2v => None,
        // Seedance 2.0 / 2.5 and Wan 3.0 spell `.auto` as `adaptive`.
        SEEDANCE_20 | SEEDANCE_20_FAST | SEEDANCE_25 | WAN_30 | WAN_30_PRIME => match aspect_ratio {
            Auto => Some(("aspect_ratio", "adaptive")),
            Square | NarrowLandscape | NarrowPortrait | Standard | Portrait | Landscape | Tall | Wide => Some(("aspect_ratio", aspect_ratio.raw())),
        },
        // minimax/h3 names the field `ratio`, and spells `.auto` as `adaptive`.
        MINIMAX_H3 => match aspect_ratio {
            Auto => Some(("ratio", "adaptive")),
            NarrowLandscape | NarrowPortrait => None,
            Square | Standard | Portrait | Landscape | Tall | Wide => Some(("ratio", aspect_ratio.raw())),
        },
        // Most slugs accept VideoAspectRatio raw value verbatim.
        _ => Some(("aspect_ratio", aspect_ratio.raw())),
    }
}

/// Maps our `VideoResolution` to the slug's resolution-like field.
/// Returns `None` when the model has no resolution param or the requested
/// value isn't in the slug's enum.
pub fn api_video_resolution(model: &VideoModel, resolution: VideoResolution, _variant: VideoEndpointVariant) -> Option<(&'static str, &'static str)> {
    use VideoResolution::*;
    match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => match resolution {
            Sd | Uhd => None,
            Hd => Some(("resolution", "720p")),
            Fhd => Some(("resolution", "1080p")),
        },
        SORA_2 => None,
        SORA_2_PRO => match resolution {
            Sd | Uhd => None,
            Hd => Some(("resolution", "standard")),
            Fhd => Some(("resolution", "high")),
        },
        KLING_30_PRO | KLING_25_TURBO_PRO | KLING_26_PRO => None,
        SEEDANCE_15_PRO | SEEDANCE_20 => match resolution {
            Uhd => None,
            Sd => Some(("resolution", "480p")),
            Hd => Some(("resolution", "720p")),
            Fhd => Some(("resolution", "1080p")),
        },
        SEEDANCE_20_FAST => match resolution {
            Fhd | Uhd => None,
            Sd => Some(("resolution", "480p")),
            Hd => Some(("resolution", "720p")),
        },
        WAN_27 => match resolution {
            Sd | Uhd => None,
            Hd => Some(("resolution", "720p")),
            Fhd => Some(("resolution", "1080p")),
        },
        HAPPY_HORSE_10 => match resolution {
            Sd | Uhd => None,
            Hd => Some(("resolution", "720p")),
            Fhd => Some(("resolution", "1080p")),
        },
        // pixverse uses `quality`, not `resolution`.
        PIXVERSE_V6 => match resolution {
            Sd | Uhd => None,
            Hd => Some(("quality", "720p")),
            Fhd => Some(("quality", "1080p")),
        },
        GROK_IMAGINE_VIDEO | GROK_IMAGINE_VIDEO_15 => match resolution {
            Fhd | Uhd => None,
            Sd => Some(("resolution", "480p")),
            Hd => Some(("resolution", "720p")),
        },
        SEEDANCE_25 => match resolution {
            Fhd | Uhd => None,
            Sd => Some(("resolution", "480p")),
            Hd => Some(("resolution", "720p")),
        },
        // Replicate's minimax/h3 offers only the two middle tiers, uppercase-P.
        MINIMAX_H3 => match resolution {
            Sd | Uhd => None,
            Hd => Some(("resolution", "768P")),
            Fhd => Some(("resolution", "2K")),
        },
        FLUX_3 => match resolution {
            Sd | Uhd => None,
            Hd => Some(("resolution", "720p")),
            Fhd => Some(("resolution", "1080p")),
        },
        HAPPY_HORSE_11 | WAN_30 | WAN_30_PRIME => match resolution {
            Uhd => None,
            Sd => Some(("resolution", "480p")),
            Hd => Some(("resolution", "720p")),
            Fhd => Some(("resolution", "1080p")),
        },
        _ => None,
    }
}

/// Per-model duration encoding. Every supported slug takes an integer; Sora 2
/// has no duration field at all.
pub fn api_duration(model: &VideoModel, duration: u32) -> Option<Value> {
    match model.id {
        // duration is an integer enum [4, 6, 8].
        VEO_31 | VEO_31_FAST | VEO_31_LITE => Some(json!(duration)),
        // No duration field on Replicate's Sora slugs.
        SORA_2 | SORA_2_PRO => None,
        KLING_30_PRO | KLING_25_TURBO_PRO | KLING_26_PRO => Some(json!(duration)),
        SEEDANCE_15_PRO | SEEDANCE_20 | SEEDANCE_20_FAST => Some(json!(duration)),
        HAPPY_HORSE_10 => Some(json!(duration)),
        WAN_27 => Some(json!(duration)),
        PIXVERSE_V6 => Some(json!(duration)),
        GROK_IMAGINE_VIDEO | GROK_IMAGINE_VIDEO_15 => Some(json!(duration)),
        SEEDANCE_25 | MINIMAX_H3 => Some(json!(duration)),
        // Replicate's flux-3 takes the duration as a string, matching fal's mixed enum.
        FLUX_3 => Some(json!(duration.to_string())),
        HAPPY_HORSE_11 | WAN_30 | WAN_30_PRIME => Some(json!(duration)),
        _ => None,
    }
}

pub fn api_start_frame_param(model: &VideoModel, variant: VideoEndpointVariant) -> Option<&'static str> {
    match variant {
        VideoEndpointVariant::T2v | VideoEndpointVariant::FirstLast | VideoEndpointVariant::Reference => None,
        VideoEndpointVariant::I2v => api_i2v_start_frame_param(model),
    }
}

pub fn api_end_frame_param(model: &VideoModel, variant: VideoEndpointVariant) -> Option<&'static str> {
    match variant {
        VideoEndpointVariant::T2v | VideoEndpointVariant::FirstLast | VideoEndpointVariant::Reference => None,
        VideoEndpointVariant::I2v => api_i2v_end_frame_param(model),
    }
}

fn api_i2v_start_frame_param(model: &VideoModel) -> Option<&'static str> {
    match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => Some("image"),
        SORA_2 | SORA_2_PRO => None,
        KLING_30_PRO | KLING_26_PRO => Some("start_image"),
        KLING_25_TURBO_PRO => Some("image"),
        SEEDANCE_15_PRO | SEEDANCE_20 | SEEDANCE_20_FAST => Some("image"),
        HAPPY_HORSE_10 => Some("image"),
        WAN_27 => Some("first_frame"),
        PIXVERSE_V6 => Some("image"),
        GROK_IMAGINE_VIDEO | GROK_IMAGINE_VIDEO_15 => Some("image"),
        SEEDANCE_25 => Some("image"),
        MINIMAX_H3 => Some("first_frame_image"),
        // flux-3 takes its opening frame in the `images` array; the body builder sends one entry.
        FLUX_3 => Some("images"),
        HAPPY_HORSE_11 => Some("images"),
        WAN_30 | WAN_30_PRIME => Some("image"),
        _ => None,
    }
}

fn api_i2v_end_frame_param(model: &VideoModel) -> Option<&'static str> {
    match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => Some("last_frame"),
        SORA_2 | SORA_2_PRO => None,
        KLING_30_PRO | KLING_25_TURBO_PRO => Some("end_image"),
        // kwaivgi/kling-v2.6 has no end_image in schema.
        KLING_26_PRO => None,
        SEEDANCE_15_PRO | SEEDANCE_20 | SEEDANCE_20_FAST => Some("last_frame_image"),
        HAPPY_HORSE_10 => None,
        WAN_27 => Some("last_frame"),
        PIXVERSE_V6 => Some("last_frame_image"),
        // No last_frame on Grok Imagine.
        GROK_IMAGINE_VIDEO | GROK_IMAGINE_VIDEO_15 => None,
        SEEDANCE_25 => Some("last_frame_image"),
        MINIMAX_H3 => Some("last_frame_image"),
        // flux-3, happyhorse-1.1 and the wan-3 slugs take an opening frame only.
        FLUX_3 | HAPPY_HORSE_11 | WAN_30 | WAN_30_PRIME => None,
        _ => None,
    }
}

/// Audio toggle parameter key. `None` for slugs with no audio toggle (or
/// where audio is fixed regardless of input).
pub fn api_audio_param(model: &VideoModel) -> Option<&'static str> {
    match model.id {
        VEO_31 | VEO_31_FAST => Some("generate_audio"),
        // Veo 3.1 Lite has no generate_audio field.
        VEO_31_LITE => None,
        // Sora always generates audio; no toggle.
        SORA_2 | SORA_2_PRO => None,
        KLING_30_PRO => Some("generate_audio"),
        KLING_25_TURBO_PRO => None,
        KLING_26_PRO => Some("generate_audio"),
        SEEDANCE_15_PRO | SEEDANCE_20 | SEEDANCE_20_FAST => Some("generate_audio"),
        HAPPY_HORSE_10 => None,
        // wan-2.7's `audio` is an audio-file URL string, not a toggle.
        WAN_27 => None,
        PIXVERSE_V6 => Some("generate_audio_switch"),
        GROK_IMAGINE_VIDEO | GROK_IMAGINE_VIDEO_15 => None,
        SEEDANCE_25 => Some("generate_audio"),
        MINIMAX_H3 => None,
        FLUX_3 => Some("generate_audio"),
        // Happy Horse always renders audio, and the wan-3 slugs expose no toggle.
        HAPPY_HORSE_11 | WAN_30 | WAN_30_PRIME => None,
        _ => None,
    }
}

/// API parameter key for user-supplied audio conditioning.
pub fn api_audio_input_param(model: &VideoModel) -> Option<&'static str> {
    match model.id {
        VEO_31 | VEO_31_FAST | VEO_31_LITE => None,
        SORA_2 | SORA_2_PRO => None,
        KLING_30_PRO | KLING_25_TURBO_PRO | KLING_26_PRO => None,
        SEEDANCE_15_PRO | SEEDANCE_20 | SEEDANCE_20_FAST => None,
        WAN_27 => Some("audio"),
        PIXVERSE_V6 => None,
        GROK_IMAGINE_VIDEO => None,
        _ => None,
    }
}

// ----- Tools --------------------------------------------------------------------------------------

/// Checked 2026-09-02 against `replicate.com/philz1337x/clarity-upscaler` and
/// `replicate.com/851-labs/background-remover`. Neither exposes an enhancement-model choice; only
/// Clarity takes a scale factor.
pub fn tool_capabilities(model: &ToolModel) -> Option<ToolModelCapabilities> {
    Some(match model.id {
        "clarity-upscaler" => ToolModelCapabilities::new(10).with_factors([2, 4]),
        "rembg" => ToolModelCapabilities::new(10),
        _ => return None,
    })
}
