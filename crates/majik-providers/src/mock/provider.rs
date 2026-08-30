//! In-process fake provider used for UI tests and local development. Generates deterministic
//! fixture bytes on the fly and never makes network calls. Behavior is controlled entirely by
//! `#directives` embedded in the prompt text (see [`super::directives`]).

use std::time::Duration;

use async_trait::async_trait;

use crate::asset::ProviderAsset;
use crate::catalog;
use crate::client::{AudioProviderClient, ClientOptions, ImageProviderClient, JobHandle, OnAccepted, ResumableClient, TextProviderClient, TraceSink, VideoProviderClient};
use crate::error::{GenerationError, Result};
use crate::models::{AspectRatio, ImageModel, ImageResolution};
use crate::settings::{AudioGenerationSettings, VideoGenerationSettings};
use crate::{Bytes, ProviderId};
use majik_core::model::{JobTrace, MediaType, TraceLabel};
use sha2::{Digest, Sha256};

use super::directives::{parse_directives, Parsed};
use super::{image_renderer, video_renderer};

#[derive(Clone, Default)]
pub struct MockClient {
    on_accepted: Option<OnAccepted>,
    on_trace: Option<TraceSink>,
}

impl std::fmt::Debug for MockClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockClient").finish()
    }
}

/// Job ids the Mock hands out: `mock-<kind>-<prompt hash>`. A `-gone` suffix makes a resume fail
/// the way an expired provider job does.
const JOB_ID_PREFIX: &str = "mock-";

impl MockClient {
    /// The API key is ignored (any non-empty value works in the UI).
    pub fn new(_api_key: &str) -> Self {
        Self::default()
    }

    pub fn from_options(options: &ClientOptions) -> Self {
        Self { on_accepted: options.on_accepted.clone(), on_trace: options.on_trace.clone() }
    }

    pub fn with_on_accepted(mut self, on_accepted: OnAccepted) -> Self {
        self.on_accepted = Some(on_accepted);
        self
    }

    pub fn with_on_trace(mut self, on_trace: TraceSink) -> Self {
        self.on_trace = Some(on_trace);
        self
    }

    /// The exchanges a real provider would have made, so the traces the app keeps are exercised end
    /// to end without a network: a submit answered with the job id, and the result once the
    /// `#delay:` has passed (carrying the `#fail:` error when there is one).
    fn trace(&self, label: TraceLabel, kind: &str, parsed: &Parsed, error: Option<&GenerationError>) {
        let Some(on_trace) = &self.on_trace else { return };
        let job_id = Self::job_id(kind, &parsed.clean_prompt);
        let (request_body, response_body, status) = match label {
            TraceLabel::Submit => (
                Some(serde_json::json!({ "prompt": parsed.clean_prompt, "delay": parsed.delay, "fail": parsed.failure.as_ref().map(|e| e.to_string()) }).to_string()),
                Some(serde_json::json!({ "request_id": job_id }).to_string()),
                Some(202),
            ),
            _ => match error {
                Some(e) => (None, Some(serde_json::json!({ "request_id": job_id, "status": "FAILED", "error": e.to_string() }).to_string()), Some(500)),
                None => (None, Some(serde_json::json!({ "request_id": job_id, "status": "COMPLETED" }).to_string()), Some(200)),
            },
        };
        on_trace(JobTrace {
            at_ms: majik_core::now_ms(),
            label,
            method: if label == TraceLabel::Submit { "POST".into() } else { "GET".into() },
            url: format!("mock://{kind}/{job_id}"),
            status,
            duration_ms: 0,
            request_body,
            response_body,
            error: None,
        });
    }

    /// The handle a generation of `kind` for `prompt` is accepted under.
    pub fn job_id(kind: &str, clean_prompt: &str) -> String {
        let digest = Sha256::digest(clean_prompt.as_bytes());
        let short: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        format!("{JOB_ID_PREFIX}{kind}-{short}")
    }

    /// Like a queue: the job is accepted (and reported) before any `#delay:` elapses.
    async fn apply_directives(&self, prompt: &str, kind: &str) -> Result<Parsed> {
        let parsed = parse_directives(prompt);
        self.trace(TraceLabel::Submit, kind, &parsed, None);
        if let Some(on_accepted) = &self.on_accepted {
            on_accepted(JobHandle { job_id: Self::job_id(kind, &parsed.clean_prompt), poll_url: None });
        }
        if parsed.delay > 0.0 && parsed.delay.is_finite() {
            tokio::time::sleep(Duration::from_secs_f64(parsed.delay)).await;
        }
        self.trace(TraceLabel::Result, kind, &parsed, parsed.failure.as_ref());
        if let Some(failure) = parsed.failure.clone() {
            return Err(failure);
        }
        Ok(parsed)
    }

