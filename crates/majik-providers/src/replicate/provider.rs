//! The HTTP client (predictions API), request-body builders, submit + poll loop and output
//! extraction.

use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::asset::{AssetRole, ProviderAsset};
use crate::client::{
    ClientOptions, ImageProviderClient, JobHandle, OnAccepted, ResumableClient, TextProviderClient, ToolProviderClient, TraceSink, VideoProviderClient,
};
use crate::constants;
use crate::data_uri::{from_data_uri, is_data_uri, to_data_uri};
use crate::error::{GenerationError, Result};
use crate::http::{self, Timeouts};
use crate::models::{AspectRatio, ImageModel, ImageResolution, ToolId};
use crate::replicate::capabilities::{self as caps, VideoEndpointVariant};
use crate::replicate::error::{ReplicateError, ReplicateResult};
use crate::references::{rewrite_handles, ReferenceAssets, ReferenceTagStyle};
use crate::replicate::models::{ReplicatePrediction, ReplicatePredictionStatus};
use crate::settings::{ToolSettings, VideoGenerationSettings};
use crate::transcode::transcode_to_png;
use crate::Bytes;
use majik_core::model::{MediaType, TraceLabel};

/// Default MIME type for image data URIs.
const IMAGE_DATA_URI_MIME: &str = "image/png";

/// Seconds Replicate should hold the submit request open before returning an in-progress prediction.
const PREFER_WAIT_SECONDS: u32 = 60;

/// Where to POST a Replicate prediction. Official models use the slug
/// endpoint that auto-uses the latest version; community models 404 there
/// and have to be pinned to a version on `/v1/predictions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmissionTarget {
    OfficialModel(String),
    Versioned(String),
}

#[derive(Clone)]
pub struct ReplicateClient {
    api_key: String,
    base_url: String,
    on_accepted: Option<OnAccepted>,
    pub(super) on_trace: Option<TraceSink>,
}

impl std::fmt::Debug for ReplicateClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicateClient").field("base_url", &self.base_url).finish()
    }
}

impl ReplicateClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, constants::replicate::BASE_URL)
    }

    pub fn from_options(options: &ClientOptions) -> Self {
        Self { on_accepted: options.on_accepted.clone(), on_trace: options.on_trace.clone(), ..Self::new(options.api_key.clone()) }
    }

    /// Report every accepted prediction to `on_accepted`.
    pub fn with_on_accepted(mut self, on_accepted: OnAccepted) -> Self {
        self.on_accepted = Some(on_accepted);
        self
    }

    /// Report every HTTP exchange to `on_trace`.
    pub fn with_on_trace(mut self, on_trace: TraceSink) -> Self {
        self.on_trace = Some(on_trace);
        self
    }

    /// Same as [`ReplicateClient::new`] but with a custom API root (e.g. a local mock server).
    /// `base_url` should include the `/v1` path segment, like [`constants::replicate::BASE_URL`].
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let mut base_url: String = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self { api_key: api_key.into(), base_url, on_accepted: None, on_trace: None }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn authorized(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("Token {}", self.api_key))
    }
}

// ----- ImageProviderClient ----------------------------------------------------------------------

