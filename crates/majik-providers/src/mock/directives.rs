//! Parses mock-provider directives out of a prompt string.
//!
//! Directives are whitespace-separated tokens beginning with `#`. Recognized directives are
//! consumed and interpreted; unrecognized `#tokens` are silently stripped from the clean prompt
//! (forward-compatible). The remaining words form the `clean_prompt`, which callers pass to the
//! renderers so visual output stays stable regardless of behavioral directives.

use crate::error::GenerationError;

#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    pub clean_prompt: String,
    /// Seconds. `0` when no (valid) `#delay:` directive was present.
    pub delay: f64,
    pub failure: Option<GenerationError>,
}

/// Parse `#delay:<secs>` / `#fail:<outcome>` directives out of `prompt`.
pub fn parse_directives(prompt: &str) -> Parsed {
    let mut delay: f64 = 0.0;
    let mut failure: Option<GenerationError> = None;
    let mut clean_tokens: Vec<&str> = Vec::new();

    for token in prompt.split_whitespace() {
        let Some(body) = token.strip_prefix('#') else {
            clean_tokens.push(token);
            continue;
        };

        let (name, value): (&str, Option<&str>) = match body.split_once(':') {
            Some((n, v)) => (n, Some(v)),
            None => (body, None),
        };

        match name {
            "delay" => {
                if let Some(seconds) = value.and_then(|v| v.parse::<f64>().ok()) {
                    delay = seconds;
                }
            }
            "fail" => failure = Some(failure_error(value)),
            _ => {} // unknown directive silently stripped
        }
    }

    Parsed { clean_prompt: clean_tokens.join(" "), delay, failure }
}

fn failure_error(outcome: Option<&str>) -> GenerationError {
    match outcome {
        Some("unauthorized") => GenerationError::Unauthorized("mock: unauthorized".into()),
        Some("rateLimited") => GenerationError::RateLimited("mock: rate limited".into()),
        Some("contentFiltered") => GenerationError::ContentFiltered("mock: content filtered".into()),
        Some("timeout") => GenerationError::Timeout,
        Some("noResult") => GenerationError::NoResultGenerated,
        Some("paymentRequired") => GenerationError::PaymentRequired("mock: payment required".into()),
        Some("serverError") => GenerationError::server(Some(500), "mock: server error"),
        Some("invalidRequest") => GenerationError::InvalidRequest("mock: invalid request".into()),
        _ => GenerationError::Unknown("mock: unknown failure".into()),
    }
}
