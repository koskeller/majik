//! The provider client traits — image, video, audio — and the [`ProviderClient`] facade over them.

use async_trait::async_trait;
use std::sync::Arc;

use crate::asset::ProviderAsset;
use crate::descriptor::ProviderDescriptor;
use crate::error::{GenerationError, Result};
use crate::models::{AspectRatio, ImageModel, ImageResolution};
use crate::registry::ProviderRegistry;
use crate::settings::{AudioGenerationSettings, VideoGenerationSettings};
use crate::{Bytes, ProviderId};
use majik_core::model::{JobTrace, MediaType};

#[async_trait]
pub trait ImageProviderClient: Send + Sync {
    async fn generate_image(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Result<Bytes>;

    async fn upscale_image(&self, image: &[u8]) -> Result<Bytes>;

    async fn remove_background(&self, image: &[u8]) -> Result<Bytes>;
}

#[async_trait]
pub trait VideoProviderClient: Send + Sync {
    async fn generate_video(&self, prompt: &str, assets: &[ProviderAsset], settings: &VideoGenerationSettings) -> Result<Bytes>;
}

#[async_trait]
pub trait AudioProviderClient: Send + Sync {
    async fn generate_audio(&self, prompt: &str, settings: &AudioGenerationSettings) -> Result<Bytes>;
}

/// One-shot text completion, used to rewrite a prompt. Not a chat: no history, no streaming, no
/// tools. Each provider picks the small model it routes this to.
#[async_trait]
pub trait TextProviderClient: Send + Sync {
    async fn complete_text(&self, system: &str, user: &str, max_tokens: usize) -> Result<String>;
}

/// The provider's handle for a queued generation, reported the moment the provider accepts it so
/// the app can persist it and re-attach through [`ResumableClient::resume`] after a relaunch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobHandle {
    pub job_id: String,
    pub poll_url: Option<String>,
}

/// Where queue-backed clients report a [`JobHandle`] (called from the request's async context).
pub type OnAccepted = Arc<dyn Fn(JobHandle) + Send + Sync>;

/// Where clients report every HTTP exchange they make (see [`crate::http::send_traced`]), for
/// the library's per-attempt trail. Never sees headers.
pub type TraceSink = Arc<dyn Fn(JobTrace) + Send + Sync>;

/// What a provider client is built from.
#[derive(Clone, Default)]
pub struct ClientOptions {
    pub api_key: String,
    pub on_accepted: Option<OnAccepted>,
    pub on_trace: Option<TraceSink>,
}

impl ClientOptions {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), on_accepted: None, on_trace: None }
    }
}

/// Re-attaching to a generation the provider accepted earlier (fal's queue, Replicate's
/// predictions). Synchronous providers have nothing to resume and don't implement it.
#[async_trait]
pub trait ResumableClient: Send + Sync {
    /// Await the result of the job behind `handle`; `media_type` says how to read the payload.
    async fn resume(&self, handle: &JobHandle, media_type: MediaType) -> Result<Bytes>;
}

/// Façade over a provider's clients, built from a descriptor and an API key.
#[derive(Clone)]
pub struct ProviderClient {
    pub descriptor: &'static ProviderDescriptor,
    image: Arc<dyn ImageProviderClient>,
    video: Option<Arc<dyn VideoProviderClient>>,
    audio: Option<Arc<dyn AudioProviderClient>>,
    text: Option<Arc<dyn TextProviderClient>>,
    resume: Option<Arc<dyn ResumableClient>>,
}

impl ProviderClient {
    pub fn new(descriptor: &'static ProviderDescriptor, api_key: &str) -> Self {
        Self::with_options(descriptor, &ClientOptions::new(api_key))
    }

    pub fn with_options(descriptor: &'static ProviderDescriptor, options: &ClientOptions) -> Self {
        Self {
            descriptor,
            image: (descriptor.make_image_client)(options),
            video: (descriptor.make_video_client)(options),
            audio: (descriptor.make_audio_client)(options),
            text: (descriptor.make_text_client)(options),
            resume: (descriptor.make_resume_client)(options),
        }
    }

    pub fn supports_resume(&self) -> bool {
        self.resume.is_some()
    }

    pub async fn resume(&self, handle: &JobHandle, media_type: MediaType) -> Result<Bytes> {
        match &self.resume {
            Some(r) => r.resume(handle, media_type).await,
            None => Err(GenerationError::JobGone),
        }
    }

    pub fn for_provider(id: &ProviderId, api_key: &str) -> Option<Self> {
        ProviderRegistry::shared().descriptor(id).map(|d| Self::new(d, api_key))
    }

    pub async fn generate_image(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Result<Bytes> {
        self.image.generate_image(prompt, model, assets, aspect_ratio, resolution).await
    }

    pub async fn generate_video(&self, prompt: &str, assets: &[ProviderAsset], settings: &VideoGenerationSettings) -> Result<Bytes> {
        match &self.video {
            Some(v) => v.generate_video(prompt, assets, settings).await,
            None => Err(GenerationError::InvalidRequest(format!("Video generation is not supported by {}", self.descriptor.display_name))),
        }
    }

    pub async fn generate_audio(&self, prompt: &str, settings: &AudioGenerationSettings) -> Result<Bytes> {
        match &self.audio {
            Some(a) => a.generate_audio(prompt, settings).await,
            None => Err(GenerationError::InvalidRequest(format!("Audio generation is not supported by {}", self.descriptor.display_name))),
        }
    }

    /// Rewrite `user` under the instruction `system`. Providers that route no text model refuse.
    pub async fn complete_text(&self, system: &str, user: &str, max_tokens: usize) -> Result<String> {
        match &self.text {
            Some(t) => t.complete_text(system, user, max_tokens).await,
            None => Err(GenerationError::InvalidRequest(format!("Prompt improvement is not supported by {}", self.descriptor.display_name))),
        }
    }

    pub async fn upscale_image(&self, image: &[u8]) -> Result<Bytes> {
        self.image.upscale_image(image).await
    }

    pub async fn remove_background(&self, image: &[u8]) -> Result<Bytes> {
        self.image.remove_background(image).await
    }
}