#[async_trait]
impl ImageProviderClient for ReplicateClient {
    async fn generate_image(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Result<Bytes> {
        self.generate_image_impl(prompt, model, assets, aspect_ratio, resolution).await.map_err(Into::into)
    }

}

// ----- ToolProviderClient -----------------------------------------------------------------------

#[async_trait]
impl ToolProviderClient for ReplicateClient {
    async fn run_tool(&self, settings: &ToolSettings, input: &ProviderAsset) -> Result<Bytes> {
        self.run_tool_impl(settings, input).await.map_err(Into::into)
    }
}

// ----- VideoProviderClient ----------------------------------------------------------------------

#[async_trait]
impl VideoProviderClient for ReplicateClient {
    async fn generate_video(&self, prompt: &str, assets: &[ProviderAsset], settings: &VideoGenerationSettings) -> Result<Bytes> {
        self.generate_video_impl(prompt, assets, settings).await.map_err(Into::into)
    }
}

// ----- image generation impl --------------------------------------------------------------------

impl ReplicateClient {
    async fn generate_image_impl(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> ReplicateResult<Bytes> {
        let mut reference_images: Vec<&[u8]> = Vec::new();
        let mut mask: Option<&[u8]> = None;
        for asset in assets {
            match asset.role {
                AssetRole::ReferenceImage => reference_images.push(&asset.data),
                AssetRole::MaskImage => {
                    if caps::api_mask_param(model).is_none() {
                        return Err(ReplicateError::BadRequest("This model does not accept a mask input".into()));
                    }
                    if mask.is_some() {
                        return Err(ReplicateError::BadRequest("At most one mask is supported".into()));
                    }
                    mask = Some(&asset.data);
                }
                AssetRole::ReferenceVideo | AssetRole::FirstFrame | AssetRole::LastFrame | AssetRole::ControlImage | AssetRole::Audio => {
                    return Err(ReplicateError::BadRequest(format!(
                        "Role '{}' is not supported by Replicate image endpoints",
                        asset.role.raw()
                    )));
                }
            }
        }

        if mask.is_some() && reference_images.is_empty() {
            return Err(ReplicateError::BadRequest("A mask requires at least one reference image".into()));
        }

        let slug = if let (false, Some(edit)) = (reference_images.is_empty(), caps::edit_endpoint(model)) {
            edit
        } else if let Some(t2i) = caps::endpoint(model) {
            t2i
        } else {
            return Err(ReplicateError::UnsupportedModel(model.id.to_string()));
        };

        let body = build_request_body(prompt, model, &reference_images, mask, aspect_ratio, resolution);

        let prediction = self
            .submit_and_await_prediction(SubmissionTarget::OfficialModel(slug.to_string()), body, Timeouts::IMAGE, "image")
            .await?;

        self.extract_image_data(&prediction, Timeouts::IMAGE, caps::api_supports_output_format(model) == Some(true)).await
    }
}

// ----- image processing impl --------------------------------------------------------------------

impl ReplicateClient {
    async fn run_tool_impl(&self, settings: &ToolSettings, input: &ProviderAsset) -> ReplicateResult<Bytes> {
        let (version, body, label) = match settings.model.kind {
            ToolId::Upscale => (
                constants::replicate::UPSCALE_VERSION,
                build_upscale_request_body(&input.data, settings.upscale_factor),
                "upscale",
            ),
            ToolId::RemoveBackground => (constants::replicate::REMOVE_BACKGROUND_VERSION, build_remove_background_request_body(&input.data), "remove-bg"),
        };
        let prediction =
            self.submit_and_await_prediction(SubmissionTarget::Versioned(version.to_string()), body, Timeouts::IMAGE, label).await?;
        self.extract_image_data(&prediction, Timeouts::IMAGE, true).await
    }
}

// ----- video generation impl --------------------------------------------------------------------

impl ReplicateClient {
    async fn generate_video_impl(&self, prompt: &str, assets: &[ProviderAsset], settings: &VideoGenerationSettings) -> ReplicateResult<Bytes> {
        let model = &settings.model;
        let first_frame = assets.iter().find(|a| a.role == AssetRole::FirstFrame).map(|a| a.data.as_slice());
        let last_frame = assets.iter().find(|a| a.role == AssetRole::LastFrame).map(|a| a.data.as_slice());
        let audio_asset = assets.iter().find(|a| a.role == AssetRole::Audio);
        if let Some(asset) = assets.iter().find(|a| !a.role.is_reference() && !a.role.is_frame_input()) {
            return Err(ReplicateError::BadRequest(format!(
                "Role '{}' is not supported by Replicate video endpoints",
                asset.role.raw()
            )));
        }
        let references = ReferenceAssets::from_assets(assets);
        let takes_audio_input = caps::api_audio_input_param(model).is_some();

        // Audio counts as a reference only where the model has a reference audio list; on a model
        // with neither that nor a conditioning input, the answer is "no audio input".
        let audio_is_reference = !references.audio.is_empty() && !takes_audio_input && caps::video_reference_params(model).is_some_and(|p| p.audio.is_some());
        if references.has_visual() || audio_is_reference {
            Self::validate_references(&references, first_frame, last_frame, settings)?;
            let slug = caps::video_endpoint(model).ok_or_else(|| ReplicateError::UnsupportedModel(model.id.to_string()))?;
            let body = build_video_reference_body(prompt, &references, settings);
            let prediction = self
                .submit_and_await_prediction(SubmissionTarget::OfficialModel(slug.to_string()), body, Timeouts::video(settings.duration), "video")
                .await?;
            return self.download_video_output(&prediction).await;
        }

        if audio_asset.is_some() && !takes_audio_input {
            return Err(ReplicateError::BadRequest("This model does not accept an audio input".into()));
        }
        if last_frame.is_some() && first_frame.is_none() {
            return Err(ReplicateError::BadRequest("A last frame requires a first frame".into()));
        }

        let (slug, variant) = caps::resolve_video_endpoint(model, first_frame.is_some(), last_frame.is_some())?;

        let body = build_video_request_body(prompt, first_frame, last_frame, audio_asset, variant, settings);

        let prediction = self
            .submit_and_await_prediction(SubmissionTarget::OfficialModel(slug.to_string()), body, Timeouts::video(settings.duration), "video")
            .await?;
        self.download_video_output(&prediction).await
    }

