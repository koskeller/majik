//! The global catalog of every video model. Same pattern as
//! [`super::image`] — each model is defined once as a `const` in [`defs`] and exposed both as a
//! named `pub static` and inside [`ALL`]. Ids, names and descriptions are persistence keys / UI copy.

use crate::models::VideoModel;

mod defs {
    use crate::logo;
    use crate::models::VideoModel;

    // Google
    pub const GEMINI_OMNI_FLASH_11: VideoModel =
        VideoModel::new("gemini-omni-flash-1.1", "Gemini Omni Flash 1.1", "Google", logo::GOOGLE, "TBD");
    pub const VEO_31: VideoModel = VideoModel::new("veo-3.1", "Veo 3.1", "Google", logo::GOOGLE, "TBD");
    pub const VEO_31_FAST: VideoModel = VideoModel::new("veo-3.1-fast", "Veo 3.1 Fast", "Google", logo::GOOGLE, "TBD");
    pub const VEO_31_LITE: VideoModel = VideoModel::new("veo-3.1-lite", "Veo 3.1 Lite", "Google", logo::GOOGLE, "TBD");
    // OpenAI
    pub const SORA_2: VideoModel = VideoModel::new("sora-2", "Sora 2", "OpenAI", logo::OPEN_AI, "TBD");
    pub const SORA_2_PRO: VideoModel = VideoModel::new("sora-2-pro", "Sora 2 Pro", "OpenAI", logo::OPEN_AI, "TBD");
    // Kuaishou
    pub const KLING_30_PRO: VideoModel = VideoModel::new("kling-3-pro", "Kling 3.0 Pro", "Kuaishou", logo::KLING, "TBD");
    pub const KLING_30_STANDARD: VideoModel = VideoModel::new("kling-3-standard", "Kling 3.0 Standard", "Kuaishou", logo::KLING, "TBD");
    pub const KLING_25_TURBO_PRO: VideoModel = VideoModel::new("kling-2.5-turbo-pro", "Kling 2.5 Turbo Pro", "Kuaishou", logo::KLING, "TBD");
    pub const KLING_26_PRO: VideoModel = VideoModel::new("kling-2.6-pro", "Kling 2.6 Pro", "Kuaishou", logo::KLING, "TBD");
    // ByteDance
    pub const SEEDANCE_25: VideoModel = VideoModel::new("seedance-2.5", "Seedance 2.5", "ByteDance", logo::BYTE_DANCE, "TBD");
    pub const SEEDANCE_15_PRO: VideoModel = VideoModel::new("seedance-1.5-pro", "Seedance 1.5 Pro", "ByteDance", logo::BYTE_DANCE, "TBD");
    pub const SEEDANCE_20: VideoModel = VideoModel::new("seedance-2", "Seedance 2.0", "ByteDance", logo::BYTE_DANCE, "TBD");
    pub const SEEDANCE_20_FAST: VideoModel = VideoModel::new("seedance-2-fast", "Seedance 2.0 Fast", "ByteDance", logo::BYTE_DANCE, "TBD");
    // MiniMax
    pub const MINIMAX_H3: VideoModel = VideoModel::new("minimax-h3", "MiniMax H3", "MiniMax", logo::MINIMAX, "TBD");
    pub const MINIMAX_H3_MAX: VideoModel = VideoModel::new("minimax-h3-max", "MiniMax H3 Max", "MiniMax", logo::MINIMAX, "TBD");
    // Black Forest Labs
    pub const FLUX_3: VideoModel = VideoModel::new("flux-3", "FLUX 3", "Black Forest Labs", logo::FLUX, "TBD");
    // Alibaba
    pub const HAPPY_HORSE_11: VideoModel = VideoModel::new("happyhorse-1.1", "HappyHorse 1.1", "Alibaba", logo::ALIBABA, "TBD");
    pub const WAN_30: VideoModel = VideoModel::new("wan-3.0", "WAN 3.0", "Alibaba", logo::ALIBABA, "TBD");
    pub const WAN_30_PRIME: VideoModel = VideoModel::new("wan-3.0-prime", "WAN 3.0 Prime", "Alibaba", logo::ALIBABA, "TBD");
    pub const HAPPY_HORSE_10: VideoModel = VideoModel::new("happyhorse-1.0", "HappyHorse 1.0", "Alibaba", logo::ALIBABA, "TBD");
    pub const WAN_27: VideoModel = VideoModel::new("wan-2.7", "WAN 2.7", "Alibaba", logo::ALIBABA, "TBD");
    // PixVerse
    pub const PIXVERSE_V6: VideoModel = VideoModel::new("pixverse-6", "PixVerse V6", "PixVerse", logo::PIXVERSE, "TBD");
    // xAI
    pub const GROK_IMAGINE_VIDEO_15: VideoModel =
        VideoModel::new("grok-imagine-video-1.5", "Grok Imagine Video 1.5", "xAI", logo::GROK, "TBD");
    pub const GROK_IMAGINE_VIDEO: VideoModel = VideoModel::new("grok-imagine-video", "Grok Imagine Video", "xAI", logo::GROK, "TBD");
}

