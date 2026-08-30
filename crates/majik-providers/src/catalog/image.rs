//! The global catalog of every image model the app knows about.
//!
//! Providers reference entries by id; the same static is shared across providers so switching
//! providers for the same model preserves the exact display name, logo, and description.
//! Ids, names and descriptions are persistence keys / UI copy — don't change them lightly.
//!
//! Each model is defined once as a `const` in [`defs`] and exposed both as a named `pub static`
//! and inside [`ALL`] (a `static` cannot be copied into another static's initializer).

use crate::models::ImageModel;

mod defs {
    use crate::logo;
    use crate::models::ImageModel;

    // Google
    pub const GEMINI_31_FLASH: ImageModel = ImageModel::new("gemini-3.1-flash", "Nano Banana 2", "Google", logo::GOOGLE, "TBD");
    pub const GEMINI_3_PRO: ImageModel = ImageModel::new("gemini-3-pro", "Nano Banana Pro", "Google", logo::GOOGLE, "TBD");
    pub const GEMINI_25_FLASH: ImageModel = ImageModel::new("gemini-2.5-flash", "Nano Banana", "Google", logo::GOOGLE, "TBD");
    // OpenAI
    pub const GPT_5: ImageModel = ImageModel::new("gpt-5-image", "GPT-5 Image", "OpenAI", logo::OPEN_AI, "TBD");
    pub const GPT_5_MINI: ImageModel = ImageModel::new("gpt-5-image-mini", "GPT-5 Image Mini", "OpenAI", logo::OPEN_AI, "TBD");
    pub const GPT_IMAGE_2: ImageModel = ImageModel::new("gpt-image-2", "GPT Image 2", "OpenAI", logo::OPEN_AI, "TBD");
    // ByteDance
    pub const SEEDREAM_5_PRO: ImageModel = ImageModel::new("seedream-5-pro", "Seedream 5.0 Pro", "ByteDance", logo::BYTE_DANCE, "TBD");
    pub const SEEDREAM_5_LITE: ImageModel = ImageModel::new("seedream-5-lite", "Seedream 5.0 Lite", "ByteDance", logo::BYTE_DANCE, "TBD");
    pub const SEEDREAM_45: ImageModel = ImageModel::new("seedream-4.5", "Seedream 4.5", "ByteDance", logo::BYTE_DANCE, "TBD");
    // Meta
    pub const MUSE_IMAGE: ImageModel = ImageModel::new("muse-image", "Muse Image", "Meta", logo::META, "TBD");
    // Sourceful
    pub const RIVERFLOW_2_MAX: ImageModel = ImageModel::new("riverflow-2-max", "Riverflow V2 Max", "Sourceful", logo::SOURCEFUL, "TBD");
    pub const RIVERFLOW_2_STD: ImageModel = ImageModel::new("riverflow-2-std", "Riverflow V2 Standard", "Sourceful", logo::SOURCEFUL, "TBD");
    pub const RIVERFLOW_2_FAST: ImageModel = ImageModel::new("riverflow-2-fast", "Riverflow V2 Fast", "Sourceful", logo::SOURCEFUL, "TBD");
    // Black Forest Labs
    pub const FLUX_2_MAX: ImageModel = ImageModel::new("flux-2-max", "FLUX.2 Max", "Black Forest Labs", logo::FLUX, "TBD");
    pub const FLUX_2_PRO: ImageModel = ImageModel::new("flux-2-pro", "FLUX.2 Pro", "Black Forest Labs", logo::FLUX, "TBD");
    pub const FLUX_2_FLEX: ImageModel = ImageModel::new("flux-2-flex", "FLUX.2 Flex", "Black Forest Labs", logo::FLUX, "TBD");
    pub const FLUX_2_KLEIN: ImageModel = ImageModel::new("flux-2-klein", "FLUX.2 Klein 4B", "Black Forest Labs", logo::FLUX, "TBD");
    pub const FLUX_1_DEV: ImageModel = ImageModel::new("flux-1-dev", "FLUX.1 Dev", "Black Forest Labs", logo::FLUX, "TBD");
    pub const FLUX_1_SCHNELL: ImageModel = ImageModel::new("flux-1-schnell", "FLUX.1 Schnell", "Black Forest Labs", logo::FLUX, "TBD");
    // Recraft
    pub const RECRAFT_V4_PRO: ImageModel = ImageModel::new("recraft-4-pro", "Recraft V4 Pro", "Recraft", logo::RECRAFT, "TBD");
    // Alibaba
    pub const QWEN_IMAGE_3_PRO: ImageModel = ImageModel::new("qwen-image-3-pro", "Qwen Image 3 Pro", "Alibaba", logo::ALIBABA, "TBD");
    pub const QWEN_IMAGE_3: ImageModel = ImageModel::new("qwen-image-3", "Qwen Image 3", "Alibaba", logo::ALIBABA, "TBD");
    pub const WAN_27_PRO: ImageModel = ImageModel::new("wan-2.7-pro", "WAN 2.7 Pro", "Alibaba", logo::ALIBABA, "TBD");
    // xAI
    pub const GROK_IMAGINE_IMAGE_2: ImageModel = ImageModel::new("grok-imagine-image-2", "Grok Imagine Image 2", "xAI", logo::GROK, "TBD");
}

