//! An offline provider that claims every catalog model and renders deterministic fixtures.

pub mod directives;
pub mod image_renderer;
pub mod pricing;
pub mod provider;
pub mod video_renderer;

use std::sync::{Arc, OnceLock};

use crate::catalog;
use crate::client::{AudioProviderClient, ClientOptions, ImageProviderClient, ResumableClient, TextProviderClient, VideoProviderClient};
use crate::descriptor::ProviderDescriptor;
use crate::logo;
use crate::models::{AudioModel, AudioModelCapabilities, ImageModel, ModelCapabilities, VideoModel, VideoModelCapabilities};
use crate::ProviderId;

pub use directives::{parse_directives, Parsed};
pub use provider::MockClient;

pub fn descriptor() -> &'static ProviderDescriptor {
    static DESCRIPTOR: OnceLock<ProviderDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| ProviderDescriptor {
        id: ProviderId::mock(),
        display_name: "Mock",
        logo_asset_name: logo::FAL, // borrows the fal logo for now
        api_key_placeholder: "mock-any-key",
        api_key_instructions: "Mock provider — any non-empty value works",
        api_key_url: "https://github.com",
        billing_url: None,
        requires_api_key: true,
        // Debug builds only.
        is_user_selectable: cfg!(debug_assertions),
        supported_image_models: catalog::image::ALL.to_vec(),
        supported_video_models: catalog::video::ALL.to_vec(),
        supported_audio_models: catalog::audio::ALL.to_vec(),
        image_capabilities,
        video_capabilities,
        audio_capabilities,
        pricing: pricing::pricing,
        supported_tool_models: vec![catalog::tool::MOCK_UPSCALE.clone(), catalog::tool::MOCK_REMOVE_BACKGROUND.clone()],
        make_image_client,
        make_video_client,
        make_audio_client,
        make_text_client,
        make_resume_client,
    })
}

// Mock supports every catalog model; capabilities come from whichever first-party provider knows
// the model. This is not a defensive fallback.

fn image_capabilities(model: &ImageModel) -> Option<ModelCapabilities> {
    crate::fal::descriptor().image_capabilities(model).or_else(|| crate::openrouter::descriptor().image_capabilities(model))
}

fn video_capabilities(model: &VideoModel) -> Option<VideoModelCapabilities> {
    crate::fal::descriptor().video_capabilities(model).or_else(|| crate::replicate::descriptor().video_capabilities(model))
}

fn audio_capabilities(model: &AudioModel) -> Option<AudioModelCapabilities> {
    crate::fal::descriptor().audio_capabilities(model)
}

fn make_image_client(options: &ClientOptions) -> Arc<dyn ImageProviderClient> {
    Arc::new(MockClient::from_options(options))
}

fn make_resume_client(options: &ClientOptions) -> Option<Arc<dyn ResumableClient>> {
    Some(Arc::new(MockClient::from_options(options)))
}

// One `MockClient` serves all three media types; the factories hand out the same client.
fn make_video_client(options: &ClientOptions) -> Option<Arc<dyn VideoProviderClient>> {
    Some(Arc::new(MockClient::from_options(options)))
}

fn make_audio_client(options: &ClientOptions) -> Option<Arc<dyn AudioProviderClient>> {
    Some(Arc::new(MockClient::from_options(options)))
}

fn make_text_client(options: &ClientOptions) -> Option<Arc<dyn TextProviderClient>> {
    Some(Arc::new(MockClient::from_options(options)))
}
