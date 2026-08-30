//! OpenRouter's error type and the HTTP-status classification behind it.

use thiserror::Error;

use super::models::{ErrorMetadata, ErrorResponse};
use crate::error::GenerationError;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum OpenRouterError {
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
    #[error("{}", forbidden_message(.moderation_reasons, .flagged_input))]
    Forbidden { moderation_reasons: Option<Vec<String>>, flagged_input: Option<String> },
    #[error("Request timed out: {0}")]
    RequestTimeout(String),
    #[error("Rate limited: {0}")]
    RateLimited(String),
    #[error("{}", bad_gateway_message(.provider_name, .raw_error))]
    BadGateway { provider_name: Option<String>, raw_error: Option<String> },
    #[error("No available provider: {0}")]
    ServiceUnavailable(String),
    #[error("{}", http_error_message(*.status_code, .message))]
    HttpError { status_code: u16, message: String },
    #[error("{}", generation_error_message(.code, .message))]
    GenerationError { code: Option<i64>, message: String, metadata: Option<Box<ErrorMetadata>> },
    #[error("Content was filtered by the provider's safety system")]
    ContentFiltered,
    #[error("Failed to decode response: {0}")]
    DecodingError(String),
    #[error("No image generated. The model may be warming up. Please try again.")]
    NoImageGenerated,
    #[error("No text generated. Please try again.")]
    NoTextGenerated,
    #[error("Invalid image data received from the server")]
    InvalidImageData,
}

fn forbidden_message(reasons: &Option<Vec<String>>, flagged_input: &Option<String>) -> String {
    let mut message = String::from("Content flagged by moderation");
    if let Some(reasons) = reasons.as_ref().filter(|r| !r.is_empty()) {
        message.push_str(": ");
        message.push_str(&reasons.join(", "));
    }
    if let Some(flagged) = flagged_input.as_ref().filter(|f| !f.is_empty()) {
        message.push_str(&format!(" (flagged: \"{flagged}\")"));
    }
    message
}

fn bad_gateway_message(provider_name: &Option<String>, raw_error: &Option<String>) -> String {
    let mut message = String::from("Model unavailable");
    if let Some(name) = provider_name {
        message.push_str(&format!(" ({name})"));
    }
    if let Some(raw) = raw_error {
        message.push_str(&format!(": {raw}"));
    }
    message
}

fn http_error_message(status_code: u16, message: &str) -> String {
    match reqwest::StatusCode::from_u16(status_code).ok().and_then(|s| s.canonical_reason()) {
        Some(reason) => format!("HTTP {status_code} {reason}: {message}"),
        None => format!("HTTP {status_code}: {message}"),
    }
}

fn generation_error_message(code: &Option<i64>, message: &str) -> String {
    match code {
        Some(code) => format!("Generation failed ({code}): {message}"),
        None => format!("Generation failed: {message}"),
    }
}

impl OpenRouterError {
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            OpenRouterError::RequestTimeout(_)
                | OpenRouterError::RateLimited(_)
                | OpenRouterError::BadGateway { .. }
                | OpenRouterError::ServiceUnavailable(_)
                | OpenRouterError::NoImageGenerated
        )
    }

    /// Port of `OpenRouterProvider.handleHTTPError`: classify a non-200 response by status and the
    /// (optional) `{"error": {...}}` body.
    pub fn from_http(status_code: u16, body: &[u8]) -> OpenRouterError {
        let parsed = serde_json::from_slice::<ErrorResponse>(body).ok();
        let message = parsed.as_ref().map(|r| r.error.message.clone()).unwrap_or_else(|| "Unknown error".to_string());
        let metadata = parsed.and_then(|r| r.error.metadata);

        match status_code {
            400 => OpenRouterError::BadRequest(message),
            401 => OpenRouterError::Unauthorized(message),
            402 => OpenRouterError::PaymentRequired(message),
            403 => OpenRouterError::Forbidden {
                moderation_reasons: metadata.as_ref().and_then(|m| m.reasons.clone()),
                flagged_input: metadata.as_ref().and_then(|m| m.flagged_input.clone()),
            },
            408 => OpenRouterError::RequestTimeout(message),
            429 => OpenRouterError::RateLimited(message),
            502 => OpenRouterError::BadGateway {
                provider_name: metadata.as_ref().and_then(|m| m.provider_name.clone()),
                raw_error: metadata.as_ref().and_then(|m| m.raw_string()),
            },
            503 => OpenRouterError::ServiceUnavailable(message),
            _ => OpenRouterError::HttpError { status_code, message },
        }
    }

    /// Port of `OpenRouterError.asGenerationError()`.
    pub fn into_generation_error(self) -> GenerationError {
        match self {
            OpenRouterError::Unauthorized(msg) => GenerationError::Unauthorized(msg),
            OpenRouterError::RateLimited(msg) => GenerationError::RateLimited(msg),
            OpenRouterError::ContentFiltered => GenerationError::ContentFiltered("Content filtered by provider's safety system".into()),
            OpenRouterError::Forbidden { moderation_reasons, .. } => {
                GenerationError::ContentFiltered(moderation_reasons.map(|r| r.join(", ")).unwrap_or_else(|| "Content flagged by moderation".into()))
            }
            OpenRouterError::PaymentRequired(msg) => GenerationError::PaymentRequired(msg),
            OpenRouterError::ServiceUnavailable(msg) => GenerationError::server(Some(503), msg),
            OpenRouterError::BadGateway { provider_name, raw_error } => {
                let msg = [provider_name, raw_error].into_iter().flatten().collect::<Vec<_>>().join(": ");
                GenerationError::server(Some(502), if msg.is_empty() { "Bad gateway".to_string() } else { msg })
            }
            OpenRouterError::RequestTimeout(_) => GenerationError::Timeout,
            OpenRouterError::NoImageGenerated | OpenRouterError::NoTextGenerated => GenerationError::NoResultGenerated,
            OpenRouterError::BadRequest(msg) => GenerationError::InvalidRequest(msg),
            OpenRouterError::GenerationError { code, message, .. } => GenerationError::server(code.and_then(|c| u16::try_from(c).ok()), message),
            OpenRouterError::InvalidUrl(url) => GenerationError::InvalidRequest(format!("Invalid URL: {url}")),
            OpenRouterError::InvalidResponse => GenerationError::Unknown("Invalid response type".into()),
            OpenRouterError::InvalidImageData => GenerationError::Unknown("Invalid image data received from the server".into()),
            OpenRouterError::DecodingError(reason) => GenerationError::Unknown(format!("Failed to decode response: {reason}")),
            OpenRouterError::HttpError { status_code, message } => GenerationError::Unknown(format!("HTTP {status_code}: {message}")),
        }
    }
}

impl From<OpenRouterError> for GenerationError {
    fn from(e: OpenRouterError) -> Self {
        e.into_generation_error()
    }
}