    /// The video file behind a succeeded prediction.
    /// Everything Replicate would reject about a reference request, in a sentence the user can act
    /// on. `majik-generation`'s validation catches these first; this is the client's own check.
    fn validate_references(
        references: &ReferenceAssets<'_>,
        first_frame: Option<&[u8]>,
        last_frame: Option<&[u8]>,
        settings: &VideoGenerationSettings,
    ) -> ReplicateResult<()> {
        let model = &settings.model;
        let declared = caps::video_capabilities(model).and_then(|c| c.references);
        let (Some(params), Some(declared)) = (caps::video_reference_params(model), declared) else {
            return Err(ReplicateError::BadRequest(format!("{} does not take reference inputs", model.name)));
        };
        if first_frame.is_some() || last_frame.is_some() {
            return Err(ReplicateError::BadRequest("References and a start or end frame can't be used together".into()));
        }
        for (role, list) in references.lists() {
            if list.is_empty() {
                continue;
            }
            let max = declared.max_for(role);
            if params.param_for(role).is_none() || max == 0 {
                return Err(ReplicateError::BadRequest(format!("{} does not take {} references", model.name, role.display_name().to_lowercase())));
            }
            if list.len() > max {
                return Err(ReplicateError::BadRequest(format!("{} takes at most {max} {} references", model.name, role.display_name().to_lowercase())));
            }
        }
        if let Some(combined) = declared.combined_max {
            let total = references.counts().total();
            if total > combined {
                return Err(ReplicateError::BadRequest(format!("{} takes at most {combined} references in total (got {total})", model.name)));
            }
        }
        if !references.audio.is_empty() && !references.has_visual() {
            return Err(ReplicateError::BadRequest("An audio reference needs at least one image or video reference".into()));
        }
        Ok(())
    }