// ----- Google -------------------------------------------------------------------------------------

pub static GEMINI_31_FLASH: ImageModel = defs::GEMINI_31_FLASH;
pub static GEMINI_3_PRO: ImageModel = defs::GEMINI_3_PRO;
pub static GEMINI_25_FLASH: ImageModel = defs::GEMINI_25_FLASH;

// ----- OpenAI -------------------------------------------------------------------------------------

pub static GPT_5: ImageModel = defs::GPT_5;
pub static GPT_5_MINI: ImageModel = defs::GPT_5_MINI;
pub static GPT_IMAGE_2: ImageModel = defs::GPT_IMAGE_2;

// ----- ByteDance ----------------------------------------------------------------------------------

pub static SEEDREAM_5_PRO: ImageModel = defs::SEEDREAM_5_PRO;
pub static SEEDREAM_5_LITE: ImageModel = defs::SEEDREAM_5_LITE;
pub static SEEDREAM_45: ImageModel = defs::SEEDREAM_45;

// ----- Meta ---------------------------------------------------------------------------------------

pub static MUSE_IMAGE: ImageModel = defs::MUSE_IMAGE;

// ----- Sourceful ----------------------------------------------------------------------------------

pub static RIVERFLOW_2_MAX: ImageModel = defs::RIVERFLOW_2_MAX;
pub static RIVERFLOW_2_STD: ImageModel = defs::RIVERFLOW_2_STD;
pub static RIVERFLOW_2_FAST: ImageModel = defs::RIVERFLOW_2_FAST;

// ----- Black Forest Labs --------------------------------------------------------------------------

pub static FLUX_2_MAX: ImageModel = defs::FLUX_2_MAX;
pub static FLUX_2_PRO: ImageModel = defs::FLUX_2_PRO;
pub static FLUX_2_FLEX: ImageModel = defs::FLUX_2_FLEX;
pub static FLUX_2_KLEIN: ImageModel = defs::FLUX_2_KLEIN;
pub static FLUX_1_DEV: ImageModel = defs::FLUX_1_DEV;
pub static FLUX_1_SCHNELL: ImageModel = defs::FLUX_1_SCHNELL;

// ----- Recraft ------------------------------------------------------------------------------------

pub static RECRAFT_V4_PRO: ImageModel = defs::RECRAFT_V4_PRO;

// ----- Alibaba ------------------------------------------------------------------------------------

pub static QWEN_IMAGE_3_PRO: ImageModel = defs::QWEN_IMAGE_3_PRO;
pub static QWEN_IMAGE_3: ImageModel = defs::QWEN_IMAGE_3;
pub static WAN_27_PRO: ImageModel = defs::WAN_27_PRO;

// ----- xAI ----------------------------------------------------------------------------------------

pub static GROK_IMAGINE_IMAGE_2: ImageModel = defs::GROK_IMAGINE_IMAGE_2;

// ----- Catalog ------------------------------------------------------------------------------------

/// Every image model, in the same (UI) order as `ImageModelCatalog.all`.
pub static ALL: &[ImageModel] = &[
    defs::GEMINI_3_PRO,
    defs::GEMINI_31_FLASH,
    defs::GEMINI_25_FLASH,
    defs::GPT_IMAGE_2,
    defs::GPT_5,
    defs::GPT_5_MINI,
    defs::SEEDREAM_5_PRO,
    defs::SEEDREAM_5_LITE,
    defs::SEEDREAM_45,
    defs::MUSE_IMAGE,
    defs::RIVERFLOW_2_MAX,
    defs::RIVERFLOW_2_STD,
    defs::RIVERFLOW_2_FAST,
    defs::FLUX_2_MAX,
    defs::FLUX_2_PRO,
    defs::FLUX_2_FLEX,
    defs::FLUX_2_KLEIN,
    defs::FLUX_1_DEV,
    defs::FLUX_1_SCHNELL,
    defs::RECRAFT_V4_PRO,
    defs::QWEN_IMAGE_3_PRO,
    defs::QWEN_IMAGE_3,
    defs::WAN_27_PRO,
    defs::GROK_IMAGINE_IMAGE_2,
];

/// Looks a model up by its persistence id (`ImageModelCatalog.model(id:)`).
pub fn model(id: &str) -> Option<&'static ImageModel> {
    ALL.iter().find(|m| m.id == id)
}
