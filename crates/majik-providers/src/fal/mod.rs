//! fal.ai provider.
//!
//! - [`capabilities`]: supported models, capability tables and endpoint / parameter routing.
//! - [`provider`]: [`FalClient`], the HTTP client (queue polling and sync tool endpoints).
//! - [`audio`]: ElevenLabs v3 / Gemini TTS routing and request bodies.
//! - [`models`]: wire types. [`error`]: `FalError` and HTTP error mapping.

pub mod audio;
pub mod capabilities;
pub mod pricing;
pub mod error;
pub mod models;
pub mod provider;

use std::sync::{Arc, OnceLock};

use crate::catalog;
use crate::client::{AudioProviderClient, ImageProviderClient, ResumableClient, TextProviderClient, ToolProviderClient, VideoProviderClient};
use crate::descriptor::ProviderDescriptor;
use crate::ProviderId;

pub use audio::{audio_routing, build_audio_request_body, normalize_speaker_prefixes, AudioRouting};
pub use capabilities::VideoEndpointVariant;
pub use error::{build_message, handle_http_error, FalError};
pub use provider::FalClient;

/// First-party descriptor registered into `ProviderRegistry::shared()`.
///
/// Panics on first use if a model id from the fal tables is missing from the catalogs.
pub fn descriptor() -> &'static ProviderDescriptor {
    static DESCRIPTOR: OnceLock<ProviderDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| ProviderDescriptor {
        id: ProviderId::fal(),
        display_name: "fal.ai",
        logo_asset_name: crate::logo::FAL,
        api_key_placeholder: "9...",
        api_key_instructions: "Get your API key from fal.ai/dashboard/keys",
        api_key_url: "https://fal.ai/dashboard/keys",
        billing_url: Some("https://fal.ai/dashboard/usage-billing"),
        requires_api_key: true,
        is_user_selectable: true,
        supported_image_models: capabilities::SUPPORTED_IMAGE_MODEL_IDS
            .iter()
            .map(|id| catalog::image::model(id).unwrap_or_else(|| panic!("fal: image model '{id}' missing from catalog")).clone())
            .collect(),
        supported_video_models: capabilities::SUPPORTED_VIDEO_MODEL_IDS
            .iter()
            .map(|id| catalog::video::model(id).unwrap_or_else(|| panic!("fal: video model '{id}' missing from catalog")).clone())
            .collect(),
        supported_audio_models: capabilities::SUPPORTED_AUDIO_MODEL_IDS
            .iter()
            .map(|id| catalog::audio::model(id).unwrap_or_else(|| panic!("fal: audio model '{id}' missing from catalog")).clone())
            .collect(),
        image_capabilities: capabilities::image_capabilities,
        video_capabilities: capabilities::video_capabilities,
        audio_capabilities: capabilities::audio_capabilities,
        tool_capabilities: capabilities::tool_capabilities,
        pricing: pricing::pricing,
        supported_tool_models: vec![
            catalog::tool::TOPAZ_UPSCALE.clone(),
            catalog::tool::TOPAZ_UPSCALE_VIDEO.clone(),
            catalog::tool::BRIA_BACKGROUND_REMOVE.clone(),
        ],
        make_image_client: |options| Arc::new(FalClient::from_options(options)) as Arc<dyn ImageProviderClient>,
        make_video_client: |options| Some(Arc::new(FalClient::from_options(options)) as Arc<dyn VideoProviderClient>),
        make_audio_client: |options| Some(Arc::new(FalClient::from_options(options)) as Arc<dyn AudioProviderClient>),
        make_text_client: |options| Some(Arc::new(FalClient::from_options(options)) as Arc<dyn TextProviderClient>),
        make_tool_client: |options| Some(Arc::new(FalClient::from_options(options)) as Arc<dyn ToolProviderClient>),
        make_resume_client: |options| Some(Arc::new(FalClient::from_options(options)) as Arc<dyn ResumableClient>),
    })
}