    /// 16-bit mono 8 kHz PCM WAV with the requested duration of silence. Produced inline so tests
    /// don't need fixture files.
    pub fn silent_wav(milliseconds: usize) -> Bytes {
        let sample_rate: usize = 8000;
        let num_samples = (sample_rate * milliseconds / 1000).max(1);
        let bytes_per_sample: usize = 2;
        let data_size = num_samples * bytes_per_sample;

        let mut data = Vec::with_capacity(44 + data_size);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&u32_le(36 + data_size));
        data.extend_from_slice(b"WAVE");
        data.extend_from_slice(b"fmt ");
        data.extend_from_slice(&u32_le(16)); // fmt chunk size
        data.extend_from_slice(&u16_le(1)); // PCM format
        data.extend_from_slice(&u16_le(1)); // channels
        data.extend_from_slice(&u32_le(sample_rate));
        data.extend_from_slice(&u32_le(sample_rate * bytes_per_sample));
        data.extend_from_slice(&u16_le(bytes_per_sample));
        data.extend_from_slice(&u16_le(16)); // bits per sample
        data.extend_from_slice(b"data");
        data.extend_from_slice(&u32_le(data_size));
        data.resize(data.len() + data_size, 0);
        data
    }
}

fn u16_le(v: usize) -> [u8; 2] {
    (v as u16).to_le_bytes()
}

fn u32_le(v: usize) -> [u8; 4] {
    (v as u32).to_le_bytes()
}

#[async_trait]
impl ImageProviderClient for MockClient {
    async fn generate_image(
        &self,
        prompt: &str,
        model: &ImageModel,
        _assets: &[ProviderAsset],
        aspect_ratio: Option<AspectRatio>,
        resolution: Option<ImageResolution>,
    ) -> Result<Bytes> {
        let parsed = self.apply_directives(prompt, "image").await?;
        Ok(image_renderer::render(&ProviderId::mock(), model, &parsed.clean_prompt, aspect_ratio, resolution))
    }

    async fn upscale_image(&self, image: &[u8]) -> Result<Bytes> {
        Ok(image.to_vec())
    }

    /// Corner-key matting: the top-left pixel is the "background" and
    /// every pixel of that colour turns transparent, so the result exercises the app's alpha
    /// handling (RGBA PNG, checkerboard) without a model. Bytes that aren't an image pass through.
    async fn remove_background(&self, image: &[u8]) -> Result<Bytes> {
        let Ok(decoded) = image::load_from_memory(image) else { return Ok(image.to_vec()) };
        let mut rgba = decoded.into_rgba8();
        let key = *rgba.get_pixel(0, 0);
        for pixel in rgba.pixels_mut() {
            if pixel.0[..3] == key.0[..3] {
                pixel.0[3] = 0;
            }
        }
        let mut out = std::io::Cursor::new(Vec::new());
        match rgba.write_to(&mut out, image::ImageFormat::Png) {
            Ok(()) => Ok(out.into_inner()),
            Err(_) => Ok(image.to_vec()),
        }
    }
}

#[async_trait]
impl VideoProviderClient for MockClient {
    async fn generate_video(&self, prompt: &str, _assets: &[ProviderAsset], settings: &VideoGenerationSettings) -> Result<Bytes> {
        let parsed = self.apply_directives(prompt, "video").await?;
        Ok(video_renderer::render(
            &ProviderId::mock(),
            &settings.model,
            &parsed.clean_prompt,
            settings.duration,
            settings.aspect_ratio,
            settings.resolution,
        )
        .await?)
    }
}

#[async_trait]
impl AudioProviderClient for MockClient {
    async fn generate_audio(&self, prompt: &str, _settings: &AudioGenerationSettings) -> Result<Bytes> {
        self.apply_directives(prompt, "audio").await?;
        Ok(Self::silent_wav(250))
    }
}

#[async_trait]
impl TextProviderClient for MockClient {
    /// A deterministic "rewrite": the prompt with a fixed suffix, so a test can tell the improved
    /// text from what was typed. Honours `#delay:` / `#fail:` like every other Mock call.
    async fn complete_text(&self, _system: &str, user: &str, _max_tokens: usize) -> Result<String> {
        let parsed = self.apply_directives(user, "text").await?;
        Ok(format!("{}, cinematic lighting, highly detailed", parsed.clean_prompt))
    }
}

#[async_trait]
impl ResumableClient for MockClient {
    /// Every Mock job "is still there" and completes at once with a fixture for its media type;
    /// a foreign id or a `-gone` suffix is the expired-job case.
    async fn resume(&self, handle: &JobHandle, media_type: MediaType) -> Result<Bytes> {
        if !handle.job_id.starts_with(JOB_ID_PREFIX) || handle.job_id.ends_with("-gone") {
            return Err(GenerationError::JobGone);
        }
        Ok(match media_type {
            MediaType::Image => image_renderer::render(&ProviderId::mock(), &catalog::image::ALL[0], "resumed", None, None),
            MediaType::Video => video_renderer::render(&ProviderId::mock(), &catalog::video::ALL[0], "resumed", 2, None, None).await?,
            MediaType::Audio => Self::silent_wav(250),
        })
    }
}
