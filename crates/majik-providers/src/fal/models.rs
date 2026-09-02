//! The wire types for fal's queue API and result payloads.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalImage {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalResponse {
    #[serde(default)]
    pub images: Option<Vec<FalImage>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalQueueSubmitResponse {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FalQueueStatus {
    InQueue,
    InProgress,
    Completed,
    Failed,
    Unknown(String),
}

impl FalQueueStatus {
    pub fn as_str(&self) -> &str {
        match self {
            FalQueueStatus::InQueue => "IN_QUEUE",
            FalQueueStatus::InProgress => "IN_PROGRESS",
            FalQueueStatus::Completed => "COMPLETED",
            FalQueueStatus::Failed => "FAILED",
            FalQueueStatus::Unknown(v) => v,
        }
    }

    pub fn from_raw(value: &str) -> Self {
        match value {
            "IN_QUEUE" => FalQueueStatus::InQueue,
            "IN_PROGRESS" => FalQueueStatus::InProgress,
            "COMPLETED" => FalQueueStatus::Completed,
            "FAILED" => FalQueueStatus::Failed,
            other => FalQueueStatus::Unknown(other.to_string()),
        }
    }
}

impl Serialize for FalQueueStatus {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FalQueueStatus {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        Ok(FalQueueStatus::from_raw(&value))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalQueueStatusResponse {
    pub status: FalQueueStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalVideoResponse {
    pub video: FalImage,
}

/// fal returns either `{"video": {...}}` or the queue-wrapped `{"response": {"video": {...}}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalQueuedVideoResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<FalImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<FalVideoResponse>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalSingleImageResponse {
    pub image: FalImage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalAudioFileResponse {
    pub audio: FalImage,
}

/// A single item in fal's structured `detail` array. fal returns mixed-type fields (`loc` is
/// `[String]`, `ctx` is an object, etc.) so only the string fields we care about are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FalErrorItem {
    pub msg: Option<String>,
    pub r#type: Option<String>,
    pub url: Option<String>,
}

/// fal's error body. `detail` is polymorphic: either a plain string or an array of validation
/// items. For the array form, `detail` becomes the `; `-joined item messages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FalErrorDetail {
    pub detail: Option<String>,
    pub items: Vec<FalErrorItem>,
}

impl<'de> Deserialize<'de> for FalErrorDetail {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let Value::Object(map) = value else {
            return Err(de::Error::custom("expected a JSON object"));
        };
        let detail = match map.get("detail") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(array)) => {
                // The array decodes as `[{ String: <string-ish> }]`; a non-object element fails the
                // whole array decode and falls through to `None`.
                let mut items = Vec::with_capacity(array.len());
                for element in array {
                    let Value::Object(dict) = element else {
                        return Ok(Self::default());
                    };
                    let string = |key: &str| dict.get(key).and_then(Value::as_str).map(str::to_string);
                    items.push(FalErrorItem { msg: string("msg"), r#type: string("type"), url: string("url") });
                }
                let detail = items.iter().filter_map(|i| i.msg.as_deref()).collect::<Vec<_>>().join("; ");
                return Ok(Self { detail: Some(detail), items });
            }
            _ => return Ok(Self::default()),
        };
        Ok(Self { detail: Some(detail), items: Vec::new() })
    }
}

/// `fal-ai/any-llm`'s result: the completion, plus the fields the endpoint sets when it could not
/// produce one.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FalTextResponse {
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub partial: bool,
}

/// `POST /storage/upload/initiate`: where to PUT the bytes, and the URL to reference them by.
#[derive(Debug, Deserialize)]
pub struct FalUploadInitiateResponse {
    pub file_url: String,
    pub upload_url: String,
}
