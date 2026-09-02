//! Tool models: the upscalers and background removers each provider exposes, in the same shape as
//! [`super::image`] so the composer can list and select them. Names are persisted as `model_name`
//! on generated rows, so they never change.
//!
//! A tool model declares the media it works on ([`ToolModel::media`]): the composer's Upscale tab
//! draws an image card or a video card depending on which one is selected, rather than splitting
//! into two tabs.

use crate::models::ToolModel;
use majik_core::model::{MediaType, ToolId};

mod defs {
    use crate::logo;
    use crate::models::ToolModel;
    use majik_core::model::{MediaType, ToolId};

    pub const TOPAZ_UPSCALE: ToolModel =
        ToolModel::new("topaz-upscale", ToolId::Upscale, MediaType::Image, "Topaz Upscale", "Topaz Labs", logo::FAL, "Upscale with detail recovery");
    pub const TOPAZ_UPSCALE_VIDEO: ToolModel = ToolModel::new(
        "topaz-upscale-video",
        ToolId::Upscale,
        MediaType::Video,
        "Topaz Video Upscale",
        "Topaz Labs",
        logo::FAL,
        "Upscale a clip with Topaz Video AI",
    );
    pub const BRIA_BACKGROUND_REMOVE: ToolModel = ToolModel::new(
        "bria-background-remove",
        ToolId::RemoveBackground,
        MediaType::Image,
        "BRIA Background Remove",
        "BRIA",
        logo::FAL,
        "Cut out the subject on a transparent background",
    );
    pub const CLARITY_UPSCALER: ToolModel =
        ToolModel::new("clarity-upscaler", ToolId::Upscale, MediaType::Image, "Clarity Upscaler", "philz1337x", logo::REPLICATE, "2× upscale with creative detail");
    pub const REMBG: ToolModel =
        ToolModel::new("rembg", ToolId::RemoveBackground, MediaType::Image, "rembg", "danielgatis", logo::REPLICATE, "Cut out the subject on a transparent background");
    pub const MOCK_UPSCALE: ToolModel = ToolModel::new("mock-upscale", ToolId::Upscale, MediaType::Image, "Mock Upscale", "Mock", logo::MOCK, "Returns the input unchanged");
    pub const MOCK_UPSCALE_VIDEO: ToolModel =
        ToolModel::new("mock-upscale-video", ToolId::Upscale, MediaType::Video, "Mock Video Upscale", "Mock", logo::MOCK, "Returns the input unchanged");
    pub const MOCK_REMOVE_BACKGROUND: ToolModel =
        ToolModel::new("mock-remove-background", ToolId::RemoveBackground, MediaType::Image, "Mock Remove Background", "Mock", logo::MOCK, "Returns the input unchanged");
}

pub static TOPAZ_UPSCALE: ToolModel = defs::TOPAZ_UPSCALE;
pub static TOPAZ_UPSCALE_VIDEO: ToolModel = defs::TOPAZ_UPSCALE_VIDEO;
pub static BRIA_BACKGROUND_REMOVE: ToolModel = defs::BRIA_BACKGROUND_REMOVE;
pub static CLARITY_UPSCALER: ToolModel = defs::CLARITY_UPSCALER;
pub static REMBG: ToolModel = defs::REMBG;
pub static MOCK_UPSCALE: ToolModel = defs::MOCK_UPSCALE;
pub static MOCK_UPSCALE_VIDEO: ToolModel = defs::MOCK_UPSCALE_VIDEO;
pub static MOCK_REMOVE_BACKGROUND: ToolModel = defs::MOCK_REMOVE_BACKGROUND;

/// Every tool model, grouped by provider then kind.
pub static ALL: &[ToolModel] = &[
    defs::TOPAZ_UPSCALE,
    defs::TOPAZ_UPSCALE_VIDEO,
    defs::BRIA_BACKGROUND_REMOVE,
    defs::CLARITY_UPSCALER,
    defs::REMBG,
    defs::MOCK_UPSCALE,
    defs::MOCK_UPSCALE_VIDEO,
    defs::MOCK_REMOVE_BACKGROUND,
];

/// Looks a model up by its persistence id.
pub fn model(id: &str) -> Option<&'static ToolModel> {
    ALL.iter().find(|m| m.id == id)
}

/// Every catalog model implementing `kind`, in catalog order.
pub fn of_kind(kind: ToolId) -> impl Iterator<Item = &'static ToolModel> {
    ALL.iter().filter(move |m| m.kind == kind)
}

/// Every catalog model implementing `kind` over `media`, in catalog order.
pub fn of_kind_and_media(kind: ToolId, media: MediaType) -> impl Iterator<Item = &'static ToolModel> {
    ALL.iter().filter(move |m| m.kind == kind && m.media == media)
}
