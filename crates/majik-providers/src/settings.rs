//! Per-media-type generation settings.

use serde::{Deserialize, Serialize};

use crate::models::{AspectRatio, AudioModel, AudioVoice, ImageModel, ImageResolution, ToolModel, VideoAspectRatio, VideoModel, VideoResolution};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageGenerationSettings {
    pub model: ImageModel,
    pub aspect_ratio: AspectRatio,
    pub resolution: ImageResolution,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoGenerationSettings {
    pub model: VideoModel,
    pub aspect_ratio: Option<VideoAspectRatio>,
    pub resolution: Option<VideoResolution>,
    pub duration: u32,
    #[serde(default = "default_true")]
    pub audio_enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioGenerationSettings {
    pub model: AudioModel,
    pub speaker1: AudioVoice,
    /// `None` = monologue.
    pub speaker2: Option<AudioVoice>,
}

/// What one run of a tool does: which of the provider's tool models, and the settings that model
/// offers (see `ToolModelCapabilities`). Both extra fields default, so a request stored before the
/// tools took parameters still reads back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSettings {
    pub model: ToolModel,
    /// How much bigger the output is. Meaningless for background removal, which offers no factors.
    #[serde(default = "default_upscale_factor")]
    pub upscale_factor: u32,
    /// The provider's own enhancement model, as a `ToolVariant::id` slug; `None` = its default.
    #[serde(default)]
    pub variant: Option<String>,
}

pub const DEFAULT_UPSCALE_FACTOR: u32 = 2;

fn default_upscale_factor() -> u32 {
    DEFAULT_UPSCALE_FACTOR
}

impl ToolSettings {
    /// The model's own defaults: what the composer starts a tool tab on.
    pub fn new(model: ToolModel) -> Self {
        Self { model, upscale_factor: DEFAULT_UPSCALE_FACTOR, variant: None }
    }

    pub fn with_factor(mut self, factor: u32) -> Self {
        self.upscale_factor = factor;
        self
    }

    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }
}
