//! fal's HTTP client.
//!
//! Generation goes through fal's queue API (`https://queue.fal.run/<endpoint>` → poll
//! `/requests/<id>/status` → fetch `/requests/<id>`); the upscale / background-removal tools use the
//! synchronous `https://fal.run/<endpoint>` path.

use std::time::Instant;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Map, Value};

use crate::asset::{AssetRole, ProviderAsset};
use crate::client::{AudioProviderClient, ClientOptions, ImageProviderClient, JobHandle, OnAccepted, ResumableClient, TextProviderClient, TraceSink, VideoProviderClient};
use crate::constants::fal as constants;
use crate::data_uri::{from_data_uri, to_data_uri};
use crate::error::{GenerationError, Result};
use crate::fal::audio::{audio_routing, build_audio_request_body};
use crate::fal::capabilities as caps;
use crate::fal::capabilities::VideoEndpointVariant;
use crate::fal::error::{handle_http_error, FalError};
use crate::fal::models::{
    FalAudioFileResponse, FalErrorDetail, FalQueueStatus, FalQueueStatusResponse, FalQueueSubmitResponse, FalQueuedVideoResponse, FalResponse,
    FalSingleImageResponse, FalTextResponse, FalVideoResponse,
};
use crate::http::{self, Timeouts};
use crate::models::{AspectRatio, ImageModel, ImageResolution, VideoModel, VideoResolution};
use crate::references::{rewrite_handles, ReferenceAssets, ReferenceTagStyle};
use crate::settings::{AudioGenerationSettings, VideoGenerationSettings};
use crate::transcode::transcode_to_png;
use crate::Bytes;
use majik_core::model::{MediaType, TraceLabel};

const PNG_MIME: &str = "image/png";

/// fal.ai client implementing image, video and audio generation.
#[derive(Clone)]
pub struct FalClient {
    api_key: String,
    base_url: String,
    queue_base_url: String,
    on_accepted: Option<OnAccepted>,
    on_trace: Option<TraceSink>,
}

impl std::fmt::Debug for FalClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FalClient").field("base_url", &self.base_url).field("queue_base_url", &self.queue_base_url).finish()
    }
}

/// A submitted queue request: where to poll and where the result will be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTicket {
    pub request_id: String,
    pub status_url: String,
    pub result_url: String,
}

impl FalClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), base_url: constants::BASE_URL.to_string(), queue_base_url: constants::QUEUE_BASE_URL.to_string(), on_accepted: None, on_trace: None }
    }

    pub fn from_options(options: &ClientOptions) -> Self {
        Self { on_accepted: options.on_accepted.clone(), on_trace: options.on_trace.clone(), ..Self::new(options.api_key.clone()) }
    }

    /// Report every accepted queue request to `on_accepted`.
    pub fn with_on_accepted(mut self, on_accepted: OnAccepted) -> Self {
        self.on_accepted = Some(on_accepted);
        self
    }

    /// Report every HTTP exchange to `on_trace`.
    pub fn with_on_trace(mut self, on_trace: TraceSink) -> Self {
        self.on_trace = Some(on_trace);
        self
    }

    /// Override the sync (`https://fal.run`) and queue (`https://queue.fal.run`) base URLs — for
    /// tests pointing at a local mock server.
    pub fn with_base_urls(mut self, base_url: impl Into<String>, queue_base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self.queue_base_url = queue_base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn queue_base_url(&self) -> &str {
        &self.queue_base_url
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header(AUTHORIZATION, format!("Key {}", self.api_key))
    }

    fn parse_url(raw: &str) -> std::result::Result<reqwest::Url, FalError> {
        reqwest::Url::parse(raw).map_err(|_| FalError::InvalidUrl(raw.to_string()))
    }

    async fn post_json(&self, url: &str, body: &Map<String, Value>, timeouts: Timeouts, label: TraceLabel) -> std::result::Result<(u16, Vec<u8>), FalError> {
        let url = Self::parse_url(url)?;
        let payload = serde_json::to_vec(body).map_err(|e| FalError::DecodingError(e.to_string()))?;
        let request = self.authorized(http::client().post(url)).header(CONTENT_TYPE, "application/json").body(payload).timeout(timeouts.request);
        Ok(http::send_traced(request, label, self.on_trace.as_ref()).await?)
    }

    async fn get(&self, url: &str, timeouts: Timeouts, label: TraceLabel) -> std::result::Result<(u16, Vec<u8>), FalError> {
        let url = Self::parse_url(url)?;
        let request = self.authorized(http::client().get(url)).timeout(timeouts.request);
        Ok(http::send_traced(request, label, self.on_trace.as_ref()).await?)
    }
}

