//! The provider descriptor. Providers are static, so the capability and factory hooks are plain
//! function pointers.

use std::sync::Arc;

use crate::client::{AudioProviderClient, ClientOptions, ImageProviderClient, ResumableClient, TextProviderClient, VideoProviderClient};
use crate::models::{AudioModel, AudioModelCapabilities, ImageModel, ModelCapabilities, ToolId, ToolModel, VideoModel, VideoModelCapabilities};
use crate::pricing::{Estimate, PricedJob};
use crate::ProviderId;

pub type ImageCapabilitiesFn = fn(&ImageModel) -> Option<ModelCapabilities>;
pub type VideoCapabilitiesFn = fn(&VideoModel) -> Option<VideoModelCapabilities>;
pub type AudioCapabilitiesFn = fn(&AudioModel) -> Option<AudioModelCapabilities>;
/// What one output of a configured job costs. `Estimate::Unknown` for a model this provider has no
/// price for, which is a valid answer rather than an error.
pub type PricingFn = fn(&PricedJob<'_>) -> Estimate;
pub type MakeImageClientFn = fn(&ClientOptions) -> Arc<dyn ImageProviderClient>;
pub type MakeVideoClientFn = fn(&ClientOptions) -> Option<Arc<dyn VideoProviderClient>>;
pub type MakeAudioClientFn = fn(&ClientOptions) -> Option<Arc<dyn AudioProviderClient>>;
/// `None` for providers whose generations can't be re-attached to (synchronous APIs).
pub type MakeResumeClientFn = fn(&ClientOptions) -> Option<Arc<dyn ResumableClient>>;
/// `None` for providers that route no text model (the composer then hides Improve Prompt).
pub type MakeTextClientFn = fn(&ClientOptions) -> Option<Arc<dyn TextProviderClient>>;

pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: &'static str,
    // UI metadata; the app reads these directly, so it needs no per-provider switches.
    pub logo_asset_name: &'static str,
    pub api_key_placeholder: &'static str,
    pub api_key_instructions: &'static str,
    pub api_key_url: &'static str,
    pub billing_url: Option<&'static str>,
    // Configuration.
    pub requires_api_key: bool,
    pub is_user_selectable: bool,
    // Static capability data.
    pub supported_image_models: Vec<ImageModel>,
    pub supported_video_models: Vec<VideoModel>,
    pub supported_audio_models: Vec<AudioModel>,
    pub image_capabilities: ImageCapabilitiesFn,
    pub video_capabilities: VideoCapabilitiesFn,
    pub audio_capabilities: AudioCapabilitiesFn,
    pub pricing: PricingFn,
    /// Tool implementations (upscalers, background removers) this provider offers; empty = no tools.
    pub supported_tool_models: Vec<ToolModel>,
    // Factories.
    pub make_image_client: MakeImageClientFn,
    pub make_video_client: MakeVideoClientFn,
    pub make_audio_client: MakeAudioClientFn,
    pub make_text_client: MakeTextClientFn,
    pub make_resume_client: MakeResumeClientFn,
}

impl std::fmt::Debug for ProviderDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDescriptor").field("id", &self.id).field("display_name", &self.display_name).finish()
    }
}

impl ProviderDescriptor {
    /// Whether an accepted generation can be re-attached to after a relaunch.
    pub fn supports_resume(&self) -> bool {
        (self.make_resume_client)(&ClientOptions::default()).is_some()
    }

    /// Whether this provider can rewrite a prompt. The composer only draws its Improve button for
    /// providers that can.
    pub fn supports_prompt_improvement(&self) -> bool {
        (self.make_text_client)(&ClientOptions::default()).is_some()
    }

    pub fn supports_video_generation(&self) -> bool {
        !self.supported_video_models.is_empty()
    }

    pub fn supports_audio_generation(&self) -> bool {
        !self.supported_audio_models.is_empty()
    }

    pub fn supports_image_model(&self, model: &ImageModel) -> bool {
        self.supported_image_models.contains(model)
    }

    pub fn supports_video_model(&self, model: &VideoModel) -> bool {
        self.supported_video_models.contains(model)
    }

    pub fn supports_audio_model(&self, model: &AudioModel) -> bool {
        self.supported_audio_models.contains(model)
    }

    pub fn image_capabilities(&self, model: &ImageModel) -> Option<ModelCapabilities> {
        (self.image_capabilities)(model)
    }

    pub fn video_capabilities(&self, model: &VideoModel) -> Option<VideoModelCapabilities> {
        (self.video_capabilities)(model)
    }

    pub fn audio_capabilities(&self, model: &AudioModel) -> Option<AudioModelCapabilities> {
        (self.audio_capabilities)(model)
    }

    /// What one output of `job` costs on this provider. An *estimate*: prices change and some
    /// models bill on the size of an output that doesn't exist yet.
    pub fn price(&self, job: &PricedJob<'_>) -> Estimate {
        (self.pricing)(job)
    }

    pub fn supports_tool(&self, kind: ToolId) -> bool {
        self.supported_tool_models.iter().any(|m| m.kind == kind)
    }

    /// The models implementing `kind`, in declaration order (the composer indexes into this list).
    pub fn tool_models(&self, kind: ToolId) -> Vec<&ToolModel> {
        self.supported_tool_models.iter().filter(|m| m.kind == kind).collect()
    }

    /// The model the direct-run entry points (context menus) use: the first one declared.
    pub fn default_tool_model(&self, kind: ToolId) -> Option<&ToolModel> {
        self.supported_tool_models.iter().find(|m| m.kind == kind)
    }
}
