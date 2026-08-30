//! OpenRouter capabilities: supported models, capability tables and API slugs.
//!
//! Models are matched by catalog id, so the tables work for any `ImageModel` carrying one of these ids.

use crate::models::{AspectRatio, ImageModel, ImageResolution, ModelCapabilities};

use AspectRatio::{Landscape, Portrait, Square, Standard, Tall, ThreeToFour, Wide};
use ImageResolution::{Fhd, Hd, Sd, Uhd};

// Catalog ids.
pub const GEMINI_31_FLASH: &str = "gemini-3.1-flash";
pub const GEMINI_25_FLASH: &str = "gemini-2.5-flash";
pub const GEMINI_3_PRO: &str = "gemini-3-pro";
pub const GPT_IMAGE_2: &str = "gpt-image-2";
pub const GPT_5: &str = "gpt-5-image";
pub const GPT_5_MINI: &str = "gpt-5-image-mini";
pub const SEEDREAM_45: &str = "seedream-4.5";
pub const RIVERFLOW_2_MAX: &str = "riverflow-2-max";
pub const RIVERFLOW_2_STD: &str = "riverflow-2-std";
pub const RIVERFLOW_2_FAST: &str = "riverflow-2-fast";
pub const FLUX_2_MAX: &str = "flux-2-max";
pub const FLUX_2_PRO: &str = "flux-2-pro";
pub const FLUX_2_FLEX: &str = "flux-2-flex";
pub const FLUX_2_KLEIN: &str = "flux-2-klein";
pub const SEEDREAM_5_PRO: &str = "seedream-5-pro";
pub const SEEDREAM_5_LITE: &str = "seedream-5-lite";
pub const MUSE_IMAGE: &str = "muse-image";
pub const QWEN_IMAGE_3: &str = "qwen-image-3";
pub const QWEN_IMAGE_3_PRO: &str = "qwen-image-3-pro";
pub const GROK_IMAGINE_IMAGE_2: &str = "grok-imagine-image-2";

/// The image models OpenRouter supports, in the order the picker shows them.
pub const SUPPORTED_IMAGE_MODEL_IDS: [&str; 20] = [
    GEMINI_31_FLASH,
    GEMINI_25_FLASH,
    GEMINI_3_PRO,
    GPT_IMAGE_2,
    GPT_5,
    GPT_5_MINI,
    SEEDREAM_5_PRO,
    SEEDREAM_5_LITE,
    SEEDREAM_45,
    MUSE_IMAGE,
    RIVERFLOW_2_MAX,
    RIVERFLOW_2_STD,
    RIVERFLOW_2_FAST,
    FLUX_2_MAX,
    FLUX_2_PRO,
    FLUX_2_FLEX,
    FLUX_2_KLEIN,
    QWEN_IMAGE_3_PRO,
    QWEN_IMAGE_3,
    GROK_IMAGINE_IMAGE_2,
];

const ALL_RATIOS: [AspectRatio; 7] = [Square, Standard, ThreeToFour, Portrait, Landscape, Tall, Wide];
const FIVE_RATIOS: [AspectRatio; 5] = [Square, ThreeToFour, Standard, Tall, Landscape];

/// `OpenRouterProvider.imageCapabilities(for:)`.
pub fn image_capabilities(model: &ImageModel) -> Option<ModelCapabilities> {
    let caps = match model.id {
        GEMINI_3_PRO => ModelCapabilities::new(ALL_RATIOS, [Hd, Fhd, Uhd], 14),
        GEMINI_31_FLASH => ModelCapabilities::new(ALL_RATIOS, [Sd, Hd, Fhd, Uhd], 14),
        GEMINI_25_FLASH => ModelCapabilities::new(ALL_RATIOS, [Hd, Fhd, Uhd], 14),
        GPT_5 | GPT_5_MINI | GPT_IMAGE_2 => ModelCapabilities::new([Square], [], 1),
        SEEDREAM_45 => ModelCapabilities::new(FIVE_RATIOS, [], 1),
        RIVERFLOW_2_MAX | RIVERFLOW_2_STD | RIVERFLOW_2_FAST => ModelCapabilities::new(ALL_RATIOS, [], 1),
        FLUX_2_MAX | FLUX_2_PRO | FLUX_2_FLEX | FLUX_2_KLEIN => ModelCapabilities::new(FIVE_RATIOS, [], 1),
        SEEDREAM_5_PRO | SEEDREAM_5_LITE => ModelCapabilities::new(FIVE_RATIOS, [Hd, Fhd], 10),
        MUSE_IMAGE => ModelCapabilities::new([Square, Standard, ThreeToFour, Landscape, Tall, Wide], [], 10),
        QWEN_IMAGE_3 | QWEN_IMAGE_3_PRO => ModelCapabilities::new(FIVE_RATIOS, [], 1),
        GROK_IMAGINE_IMAGE_2 => ModelCapabilities::new(FIVE_RATIOS, [Hd, Fhd], 1),
        _ => return None,
    };
    Some(caps)
}

/// `OpenRouterProvider.modelSlug(for:)`: the OpenRouter model identifier sent in the request.
pub fn model_slug(model: &ImageModel) -> Option<&'static str> {
    model_slug_for_id(model.id)
}

pub fn model_slug_for_id(id: &str) -> Option<&'static str> {
    Some(match id {
        GEMINI_3_PRO => "google/gemini-3-pro-image-preview",
        GEMINI_25_FLASH => "google/gemini-2.5-flash-image",
        GEMINI_31_FLASH => "google/gemini-3.1-flash-image-preview",
        GPT_5 => "openai/gpt-5-image",
        GPT_5_MINI => "openai/gpt-5-image-mini",
        GPT_IMAGE_2 => "openai/gpt-5.4-image-2",
        SEEDREAM_45 => "bytedance-seed/seedream-4.5",
        RIVERFLOW_2_MAX => "sourceful/riverflow-v2-max-preview",
        RIVERFLOW_2_STD => "sourceful/riverflow-v2-standard-preview",
        RIVERFLOW_2_FAST => "sourceful/riverflow-v2-fast-preview",
        FLUX_2_MAX => "black-forest-labs/flux.2-max",
        FLUX_2_PRO => "black-forest-labs/flux.2-pro",
        FLUX_2_FLEX => "black-forest-labs/flux.2-flex",
        FLUX_2_KLEIN => "black-forest-labs/flux.2-klein-4b",
        SEEDREAM_5_PRO => "bytedance-seed/seedream-5-0-pro",
        SEEDREAM_5_LITE => "bytedance-seed/seedream-5-0-lite",
        MUSE_IMAGE => "meta/muse-image",
        QWEN_IMAGE_3 => "qwen/qwen-image-3",
        QWEN_IMAGE_3_PRO => "qwen/qwen-image-3-pro",
        GROK_IMAGINE_IMAGE_2 => "x-ai/grok-imagine-image-2.0",
        _ => return None,
    })
}
