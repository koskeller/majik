//! Aspect ratios, the image / video / audio model types, voices and model capabilities.
//!
//! Models serialize as their id only; deserialization looks the id up in the catalog.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::asset::AssetConstraints;
use crate::catalog;
use majik_core::model::MediaType;

// ----- aspect ratios / resolutions ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AspectRatio {
    #[serde(rename = "1:1")]
    Square,
    #[serde(rename = "4:3")]
    Standard,
    #[serde(rename = "3:4")]
    ThreeToFour,
    #[serde(rename = "4:5")]
    Portrait,
    #[serde(rename = "16:9")]
    Landscape,
    #[serde(rename = "9:16")]
    Tall,
    #[serde(rename = "21:9")]
    Wide,
}

impl AspectRatio {
    pub const ALL: [AspectRatio; 7] = [
        AspectRatio::Square,
        AspectRatio::Standard,
        AspectRatio::ThreeToFour,
        AspectRatio::Portrait,
        AspectRatio::Landscape,
        AspectRatio::Tall,
        AspectRatio::Wide,
    ];

    /// Raw value / display name, e.g. `"16:9"`.
    pub fn raw(self) -> &'static str {
        match self {
            AspectRatio::Square => "1:1",
            AspectRatio::Standard => "4:3",
            AspectRatio::ThreeToFour => "3:4",
            AspectRatio::Portrait => "4:5",
            AspectRatio::Landscape => "16:9",
            AspectRatio::Tall => "9:16",
            AspectRatio::Wide => "21:9",
        }
    }

    pub fn from_raw(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.raw() == raw)
    }

    pub fn ratio(self) -> (u32, u32) {
        let (n, d) = self.raw().split_once(':').unwrap();
        (n.parse().unwrap(), d.parse().unwrap())
    }

    pub fn is_portrait(self) -> bool {
        let (n, d) = self.ratio();
        n < d
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoAspectRatio {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "1:1")]
    Square,
    #[serde(rename = "3:2")]
    NarrowLandscape,
    #[serde(rename = "2:3")]
    NarrowPortrait,
    #[serde(rename = "4:3")]
    Standard,
    #[serde(rename = "3:4")]
    Portrait,
    #[serde(rename = "16:9")]
    Landscape,
    #[serde(rename = "9:16")]
    Tall,
    #[serde(rename = "21:9")]
    Wide,
}

impl VideoAspectRatio {
    pub const ALL: [VideoAspectRatio; 9] = [
        VideoAspectRatio::Auto,
        VideoAspectRatio::Square,
        VideoAspectRatio::NarrowLandscape,
        VideoAspectRatio::NarrowPortrait,
        VideoAspectRatio::Standard,
        VideoAspectRatio::Portrait,
        VideoAspectRatio::Landscape,
        VideoAspectRatio::Tall,
        VideoAspectRatio::Wide,
    ];

