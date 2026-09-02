//! fal's error type and the HTTP-status / message mapping behind it.

use thiserror::Error;

use crate::error::GenerationError;
use crate::fal::models::FalErrorDetail;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum FalError {
    #[error("Invalid URL: '{0}' is not a valid URL format")]
    InvalidUrl(String),
    #[error("Invalid response: expected HTTPURLResponse but received a different response type")]
    InvalidResponse,
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Authentication failed: {0}")]
    Unauthorized(String),
    #[error("Insufficient credits: {0}")]
    PaymentRequired(String),
    #[error("Rate limited: {0}")]
    RateLimited(String),
    #[error("Server error ({status_code}): {message}")]
    ServerError { status_code: u16, message: String },
    #[error("HTTP {status_code} {}: {message}", status_name(*status_code))]
    HttpError { status_code: u16, message: String },
    #[error("Content was filtered: {0}")]
    ContentFiltered(String),
    #[error("No image generated. The model may be warming up. Please try again.")]
    NoImageGenerated,
    #[error("Invalid image data received from the server")]
    InvalidImageData,
    #[error("Failed to decode response: {0}")]
    DecodingError(String),
    #[error("Model '{0}' is not supported by fal.ai")]
    UnsupportedModel(String),
    #[error("No video generated. Please try again.")]
    NoVideoGenerated,
    #[error("No audio generated. The model may be warming up. Please try again.")]
    NoResultGenerated,
    #[error("Failed to download video: {0}")]
    VideoDownloadFailed(String),
    #[error("Failed to download audio: {0}")]
    AudioDownloadFailed(String),
    #[error("Failed to upload the input: {0}")]
    UploadFailed(String),
    #[error("Video generation timed out. Please try again.")]
    QueueTimeout,
    #[error("Video generation failed: {0}")]
    QueueFailed(String),
    /// A resumed request the queue no longer knows.
    #[error("The provider no longer has this generation. Please try again.")]
    JobGone,
    /// Transport-level failure, already mapped by `From<reqwest::Error> for GenerationError`.
    #[error("{0}")]
    Transport(GenerationError),
}

fn status_name(code: u16) -> String {
    reqwest::StatusCode::from_u16(code).ok().and_then(|s| s.canonical_reason()).unwrap_or("Unknown").to_string()
}

impl FalError {
    /// Port of `FalError.asGenerationError()`.
    pub fn into_generation_error(self) -> GenerationError {
        match self {
            FalError::Unauthorized(msg) => GenerationError::Unauthorized(msg),
            FalError::PaymentRequired(msg) => GenerationError::PaymentRequired(msg),
            FalError::RateLimited(msg) => GenerationError::RateLimited(msg),
            FalError::ContentFiltered(msg) => GenerationError::ContentFiltered(msg),
            FalError::ServerError { status_code, message } => GenerationError::server(Some(status_code), message),
            FalError::QueueFailed(msg) => GenerationError::ProviderFailed(msg),
            FalError::QueueTimeout => GenerationError::Timeout,
            FalError::JobGone => GenerationError::JobGone,
            FalError::NoImageGenerated | FalError::NoVideoGenerated | FalError::NoResultGenerated => GenerationError::NoResultGenerated,
            FalError::BadRequest(msg) => GenerationError::InvalidRequest(msg),
            FalError::UnsupportedModel(name) => GenerationError::InvalidRequest(format!("Model '{name}' is not supported by fal.ai")),
            FalError::InvalidUrl(url) => GenerationError::InvalidRequest(format!("Invalid URL: {url}")),
            FalError::InvalidResponse => GenerationError::Unknown("Invalid response type".into()),
            FalError::InvalidImageData => GenerationError::Unknown("Invalid image data received from the server".into()),
            FalError::DecodingError(reason) => GenerationError::Unknown(format!("Failed to decode response: {reason}")),
            FalError::VideoDownloadFailed(reason) => GenerationError::Unknown(format!("Failed to download video: {reason}")),
            FalError::AudioDownloadFailed(reason) => GenerationError::Unknown(format!("Failed to download audio: {reason}")),
            FalError::UploadFailed(reason) => GenerationError::Unknown(format!("Failed to upload the input: {reason}")),
            FalError::HttpError { status_code, message } => GenerationError::Unknown(format!("HTTP {status_code}: {message}")),
            FalError::Transport(err) => err,
        }
    }
}

impl From<FalError> for GenerationError {
    fn from(e: FalError) -> Self {
        e.into_generation_error()
    }
}

impl From<reqwest::Error> for FalError {
    fn from(e: reqwest::Error) -> Self {
        FalError::Transport(GenerationError::from(e))
    }
}

/// Port of `FalProvider.handleHTTPError`: maps a non-success HTTP response to a `FalError`.
/// `content_policy_violation` items win over the status code.
pub fn handle_http_error(status_code: u16, body: &[u8]) -> FalError {
    let raw_body = match std::str::from_utf8(body) {
        Ok(s) => s.to_string(),
        Err(_) => format!("<non-utf8 body, {} bytes>", body.len()),
    };
    tracing::error!(status_code, body = %raw_body, "fal.ai HTTP error");

    let error_response = serde_json::from_slice::<FalErrorDetail>(body).ok();
    let message = error_response.as_ref().and_then(|e| e.detail.clone()).unwrap_or_else(|| "Unknown error".to_string());

    if let Some(item) =
        error_response.as_ref().and_then(|e| e.items.iter().find(|i| i.r#type.as_deref() == Some("content_policy_violation")))
    {
        return FalError::ContentFiltered(build_message(item.msg.as_deref(), item.url.as_deref()));
    }

    match status_code {
        400 | 422 => {
            let url = error_response.as_ref().and_then(|e| e.items.first()).and_then(|i| i.url.as_deref());
            FalError::BadRequest(build_message(Some(&message), url))
        }
        401 | 403 => FalError::Unauthorized(message),
        402 => FalError::PaymentRequired(message),
        429 => FalError::RateLimited(message),
        500..=599 => FalError::ServerError { status_code, message },
        _ => FalError::HttpError { status_code, message },
    }
}

/// Port of `FalProvider.buildMessage`.
pub fn build_message(message: Option<&str>, url: Option<&str>) -> String {
    let msg = message.unwrap_or("Unknown error");
    match url {
        Some(url) => format!("{msg} ({url})"),
        None => msg.to_string(),
    }
}
