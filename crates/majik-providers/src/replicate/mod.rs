//! Replicate provider: descriptor, capability tables, predictions client.

pub mod audio;
pub mod capabilities;
pub mod pricing;
pub mod error;
pub mod models;
pub mod provider;

use std::sync::{Arc, OnceLock};

use crate::client::{AudioProviderClient, ClientOptions, ImageProviderClient, ResumableClient, TextProviderClient, VideoProviderClient};
use crate::descriptor::ProviderDescriptor;
use crate::ProviderId;

pub use capabilities::VideoEndpointVariant;
pub use error::ReplicateError;
pub use provider::{ReplicateClient, SubmissionTarget};

fn make_image_client(options: &ClientOptions) -> Arc<dyn ImageProviderClient> {
    Arc::new(ReplicateClient::from_options(options))
}

fn make_video_client(options: &ClientOptions) -> Option<Arc<dyn VideoProviderClient>> {
    Some(Arc::new(ReplicateClient::from_options(options)))
}

fn make_audio_client(options: &ClientOptions) -> Option<Arc<dyn AudioProviderClient>> {
    Some(Arc::new(ReplicateClient::from_options(options)))
}

fn make_text_client(options: &ClientOptions) -> Option<Arc<dyn TextProviderClient>> {
    Some(Arc::new(ReplicateClient::from_options(options)))
}

fn make_resume_client(options: &ClientOptions) -> Option<Arc<dyn ResumableClient>> {
    Some(Arc::new(ReplicateClient::from_options(options)))
}

/// First-party descriptor registered into `ProviderRegistry::shared()`.
///
/// One `ReplicateClient` serves all three media types, and `ProviderClient` consults each factory
/// directly, so all three build one.
pub fn descriptor() -> &'static ProviderDescriptor {
    static DESCRIPTOR: OnceLock<ProviderDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| ProviderDescriptor {
        id: ProviderId::replicate(),
        display_name: "Replicate",
        logo_asset_name: crate::logo::REPLICATE,
        api_key_placeholder: "r8_...",
        api_key_instructions: "Get your API token from replicate.com/account/api-tokens",
        api_key_url: "https://replicate.com/account/api-tokens",
        billing_url: Some("https://replicate.com/account/billing"),
        requires_api_key: true,
        is_user_selectable: true,
        supported_image_models: capabilities::supported_image_models(),
        supported_video_models: capabilities::supported_video_models(),
        supported_audio_models: audio::supported_audio_models(),
        image_capabilities: capabilities::image_capabilities,
        video_capabilities: capabilities::video_capabilities,
        audio_capabilities: audio::audio_capabilities,
        pricing: pricing::pricing,
        supported_tool_models: vec![crate::catalog::tool::CLARITY_UPSCALER.clone(), crate::catalog::tool::REMBG.clone()],
        make_image_client,
        make_video_client,
        make_audio_client,
        make_text_client,
        make_resume_client,
    })
}