    pub fn raw(self) -> &'static str {
        match self {
            VideoAspectRatio::Auto => "auto",
            VideoAspectRatio::Square => "1:1",
            VideoAspectRatio::NarrowLandscape => "3:2",
            VideoAspectRatio::NarrowPortrait => "2:3",
            VideoAspectRatio::Standard => "4:3",
            VideoAspectRatio::Portrait => "3:4",
            VideoAspectRatio::Landscape => "16:9",
            VideoAspectRatio::Tall => "9:16",
            VideoAspectRatio::Wide => "21:9",
        }
    }

    pub fn from_raw(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.raw() == raw)
    }

    /// `None` for `Auto`.
    pub fn ratio(self) -> Option<(u32, u32)> {
        let (n, d) = self.raw().split_once(':')?;
        Some((n.parse().ok()?, d.parse().ok()?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ImageResolution {
    #[serde(rename = "0.5K")]
    Sd,
    #[serde(rename = "1K")]
    Hd,
    #[serde(rename = "2K")]
    Fhd,
    #[serde(rename = "4K")]
    Uhd,
}

impl ImageResolution {
    pub const ALL: [ImageResolution; 4] = [ImageResolution::Sd, ImageResolution::Hd, ImageResolution::Fhd, ImageResolution::Uhd];

    pub fn raw(self) -> &'static str {
        match self {
            ImageResolution::Sd => "0.5K",
            ImageResolution::Hd => "1K",
            ImageResolution::Fhd => "2K",
            ImageResolution::Uhd => "4K",
        }
    }

    pub fn from_raw(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.raw() == raw)
    }

    pub fn long_edge(self) -> u32 {
        match self {
            ImageResolution::Sd => 512,
            ImageResolution::Hd => 1024,
            ImageResolution::Fhd => 2048,
            ImageResolution::Uhd => 3840,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum VideoResolution {
    #[serde(rename = "480p")]
    Sd,
    #[serde(rename = "720p")]
    Hd,
    #[serde(rename = "1080p")]
    Fhd,
    #[serde(rename = "4k")]
    Uhd,
}

impl VideoResolution {
    pub const ALL: [VideoResolution; 4] = [VideoResolution::Sd, VideoResolution::Hd, VideoResolution::Fhd, VideoResolution::Uhd];

    pub fn raw(self) -> &'static str {
        match self {
            VideoResolution::Sd => "480p",
            VideoResolution::Hd => "720p",
            VideoResolution::Fhd => "1080p",
            VideoResolution::Uhd => "4k",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            VideoResolution::Uhd => "4K",
            other => other.raw(),
        }
    }

    pub fn from_raw(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.raw() == raw)
    }
}

// ----- models ---------------------------------------------------------------------------------

macro_rules! model_type {
    ($name:ident, $lookup:path, $label:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name {
            pub id: &'static str,
            pub name: &'static str,
            pub manufacturer: &'static str,
            pub logo: &'static str,
            pub short_description: &'static str,
        }

        impl $name {
            pub const fn new(id: &'static str, name: &'static str, manufacturer: &'static str, logo: &'static str, short_description: &'static str) -> Self {
                Self { id, name, manufacturer, logo, short_description }
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.id)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let id = String::deserialize(d)?;
                $lookup(&id).cloned().ok_or_else(|| de::Error::custom(format!(concat!("Unknown ", $label, " id: {}"), id)))
            }
        }
    };
}

model_type!(ImageModel, catalog::image::model, "ImageModel");
model_type!(VideoModel, catalog::video::model, "VideoModel");
model_type!(AudioModel, catalog::audio::model, "AudioModel");

pub use majik_core::model::ToolId;

/// A tool implementation a provider offers, selectable in the composer like any other model.
/// Not generated by [`model_type!`] because it also carries the [`ToolId`] it implements.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToolModel {
    pub id: &'static str,
    pub kind: ToolId,
    /// What the model consumes and produces. An upscaler is either an image one or a video one, and
    /// this is what the composer's tool tab reads to decide which role card to draw.
    pub media: MediaType,
    pub name: &'static str,
    pub manufacturer: &'static str,
    pub logo: &'static str,
    pub short_description: &'static str,
}

impl ToolModel {
    pub const fn new(
        id: &'static str,
        kind: ToolId,
        media: MediaType,
        name: &'static str,
        manufacturer: &'static str,
        logo: &'static str,
        short_description: &'static str,
    ) -> Self {
        Self { id, kind, media, name, manufacturer, logo, short_description }
    }

    /// The role a run of this model takes its one input in.
    pub fn input_role(&self) -> crate::asset::AssetRole {
        match self.media {
            MediaType::Video => crate::asset::AssetRole::ReferenceVideo,
            _ => crate::asset::AssetRole::ReferenceImage,
        }
    }
}

/// One of the provider's own enhancement models for a tool (Topaz's "Standard V2", "Starlight HQ", …).
/// `id` is a stable slug that goes into the stored request; the provider's wire string comes from
/// its own `api_*` mapper, so renaming one upstream never invalidates a saved row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToolVariant {
    pub id: &'static str,
    pub name: &'static str,
}

impl ToolVariant {
    pub const fn new(id: &'static str, name: &'static str) -> Self {
        Self { id, name }
    }
}

/// What a tool model lets the composer choose. The first entry of each list is the default, the
/// same way [`ModelCapabilities`] treats its resolutions; an empty list draws no capsule at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolModelCapabilities {
    pub upscale_factors: Vec<u32>,
    pub variants: Vec<ToolVariant>,
    /// How many inputs one run may take: several images, but only one video.
    pub max_inputs: usize,
}

impl ToolModelCapabilities {
    pub fn new(max_inputs: usize) -> Self {
        Self { upscale_factors: Vec::new(), variants: Vec::new(), max_inputs }
    }

    pub fn with_factors(mut self, factors: impl Into<Vec<u32>>) -> Self {
        self.upscale_factors = factors.into();
        self
    }

    pub fn with_variants(mut self, variants: impl Into<Vec<ToolVariant>>) -> Self {
        self.variants = variants.into();
        self
    }

    pub fn default_factor(&self) -> Option<u32> {
        self.upscale_factors.first().copied()
    }

    pub fn default_variant(&self) -> Option<&'static str> {
        self.variants.first().map(|v| v.id)
    }
}

impl Serialize for ToolModel {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.id)
    }
}

impl<'de> Deserialize<'de> for ToolModel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let id = String::deserialize(d)?;
        catalog::tool::model(&id).cloned().ok_or_else(|| de::Error::custom(format!("Unknown ToolModel id: {id}")))
    }
}

