//! The cross-provider [`GenerationError`], and the transport errors that map into it.

use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum GenerationError {
    #[error("Authentication failed: {0}")]
    Unauthorized(String),
    #[error("Rate limited: {0}")]
    RateLimited(String),
    #[error("Content was filtered: {0}")]
    ContentFiltered(String),
    #[error("{}", server_error_message(.status_code, .message))]
    ServerError { status_code: Option<u16>, message: String },
    #[error("Request timed out. Please try again.")]
    Timeout,
    #[error("No result generated. The model may be warming up. Please try again.")]
    NoResultGenerated,
    #[error("Provider generation failed: {0}")]
    ProviderFailed(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("Insufficient credits: {0}")]
    PaymentRequired(String),
    /// A resumed job the provider no longer knows (expired, or never reached it).
    #[error("The provider no longer has this generation. Please try again.")]
    JobGone,
    #[error("Unexpected error: {0}")]
    Unknown(String),
}

fn server_error_message(status: &Option<u16>, message: &str) -> String {
    match status {
        Some(code) => format!("Server error ({code}): {message}"),
        None => format!("Server error: {message}"),
    }
}

impl GenerationError {
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            GenerationError::RateLimited(_) | GenerationError::ServerError { .. } | GenerationError::Timeout | GenerationError::NoResultGenerated
        )
    }

    /// Stable machine-readable kind (persisted with failed rows).
    pub fn kind(&self) -> &'static str {
        match self {
            GenerationError::Unauthorized(_) => "unauthorized",
            GenerationError::RateLimited(_) => "rate_limited",
            GenerationError::ContentFiltered(_) => "content_filtered",
            GenerationError::ServerError { .. } => "server_error",
            GenerationError::Timeout => "timeout",
            GenerationError::NoResultGenerated => "no_result",
            GenerationError::ProviderFailed(_) => "provider_failed",
            GenerationError::InvalidRequest(_) => "invalid_request",
            GenerationError::PaymentRequired(_) => "payment_required",
            GenerationError::JobGone => "job_gone",
            GenerationError::Unknown(_) => "unknown",
        }
    }

    pub fn server(status_code: Option<u16>, message: impl Into<String>) -> Self {
        GenerationError::ServerError { status_code, message: message.into() }
    }
}

impl From<reqwest::Error> for GenerationError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            GenerationError::Timeout
        } else {
            GenerationError::Unknown(error_chain(&e))
        }
    }
}

/// `reqwest::Error`'s `Display` is just "error sending request for url (…)"; the cause (connection
/// reset, refused, TLS…) is in the source chain, which is what anyone reading the failure needs.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl From<serde_json::Error> for GenerationError {
    fn from(e: serde_json::Error) -> Self {
        GenerationError::Unknown(format!("Invalid JSON: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, GenerationError>;
