//! Tool models: the upscalers and background removers each provider exposes, in the same shape as
//! [`super::image`] so the composer can list and select them. Names are persisted as `model_name`
//! on generated rows, so they never change.

use crate::models::ToolModel;
use majik_core::model::ToolId;

mod defs {
    use crate::logo;
    use crate::models::ToolModel;
use majik_core::model::ToolId;

    pub const TOPAZ_UPSCALE: ToolModel = ToolModel::new("topaz-upscale", ToolId::Upscale, "Topaz Upscale", "Topaz Labs", logo::FAL, "2× upscale with detail recovery");
    pub const BRIA_BACKGROUND_REMOVE: ToolModel =
        ToolModel::new("bria-background-remove", ToolId::RemoveBackground, "BRIA Background Remove", "BRIA", logo::FAL, "Cut out the subject on a transparent background");
    pub const CLARITY_UPSCALER: ToolModel =
        ToolModel::new("clarity-upscaler", ToolId::Upscale, "Clarity Upscaler", "philz1337x", logo::REPLICATE, "2× upscale with creative detail");
    pub const REMBG: ToolModel = ToolModel::new("rembg", ToolId::RemoveBackground, "rembg", "danielgatis", logo::REPLICATE, "Cut out the subject on a transparent background");
    pub const MOCK_UPSCALE: ToolModel = ToolModel::new("mock-upscale", ToolId::Upscale, "Mock Upscale", "Mock", logo::MOCK, "Returns the input unchanged");
    pub const MOCK_REMOVE_BACKGROUND: ToolModel =
        ToolModel::new("mock-remove-background", ToolId::RemoveBackground, "Mock Remove Background", "Mock", logo::MOCK, "Returns the input unchanged");
}

pub static TOPAZ_UPSCALE: ToolModel = defs::TOPAZ_UPSCALE;
pub static BRIA_BACKGROUND_REMOVE: ToolModel = defs::BRIA_BACKGROUND_REMOVE;
pub static CLARITY_UPSCALER: ToolModel = defs::CLARITY_UPSCALER;
pub static REMBG: ToolModel = defs::REMBG;
pub static MOCK_UPSCALE: ToolModel = defs::MOCK_UPSCALE;
pub static MOCK_REMOVE_BACKGROUND: ToolModel = defs::MOCK_REMOVE_BACKGROUND;

/// Every tool model, grouped by provider then kind.
pub static ALL: &[ToolModel] = &[defs::TOPAZ_UPSCALE, defs::BRIA_BACKGROUND_REMOVE, defs::CLARITY_UPSCALER, defs::REMBG, defs::MOCK_UPSCALE, defs::MOCK_REMOVE_BACKGROUND];

/// Looks a model up by its persistence id.
pub fn model(id: &str) -> Option<&'static ToolModel> {
    ALL.iter().find(|m| m.id == id)
}

/// Every catalog model implementing `kind`, in catalog order.
pub fn of_kind(kind: ToolId) -> impl Iterator<Item = &'static ToolModel> {
    ALL.iter().filter(move |m| m.kind == kind)
}