// ----- capabilities ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub supported_aspect_ratios: Vec<AspectRatio>,
    pub supported_resolutions: Vec<ImageResolution>,
    pub max_input_images: usize,
    pub asset_constraints: AssetConstraints,
    pub prompt_optional: bool,
    /// The provider's documented prompt cap, when it has one; `None` leaves the length to the
    /// provider, which rejects an overlong prompt on its own.
    pub max_prompt_characters: Option<usize>,
    default_resolution_override: Option<ImageResolution>,
}

impl ModelCapabilities {
    pub fn new(aspect_ratios: impl Into<Vec<AspectRatio>>, resolutions: impl Into<Vec<ImageResolution>>, max_input_images: usize) -> Self {
        Self {
            supported_aspect_ratios: aspect_ratios.into(),
            supported_resolutions: resolutions.into(),
            max_input_images,
            asset_constraints: if max_input_images > 0 { AssetConstraints::reference_images(max_input_images) } else { AssetConstraints::none() },
            prompt_optional: false,
            max_prompt_characters: None,
            default_resolution_override: None,
        }
    }

    pub fn with_asset_constraints(mut self, c: AssetConstraints) -> Self {
        self.asset_constraints = c;
        self
    }

    pub fn with_prompt_optional(mut self, v: bool) -> Self {
        self.prompt_optional = v;
        self
    }

    pub fn with_max_prompt_characters(mut self, n: usize) -> Self {
        self.max_prompt_characters = Some(n);
        self
    }

    pub fn with_default_resolution(mut self, r: ImageResolution) -> Self {
        self.default_resolution_override = Some(r);
        self
    }

    pub fn supports_aspect_ratio(&self) -> bool {
        !self.supported_aspect_ratios.is_empty()
    }

    pub fn supports_resolution(&self) -> bool {
        !self.supported_resolutions.is_empty()
    }

    pub fn default_aspect_ratio(&self) -> Option<AspectRatio> {
        self.supported_aspect_ratios.first().copied()
    }

    pub fn default_resolution(&self) -> Option<ImageResolution> {
        self.default_resolution_override.or_else(|| self.supported_resolutions.first().copied())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoDurationRange {
    pub min: u32,
    pub max: u32,
    pub presets: Option<Vec<u32>>,
}

impl VideoDurationRange {
    pub fn new(min: u32, max: u32, presets: Option<Vec<u32>>) -> Self {
        Self { min, max, presets }
    }

    pub fn presets_or_range(&self) -> Vec<u32> {
        match &self.presets {
            Some(p) => p.clone(),
            None => (self.min..=self.max).collect(),
        }
    }

    pub fn contains(&self, d: u32) -> bool {
        match &self.presets {
            Some(p) => p.contains(&d),
            None => (self.min..=self.max).contains(&d),
        }
    }
}

/// How many references of each kind a video model's reference-to-video path takes. `None` on a
/// model that has no such path. The counts drive the composer's cards; the request keys and the
/// prompt dialect live in each provider's own table, because the same model differs between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoReferences {
    pub images: usize,
    pub videos: usize,
    pub audio: usize,
    /// Cap on the total across all three, where the provider states one (Seedance, H3).
    pub combined_max: Option<usize>,
    /// Resolutions the reference path offers, where they are narrower than the model's own
    /// (Grok Imagine Video 1.5 renders references at 720p at most).
    pub resolutions: Option<&'static [VideoResolution]>,
    /// The longest reference clip the provider takes, in seconds, where it states one (Gemini Omni
    /// Flash 1.1 takes three). A longer clip is refused before it is sent, because the provider
    /// rejects the whole request over it.
    pub max_video_secs: Option<u32>,
}

impl VideoReferences {
    /// Images only, which is the common case.
    pub fn images(images: usize) -> Self {
        Self { images, ..Self::default() }
    }

    pub fn with_videos(mut self, videos: usize) -> Self {
        self.videos = videos;
        self
    }

    pub fn with_audio(mut self, audio: usize) -> Self {
        self.audio = audio;
        self
    }

    pub fn with_combined_max(mut self, combined_max: usize) -> Self {
        self.combined_max = Some(combined_max);
        self
    }

    pub fn with_resolutions(mut self, resolutions: &'static [VideoResolution]) -> Self {
        self.resolutions = Some(resolutions);
        self
    }

    pub fn with_max_video_secs(mut self, max_video_secs: u32) -> Self {
        self.max_video_secs = Some(max_video_secs);
        self
    }

    /// Whether the reference path renders at `resolution`.
    pub fn allows_resolution(&self, resolution: VideoResolution) -> bool {
        self.resolutions.is_none_or(|allowed| allowed.contains(&resolution))
    }

