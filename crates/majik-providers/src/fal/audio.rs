//! fal audio: routing between the ElevenLabs v3 monologue / dialogue endpoints and Gemini TTS,
//! plus the request bodies for each.

use serde_json::{json, Map, Value};

use crate::constants::fal as constants;
use crate::dialogue::{parse_dialogue, DialogueTurn, Speaker};
use crate::fal::capabilities::ids;
use crate::fal::error::FalError;
use crate::settings::AudioGenerationSettings;

#[derive(Clone, Debug, PartialEq)]
pub enum AudioRouting {
    ElevenLabsMonologue,
    ElevenLabsDialogue { turns: Vec<DialogueTurn> },
    GeminiMonologue,
    GeminiDialogue,
}

/// Decides which fal endpoint serves a given (model, settings) pair.
pub fn audio_routing(settings: &AudioGenerationSettings, prompt: &str) -> Result<(&'static str, AudioRouting), FalError> {
    match settings.model.id {
        ids::ELEVEN_LABS_V3 => {
            if settings.speaker2.is_some() {
                let turns = parse_dialogue(prompt);
                if turns.is_empty() {
                    return Err(FalError::BadRequest("Add at least one Speaker 1 or Speaker 2 line.".into()));
                }
                return Ok((constants::ELEVENLABS_V3_DIALOGUE_ENDPOINT, AudioRouting::ElevenLabsDialogue { turns }));
            }
            Ok((constants::ELEVENLABS_V3_MONOLOGUE_ENDPOINT, AudioRouting::ElevenLabsMonologue))
        }
        ids::GEMINI_25_PRO => Ok((
            constants::GEMINI_TTS_ENDPOINT,
            if settings.speaker2.is_none() { AudioRouting::GeminiMonologue } else { AudioRouting::GeminiDialogue },
        )),
        other => Err(FalError::UnsupportedModel(other.to_string())),
    }
}

/// Builds the JSON body for a routing decision.
pub fn build_audio_request_body(prompt: &str, settings: &AudioGenerationSettings, routing: &AudioRouting) -> Map<String, Value> {
    let body = match routing {
        AudioRouting::ElevenLabsMonologue => json!({
            "text": prompt,
            "voice": settings.speaker1.id,
        }),
        AudioRouting::ElevenLabsDialogue { turns } => {
            let inputs: Vec<Value> = turns
                .iter()
                .map(|turn| {
                    let voice = match turn.speaker {
                        Speaker::One => &settings.speaker1.id,
                        // `audio_routing` only emits a dialogue when speaker2 is set.
                        Speaker::Two => settings.speaker2.as_ref().map(|v| &v.id).unwrap_or(&settings.speaker1.id),
                    };
                    json!({ "text": turn.text, "voice": voice })
                })
                .collect();
            json!({ "inputs": inputs })
        }
        AudioRouting::GeminiMonologue => json!({
            "model": "gemini-2.5-pro-tts",
            "prompt": prompt,
            "voice": settings.speaker1.id,
            "output_format": "mp3",
        }),
        AudioRouting::GeminiDialogue => {
            // Two speakers required for Gemini multi-speaker mode; `audio_routing` only emits
            // `GeminiDialogue` when speaker2 is set.
            let speaker2 = settings.speaker2.as_ref().unwrap_or(&settings.speaker1);
            // fal's gemini-tts SpeakerConfig requires speaker_id to match ^\w+$ (no whitespace).
            // The user types "Speaker 1: …" / "Speaker 2: …" — rewrite those prefixes to
            // "Speaker1:" / "Speaker2:" so they match the speaker_ids we send.
            json!({
                "model": "gemini-2.5-pro-tts",
                "prompt": normalize_speaker_prefixes(prompt),
                "speakers": [
                    { "speaker_id": "Speaker1", "voice": settings.speaker1.id },
                    { "speaker_id": "Speaker2", "voice": speaker2.id },
                ],
                "output_format": "mp3",
            })
        }
    };
    match body {
        Value::Object(map) => map,
        _ => unreachable!("audio bodies are objects"),
    }
}

/// Replaces line-leading "Speaker 1:" / "Speaker 2:" labels (case-insensitive) with
/// "Speaker1:" / "Speaker2:" so they match the alphanumeric speaker_id values fal accepts.
/// Idempotent.
pub fn normalize_speaker_prefixes(prompt: &str) -> String {
    prompt
        .split('\n')
        .map(|raw| {
            // Trim spaces but not newlines (incl. `\r`).
            let trimmed = raw.trim_matches(|c: char| c.is_whitespace() && c != '\r');
            let lowered = trimmed.to_lowercase();
            let leading: String = raw.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            if lowered.starts_with("speaker 1:") {
                let body = &trimmed["speaker 1:".len()..];
                return format!("{leading}Speaker1:{body}");
            }
            if lowered.starts_with("speaker 2:") {
                let body = &trimmed["speaker 2:".len()..];
                return format!("{leading}Speaker2:{body}");
            }
            raw.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
