//! Prediction / error response shapes.

use serde::Deserialize;

/// Prediction lifecycle states reported by Replicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplicatePredictionStatus {
    Starting,
    Processing,
    Succeeded,
    Failed,
    Canceled,
}

/// Shape returned by `POST /v1/models/{owner}/{name}/predictions` and the
/// status-poll GET that follows. Replicate's API documents many fields;
/// we only decode the ones the client needs.
#[derive(Clone, Debug, Deserialize)]
pub struct ReplicatePrediction {
    pub id: String,
    pub status: ReplicatePredictionStatus,
    #[serde(default)]
    pub output: Option<ReplicateOutput>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub urls: Option<ReplicateUrls>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReplicateUrls {
    #[serde(default)]
    pub get: Option<String>,
    #[serde(default)]
    pub cancel: Option<String>,
}

/// Replicate's `output` field is loosely typed across models. Image models
/// return either a single URL string or an array of URL strings; video
/// models always return a single URL string. Decoding tries both shapes
/// in order; any other shape is a decode error.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ReplicateOutput {
    Url(String),
    Urls(Vec<String>),
}

impl ReplicateOutput {
    /// A text model streams its answer as chunks the API returns as an array; a single string is
    /// the whole answer. Either way the completion is the concatenation.
    pub fn text(&self) -> String {
        match self {
            ReplicateOutput::Url(s) => s.clone(),
            ReplicateOutput::Urls(chunks) => chunks.concat(),
        }
    }

    pub fn first_url(&self) -> Option<&str> {
        match self {
            ReplicateOutput::Url(s) => Some(s.as_str()),
            ReplicateOutput::Urls(arr) => arr.first().map(String::as_str),
        }
    }
}

/// Replicate's standard error envelope on 4xx/5xx responses.
#[derive(Clone, Debug, Deserialize)]
pub struct ReplicateErrorResponse {
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_single_and_array_outputs() {
        let single: ReplicatePrediction = serde_json::from_str(r#"{"id":"a","status":"succeeded","output":"https://x/1.png"}"#).unwrap();
        assert_eq!(single.output.as_ref().and_then(|o| o.first_url()), Some("https://x/1.png"));

        let arr: ReplicatePrediction =
            serde_json::from_str(r#"{"id":"a","status":"succeeded","output":["https://x/1.png","https://x/2.png"]}"#).unwrap();
        assert_eq!(arr.output.as_ref().and_then(|o| o.first_url()), Some("https://x/1.png"));

        let none: ReplicatePrediction = serde_json::from_str(r#"{"id":"a","status":"processing","output":null}"#).unwrap();
        assert!(none.output.is_none());
        assert_eq!(none.status, ReplicatePredictionStatus::Processing);
    }

    #[test]
    fn rejects_unexpected_output_shape() {
        assert!(serde_json::from_str::<ReplicatePrediction>(r#"{"id":"a","status":"succeeded","output":{"k":1}}"#).is_err());
    }
}
