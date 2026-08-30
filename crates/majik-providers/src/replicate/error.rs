//! Replicate's error type plus the HTTP-status and content-policy mapping helpers.

use thiserror::Error;

use crate::error::GenerationError;
use crate::replicate::models::ReplicateErrorResponse;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ReplicateError {
    #[error("Invalid URL: '{0}' is not a valid URL format")]
    InvalidUrl(String),
    #[error("Invalid response: expected HTTPURLResponse but received a different response type")]
    InvalidResponse,
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Authentication failed: {0}")]
    Unauthorized(String),
    #[error("Insufficient credits on Replicate: {0}")]
    PaymentRequired(String),
    #[error("Rate limited: {0}")]
    RateLimited(String),
    #[error("Server error ({status_code}): {message}")]
    ServerError { status_code: u16, message: String },
    #[error("HTTP {status_code}: {message}")]
    HttpError { status_code: u16, message: String },
    #[error("Content was filtered: {0}")]
    ContentFiltered(String),
    #[error("No image generated. The model may be warming up. Please try again.")]
    NoImageGenerated,
    #[error("Invalid image data received from the server")]
    InvalidImageData,
    #[error("Failed to decode response: {0}")]
    DecodingError(String),
    #[error("Model '{0}' is not supported by Replicate")]
    UnsupportedModel(String),
    #[error("No video generated. Please try again.")]
    NoVideoGenerated,
    #[error("Failed to download video: {0}")]
    VideoDownloadFailed(String),
    #[error("No audio generated. The model may be warming up. Please try again.")]
    NoResultGenerated,
    #[error("Failed to download audio: {0}")]
    AudioDownloadFailed(String),
    #[error("Generation timed out. Please try again.")]
    PredictionTimeout,
    #[error("Generation failed: {0}")]
    PredictionFailed(String),
    #[error("Generation was canceled.")]
    PredictionCanceled,
    /// A resumed prediction Replicate no longer knows.
    #[error("The provider no longer has this generation. Please try again.")]
    JobGone,
    /// Transport-level failure, already mapped to a `GenerationError`.
    #[error("{0}")]
    Transport(GenerationError),
}

impl ReplicateError {
    /// Port of `ReplicateError.asGenerationError()`.
    pub fn into_generation_error(self) -> GenerationError {
        match self {
            ReplicateError::Unauthorized(msg) => GenerationError::Unauthorized(msg),
            ReplicateError::PaymentRequired(msg) => GenerationError::PaymentRequired(msg),
            ReplicateError::RateLimited(msg) => GenerationError::RateLimited(msg),
            ReplicateError::ContentFiltered(msg) => GenerationError::ContentFiltered(msg),
            ReplicateError::ServerError { status_code, message } => GenerationError::server(Some(status_code), message),
            ReplicateError::PredictionFailed(msg) => GenerationError::ProviderFailed(msg),
            ReplicateError::PredictionTimeout => GenerationError::Timeout,
            ReplicateError::PredictionCanceled => GenerationError::Unknown("Prediction canceled".into()),
            ReplicateError::JobGone => GenerationError::JobGone,
            ReplicateError::NoImageGenerated | ReplicateError::NoVideoGenerated | ReplicateError::NoResultGenerated => {
                GenerationError::NoResultGenerated
            }
            ReplicateError::BadRequest(msg) => GenerationError::InvalidRequest(msg),
            ReplicateError::UnsupportedModel(name) => GenerationError::InvalidRequest(format!("Model '{name}' is not supported by Replicate")),
            ReplicateError::InvalidUrl(url) => GenerationError::InvalidRequest(format!("Invalid URL: {url}")),
            ReplicateError::InvalidResponse => GenerationError::Unknown("Invalid response type".into()),
            ReplicateError::InvalidImageData => GenerationError::Unknown("Invalid image data received from the server".into()),
            ReplicateError::DecodingError(reason) => GenerationError::Unknown(format!("Failed to decode response: {reason}")),
            ReplicateError::VideoDownloadFailed(reason) => GenerationError::Unknown(format!("Failed to download video: {reason}")),
            ReplicateError::AudioDownloadFailed(reason) => GenerationError::Unknown(format!("Failed to download audio: {reason}")),
            ReplicateError::HttpError { status_code, message } => GenerationError::Unknown(format!("HTTP {status_code}: {message}")),
            ReplicateError::Transport(e) => e,
        }
    }

