//! The chat/completions image client.

use async_trait::async_trait;

use super::capabilities::model_slug;
use super::error::OpenRouterError;
use super::models::{ContentPart, ImageConfig, Message, Request, Response};
use majik_core::model::TraceLabel;

use crate::asset::{AssetRole, ProviderAsset};
use crate::client::{ClientOptions, ImageProviderClient, TextProviderClient, TraceSink};
use crate::constants::openrouter as constants;
use crate::data_uri::{from_data_uri, to_data_uri};
use crate::error::Result;
use crate::http::{self, Timeouts};
use crate::models::{AspectRatio, ImageModel, ImageResolution};
use crate::Bytes;

/// OpenRouter's HTTP client. Cheap to clone; shares the process-wide `reqwest::Client`.
#[derive(Clone)]
pub struct OpenRouterClient {
    api_key: String,
    /// Full `chat/completions` endpoint URL.
    endpoint: String,
    on_trace: Option<TraceSink>,
}

impl std::fmt::Debug for OpenRouterClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterClient").field("endpoint", &self.endpoint).finish()
    }
}

impl OpenRouterClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_endpoint(api_key, constants::BASE_URL)
    }

    pub fn from_options(options: &ClientOptions) -> Self {
        Self { on_trace: options.on_trace.clone(), ..Self::new(options.api_key.clone()) }
    }

    /// Point the client at a different `chat/completions` URL (tests).
    pub fn with_endpoint(api_key: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), endpoint: endpoint.into(), on_trace: None }
    }

    /// Report every HTTP exchange to `on_trace`.
    pub fn with_on_trace(mut self, on_trace: TraceSink) -> Self {
        self.on_trace = Some(on_trace);
        self
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Resolve the slug, validate asset roles, send.
    async fn generate_image_impl(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Result<Bytes> {
        let Some(slug) = model_slug(model) else {
            return Err(OpenRouterError::BadRequest(format!("Model '{}' is not supported by OpenRouter", model.id)).into());
        };
        if let Some(asset) = assets.iter().find(|a| a.role != AssetRole::ReferenceImage) {
            return Err(OpenRouterError::BadRequest(format!("Role '{}' is not supported by OpenRouter", asset.role.raw())).into());
        }
        let images: Vec<&[u8]> = assets.iter().map(|a| a.data.as_slice()).collect();
        self.generate_with_slug(prompt, slug, &images, aspect_ratio.map(AspectRatio::raw), resolution.map(ImageResolution::raw)).await
    }

    /// Generate against an explicit OpenRouter slug. Transport failures map through
    /// `GenerationError::from(reqwest::Error)`.
    pub async fn generate_with_slug(
        &self,
        prompt: &str,
        model_slug: &str,
        images: &[&[u8]],
        aspect_ratio: Option<&str>,
        image_size: Option<&str>,
    ) -> Result<Bytes> {
        let body = build_request(prompt, model_slug, images, aspect_ratio, image_size);

        let request = http::client()
            .post(&self.endpoint)
            .timeout(Timeouts::IMAGE.request)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", constants::HTTP_REFERER)
            .header("X-Title", constants::TITLE)
            .json(&body);
        // One synchronous POST: the submit is the whole exchange.
        let (status, data) = http::send_traced(request, TraceLabel::Submit, self.on_trace.as_ref()).await?;

        if status != 200 {
            tracing::error!(status, body = %String::from_utf8_lossy(&data), "OpenRouter HTTP error");
            return Err(OpenRouterError::from_http(status, &data).into());
        }

        let response = parse_response(&data)?;
        check_for_embedded_errors(&response)?;
        Ok(extract_image_data(&response)?)
    }
}

/// The JSON body; headers are added by the caller.
pub fn build_request(prompt: &str, model_slug: &str, images: &[&[u8]], aspect_ratio: Option<&str>, image_size: Option<&str>) -> Request {
    let mut content: Vec<ContentPart> = images.iter().map(|data| ContentPart::image_url(to_data_uri(data, "image/png"))).collect();
    if !prompt.is_empty() {
        content.push(ContentPart::text(prompt));
    }
    let image_config = if aspect_ratio.is_some() || image_size.is_some() {
        Some(ImageConfig { aspect_ratio: aspect_ratio.map(str::to_string), image_size: image_size.map(str::to_string) })
    } else {
        None
    };
    Request {
        model: model_slug.to_string(),
        messages: vec![Message { role: "user".into(), content }],
        modalities: vec!["image".into()],
        image_config,
        max_tokens: None,
    }
}

/// The body of a prompt-rewriting completion: the instruction as the system turn, the user's
/// prompt as theirs, and no modalities (text is the default).
pub fn build_text_request(system: &str, user: &str, max_tokens: usize) -> Request {
    Request {
        model: constants::TEXT_MODEL.to_string(),
        messages: vec![
            Message { role: "system".into(), content: vec![ContentPart::text(system)] },
            Message { role: "user".into(), content: vec![ContentPart::text(user)] },
        ],
        modalities: Vec::new(),
        image_config: None,
        max_tokens: Some(max_tokens),
    }
}

/// Decode a chat/completions response body.
pub fn parse_response(data: &[u8]) -> std::result::Result<Response, OpenRouterError> {
    serde_json::from_slice(data).map_err(|e| {
        tracing::error!(error = %e, "Failed to decode OpenRouter response");
        OpenRouterError::DecodingError(e.to_string())
    })
}

/// Catches a 200 whose first choice carries an error.
pub fn check_for_embedded_errors(response: &Response) -> std::result::Result<(), OpenRouterError> {
    let Some(first) = response.choices.first() else { return Ok(()) };

    if let Some(err) = &first.error {
        tracing::error!(message = %err.message, "Error in OpenRouter choice");
        return Err(OpenRouterError::GenerationError { code: Some(err.code), message: err.message.clone(), metadata: err.metadata.clone().map(Box::new) });
    }

    match first.finish_reason.as_deref() {
        Some("error") => {
            tracing::error!("OpenRouter generation finished with error");
            Err(OpenRouterError::GenerationError { code: None, message: "Generation failed with error finish reason".into(), metadata: None })
        }
        Some("content_filter") => {
            tracing::error!("OpenRouter content filtered by provider");
            Err(OpenRouterError::ContentFiltered)
        }
        _ => Ok(()),
    }
}

/// The first choice's message text, which a completion must carry.
pub fn extract_text(response: &Response) -> std::result::Result<String, OpenRouterError> {
    let text = response.choices.first().and_then(|c| c.message.content.as_deref()).map(str::trim).unwrap_or_default();
    if text.is_empty() {
        tracing::error!("No text found in OpenRouter response");
        return Err(OpenRouterError::NoTextGenerated);
    }
    Ok(text.to_string())
}

/// The first image of the first choice, which must be a data URI.
pub fn extract_image_data(response: &Response) -> std::result::Result<Bytes, OpenRouterError> {
    let first_image = response.choices.first().and_then(|c| c.message.images.as_ref()).and_then(|imgs| imgs.first());
    let Some(image) = first_image else {
        tracing::error!("No image found in OpenRouter response");
        return Err(OpenRouterError::NoImageGenerated);
    };
    from_data_uri(&image.image_url.url).ok_or_else(|| {
        let preview: String = image.image_url.url.chars().take(200).collect();
        tracing::error!(url = %preview, "OpenRouter image_url is not a data URI");
        OpenRouterError::InvalidImageData
    })
}

#[async_trait]
impl TextProviderClient for OpenRouterClient {
    async fn complete_text(&self, system: &str, user: &str, max_tokens: usize) -> Result<String> {
        let body = build_text_request(system, user, max_tokens);
        let request = http::client()
            .post(&self.endpoint)
            .timeout(Timeouts::TEXT.request)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", constants::HTTP_REFERER)
            .header("X-Title", constants::TITLE)
            .json(&body);
        let (status, data) = http::send_traced(request, TraceLabel::Submit, self.on_trace.as_ref()).await?;
        if status != 200 {
            tracing::error!(status, body = %String::from_utf8_lossy(&data), "OpenRouter HTTP error");
            return Err(OpenRouterError::from_http(status, &data).into());
        }
        let response = parse_response(&data)?;
        check_for_embedded_errors(&response)?;
        Ok(extract_text(&response)?)
    }
}

#[async_trait]
impl ImageProviderClient for OpenRouterClient {
    async fn generate_image(
        &self,
        prompt: &str,
        model: &ImageModel,
        assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Result<Bytes> {
        self.generate_image_impl(prompt, model, assets, aspect_ratio, resolution).await
    }
}