    pub(crate) async fn download_video_output(&self, prediction: &ReplicatePrediction) -> ReplicateResult<Bytes> {
        let url = prediction.output.as_ref().and_then(|o| o.first_url()).ok_or(ReplicateError::NoVideoGenerated)?;
        if url::Url::parse(url).is_err() {
            return Err(ReplicateError::VideoDownloadFailed("Invalid video URL".into()));
        }
        let video = match http::download_traced(url, Timeouts::VIDEO.request, self.on_trace.as_ref()).await {
            Ok(bytes) => bytes,
            Err(GenerationError::ServerError { status_code: Some(code), .. }) => {
                return Err(ReplicateError::VideoDownloadFailed(format!("HTTP {code}")));
            }
            Err(e) => return Err(ReplicateError::Transport(e)),
        };
        if video.is_empty() {
            return Err(ReplicateError::NoVideoGenerated);
        }
        Ok(video)
    }
}

// ----- request body builders --------------------------------------------------------------------

/// Port of `ReplicateProvider.buildRequestBody` (image generation input).
pub fn build_request_body(
    prompt: &str,
    model: &ImageModel,
    images: &[&[u8]],
    mask: Option<&[u8]>,
    aspect_ratio: Option<AspectRatio>,
    resolution: Option<ImageResolution>,
) -> Map<String, Value> {
    let mut input = Map::new();
    input.insert("prompt".into(), json!(prompt));

    if let Some((key, value)) = aspect_ratio.and_then(|ar| caps::api_image_size(model, ar)) {
        input.insert(key.into(), json!(value));
    }
    if let Some((key, value)) = resolution.and_then(|r| caps::api_image_resolution(model, r)) {
        input.insert(key.into(), json!(value));
    }
    if caps::api_supports_output_format(model) == Some(true) {
        input.insert("output_format".into(), json!("png"));
    }
    if !images.is_empty() {
        if let Some((key, is_array)) = caps::api_edit_image_param(model) {
            let data_uris: Vec<String> = images.iter().map(|bytes| to_data_uri(bytes, IMAGE_DATA_URI_MIME)).collect();
            let value = if is_array { json!(data_uris) } else { json!(data_uris[0]) };
            input.insert(key.into(), value);
        }
    }
    if let (Some(mask), Some(mask_key)) = (mask, caps::api_mask_param(model)) {
        input.insert(mask_key.into(), json!(to_data_uri(mask, IMAGE_DATA_URI_MIME)));
    }
    if let Some(safety) = caps::api_safety_override(model) {
        for (key, value) in safety {
            input.insert(key.into(), value);
        }
    }

    input
}

/// philz1337x/clarity-upscaler input schema. We pin the fields we care
/// about; everything else (creativity/resemblance/dynamic/seed/etc.)
/// uses the model's documented defaults.
pub fn build_upscale_request_body(image: &[u8], scale_factor: u32) -> Map<String, Value> {
    let mut input = Map::new();
    input.insert("image".into(), json!(to_data_uri(image, IMAGE_DATA_URI_MIME)));
    input.insert("scale_factor".into(), json!(scale_factor));
    input.insert("output_format".into(), json!("png"));
    input
}

/// 851-labs/background-remover input schema. `background_type: "rgba"`
/// gives a transparent background; `format: "png"` preserves the alpha.
pub fn build_remove_background_request_body(image: &[u8]) -> Map<String, Value> {
    let mut input = Map::new();
    input.insert("image".into(), json!(to_data_uri(image, IMAGE_DATA_URI_MIME)));
    input.insert("format".into(), json!("png"));
    input.insert("background_type".into(), json!("rgba"));
    input
}

/// Port of `ReplicateProvider.buildVideoRequestBody`.
/// The reference request's input: the same settings the reference-less body writes, the prompt with
/// its handles in this slug's dialect, and one array of data URIs per reference kind.
pub fn build_video_reference_body(prompt: &str, references: &ReferenceAssets<'_>, settings: &VideoGenerationSettings) -> Map<String, Value> {
    let params = caps::video_reference_params(&settings.model);
    let style = params.map(|p| p.style).unwrap_or(ReferenceTagStyle::Prose);
    let prompt = rewrite_handles(prompt, references.counts(), style);
    let mut input = build_video_request_body(&prompt, None, None, None, VideoEndpointVariant::Reference, settings);
    for (role, list) in references.lists() {
        let Some(key) = params.and_then(|p| p.param_for(role)) else { continue };
        if list.is_empty() {
            continue;
        }
        let urls: Vec<Value> = list.iter().map(|a| json!(to_data_uri(&a.data, &a.mime_type()))).collect();
        input.insert(key.into(), Value::Array(urls));
    }
    input
}

pub fn build_video_request_body(
    prompt: &str,
    first_frame: Option<&[u8]>,
    last_frame: Option<&[u8]>,
    audio_asset: Option<&ProviderAsset>,
    variant: VideoEndpointVariant,
    settings: &VideoGenerationSettings,
) -> Map<String, Value> {
    let model = &settings.model;
    let mut input = Map::new();
    input.insert("prompt".into(), json!(prompt));

    if let Some((key, value)) = settings.aspect_ratio.and_then(|ar| caps::api_video_aspect_ratio(model, ar, variant)) {
        input.insert(key.into(), json!(value));
    }
    if let Some((key, value)) = settings.resolution.and_then(|r| caps::api_video_resolution(model, r, variant)) {
        input.insert(key.into(), json!(value));
    }
    if let Some(duration) = caps::api_duration(model, settings.duration) {
        input.insert("duration".into(), duration);
    }
    if let (Some(first), Some(start_key)) = (first_frame, caps::api_start_frame_param(model, variant)) {
        input.insert(start_key.into(), json!(to_data_uri(first, IMAGE_DATA_URI_MIME)));
    }
    if let (Some(last), Some(end_key)) = (last_frame, caps::api_end_frame_param(model, variant)) {
        input.insert(end_key.into(), json!(to_data_uri(last, IMAGE_DATA_URI_MIME)));
    }
    if let (Some(audio), Some(audio_input_key)) = (audio_asset, caps::api_audio_input_param(model)) {
        input.insert(audio_input_key.into(), json!(to_data_uri(&audio.data, &audio.mime_type())));
    }
    if let Some(audio_key) = caps::api_audio_param(model) {
        input.insert(audio_key.into(), json!(settings.audio_enabled));
    }

    input
}

// ----- submit + poll ----------------------------------------------------------------------------

impl ReplicateClient {
    /// POSTs a prediction (with `Prefer: wait=60`) and polls until it reaches a terminal state or
    /// `timeouts.total` elapses. Returns the succeeded prediction.
    pub async fn submit_and_await_prediction(
        &self,
        target: SubmissionTarget,
        input: Map<String, Value>,
        timeouts: Timeouts,
        kind: &str,
    ) -> ReplicateResult<ReplicatePrediction> {
        let (prediction, poll_url) = self.submit_prediction(target, input, timeouts, kind).await?;
        self.await_prediction(prediction, &poll_url, timeouts).await
    }