    /// Builds the error for a non-success HTTP status. 403 maps alongside 401 to `Unauthorized`,
    /// per the shared cross-provider contract.
    pub fn from_http_status(status_code: u16, body: &[u8]) -> Self {
        let raw_body = String::from_utf8_lossy(body);
        tracing::error!(status = status_code, body = %raw_body, "Replicate HTTP error");
        let parsed: Option<ReplicateErrorResponse> = serde_json::from_slice(body).ok();
        let message = parsed
            .as_ref()
            .and_then(|p| p.detail.clone().or_else(|| p.title.clone()))
            .unwrap_or_else(|| "Unknown error".to_string());

        match status_code {
            400 | 422 => ReplicateError::BadRequest(message),
            401 | 403 => ReplicateError::Unauthorized(message),
            402 => ReplicateError::PaymentRequired(message),
            429 => ReplicateError::RateLimited(message),
            500..=599 => ReplicateError::ServerError { status_code, message },
            _ => ReplicateError::HttpError { status_code, message },
        }
    }

    /// Port of `ReplicateProvider.looksLikeContentPolicy`.
    pub fn looks_like_content_policy(message: &str) -> bool {
        let lower = message.to_lowercase();
        lower.contains("nsfw") || lower.contains("safety") || lower.contains("content policy") || lower.contains("flagged")
    }
}

impl From<reqwest::Error> for ReplicateError {
    fn from(e: reqwest::Error) -> Self {
        ReplicateError::Transport(GenerationError::from(e))
    }
}

impl From<ReplicateError> for GenerationError {
    fn from(e: ReplicateError) -> Self {
        e.into_generation_error()
    }
}

pub type ReplicateResult<T> = std::result::Result<T, ReplicateError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prediction_failed_maps_to_provider_failed() {
        assert_eq!(
            ReplicateError::PredictionFailed("model crashed".into()).into_generation_error(),
            GenerationError::ProviderFailed("model crashed".into())
        );
    }

    #[test]
    fn http_status_mapping() {
        let body = br#"{"detail":"nope"}"#;
        assert_eq!(ReplicateError::from_http_status(401, body), ReplicateError::Unauthorized("nope".into()));
        assert_eq!(ReplicateError::from_http_status(403, body), ReplicateError::Unauthorized("nope".into()));
        assert_eq!(ReplicateError::from_http_status(402, body), ReplicateError::PaymentRequired("nope".into()));
        assert_eq!(ReplicateError::from_http_status(429, body), ReplicateError::RateLimited("nope".into()));
        assert_eq!(ReplicateError::from_http_status(422, body), ReplicateError::BadRequest("nope".into()));
        assert_eq!(ReplicateError::from_http_status(503, body), ReplicateError::ServerError { status_code: 503, message: "nope".into() });
        assert_eq!(ReplicateError::from_http_status(418, body), ReplicateError::HttpError { status_code: 418, message: "nope".into() });
        assert_eq!(ReplicateError::from_http_status(500, br#"{"title":"boom"}"#), ReplicateError::ServerError { status_code: 500, message: "boom".into() });
        assert_eq!(ReplicateError::from_http_status(500, b"garbage"), ReplicateError::ServerError { status_code: 500, message: "Unknown error".into() });
    }

    #[test]
    fn content_policy_heuristic() {
        assert!(ReplicateError::looks_like_content_policy("NSFW content detected"));
        assert!(ReplicateError::looks_like_content_policy("Blocked by safety filter"));
        assert!(ReplicateError::looks_like_content_policy("violates content policy"));
        assert!(ReplicateError::looks_like_content_policy("Prompt was flagged"));
        assert!(!ReplicateError::looks_like_content_policy("CUDA out of memory"));
    }
}