// ----- Google -------------------------------------------------------------------------------------

pub static VEO_31: VideoModel = defs::VEO_31;
pub static VEO_31_FAST: VideoModel = defs::VEO_31_FAST;
pub static VEO_31_LITE: VideoModel = defs::VEO_31_LITE;
pub static GEMINI_OMNI_FLASH_11: VideoModel = defs::GEMINI_OMNI_FLASH_11;

// ----- OpenAI -------------------------------------------------------------------------------------

pub static SORA_2: VideoModel = defs::SORA_2;
pub static SORA_2_PRO: VideoModel = defs::SORA_2_PRO;

// ----- Kuaishou -----------------------------------------------------------------------------------

pub static KLING_30_PRO: VideoModel = defs::KLING_30_PRO;
pub static KLING_30_STANDARD: VideoModel = defs::KLING_30_STANDARD;
pub static KLING_25_TURBO_PRO: VideoModel = defs::KLING_25_TURBO_PRO;
pub static KLING_26_PRO: VideoModel = defs::KLING_26_PRO;

// ----- ByteDance ----------------------------------------------------------------------------------

pub static SEEDANCE_25: VideoModel = defs::SEEDANCE_25;
pub static SEEDANCE_15_PRO: VideoModel = defs::SEEDANCE_15_PRO;
pub static SEEDANCE_20: VideoModel = defs::SEEDANCE_20;
pub static SEEDANCE_20_FAST: VideoModel = defs::SEEDANCE_20_FAST;

// ----- MiniMax ------------------------------------------------------------------------------------

pub static MINIMAX_H3: VideoModel = defs::MINIMAX_H3;
pub static MINIMAX_H3_MAX: VideoModel = defs::MINIMAX_H3_MAX;

// ----- Black Forest Labs --------------------------------------------------------------------------

pub static FLUX_3: VideoModel = defs::FLUX_3;

// ----- Alibaba ------------------------------------------------------------------------------------

pub static HAPPY_HORSE_11: VideoModel = defs::HAPPY_HORSE_11;
pub static HAPPY_HORSE_10: VideoModel = defs::HAPPY_HORSE_10;
pub static WAN_30_PRIME: VideoModel = defs::WAN_30_PRIME;
pub static WAN_30: VideoModel = defs::WAN_30;
pub static WAN_27: VideoModel = defs::WAN_27;

// ----- PixVerse -----------------------------------------------------------------------------------

pub static PIXVERSE_V6: VideoModel = defs::PIXVERSE_V6;

// ----- xAI ----------------------------------------------------------------------------------------

pub static GROK_IMAGINE_VIDEO_15: VideoModel = defs::GROK_IMAGINE_VIDEO_15;
pub static GROK_IMAGINE_VIDEO: VideoModel = defs::GROK_IMAGINE_VIDEO;

// ----- Catalog ------------------------------------------------------------------------------------

/// Every video model, in the same (UI) order as `VideoModelCatalog.all`.
pub static ALL: &[VideoModel] = &[
    defs::VEO_31,
    defs::VEO_31_FAST,
    defs::VEO_31_LITE,
    defs::GEMINI_OMNI_FLASH_11,
    defs::SORA_2_PRO,
    defs::SORA_2,
    defs::KLING_30_PRO,
    defs::KLING_30_STANDARD,
    defs::KLING_26_PRO,
    defs::KLING_25_TURBO_PRO,
    defs::SEEDANCE_25,
    defs::SEEDANCE_20,
    defs::SEEDANCE_20_FAST,
    defs::SEEDANCE_15_PRO,
    defs::MINIMAX_H3_MAX,
    defs::MINIMAX_H3,
    defs::FLUX_3,
    defs::HAPPY_HORSE_11,
    defs::HAPPY_HORSE_10,
    defs::WAN_30_PRIME,
    defs::WAN_30,
    defs::WAN_27,
    defs::PIXVERSE_V6,
    defs::GROK_IMAGINE_VIDEO_15,
    defs::GROK_IMAGINE_VIDEO,
];

/// Looks a model up by its persistence id (`VideoModelCatalog.model(id:)`).
pub fn model(id: &str) -> Option<&'static VideoModel> {
    ALL.iter().find(|m| m.id == id)
}