    /// One text prediction for the prompt rewriter. The model does no thinking by default, so it
    /// takes none of the reasoning knobs, and it caps output with `max_tokens` (the OpenAI models'
    /// `max_completion_tokens` is not in its schema).
    async fn complete_text_impl(&self, system: &str, user: &str, max_tokens: usize) -> ReplicateResult<String> {
        let mut input = Map::new();
        input.insert("prompt".into(), Value::String(user.to_string()));
        input.insert("system_prompt".into(), Value::String(system.to_string()));
        input.insert("max_tokens".into(), Value::from(max_tokens));

        let prediction = self
            .submit_and_await_prediction(SubmissionTarget::OfficialModel(constants::replicate::TEXT_MODEL.to_string()), input, Timeouts::TEXT, "text")
            .await?;
        let text = prediction.output.as_ref().map(|o| o.text()).unwrap_or_default().trim().to_string();
        if text.is_empty() {
            tracing::error!("No text in the Replicate prediction output");
            return Err(ReplicateError::NoResultGenerated);
        }
        Ok(text)
    }

    /// The submit half: POST the prediction, report the accepted handle, return the prediction as
    /// the provider answered (possibly already succeeded under `Prefer: wait`) plus its poll URL.
    pub async fn submit_prediction(
        &self,
        target: SubmissionTarget,
        input: Map<String, Value>,
        timeouts: Timeouts,
        kind: &str,
    ) -> ReplicateResult<(ReplicatePrediction, String)> {
        let (submit_url, payload) = match target {
            SubmissionTarget::OfficialModel(slug) => (format!("{}/models/{slug}/predictions", self.base_url), json!({ "input": input })),
            SubmissionTarget::Versioned(version_id) => {
                (format!("{}/predictions", self.base_url), json!({ "version": version_id, "input": input }))
            }
        };
        if url::Url::parse(&submit_url).is_err() {
            return Err(ReplicateError::InvalidUrl(submit_url));
        }

        let request = self
            .authorized(http::client().post(&submit_url))
            .header("Content-Type", "application/json")
            .header("Prefer", format!("wait={PREFER_WAIT_SECONDS}"))
            .timeout(timeouts.request)
            .body(serde_json::to_vec(&payload).map_err(|e| ReplicateError::DecodingError(e.to_string()))?);
        let (submit_status, submit_data) = http::send_traced(request, TraceLabel::Submit, self.on_trace.as_ref()).await?;
        if !matches!(submit_status, 200..=202) {
            return Err(ReplicateError::from_http_status(submit_status, &submit_data));
        }

        let prediction: ReplicatePrediction = match serde_json::from_slice(&submit_data) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(body = %String::from_utf8_lossy(&submit_data), "Replicate submit decode failed");
                return Err(ReplicateError::DecodingError(e.to_string()));
            }
        };
        tracing::info!(kind, id = %prediction.id, "Replicate prediction submitted");

