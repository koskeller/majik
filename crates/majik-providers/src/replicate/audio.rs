//! Replicate audio: ElevenLabs v3 monologue via Replicate's `elevenlabs/v3` slug.

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::client::AudioProviderClient;
use crate::error::Result;
use crate::http::{self, Timeouts};
use crate::models::{AudioModel, AudioModelCapabilities, AudioVoice};
use crate::replicate::error::{ReplicateError, ReplicateResult};
use crate::replicate::models::ReplicatePrediction;
use crate::replicate::provider::{ReplicateClient, SubmissionTarget};
use crate::settings::AudioGenerationSettings;
use crate::Bytes;

pub const ELEVEN_LABS_V3_ID: &str = "elevenlabs-v3";
pub const ELEVEN_LABS_V3_SLUG: &str = "elevenlabs/v3";

/// Catalog ids of the audio models Replicate supports, in display order.
pub const SUPPORTED_AUDIO_MODEL_IDS: &[&str] = &[ELEVEN_LABS_V3_ID];

pub fn supported_audio_models() -> Vec<AudioModel> {
    SUPPORTED_AUDIO_MODEL_IDS
        .iter()
        .map(|id| crate::catalog::audio::model(id).unwrap_or_else(|| panic!("audio model '{id}' missing from catalog")).clone())
        .collect()
}

pub fn eleven_labs_v3_voices() -> &'static [AudioVoice] {
    crate::voices::elevenlabs::replicate_voices()
}

pub fn audio_capabilities(model: &AudioModel) -> Option<AudioModelCapabilities> {
    match model.id {
        ELEVEN_LABS_V3_ID => {
            let voices = eleven_labs_v3_voices();
            Some(AudioModelCapabilities {
                supported_voices: voices.to_vec(),
                supports_two_speakers: false,
                max_characters_monologue: 5000,
                max_characters_dialogue: 0,
                default_voice: voices.iter().find(|v| v.id == "Rachel").cloned(),
                secondary_default_voice: None,
            })
        }
        _ => None,
    }
}

/// Builds the inner request body for Replicate's elevenlabs/v3.
/// `submit_and_await_prediction` wraps this in `{input: ...}` automatically,
/// so do NOT wrap here.
pub fn build_audio_request_body(prompt: &str, settings: &AudioGenerationSettings) -> Map<String, Value> {
    let mut input = Map::new();
    input.insert("prompt".into(), json!(prompt));
    input.insert("voice".into(), json!(settings.speaker1.id));
    input
}

#[async_trait]
impl AudioProviderClient for ReplicateClient {
    async fn generate_audio(&self, prompt: &str, settings: &AudioGenerationSettings) -> Result<Bytes> {
        self.generate_audio_impl(prompt, settings).await.map_err(Into::into)
    }
}

impl ReplicateClient {
    async fn generate_audio_impl(&self, prompt: &str, settings: &AudioGenerationSettings) -> ReplicateResult<Bytes> {
        if settings.model.id != ELEVEN_LABS_V3_ID {
            return Err(ReplicateError::UnsupportedModel(settings.model.id.to_string()));
        }
        let body = build_audio_request_body(prompt, settings);

        let prediction = self
            .submit_and_await_prediction(SubmissionTarget::OfficialModel(ELEVEN_LABS_V3_SLUG.to_string()), body, Timeouts::AUDIO, "audio")
            .await?;
        self.download_audio_output(&prediction).await
    }

    /// The audio file behind a succeeded prediction.
    pub(crate) async fn download_audio_output(&self, prediction: &ReplicatePrediction) -> ReplicateResult<Bytes> {
        let url = prediction.output.as_ref().and_then(|o| o.first_url()).ok_or(ReplicateError::NoResultGenerated)?;
        if url::Url::parse(url).is_err() {
            return Err(ReplicateError::AudioDownloadFailed("Invalid audio URL".into()));
        }

        let audio = match http::download_traced(url, Timeouts::AUDIO.request, self.on_trace.as_ref()).await {
            Ok(bytes) => bytes,
            Err(crate::error::GenerationError::ServerError { status_code: Some(code), .. }) => {
                return Err(ReplicateError::AudioDownloadFailed(format!("HTTP {code}")));
            }
            Err(e) => return Err(ReplicateError::Transport(e)),
        };
        if audio.is_empty() {
            return Err(ReplicateError::NoResultGenerated);
        }
        Ok(audio)
    }
}
