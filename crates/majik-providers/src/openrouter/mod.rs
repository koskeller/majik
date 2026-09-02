//! OpenRouter provider: a single `chat/completions` POST with
//! `modalities: ["image"]`, returning data-URI images.
//!
//! - [`capabilities`]: supported model ids, capability tables and OpenRouter slugs.
//! - [`models`]: request/response wire types.
//! - [`error`]: `OpenRouterError` and its mapping to [`GenerationError`](crate::GenerationError).
//! - [`provider`]: the HTTP client implementing [`ImageProviderClient`](crate::ImageProviderClient).

use std::sync::{Arc, OnceLock};

use crate::descriptor::ProviderDescriptor;
use crate::{catalog, logo, ProviderId};

pub mod capabilities;
pub mod pricing;
pub mod error;
pub mod models;
pub mod provider;

pub use error::OpenRouterError;
pub use provider::OpenRouterClient;

/// First-party descriptor registered into `ProviderRegistry::shared()`.
pub fn descriptor() -> &'static ProviderDescriptor {
    static DESCRIPTOR: OnceLock<ProviderDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| ProviderDescriptor {
        id: ProviderId::open_router(),
        display_name: "OpenRouter",
        logo_asset_name: logo::OPEN_ROUTER,
        api_key_placeholder: "sk-or-v1-...",
        api_key_instructions: "Get your API key from openrouter.ai/keys",
        api_key_url: "https://openrouter.ai/keys",
        billing_url: Some("https://openrouter.ai/settings/credits"),
        requires_api_key: true,
        is_user_selectable: true,
        supported_image_models: capabilities::SUPPORTED_IMAGE_MODEL_IDS.iter().filter_map(|id| catalog::image::model(id).cloned()).collect(),
        supported_video_models: Vec::new(),
        supported_audio_models: Vec::new(),
        image_capabilities: capabilities::image_capabilities,
        video_capabilities: |_| None,
        audio_capabilities: |_| None,
        tool_capabilities: |_| None,
        pricing: pricing::pricing,
        supported_tool_models: Vec::new(),
        make_image_client: |options| Arc::new(OpenRouterClient::from_options(options)),
        make_video_client: |_| None,
        make_audio_client: |_| None,
        make_text_client: |options| Some(Arc::new(OpenRouterClient::from_options(options))),
        // One synchronous POST: nothing to re-attach to.
        make_tool_client: |_| None,
        make_resume_client: |_| None,
    })
}