        // Poll at the `urls.get` href if provided; fall back to the canonical predictions endpoint.
        let poll_url = prediction
            .urls
            .as_ref()
            .and_then(|u| u.get.clone())
            .unwrap_or_else(|| format!("{}/predictions/{}", self.base_url, prediction.id));
        if url::Url::parse(&poll_url).is_err() {
            return Err(ReplicateError::InvalidUrl(poll_url));
        }
        if let Some(on_accepted) = &self.on_accepted {
            on_accepted(JobHandle { job_id: prediction.id.clone(), poll_url: Some(poll_url.clone()) });
        }
        Ok((prediction, poll_url))
    }

    /// The poll half: `prediction` as last seen; polls `poll_url` until it succeeds.
    pub async fn await_prediction(&self, mut prediction: ReplicatePrediction, poll_url: &str, timeouts: Timeouts) -> ReplicateResult<ReplicatePrediction> {
        if prediction.status == ReplicatePredictionStatus::Succeeded {
            return Ok(prediction);
        }
        ensure_not_terminal_failure(&prediction)?;

        let start = Instant::now();
        loop {
            let elapsed = start.elapsed();
            if elapsed > timeouts.total {
                return Err(ReplicateError::PredictionTimeout);
            }
            http::sleep(http::poll_interval(elapsed)).await;

            prediction = self.fetch_prediction(poll_url, timeouts).await?;
            if prediction.status == ReplicatePredictionStatus::Succeeded {
                return Ok(prediction);
            }
            ensure_not_terminal_failure(&prediction)?;
        }
    }

    /// One GET of a prediction's current state.
    async fn fetch_prediction(&self, poll_url: &str, timeouts: Timeouts) -> ReplicateResult<ReplicatePrediction> {
        let request = self.authorized(http::client().get(poll_url)).timeout(timeouts.request);
        let (poll_status, poll_data) = http::send_traced(request, TraceLabel::Poll, self.on_trace.as_ref()).await?;
        if poll_status != 200 {
            return Err(ReplicateError::from_http_status(poll_status, &poll_data));
        }
        match serde_json::from_slice(&poll_data) {
            Ok(p) => Ok(p),
            Err(e) => {
                tracing::error!(body = %String::from_utf8_lossy(&poll_data), "Replicate poll decode failed");
                Err(ReplicateError::DecodingError(e.to_string()))
            }
        }
    }