    /// Whether a reference clip `duration_secs` long is within the provider's cap.
    pub fn allows_video_duration(&self, duration_secs: f64) -> bool {
        self.max_video_secs.is_none_or(|max| duration_secs <= f64::from(max))
    }

    pub fn max_for(&self, role: crate::AssetRole) -> usize {
        match role {
            crate::AssetRole::ReferenceImage => self.images,
            crate::AssetRole::ReferenceVideo => self.videos,
            crate::AssetRole::Audio => self.audio,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoModelCapabilities {
    pub duration_range: VideoDurationRange,
    pub aspect_ratios: Vec<VideoAspectRatio>,
    pub resolutions: Vec<VideoResolution>,
    pub max_input_images: usize,
    pub asset_constraints: AssetConstraints,
    pub prompt_optional: bool,
    /// See [`ModelCapabilities::max_prompt_characters`].
    pub max_prompt_characters: Option<usize>,
    pub supports_audio: bool,
    pub audio_always_on: bool,
    /// The model's reference-to-video path, if it has one. Its ranges are merged into
    /// `asset_constraints` by [`VideoModelCapabilities::with_references`].
    pub references: Option<VideoReferences>,
}

impl VideoModelCapabilities {
    pub fn new(duration_range: VideoDurationRange, aspect_ratios: impl Into<Vec<VideoAspectRatio>>, resolutions: impl Into<Vec<VideoResolution>>, max_input_images: usize) -> Self {
        Self {
            duration_range,
            aspect_ratios: aspect_ratios.into(),
            resolutions: resolutions.into(),
            max_input_images,
            asset_constraints: if max_input_images > 0 { AssetConstraints::new([(crate::asset::AssetRole::FirstFrame, 0..=1)]) } else { AssetConstraints::none() },
            prompt_optional: false,
            max_prompt_characters: None,
            supports_audio: false,
            audio_always_on: false,
            references: None,
        }
    }

    pub fn with_asset_constraints(mut self, c: AssetConstraints) -> Self {
        self.asset_constraints = c;
        self
    }

    pub fn with_prompt_optional(mut self, v: bool) -> Self {
        self.prompt_optional = v;
        self
    }

    pub fn with_max_prompt_characters(mut self, n: usize) -> Self {
        self.max_prompt_characters = Some(n);
        self
    }

    /// Declares the model's reference lists and merges their ranges into the asset constraints, so
    /// the composer offers a card per kind. Call it after `with_asset_constraints`, which replaces
    /// the whole set. References and frames are mutually exclusive at every provider that says
    /// anything about it, which is a validation rule the constraint set cannot express.
    pub fn with_references(mut self, references: VideoReferences) -> Self {
        let mut constraints = std::mem::take(&mut self.asset_constraints);
        for role in [crate::AssetRole::ReferenceImage, crate::AssetRole::ReferenceVideo, crate::AssetRole::Audio] {
            let max = references.max_for(role);
            if max > 0 {
                constraints = constraints.with_role(role, 0..=max);
            }
        }
        self.asset_constraints = constraints;
        self.references = Some(references);
        self
    }

    pub fn with_audio(mut self, supports: bool, always_on: bool) -> Self {
        self.supports_audio = supports;
        self.audio_always_on = always_on;
        self
    }

    pub fn supports_audio_toggle(&self) -> bool {
        self.supports_audio && !self.audio_always_on
    }

    pub fn supports_resolution(&self) -> bool {
        !self.resolutions.is_empty()
    }

    pub fn default_aspect_ratio(&self) -> Option<VideoAspectRatio> {
        self.aspect_ratios.first().copied()
    }

    pub fn default_resolution(&self) -> Option<VideoResolution> {
        self.resolutions.first().copied()
    }

    pub fn lowest_resolution(&self) -> Option<VideoResolution> {
        VideoResolution::ALL.into_iter().find(|r| self.resolutions.contains(r))
    }

    pub fn default_duration(&self) -> u32 {
        self.duration_range.presets.as_ref().and_then(|p| p.first().copied()).unwrap_or(self.duration_range.min)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioVoice {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub preview_url: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub gender: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub language_codes: Option<Vec<String>>,
}

impl AudioVoice {
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self { id: id.into(), display_name: display_name.into(), subtitle: None, preview_url: None, category: None, gender: None, accent: None, language_codes: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioModelCapabilities {
    pub supported_voices: Vec<AudioVoice>,
    pub supports_two_speakers: bool,
    pub max_characters_monologue: usize,
    pub max_characters_dialogue: usize,
    pub default_voice: Option<AudioVoice>,
    pub secondary_default_voice: Option<AudioVoice>,
}
