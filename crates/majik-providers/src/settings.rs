//! Per-media-type generation settings.

use serde::{Deserialize, Serialize};

use crate::models::{AspectRatio, AudioModel, AudioVoice, ImageModel, ImageResolution, VideoAspectRatio, VideoModel, VideoResolution};

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