// ----- ImageProviderClient / VideoProviderClient / AudioProviderClient ---------------------------

#[async_trait]
impl ImageProviderClient for FalClient {
    async fn generate_image(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Result<Bytes> {
        self.generate_image_impl(prompt, model, assets, aspect_ratio, resolution).await.map_err(GenerationError::from)
    }

    async fn upscale_image(&self, image: &[u8]) -> Result<Bytes> {
        self.upscale_image_impl(image).await.map_err(GenerationError::from)
    }

    async fn remove_background(&self, image: &[u8]) -> Result<Bytes> {
        self.remove_background_impl(image).await.map_err(GenerationError::from)
    }
}

#[async_trait]
impl VideoProviderClient for FalClient {
    async fn generate_video(&self, prompt: &str, assets: &[ProviderAsset], settings: &VideoGenerationSettings) -> Result<Bytes> {
        self.generate_video_impl(prompt, assets, settings).await.map_err(GenerationError::from)
    }
}

#[async_trait]
impl TextProviderClient for FalClient {
    async fn complete_text(&self, system: &str, user: &str, max_tokens: usize) -> Result<String> {
        Ok(self.complete_text_impl(system, user, max_tokens).await?)
    }
}

#[async_trait]
impl ResumableClient for FalClient {
    async fn resume(&self, handle: &JobHandle, media_type: MediaType) -> Result<Bytes> {
        self.resume_impl(handle, media_type).await.map_err(Into::into)
    }
}

#[async_trait]
impl AudioProviderClient for FalClient {
    async fn generate_audio(&self, prompt: &str, settings: &AudioGenerationSettings) -> Result<Bytes> {
        self.generate_audio_impl(prompt, settings).await.map_err(GenerationError::from)
    }
}

// ----- Implementations ----------------------------------------------------------------------------

impl FalClient {
    async fn generate_image_impl(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> std::result::Result<Bytes, FalError> {
        let mut reference_images: Vec<&[u8]> = Vec::new();
        let mut mask: Option<&[u8]> = None;
        for asset in assets {
            match asset.role {
                AssetRole::ReferenceImage => reference_images.push(&asset.data),
                AssetRole::MaskImage => {
                    if caps::api_mask_param(model).is_none() {
                        return Err(FalError::BadRequest("This model does not accept a mask input".into()));
                    }
                    if mask.is_some() {
                        return Err(FalError::BadRequest("At most one mask is supported".into()));
                    }
                    mask = Some(&asset.data);
                }
                AssetRole::ReferenceVideo | AssetRole::FirstFrame | AssetRole::LastFrame | AssetRole::ControlImage | AssetRole::Audio => {
                    return Err(FalError::BadRequest(format!("Role '{}' is not supported by fal image endpoints", asset.role.raw())));
                }
            }
        }

        if mask.is_some() && reference_images.is_empty() {
            // Both mask-bearing endpoints (gpt-image-1.5 /edit and gpt-image-2 /edit) require
            // image_urls — sending a mask without a reference is guaranteed to 422 from fal.
            return Err(FalError::BadRequest("A mask requires at least one reference image".into()));
        }

        let endpoint = if let (false, Some(edit)) = (reference_images.is_empty(), caps::edit_endpoint(model)) {
            edit
        } else if let Some(text_to_image) = caps::endpoint(model) {
            text_to_image
        } else {
            return Err(FalError::UnsupportedModel(model.id.to_string()));
        };

        let body = Self::build_request_body(prompt, model, &reference_images, mask, aspect_ratio, resolution);

        let result_data = self.submit_and_await_queue(endpoint, &body, Timeouts::IMAGE, "image").await?;

        self.extract_image_data(&result_data).await
    }

    async fn upscale_image_impl(&self, image: &[u8]) -> std::result::Result<Bytes, FalError> {
        let mut body = Map::new();
        body.insert("image_url".into(), Value::String(to_data_uri(image, PNG_MIME)));
        body.insert("model".into(), Value::String("Standard V2".into()));
        body.insert("upscale_factor".into(), Value::from(2));
        body.insert("output_format".into(), Value::String("png".into()));
        self.process_image(constants::UPSCALE_ENDPOINT, &body).await
    }

    async fn remove_background_impl(&self, image: &[u8]) -> std::result::Result<Bytes, FalError> {
        let mut body = Map::new();
        body.insert("image_url".into(), Value::String(to_data_uri(image, PNG_MIME)));
        self.process_image(constants::REMOVE_BACKGROUND_ENDPOINT, &body).await
    }

    async fn generate_video_impl(&self, prompt: &str, assets: &[ProviderAsset], settings: &VideoGenerationSettings) -> std::result::Result<Bytes, FalError> {
        let model = &settings.model;
        let first_frame = assets.iter().find(|a| a.role == AssetRole::FirstFrame).map(|a| a.data.as_slice());
        let last_frame = assets.iter().find(|a| a.role == AssetRole::LastFrame).map(|a| a.data.as_slice());
        let audio_asset = assets.iter().find(|a| a.role == AssetRole::Audio);
        if let Some(asset) = assets.iter().find(|a| !a.role.is_reference() && !a.role.is_frame_input()) {
            return Err(FalError::BadRequest(format!("Role '{}' is not yet supported by fal video endpoints", asset.role.raw())));
        }
        let references = ReferenceAssets::from_assets(assets);

        let takes_audio_input = caps::api_audio_input_param(model).is_some();
        // Audio counts as a reference only where the model has a reference audio list; on a model
        // with neither that nor a conditioning input, "no audio input" is the honest answer.
        let audio_is_reference = !references.audio.is_empty() && !takes_audio_input && caps::video_reference_params(model).is_some_and(|p| p.audio.is_some());
        if references.has_visual() || audio_is_reference {
            Self::validate_references(&references, first_frame, last_frame, settings)?;
            let body = Self::build_video_reference_body(prompt, &references, settings);
            let endpoint_id = caps::video_reference_endpoint(model).ok_or_else(|| FalError::UnsupportedModel(model.id.to_string()))?;
            let result_data = self.submit_and_await_queue(endpoint_id, &body, Timeouts::video(settings.duration), "video").await?;
            return self.download_video_result(&result_data).await;
        }

        if audio_asset.is_some() && !takes_audio_input {
            return Err(FalError::BadRequest("This model does not accept an audio input".into()));
        }
        if last_frame.is_some() && first_frame.is_none() {
            return Err(FalError::BadRequest("A last frame requires a first frame".into()));
        }
        let (endpoint_id, variant) = Self::resolve_video_endpoint(model, first_frame, last_frame)?;

        let body = Self::build_video_request_body(prompt, first_frame, last_frame, audio_asset, variant, settings);

        let result_data = self.submit_and_await_queue(endpoint_id, &body, Timeouts::video(settings.duration), "video").await?;
        self.download_video_result(&result_data).await
    }

    /// Everything fal will reject about a reference request, said in a sentence the user can act on.
    /// `majik-generation`'s validation catches these before a job is ever queued; this is the guard
    /// for anything that reaches the client another way.
    fn validate_references(
        references: &ReferenceAssets<'_>,
        first_frame: Option<&[u8]>,
        last_frame: Option<&[u8]>,
        settings: &VideoGenerationSettings,
    ) -> std::result::Result<(), FalError> {
        let model = &settings.model;
        let (Some(params), Some(caps)) = (caps::video_reference_params(model), caps::video_capabilities(model)) else {
            return Err(FalError::BadRequest(format!("{} does not take reference inputs", model.name)));
        };
        let Some(declared) = caps.references else {
            return Err(FalError::BadRequest(format!("{} does not take reference inputs", model.name)));
        };
        // fal's reference endpoints have no frame parameter at all, and Seedance says outright that
        // references and first/last frames can't be combined.
        if first_frame.is_some() || last_frame.is_some() {
            return Err(FalError::BadRequest("References and a start or end frame can't be used together".into()));
        }
        for (role, list) in references.lists() {
            if list.is_empty() {
                continue;
            }
            let max = declared.max_for(role);
            if params.param_for(role).is_none() || max == 0 {
                return Err(FalError::BadRequest(format!("{} does not take {} references", model.name, role.display_name().to_lowercase())));
            }
            if list.len() > max {
                return Err(FalError::BadRequest(format!("{} takes at most {max} {} references", model.name, role.display_name().to_lowercase())));
            }
        }
        if let Some(combined) = declared.combined_max {
            let total = references.counts().total();
            if total > combined {
                return Err(FalError::BadRequest(format!("{} takes at most {combined} references in total (got {total})", model.name)));
            }
        }
        if !references.audio.is_empty() && !references.has_visual() {
            return Err(FalError::BadRequest("An audio reference needs at least one image or video reference".into()));
        }
        if let (Some(allowed), Some(resolution)) = (declared.resolutions, settings.resolution) {
            if !allowed.contains(&resolution) {
                return Err(FalError::BadRequest(format!("{} renders references at {} only", model.name, describe_resolutions(allowed))));
            }
        }
        Ok(())
    }

    /// The video file behind a queue result payload.
    async fn download_video_result(&self, result_data: &[u8]) -> std::result::Result<Bytes, FalError> {
        let queued: FalQueuedVideoResponse = serde_json::from_slice(result_data).map_err(|e| FalError::DecodingError(e.to_string()))?;
        let video_response = queued.response.or_else(|| queued.video.map(|video| FalVideoResponse { video }));
        let Some(video_response) = video_response else {
            return Err(FalError::NoVideoGenerated);
        };

        tracing::info!("fal.ai video ready, downloading");

        let video_url = video_response.video.url;
        if Self::parse_url(&video_url).is_err() {
            return Err(FalError::VideoDownloadFailed("Invalid video URL".into()));
        }

        let video_data = match http::download_traced(&video_url, Timeouts::VIDEO.request, self.on_trace.as_ref()).await {
            Ok(bytes) => bytes,
            Err(GenerationError::ServerError { status_code: Some(code), .. }) => return Err(FalError::VideoDownloadFailed(format!("HTTP {code}"))),
            Err(other) => return Err(FalError::Transport(other)),
        };
        if video_data.is_empty() {
            return Err(FalError::NoVideoGenerated);
        }

        Ok(video_data)
    }

    async fn generate_audio_impl(&self, prompt: &str, settings: &AudioGenerationSettings) -> std::result::Result<Bytes, FalError> {
        let (endpoint, routing) = audio_routing(settings, prompt)?;
        let body = build_audio_request_body(prompt, settings, &routing);

        let result_data = self.submit_and_await_queue(endpoint, &body, Timeouts::AUDIO, "audio").await?;
        self.download_audio_result(&result_data).await
    }

    /// The audio file behind a queue result payload.
    async fn download_audio_result(&self, result_data: &[u8]) -> std::result::Result<Bytes, FalError> {
        let response: FalAudioFileResponse = serde_json::from_slice(result_data).map_err(|e| FalError::DecodingError(e.to_string()))?;
        let url = response.audio.url;
        Self::parse_url(&url)?;

        let audio_data = match http::download_traced(&url, Timeouts::AUDIO.request, self.on_trace.as_ref()).await {
            Ok(bytes) => bytes,
            Err(GenerationError::ServerError { status_code: Some(code), .. }) => return Err(FalError::AudioDownloadFailed(format!("HTTP {code}"))),
            Err(other) => return Err(FalError::Transport(other)),
        };
        if audio_data.is_empty() {
            return Err(FalError::NoResultGenerated);
        }
        Ok(audio_data)
    }
}

// ----- Request body builders ----------------------------------------------------------------------

impl FalClient {
    /// Port of `FalProvider.buildRequestBody`. `images` are reference images (data-URI encoded as
    /// PNG); `mask` is only written when the model has a native mask field.
    pub fn build_request_body(
        prompt: &str,
        model: &ImageModel,
        images: &[&[u8]],
        mask: Option<&[u8]>,
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Map<String, Value> {
        let mut body = Map::new();
        body.insert("prompt".into(), Value::String(prompt.to_string()));
        body.insert("enable_safety_checker".into(), Value::Bool(false));

        if let Some((key, value)) = aspect_ratio.and_then(|ar| caps::api_image_size(model, ar)) {
            body.insert(key.into(), Value::String(value));
        }

        if let Some((key, value)) = resolution.and_then(|r| caps::api_image_resolution(model, r)) {
            body.insert(key.into(), Value::String(value));
        }

        if caps::api_supports_output_format(model) == Some(true) {
            body.insert("output_format".into(), Value::String("png".into()));
        }

        if !images.is_empty() {
            if let Some(key) = caps::api_edit_image_param(model) {
                let data_uris: Vec<String> = images.iter().map(|bytes| to_data_uri(bytes, PNG_MIME)).collect();
                let value = if key == "image_url" { Value::String(data_uris[0].clone()) } else { Value::from(data_uris) };
                body.insert(key.into(), value);
            }
        }

        if let (Some(mask), Some(mask_key)) = (mask, caps::api_mask_param(model)) {
            body.insert(mask_key.into(), Value::String(to_data_uri(mask, PNG_MIME)));
        }

        body
    }

    /// The settings every video endpoint takes, whichever variant it is: prompt, safety, aspect,
    /// resolution, duration, the audio toggle and any field the endpoint marks required.
    fn build_video_settings_body(prompt: &str, settings: &VideoGenerationSettings) -> Map<String, Value> {
        let model = &settings.model;
        let mut body = Map::new();
        body.insert("prompt".into(), Value::String(prompt.to_string()));
        body.insert("enable_safety_checker".into(), Value::Bool(false));

        if let Some((key, value)) = settings.aspect_ratio.and_then(|ar| caps::api_video_aspect_ratio(model, ar)) {
            body.insert(key.into(), Value::String(value.into()));
        }
        if let Some((key, value)) = settings.resolution.and_then(|r| caps::api_video_resolution(model, r)) {
            body.insert(key.into(), Value::String(value.into()));
        }
        if let Some(duration) = caps::api_duration(model, settings.duration) {
            body.insert("duration".into(), duration);
        }
        if let Some(audio_key) = caps::api_audio_param(model) {
            body.insert(audio_key.into(), Value::Bool(settings.audio_enabled));
        }
        for (key, value) in caps::api_required_defaults(model) {
            body.insert((*key).into(), Value::String((*value).into()));
        }
        body
    }

    /// The reference endpoint's body: the settings, the prompt with its handles rewritten into this
    /// model's dialect, and one array of data URIs per reference kind — in attach order, which is
    /// what makes `@Image2` the second entry of `image_urls`.
    pub fn build_video_reference_body(prompt: &str, references: &ReferenceAssets<'_>, settings: &VideoGenerationSettings) -> Map<String, Value> {
        let params = caps::video_reference_params(&settings.model);
        let style = params.map(|p| p.style).unwrap_or(ReferenceTagStyle::Prose);
        let prompt = rewrite_handles(prompt, references.counts(), style);
        let mut body = Self::build_video_settings_body(&prompt, settings);
        for (role, list) in references.lists() {
            let Some(key) = params.and_then(|p| p.param_for(role)) else { continue };
            if list.is_empty() {
                continue;
            }
            let urls: Vec<Value> = list.iter().map(|a| Value::String(to_data_uri(&a.data, &a.mime_type()))).collect();
            body.insert(key.into(), Value::Array(urls));
        }
        body
    }

    /// Port of `FalProvider.buildVideoRequestBody`.
    pub fn build_video_request_body(
        prompt: &str,
        first_frame: Option<&[u8]>,
        last_frame: Option<&[u8]>,
        audio_asset: Option<&ProviderAsset>,
        variant: VideoEndpointVariant,
        settings: &VideoGenerationSettings,
    ) -> Map<String, Value> {
        let model = &settings.model;
        let mut body = Self::build_video_settings_body(prompt, settings);

        if let (Some(first_frame), Some(start_key)) = (first_frame, caps::api_start_frame_param(model, variant)) {
            body.insert(start_key.into(), Value::String(to_data_uri(first_frame, PNG_MIME)));
        }
        if let (Some(last_frame), Some(end_key)) = (last_frame, caps::api_end_frame_param(model, variant)) {
            body.insert(end_key.into(), Value::String(to_data_uri(last_frame, PNG_MIME)));
        }
        if let (Some(audio), Some(audio_input_key)) = (audio_asset, caps::api_audio_input_param(model)) {
            body.insert(audio_input_key.into(), Value::String(to_data_uri(&audio.data, &audio.mime_type())));
        }

        body
    }
}

/// "480p or 720p" — for the one model whose reference endpoint is narrower than its own catalog entry.
fn describe_resolutions(resolutions: &[VideoResolution]) -> String {
    let names: Vec<&str> = resolutions.iter().map(|r| r.display_name()).collect();
    match names.split_last() {
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

// ----- Endpoint resolution ------------------------------------------------------------------------

impl FalClient {
    /// Picks the right fal endpoint and variant for the given frame inputs.
    /// - Both frames present AND the model has a dedicated first-last endpoint (e.g. veo3.1) →
    ///   `FirstLast`.
    /// - At least a first frame present → `I2v` (Kling/Seedance/WAN handle both frames on i2v).
    /// - No frames → `T2v`.
    ///
    /// Returns `FalError::UnsupportedModel` if the chosen variant has no endpoint for this model.
    pub fn resolve_video_endpoint(
        model: &VideoModel,
        first_frame: Option<&[u8]>,
        last_frame: Option<&[u8]>,
    ) -> std::result::Result<(&'static str, VideoEndpointVariant), FalError> {
        if first_frame.is_some() && last_frame.is_some() {
            if let Some(first_last) = caps::video_first_last_frame_endpoint(model) {
                return Ok((first_last, VideoEndpointVariant::FirstLast));
            }
        }
        if first_frame.is_some() {
            return match caps::video_i2v_endpoint(model) {
                Some(i2v) => Ok((i2v, VideoEndpointVariant::I2v)),
                None => Err(FalError::UnsupportedModel(model.id.to_string())),
            };
        }
        match caps::video_endpoint(model) {
            Some(t2v) => Ok((t2v, VideoEndpointVariant::T2v)),
            None => Err(FalError::UnsupportedModel(model.id.to_string())),
        }
    }
}

// ----- Queue + HTTP helpers -----------------------------------------------------------------------

impl FalClient {
    /// Submits a job to fal's queue API, polls until completion, and returns the final response
    /// bytes. `timeouts.total` caps the whole poll loop (→ `FalError::QueueTimeout`).
    pub async fn submit_and_await_queue(
        &self,
        endpoint_id: &str,
        body: &Map<String, Value>,
        timeouts: Timeouts,
        kind: &str,
    ) -> std::result::Result<Vec<u8>, FalError> {
        let ticket = self.submit_to_queue(endpoint_id, body, timeouts, kind).await?;
        self.await_queue(&ticket, timeouts, kind).await
    }

    /// The submit half: POST the request and report the accepted handle.
    pub async fn submit_to_queue(&self, endpoint_id: &str, body: &Map<String, Value>, timeouts: Timeouts, kind: &str) -> std::result::Result<QueueTicket, FalError> {
        let submit_url = format!("{}/{}", self.queue_base_url, endpoint_id);
        let (submit_status, submit_data) = self.post_json(&submit_url, body, timeouts, TraceLabel::Submit).await?;
        if submit_status != 200 && submit_status != 202 {
            return Err(handle_http_error(submit_status, &submit_data));
        }

        let submit_result: FalQueueSubmitResponse = serde_json::from_slice(&submit_data).map_err(|e| FalError::DecodingError(e.to_string()))?;
        let request_id = submit_result.request_id;
        tracing::info!(kind, request_id = %request_id, "fal.ai queue submitted");

        let status_url = submit_result
            .status_url
            .unwrap_or_else(|| format!("{}/{}/requests/{}/status", self.queue_base_url, endpoint_id, request_id));
        let result_url = submit_result.response_url.unwrap_or_else(|| format!("{}/{}/requests/{}", self.queue_base_url, endpoint_id, request_id));
        if let Some(on_accepted) = &self.on_accepted {
            on_accepted(JobHandle { job_id: request_id.clone(), poll_url: Some(status_url.clone()) });
        }
        Ok(QueueTicket { request_id, status_url, result_url })
    }

    /// The poll half: wait for the request to complete, then fetch the result payload.
    pub async fn await_queue(&self, ticket: &QueueTicket, timeouts: Timeouts, kind: &str) -> std::result::Result<Vec<u8>, FalError> {
        let start_time = Instant::now();

        loop {
            let elapsed = start_time.elapsed();
            if elapsed > timeouts.total {
                return Err(FalError::QueueTimeout);
            }

            let (status_code, status_data) = self.get(&ticket.status_url, timeouts, TraceLabel::Poll).await?;
            if status_code != 200 && status_code != 202 {
                return Err(handle_http_error(status_code, &status_data));
            }

            let status_result: FalQueueStatusResponse =
                serde_json::from_slice(&status_data).map_err(|e| FalError::DecodingError(e.to_string()))?;
            match status_result.status {
                FalQueueStatus::Completed => break,
                FalQueueStatus::Failed => {
                    let detail = serde_json::from_slice::<FalErrorDetail>(&status_data).ok().and_then(|d| d.detail);
                    return Err(FalError::QueueFailed(detail.unwrap_or_else(|| format!("The model failed to generate the {kind}"))));
                }
                FalQueueStatus::InQueue | FalQueueStatus::InProgress | FalQueueStatus::Unknown(_) => {
                    http::sleep(http::poll_interval(elapsed)).await;
                    continue;
                }
            }
        }

        let (result_status, result_data) = self.get(&ticket.result_url, timeouts, TraceLabel::Result).await?;
        if result_status != 200 {
            return Err(handle_http_error(result_status, &result_data));
        }

        Ok(result_data)
    }

    /// The ticket a stored handle stands for: fal's result URL is the status URL minus `/status`.
    fn ticket_from_handle(handle: &JobHandle) -> std::result::Result<QueueTicket, FalError> {
        let status_url = handle.poll_url.clone().ok_or_else(|| FalError::BadRequest("fal handle without a status URL".into()))?;
        let result_url = status_url.strip_suffix("/status").ok_or_else(|| FalError::InvalidUrl(status_url.clone()))?.to_string();
        Ok(QueueTicket { request_id: handle.job_id.clone(), status_url, result_url })
    }

    async fn resume_impl(&self, handle: &JobHandle, media_type: MediaType) -> std::result::Result<Bytes, FalError> {
        let ticket = Self::ticket_from_handle(handle)?;
        let (timeouts, kind) = match media_type {
            MediaType::Image => (Timeouts::IMAGE, "image"),
            MediaType::Video => (Timeouts::video_resume(), "video"),
            MediaType::Audio => (Timeouts::AUDIO, "audio"),
        };
        tracing::info!(kind, request_id = %ticket.request_id, "fal.ai queue resumed");
        let result_data = match self.await_queue(&ticket, timeouts, kind).await {
            Err(FalError::HttpError { status_code: 404, .. }) => return Err(FalError::JobGone),
            other => other?,
        };
        match media_type {
            MediaType::Image => self.extract_image_data(&result_data).await,
            MediaType::Video => self.download_video_result(&result_data).await,
            MediaType::Audio => self.download_audio_result(&result_data).await,
        }
    }

    /// One synchronous `fal-ai/any-llm` completion (`https://fal.run/<endpoint>`). fal routes the
    /// model itself, so there is no queue to poll.
    async fn complete_text_impl(&self, system: &str, user: &str, max_tokens: usize) -> std::result::Result<String, FalError> {
        let mut body = Map::new();
        body.insert("model".into(), Value::String(constants::TEXT_MODEL.into()));
        body.insert("prompt".into(), Value::String(user.to_string()));
        body.insert("system_prompt".into(), Value::String(system.to_string()));
        body.insert("max_tokens".into(), Value::from(max_tokens));

        let url = format!("{}/{}", self.base_url, constants::ANY_LLM_ENDPOINT);
        let (status, data) = self.post_json(&url, &body, Timeouts::TEXT, TraceLabel::Submit).await?;
        if status != 200 {
            return Err(handle_http_error(status, &data));
        }
        let response: FalTextResponse = serde_json::from_slice(&data).map_err(|e| {
            tracing::error!(body = %String::from_utf8_lossy(&data), "fal.ai text decode failed");
            FalError::DecodingError(e.to_string())
        })?;
        if let Some(message) = response.error.filter(|e| !e.is_empty()) {
            tracing::error!(%message, "fal.ai any-llm reported an error");
            return Err(FalError::QueueFailed(message));
        }
        let text = response.output.unwrap_or_default().trim().to_string();
        if text.is_empty() {
            return Err(FalError::NoResultGenerated);
        }
        Ok(text)
    }

    /// Synchronous tool call (`https://fal.run/<endpoint>`) returning a single image.
    pub async fn process_image(&self, endpoint: &str, body: &Map<String, Value>) -> std::result::Result<Vec<u8>, FalError> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let (status, data) = self.post_json(&url, body, Timeouts::IMAGE, TraceLabel::Submit).await?;
        if status != 200 {
            return Err(handle_http_error(status, &data));
        }

        let parsed: FalSingleImageResponse = match serde_json::from_slice(&data) {
            Ok(p) => p,
            Err(e) => {
                let raw = String::from_utf8(data).unwrap_or_else(|_| "Unable to decode as string".into());
                tracing::error!(body = %raw, "fal.ai single-image decode failed");
                return Err(FalError::DecodingError(e.to_string()));
            }
        };

        self.download_image(&parsed.image.url).await
    }

    pub async fn download_image(&self, url: &str) -> std::result::Result<Vec<u8>, FalError> {
        Self::parse_url(url)?;
        let data = match http::download_traced(url, Timeouts::IMAGE.request, self.on_trace.as_ref()).await {
            Ok(bytes) => bytes,
            Err(GenerationError::ServerError { status_code, message }) => {
                tracing::error!(status = ?status_code, url, %message, "fal.ai image download failed");
                return Err(FalError::InvalidImageData);
            }
            Err(other) => return Err(FalError::Transport(other)),
        };
        if data.is_empty() {
            return Err(FalError::NoImageGenerated);
        }
        Ok(data)
    }

    /// Port of `FalProvider.extractImageData`: pulls the first image out of a queue result
    /// (inline `data:` URI or CDN URL) and normalizes it to PNG.
    pub async fn extract_image_data(&self, data: &[u8]) -> std::result::Result<Vec<u8>, FalError> {
        let response: FalResponse = match serde_json::from_slice(data) {
            Ok(r) => r,
            Err(e) => {
                let raw = std::str::from_utf8(data).unwrap_or("<non-utf8>");
                tracing::error!(body = %raw, "fal.ai response decode failed");
                return Err(FalError::DecodingError(e.to_string()));
            }
        };

        let Some(first_image) = response.images.and_then(|images| images.into_iter().next()) else {
            return Err(FalError::NoImageGenerated);
        };

        // fal returns either an inlined `data:` URI (when sync_mode is supported) or an https CDN
        // URL (for models without sync_mode, e.g. recraft v4 pro).
        let raw_bytes = if first_image.url.starts_with("data:") {
            from_data_uri(&first_image.url).ok_or(FalError::InvalidImageData)?
        } else {
            self.download_image(&first_image.url).await?
        };

        if first_image.content_type.as_deref() == Some(PNG_MIME) {
            return Ok(raw_bytes);
        }
        match transcode_to_png(&raw_bytes) {
            Some(png) => Ok(png),
            None => {
                tracing::error!(content_type = ?first_image.content_type, "fal.ai transcode to PNG failed");
                Err(FalError::InvalidImageData)
            }
        }
    }
}