    /// Re-attach to a stored prediction handle: read its state now (no initial wait), keep polling
    /// if needed, then extract the output for `media_type`.
    async fn resume_impl(&self, handle: &JobHandle, media_type: MediaType) -> ReplicateResult<Bytes> {
        let poll_url = handle.poll_url.clone().unwrap_or_else(|| format!("{}/predictions/{}", self.base_url, handle.job_id));
        if url::Url::parse(&poll_url).is_err() {
            return Err(ReplicateError::InvalidUrl(poll_url));
        }
        let timeouts = match media_type {
            MediaType::Image => Timeouts::IMAGE,
            MediaType::Video => Timeouts::video_resume(),
            MediaType::Audio => Timeouts::AUDIO,
        };
        tracing::info!(id = %handle.job_id, "Replicate prediction resumed");
        let gone = |e: ReplicateError| match e {
            ReplicateError::HttpError { status_code: 404, .. } => ReplicateError::JobGone,
            other => other,
        };
        let prediction = self.fetch_prediction(&poll_url, timeouts).await.map_err(gone)?;
        let prediction = self.await_prediction(prediction, &poll_url, timeouts).await.map_err(gone)?;
        match media_type {
            MediaType::Image => self.extract_image_data(&prediction, timeouts, false).await,
            MediaType::Video => self.download_video_output(&prediction).await,
            MediaType::Audio => self.download_audio_output(&prediction).await,
        }
    }
}

#[async_trait]
impl TextProviderClient for ReplicateClient {
    async fn complete_text(&self, system: &str, user: &str, max_tokens: usize) -> Result<String> {
        Ok(self.complete_text_impl(system, user, max_tokens).await?)
    }
}

#[async_trait]
impl ResumableClient for ReplicateClient {
    async fn resume(&self, handle: &JobHandle, media_type: MediaType) -> Result<Bytes> {
        self.resume_impl(handle, media_type).await.map_err(Into::into)
    }
}

fn ensure_not_terminal_failure(prediction: &ReplicatePrediction) -> ReplicateResult<()> {
    match prediction.status {
        ReplicatePredictionStatus::Starting | ReplicatePredictionStatus::Processing | ReplicatePredictionStatus::Succeeded => Ok(()),
        ReplicatePredictionStatus::Failed => {
            let msg = prediction.error.clone().unwrap_or_else(|| "Generation failed".to_string());
            if ReplicateError::looks_like_content_policy(&msg) {
                Err(ReplicateError::ContentFiltered(msg))
            } else {
                Err(ReplicateError::PredictionFailed(msg))
            }
        }
        ReplicatePredictionStatus::Canceled => Err(ReplicateError::PredictionCanceled),
    }
}

// ----- output extraction ------------------------------------------------------------------------

impl ReplicateClient {
    /// Port of `ReplicateProvider.extractImageData`: downloads (or decodes) the first output URL
    /// and transcodes to PNG unless PNG was requested natively.
    pub async fn extract_image_data(&self, prediction: &ReplicatePrediction, timeouts: Timeouts, prefer_png: bool) -> ReplicateResult<Bytes> {
        let url = prediction.output.as_ref().and_then(|o| o.first_url()).ok_or(ReplicateError::NoImageGenerated)?;

        let raw_bytes = if is_data_uri(url) {
            from_data_uri(url).ok_or(ReplicateError::InvalidImageData)?
        } else {
            if url::Url::parse(url).is_err() {
                return Err(ReplicateError::InvalidUrl(url.to_string()));
            }
            let data = match http::download_traced(url, timeouts.request, self.on_trace.as_ref()).await {
                Ok(bytes) => bytes,
                Err(GenerationError::ServerError { .. }) => return Err(ReplicateError::InvalidImageData),
                Err(e) => return Err(ReplicateError::Transport(e)),
            };
            if data.is_empty() {
                return Err(ReplicateError::NoImageGenerated);
            }
            data
        };

        if prefer_png {
            // We requested output_format=png so the bytes are already PNG.
            return Ok(raw_bytes);
        }
        // The image decoder failed to recognize the bytes; report it as invalid data, as fal does.
        transcode_to_png(&raw_bytes).ok_or(ReplicateError::InvalidImageData)
    }
}
