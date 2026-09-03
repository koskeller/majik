//! The composer's value-typed state: per-media-type drafts, per-tab input assets, capability
//! coercion, the per-tab session assets kept beside them, and the recreate loader.
//!
//! No GPUI here: `ComposeView` owns a `ComposerState` and layers the prompt, animations and toasts on
//! top, so every rule below is testable with a plain `#[test]`.

use majik_core::model::{AssetId, MediaType, ToolId};
use majik_generation::{GenerationType, Request};
use majik_providers::{
    AspectRatio, AssetConstraints, AssetRole, AudioGenerationSettings, AudioModel, AudioModelCapabilities, AudioVoice, Estimate, ImageGenerationSettings,
    ImageModel, ImageResolution, ModelCapabilities, PricedJob, ProviderDescriptor, ToolInput, ToolModel, ToolModelCapabilities, ToolSettings, VideoAspectRatio,
    VideoGenerationSettings, VideoModel, VideoModelCapabilities, VideoResolution,
};

use crate::drafts::{AudioDraftState, ImageDraftState, ProviderDraft, ToolDraftState, VideoDraftState};

/// How many images a tool tab takes at once; each becomes its own row.
pub const TOOL_MAX_IMAGES: usize = 10;
/// The most outputs one Generate makes per prompt on the image and video tabs (flarly's cap).
pub const MAX_COUNT: usize = 8;

/// What the composer's type row selects: a kind of media to generate, or a tool to run over images.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ComposeTab {
    Media(MediaType),
    Tool(ToolId),
}

impl ComposeTab {
    /// Every tab the composer can show, in the order the type row lists them.
    pub const ALL: [ComposeTab; 5] = [ComposeTab::Media(MediaType::Image), ComposeTab::Media(MediaType::Video), ComposeTab::Media(MediaType::Audio), ComposeTab::Tool(ToolId::Upscale), ComposeTab::Tool(ToolId::RemoveBackground)];

    pub fn label(self) -> &'static str {
        match self {
            ComposeTab::Media(MediaType::Image) => "Image",
            ComposeTab::Media(MediaType::Video) => "Video",
            ComposeTab::Media(MediaType::Audio) => "Audio",
            ComposeTab::Tool(ToolId::Upscale) => "Upscale",
            ComposeTab::Tool(ToolId::RemoveBackground) => "Remove BG",
        }
    }

    /// The glyph beside the label in the type row.
    pub fn icon(self) -> &'static str {
        match self {
            ComposeTab::Media(MediaType::Image) => "image-frame",
            ComposeTab::Media(MediaType::Video) => "video-ai",
            ComposeTab::Media(MediaType::Audio) => "audio-ai",
            ComposeTab::Tool(ToolId::Upscale) => "four-k",
            ComposeTab::Tool(ToolId::RemoveBackground) => "background-eraser",
        }
    }

    /// Persistence key (`ProviderDraft::media_type`) and element id.
    pub fn raw(self) -> &'static str {
        match self {
            ComposeTab::Media(t) => majik_core::db::media_type_raw(t),
            ComposeTab::Tool(t) => majik_core::db::tool_raw(t),
        }
    }

    pub fn from_raw(raw: &str) -> Option<Self> {
        if let Some(tool) = majik_core::db::tool_from_raw(raw) {
            return Some(ComposeTab::Tool(tool));
        }
        matches!(raw, "image" | "video" | "audio").then(|| ComposeTab::Media(majik_core::db::media_type_from_raw(raw)))
    }

    pub fn is_tool(self) -> bool {
        matches!(self, ComposeTab::Tool(_))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageDraft {
    /// Index into `provider.supported_image_models`.
    pub model: usize,
    pub aspect_ratio: Option<AspectRatio>,
    pub resolution: Option<ImageResolution>,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoDraft {
    pub model: usize,
    pub aspect_ratio: Option<VideoAspectRatio>,
    pub resolution: Option<VideoResolution>,
    pub duration: u32,
    pub audio: bool,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioDraft {
    pub model: usize,
    pub speaker1: Option<AudioVoice>,
    pub speaker2: Option<AudioVoice>,
}

/// A tool tab's draft: which of the provider's models for that tool is selected, and the settings
/// that model offers. `None` on either means "the model's default", the way the image tab's
/// optional resolution does, so switching models doesn't strand a value the new one can't take.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolDraft {
    /// Index into `provider.tool_models(kind)`.
    pub model: usize,
    pub upscale_factor: Option<u32>,
    /// A `ToolVariant::id` slug.
    pub variant: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolDrafts {
    pub upscale: ToolDraft,
    pub remove_background: ToolDraft,
}

impl ToolDrafts {
    pub fn get(&self, tool: ToolId) -> &ToolDraft {
        match tool {
            ToolId::Upscale => &self.upscale,
            ToolId::RemoveBackground => &self.remove_background,
        }
    }

    pub fn get_mut(&mut self, tool: ToolId) -> &mut ToolDraft {
        match tool {
            ToolId::Upscale => &mut self.upscale,
            ToolId::RemoveBackground => &mut self.remove_background,
        }
    }
}

/// An input the composer will send: a library asset in a role. Files dropped or picked into the
/// composer are imported as assets on the spot, so the draft only ever holds ids.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftAsset {
    pub asset: AssetId,
    pub role: AssetRole,
}

/// `imageAssets` / `videoAssets`: each tab owns its own draft so switching tabs never mixes or loses
/// inputs. Audio has no asset list, so its reads are empty and its writes have nowhere to go.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabAssets {
    pub image: Vec<DraftAsset>,
    pub video: Vec<DraftAsset>,
    pub upscale: Vec<DraftAsset>,
    pub remove_background: Vec<DraftAsset>,
}

impl TabAssets {
    pub fn get(&self, tab: ComposeTab) -> &[DraftAsset] {
        match tab {
            ComposeTab::Media(MediaType::Image) => &self.image,
            ComposeTab::Media(MediaType::Video) => &self.video,
            ComposeTab::Media(MediaType::Audio) => &[],
            ComposeTab::Tool(ToolId::Upscale) => &self.upscale,
            ComposeTab::Tool(ToolId::RemoveBackground) => &self.remove_background,
        }
    }

    pub fn get_mut(&mut self, tab: ComposeTab) -> Option<&mut Vec<DraftAsset>> {
        match tab {
            ComposeTab::Media(MediaType::Image) => Some(&mut self.image),
            ComposeTab::Media(MediaType::Video) => Some(&mut self.video),
            ComposeTab::Media(MediaType::Audio) => None,
            ComposeTab::Tool(ToolId::Upscale) => Some(&mut self.upscale),
            ComposeTab::Tool(ToolId::RemoveBackground) => Some(&mut self.remove_background),
        }
    }
}

/// Why recreate changed something on the way in (`RecreateSettingsWarning`), on the tab the
/// request opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecreateWarning {
    DefaultModel { tab: ComposeTab, original_model: &'static str, replacement_model: &'static str },
    DefaultSettings { tab: ComposeTab, model: &'static str },
}

impl RecreateWarning {
    pub fn message(&self, provider_name: &str) -> String {
        match self {
            RecreateWarning::DefaultModel { tab, original_model, replacement_model } => {
                format!("{provider_name} doesn't support the original {} model, {original_model}. Using {replacement_model} instead.", noun(*tab))
            }
            RecreateWarning::DefaultSettings { tab, model } => {
                format!("Some original {} settings for {model} aren't available from {provider_name}. Using supported defaults instead.", noun(*tab))
            }
        }
    }
}

/// The provider has no model at all for what the request asks for (a media tab reads "doesn't
/// support image generation"; a tool reads "doesn't support upscale").
pub fn unsupported_message(provider_name: &str, tab: ComposeTab) -> String {
    let what = match tab {
        ComposeTab::Media(_) => format!("{} generation", noun(tab)),
        ComposeTab::Tool(_) => noun(tab).to_string(),
    };
    format!("{provider_name} doesn't support {what}, so this item can't be recreated with the current provider.")
}

fn noun(tab: ComposeTab) -> &'static str {
    match tab {
        ComposeTab::Media(MediaType::Image) => "image",
        ComposeTab::Media(MediaType::Video) => "video",
        ComposeTab::Media(MediaType::Audio) => "audio",
        ComposeTab::Tool(ToolId::Upscale) => "upscale",
        ComposeTab::Tool(ToolId::RemoveBackground) => "background removal",
    }
}

/// `RecreateSettingsLoadResult`: a whole new state to adopt, or nothing to adopt at all.
#[derive(Clone, Debug)]
pub enum RecreateOutcome {
    Loaded { state: Box<ComposerState>, warning: Option<RecreateWarning> },
    Unsupported(ComposeTab),
}

#[derive(Clone, Debug)]
pub struct ComposerState {
    pub provider: &'static ProviderDescriptor,
    pub tab: ComposeTab,
    pub image: ImageDraft,
    pub video: VideoDraft,
    pub audio: AudioDraft,
    pub tools: ToolDrafts,
    pub assets: TabAssets,
}

impl ComposerState {
    /// Provider defaults, then the persisted draft on top, then clamped to what the models support.
    pub fn new(provider: &'static ProviderDescriptor, draft: &ProviderDraft) -> Self {
        let mut this = Self {
            provider,
            tab: ComposeTab::Media(MediaType::Image),
            image: fresh_image_draft(),
            video: fresh_video_draft(),
            audio: fresh_audio_draft(),
            tools: ToolDrafts::default(),
            assets: TabAssets::default(),
        };
        this.tab = this.supported_tabs().first().copied().unwrap_or(ComposeTab::Media(MediaType::Image));
        this.restore(draft);
        this.coerce();
        this
    }

    /// Swap to another provider's draft. Assets are session state, not provider state, so they stay.
    pub fn set_provider(&mut self, provider: &'static ProviderDescriptor, draft: &ProviderDraft) {
        let assets = std::mem::take(&mut self.assets);
        *self = Self::new(provider, draft);
        self.assets = assets;
    }

    fn restore(&mut self, d: &ProviderDraft) {
        if let Some(id) = &d.image.model_id {
            if let Some(ix) = self.provider.supported_image_models.iter().position(|m| m.id == id) {
                self.image.model = ix;
            }
        }
        self.image.aspect_ratio = d.image.aspect_ratio.or(self.image.aspect_ratio);
        self.image.resolution = d.image.resolution.or(self.image.resolution);
        self.image.count = d.image.count.unwrap_or(self.image.count);
        if let Some(id) = &d.video.model_id {
            if let Some(ix) = self.provider.supported_video_models.iter().position(|m| m.id == id) {
                self.video.model = ix;
            }
        }
        self.video.aspect_ratio = d.video.aspect_ratio.or(self.video.aspect_ratio);
        self.video.resolution = d.video.resolution.or(self.video.resolution);
        self.video.duration = d.video.duration.unwrap_or(self.video.duration);
        self.video.audio = d.video.audio.unwrap_or(self.video.audio);
        self.video.count = d.video.count.unwrap_or(self.video.count);
        if let Some(id) = &d.audio.model_id {
            if let Some(ix) = self.provider.supported_audio_models.iter().position(|m| m.id == id) {
                self.audio.model = ix;
            }
        }
        if let Some(caps) = self.audio_caps() {
            if let Some(v) = d.audio.speaker1.as_deref().and_then(|id| voice_by_id(&caps, id)) {
                self.audio.speaker1 = Some(v);
            }
            if let Some(v) = d.audio.speaker2.as_deref().and_then(|id| voice_by_id(&caps, id)) {
                self.audio.speaker2 = Some(v);
            }
        }
        for tool in ToolId::ALL {
            let stored = match tool {
                ToolId::Upscale => &d.upscale,
                ToolId::RemoveBackground => &d.remove_background,
            };
            if let Some(id) = &stored.model_id {
                if let Some(ix) = self.provider.tool_models(tool).iter().position(|m| m.id == id) {
                    self.tools.get_mut(tool).model = ix;
                }
            }
            let draft = self.tools.get_mut(tool);
            draft.upscale_factor = stored.upscale_factor;
            draft.variant = stored.variant.clone();
        }
        if let Some(tab) = d.media_type.as_deref().and_then(ComposeTab::from_raw) {
            if self.supported_tabs().contains(&tab) {
                self.tab = tab;
            }
        }
    }

    pub fn to_draft(&self) -> ProviderDraft {
        ProviderDraft {
            media_type: Some(self.tab.raw().to_string()),
            image: ImageDraftState { model_id: self.image_model().map(|m| m.id.to_string()), aspect_ratio: self.image.aspect_ratio, resolution: self.image.resolution, count: Some(self.image.count) },
            video: VideoDraftState { model_id: self.video_model().map(|m| m.id.to_string()), aspect_ratio: self.video.aspect_ratio, resolution: self.video.resolution, duration: Some(self.video.duration), audio: Some(self.video.audio), count: Some(self.video.count) },
            audio: AudioDraftState { model_id: self.audio_model().map(|m| m.id.to_string()), speaker1: self.audio.speaker1.as_ref().map(|v| v.id.clone()), speaker2: self.audio.speaker2.as_ref().map(|v| v.id.clone()) },
            upscale: self.tool_draft_state(ToolId::Upscale),
            remove_background: self.tool_draft_state(ToolId::RemoveBackground),
        }
    }

    fn tool_draft_state(&self, tool: ToolId) -> ToolDraftState {
        let draft = self.tools.get(tool);
        ToolDraftState {
            model_id: self.tool_model(tool).map(|m| m.id.to_string()),
            upscale_factor: draft.upscale_factor,
            variant: draft.variant.clone(),
        }
    }

    // ----- tabs / models -----------------------------------------------------------

    pub fn supported_types(&self) -> Vec<MediaType> {
        let mut v = Vec::new();
        if !self.provider.supported_image_models.is_empty() {
            v.push(MediaType::Image);
        }
        if self.provider.supports_video_generation() {
            v.push(MediaType::Video);
        }
        if self.provider.supports_audio_generation() {
            v.push(MediaType::Audio);
        }
        v
    }

    /// The type row: media types first, then every tool this provider has a model for.
    pub fn supported_tabs(&self) -> Vec<ComposeTab> {
        let types = self.supported_types();
        ComposeTab::ALL
            .into_iter()
            .filter(|tab| match *tab {
                ComposeTab::Media(media) => types.contains(&media),
                ComposeTab::Tool(tool) => self.provider.supports_tool(tool),
            })
            .collect()
    }

    /// Returns `false` (and changes nothing) when the provider has no models for that tab.
    pub fn set_tab(&mut self, tab: ComposeTab) -> bool {
        if !self.supported_tabs().contains(&tab) {
            return false;
        }
        self.tab = tab;
        self.coerce();
        true
    }

    pub fn select_model(&mut self, ix: usize) {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => self.image.model = ix,
            ComposeTab::Media(MediaType::Video) => self.video.model = ix,
            ComposeTab::Media(MediaType::Audio) => self.audio.model = ix,
            ComposeTab::Tool(t) => self.tools.get_mut(t).model = ix,
        }
        self.coerce();
    }

    pub fn model_index(&self) -> usize {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => self.image.model,
            ComposeTab::Media(MediaType::Video) => self.video.model,
            ComposeTab::Media(MediaType::Audio) => self.audio.model,
            ComposeTab::Tool(t) => self.tools.get(t).model,
        }
    }

    pub fn tool_model(&self, tool: ToolId) -> Option<&'static ToolModel> {
        self.provider.tool_models(tool).get(self.tools.get(tool).model).copied()
    }

    /// The selected model when a tool tab is active.
    pub fn active_tool_model(&self) -> Option<&'static ToolModel> {
        match self.tab {
            ComposeTab::Tool(t) => self.tool_model(t),
            ComposeTab::Media(_) => None,
        }
    }

    /// What the active tool tab's model lets the user choose.
    pub fn tool_caps(&self) -> Option<ToolModelCapabilities> {
        self.provider.tool_capabilities(self.active_tool_model()?)
    }

    /// The active tool tab's model with its chosen settings, ready to run. Each setting falls back
    /// to the model's default, so a draft that predates a model gaining one still submits.
    pub fn tool_settings(&self) -> Option<ToolSettings> {
        let ComposeTab::Tool(tool) = self.tab else { return None };
        let model = self.tool_model(tool)?;
        let draft = self.tools.get(tool);
        let caps = self.provider.tool_capabilities(model).unwrap_or_default();
        Some(ToolSettings {
            model: model.clone(),
            upscale_factor: draft.upscale_factor.or_else(|| caps.default_factor()).unwrap_or(majik_providers::DEFAULT_UPSCALE_FACTOR),
            variant: draft.variant.clone().or_else(|| caps.default_variant().map(str::to_string)),
        })
    }

    pub fn image_model(&self) -> Option<&'static ImageModel> {
        self.provider.supported_image_models.get(self.image.model)
    }

    /// The catalog id of the active tab's selected model.
    pub fn model_id(&self) -> Option<&'static str> {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => self.image_model().map(|m| m.id),
            ComposeTab::Media(MediaType::Video) => self.video_model().map(|m| m.id),
            ComposeTab::Media(MediaType::Audio) => self.audio_model().map(|m| m.id),
            ComposeTab::Tool(tool) => self.tool_model(tool).map(|m| m.id),
        }
    }

    pub fn video_model(&self) -> Option<&'static VideoModel> {
        self.provider.supported_video_models.get(self.video.model)
    }

    pub fn audio_model(&self) -> Option<&'static AudioModel> {
        self.provider.supported_audio_models.get(self.audio.model)
    }

    pub fn image_caps(&self) -> Option<ModelCapabilities> {
        self.image_model().and_then(|m| self.provider.image_capabilities(m))
    }

    pub fn video_caps(&self) -> Option<VideoModelCapabilities> {
        self.video_model().and_then(|m| self.provider.video_capabilities(m))
    }

    pub fn audio_caps(&self) -> Option<AudioModelCapabilities> {
        self.audio_model().and_then(|m| self.provider.audio_capabilities(m))
    }

    /// Clamp every draft to what its selected model supports. Assets are never touched here: a
    /// role the new model can't use is hidden, not lost (the user may switch back).
    pub fn coerce(&mut self) {
        if let Some(caps) = self.image_caps() {
            coerce_image(&mut self.image, &caps);
        }
        if let Some(caps) = self.video_caps() {
            coerce_video(&mut self.video, &caps);
        }
        if let Some(caps) = self.audio_caps() {
            coerce_audio(&mut self.audio, &caps);
        }
        for tool in ToolId::ALL {
            let available = self.provider.tool_models(tool).len();
            if self.tools.get(tool).model >= available {
                self.tools.get_mut(tool).model = 0;
            }
            // A factor or variant the newly selected model doesn't offer falls back to its default
            // rather than being sent as-is.
            let caps = self.tool_model(tool).and_then(|m| self.provider.tool_capabilities(m)).unwrap_or_default();
            let draft = self.tools.get_mut(tool);
            if draft.upscale_factor.is_some_and(|f| !caps.upscale_factors.contains(&f)) {
                draft.upscale_factor = None;
            }
            if draft.variant.as_deref().is_some_and(|v| !caps.variants.iter().any(|t| t.id == v)) {
                draft.variant = None;
            }
        }
    }

    pub fn asset_constraints(&self) -> AssetConstraints {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => self.image_caps().map(|c| c.asset_constraints).unwrap_or_default(),
            ComposeTab::Media(MediaType::Video) => self.video_caps().map(|c| c.asset_constraints).unwrap_or_default(),
            ComposeTab::Media(MediaType::Audio) => AssetConstraints::none(),
            // The selected model decides what the tab takes: an image upscaler draws the image
            // card, a video one the video card. One clip per run; images come in batches.
            ComposeTab::Tool(_) => match self.active_tool_model() {
                Some(model) => {
                    let max = self.provider.tool_capabilities(model).map(|c| c.max_inputs).unwrap_or(TOOL_MAX_IMAGES).max(1);
                    AssetConstraints::new([(model.input_role(), 1..=max)])
                }
                None => AssetConstraints::none(),
            },
        }
    }

    pub fn prompt_optional(&self) -> bool {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => self.image_caps().map(|c| c.prompt_optional).unwrap_or(false),
            ComposeTab::Media(MediaType::Video) => self.video_caps().map(|c| c.prompt_optional).unwrap_or(false),
            ComposeTab::Media(MediaType::Audio) => false,
            ComposeTab::Tool(_) => true,
        }
    }

    /// Outputs per prompt on the image and video tabs; audio and tools always produce one per input.
    pub fn count(&self) -> usize {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => self.image.count,
            ComposeTab::Media(MediaType::Video) => self.video.count,
            ComposeTab::Media(MediaType::Audio) | ComposeTab::Tool(_) => 1,
        }
    }

    /// The active tab's output count, when it has one (see [`Self::count`]).
    pub fn count_mut(&mut self) -> Option<&mut usize> {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => Some(&mut self.image.count),
            ComposeTab::Media(MediaType::Video) => Some(&mut self.video.count),
            ComposeTab::Media(MediaType::Audio) | ComposeTab::Tool(_) => None,
        }
    }

    /// `None` on a tool tab: tools don't go through a generation request.
    pub fn generation_type(&self) -> Option<GenerationType> {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => Some(GenerationType::Image(ImageGenerationSettings {
                model: self.image_model()?.clone(),
                aspect_ratio: self.image.aspect_ratio.unwrap_or(AspectRatio::Square),
                resolution: self.image.resolution.unwrap_or(ImageResolution::Hd),
            })),
            ComposeTab::Media(MediaType::Video) => Some(GenerationType::Video(VideoGenerationSettings {
                model: self.video_model()?.clone(),
                aspect_ratio: self.video.aspect_ratio,
                resolution: self.video.resolution,
                duration: self.video.duration,
                audio_enabled: self.video.audio,
            })),
            ComposeTab::Media(MediaType::Audio) => Some(GenerationType::Audio(AudioGenerationSettings {
                model: self.audio_model()?.clone(),
                speaker1: self.audio.speaker1.clone()?,
                speaker2: self.audio.speaker2.clone(),
            })),
            ComposeTab::Tool(_) => None,
        }
    }

    /// What *one* output of the current draft costs on the current provider: an estimate, and
    /// `Estimate::Unknown` for a model the provider has no price for.
    ///
    /// `prompt_characters` is the length of the text to speak, which is all that per-character TTS
    /// pricing depends on; `tool_input` is the size of the asset a tool will run over, which is what
    /// a per-second video upscale is billed on. Both are passed in rather than read so this module
    /// stays free of GPUI.
    pub fn unit_price(&self, prompt_characters: usize, tool_input: ToolInput) -> Estimate {
        match self.tab {
            ComposeTab::Media(MediaType::Image) => {
                let Some(model) = self.image_model() else { return Estimate::Unknown };
                let settings = ImageGenerationSettings {
                    model: model.clone(),
                    aspect_ratio: self.image.aspect_ratio.unwrap_or(AspectRatio::Square),
                    resolution: self.image.resolution.unwrap_or(ImageResolution::Hd),
                };
                self.provider.price(&PricedJob::Image(&settings))
            }
            ComposeTab::Media(MediaType::Video) => {
                let Some(model) = self.video_model() else { return Estimate::Unknown };
                let settings = VideoGenerationSettings {
                    model: model.clone(),
                    aspect_ratio: self.video.aspect_ratio,
                    resolution: self.video.resolution,
                    duration: self.video.duration,
                    audio_enabled: self.video.audio,
                };
                self.provider.price(&PricedJob::Video(&settings))
            }
            ComposeTab::Media(MediaType::Audio) => {
                let (Some(model), Some(speaker1)) = (self.audio_model(), self.audio.speaker1.clone()) else { return Estimate::Unknown };
                let settings = AudioGenerationSettings { model: model.clone(), speaker1, speaker2: self.audio.speaker2.clone() };
                self.provider.price(&PricedJob::Audio { settings: &settings, characters: prompt_characters })
            }
            // Tools never reach `generation_type`, but they do cost money: one run per input.
            ComposeTab::Tool(_) => match self.tool_settings() {
                Some(settings) => self.provider.price(&PricedJob::Tool { settings: &settings, input: tool_input }),
                None => Estimate::Unknown,
            },
        }
    }

    // ----- assets --------------------------------------------------------------------

    /// The active tab's whole draft, including roles the current model can't use.
    pub fn active_assets(&self) -> &[DraftAsset] {
        self.assets.get(self.tab)
    }

    /// The active tab's assets the current model accepts: what the row shows and Generate sends.
    pub fn accepted_assets(&self) -> Vec<&DraftAsset> {
        let constraints = self.asset_constraints();
        self.active_assets().iter().filter(|a| constraints.accepts(a.role)).collect()
    }

    pub fn role_count(&self, role: AssetRole) -> usize {
        self.active_assets().iter().filter(|a| a.role == role).count()
    }

    pub fn role_is_full(&self, role: AssetRole) -> bool {
        match self.asset_constraints().range(role) {
            Some(range) => self.role_count(role) >= *range.end(),
            None => true,
        }
    }

    /// Whether the active tab has a start or end frame attached.
    pub fn has_frames(&self) -> bool {
        self.accepted_assets().iter().any(|a| a.role.is_frame_input())
    }

    /// Whether the active tab has anything the prompt can address by handle.
    pub fn has_references(&self) -> bool {
        self.accepted_assets().iter().any(|a| a.role.is_reference())
    }

    /// Whether `role` can still be filled: its model accepts it, it has room, and it isn't on the
    /// far side of the frames/references divide from what is already attached. A model's reference
    /// endpoint takes no frames at all, so the two are never sent together.
    pub fn role_is_open(&self, role: AssetRole) -> bool {
        if !self.asset_constraints().accepts(role) || self.role_is_full(role) {
            return false;
        }
        if role.is_frame_input() {
            return !self.has_references();
        }
        if role.is_reference() {
            return !self.has_frames();
        }
        true
    }

    /// Where a dropped-in picture goes (`firstAvailableImageRole`): the first open role, frames
    /// before references. A lone image is a start frame on every model that takes one, because
    /// attaching a reference should be deliberate: one dropped by accident would move the whole
    /// request onto the reference endpoint. Audio is never an image's role.
    pub fn first_available_image_role(&self) -> Option<AssetRole> {
        let constraints = self.asset_constraints();
        let frames = [AssetRole::FirstFrame, AssetRole::LastFrame];
        let rest = constraints.allowed.keys().copied().filter(|r| !r.is_frame_input());
        frames
            .into_iter()
            .filter(|role| constraints.accepts(*role))
            .chain(rest)
            .find(|role| role.accepts_kind(MediaType::Image) && self.role_is_open(*role))
    }

    /// The handles the prompt can address, as `(role, 1-based index)` in the order the references
    /// were attached: `@Image1`, `@Image2`, `@Video1`. Empty unless the active tab is a video one
    /// whose model declares a reference list of that kind. Wan 2.7's lone audio slot is a
    /// conditioning track, not something a prompt can name.
    pub fn reference_handles(&self) -> Vec<(AssetRole, usize)> {
        let Some(references) = self.video_caps().and_then(|caps| caps.references) else { return Vec::new() };
        if self.tab != ComposeTab::Media(MediaType::Video) {
            return Vec::new();
        }
        let accepted = self.accepted_assets();
        let mut handles = Vec::new();
        for role in [AssetRole::ReferenceImage, AssetRole::ReferenceVideo, AssetRole::Audio] {
            if references.max_for(role) == 0 {
                continue;
            }
            let count = accepted.iter().filter(|a| a.role == role).count();
            handles.extend((1..=count).map(|index| (role, index)));
        }
        handles
    }

    /// Where a dropped-in video goes: the reference video slot, when the model has one with room.
    pub fn first_available_video_role(&self) -> Option<AssetRole> {
        self.role_is_open(AssetRole::ReferenceVideo).then_some(AssetRole::ReferenceVideo)
    }

    /// `false` when there is no tab for it, the model doesn't take the role, the role is full, or the
    /// same file is already there.
    pub fn add_asset(&mut self, asset: DraftAsset) -> bool {
        if !self.asset_constraints().accepts(asset.role) || self.role_is_full(asset.role) {
            return false;
        }
        let Some(tab) = self.assets.get_mut(self.tab) else { return false };
        if tab.contains(&asset) {
            return false;
        }
        tab.push(asset);
        true
    }

    /// `index` addresses the active tab's full list (`active_assets`), not the accepted subset.
    pub fn remove_asset(&mut self, index: usize) -> Option<DraftAsset> {
        let tab = self.assets.get_mut(self.tab)?;
        (index < tab.len()).then(|| tab.remove(index))
    }

    pub fn clear_active_assets(&mut self) {
        if let Some(tab) = self.assets.get_mut(self.tab) {
            tab.clear();
        }
    }

    // ----- recreate --------------------------------------------------------------------

    /// Port of `loadingRecreateSettings`: rebuild the request's tab from its stored settings and
    /// select it. An unsupported model falls back to the provider's default draft (the original
    /// settings are dropped); supported settings the model can't honour are clamped. Either way at
    /// most one warning. `assets` replace that tab's draft as stored; the other tabs keep their own,
    /// and audio has nowhere to keep them. A tool request opens its tool's tab with its model and
    /// its one input (one row is one image, whatever batch it was part of). The caller applies the
    /// prompt.
    pub fn load_recreate(&self, request: &Request, assets: Vec<DraftAsset>) -> RecreateOutcome {
        let provider = self.provider;
        let mut next = self.clone();
        let media_type = request.media_type();
        let tab = match request.generation_type.tool() {
            Some(tool) => ComposeTab::Tool(tool),
            None => ComposeTab::Media(media_type),
        };
        let warning = match &request.generation_type {
            GenerationType::Image(s) => match find_image_model(provider, &s.model) {
                Some((ix, caps)) => {
                    let built = ImageDraft {
                        model: ix,
                        aspect_ratio: caps.supports_aspect_ratio().then_some(s.aspect_ratio),
                        resolution: caps.supports_resolution().then_some(s.resolution),
                        count: self.image.count,
                    };
                    let mut coerced = built.clone();
                    coerce_image(&mut coerced, &caps);
                    let warning = (coerced != built).then_some(RecreateWarning::DefaultSettings { tab, model: s.model.name });
                    next.image = coerced;
                    warning
                }
                None => {
                    let Some(draft) = default_image_draft(provider) else { return RecreateOutcome::Unsupported(tab) };
                    let replacement = provider.supported_image_models.get(draft.model).map(|m| m.name).unwrap_or_default();
                    next.image = draft;
                    Some(RecreateWarning::DefaultModel { tab, original_model: s.model.name, replacement_model: replacement })
                }
            },
            GenerationType::Video(s) => match find_video_model(provider, &s.model) {
                Some((ix, caps)) => {
                    let built = VideoDraft { model: ix, aspect_ratio: s.aspect_ratio, resolution: s.resolution, duration: s.duration, audio: s.audio_enabled, count: self.video.count };
                    let mut coerced = built.clone();
                    coerce_video(&mut coerced, &caps);
                    let warning = (coerced != built).then_some(RecreateWarning::DefaultSettings { tab, model: s.model.name });
                    next.video = coerced;
                    warning
                }
                None => {
                    let Some(draft) = default_video_draft(provider) else { return RecreateOutcome::Unsupported(tab) };
                    let replacement = provider.supported_video_models.get(draft.model).map(|m| m.name).unwrap_or_default();
                    next.video = draft;
                    Some(RecreateWarning::DefaultModel { tab, original_model: s.model.name, replacement_model: replacement })
                }
            },
            GenerationType::Audio(s) => match find_audio_model(provider, &s.model) {
                Some((ix, caps)) => {
                    // Stored voices are refreshed from the catalog by id, so only a voice this
                    // provider doesn't have counts as a substitution.
                    let built = AudioDraft {
                        model: ix,
                        speaker1: Some(voice_by_id(&caps, &s.speaker1.id).unwrap_or_else(|| s.speaker1.clone())),
                        speaker2: s.speaker2.as_ref().map(|v| voice_by_id(&caps, &v.id).unwrap_or_else(|| v.clone())),
                    };
                    let mut coerced = built.clone();
                    coerce_audio(&mut coerced, &caps);
                    let warning = (coerced != built).then_some(RecreateWarning::DefaultSettings { tab, model: s.model.name });
                    next.audio = coerced;
                    warning
                }
                None => {
                    let Some(draft) = default_audio_draft(provider) else { return RecreateOutcome::Unsupported(tab) };
                    let replacement = provider.supported_audio_models.get(draft.model).map(|m| m.name).unwrap_or_default();
                    next.audio = draft;
                    Some(RecreateWarning::DefaultModel { tab, original_model: s.model.name, replacement_model: replacement })
                }
            },
            GenerationType::Upscale(s) | GenerationType::RemoveBackground(s) => {
                let ComposeTab::Tool(tool) = tab else { return RecreateOutcome::Unsupported(tab) };
                let models = provider.tool_models(tool);
                let Some(first) = models.first() else { return RecreateOutcome::Unsupported(tab) };
                let found = models.iter().position(|m| m.id == s.model.id);
                let draft = next.tools.get_mut(tool);
                draft.model = found.unwrap_or(0);
                // The settings come back only alongside the model that offered them; on a
                // substitution `coerce` would drop them anyway.
                if found.is_some() {
                    draft.upscale_factor = Some(s.upscale_factor);
                    draft.variant = s.variant.clone();
                }
                match found {
                    Some(_) => None,
                    None => Some(RecreateWarning::DefaultModel { tab, original_model: s.model.name, replacement_model: first.name }),
                }
            }
        };
        next.tab = tab;
        if let Some(tab) = next.assets.get_mut(next.tab) {
            *tab = assets;
        }
        RecreateOutcome::Loaded { state: Box::new(next), warning }
    }
}

// ----- drafts ---------------------------------------------------------------------------------

fn fresh_image_draft() -> ImageDraft {
    ImageDraft { model: 0, aspect_ratio: None, resolution: None, count: 1 }
}

fn fresh_video_draft() -> VideoDraft {
    VideoDraft { model: 0, aspect_ratio: None, resolution: None, duration: 5, audio: true, count: 1 }
}

fn fresh_audio_draft() -> AudioDraft {
    AudioDraft { model: 0, speaker1: None, speaker2: None }
}

/// The provider's first image model with its capability defaults (`defaultImageDraft`).
pub fn default_image_draft(provider: &ProviderDescriptor) -> Option<ImageDraft> {
    let caps = provider.image_capabilities(provider.supported_image_models.first()?)?;
    let mut draft = fresh_image_draft();
    coerce_image(&mut draft, &caps);
    Some(draft)
}

pub fn default_video_draft(provider: &ProviderDescriptor) -> Option<VideoDraft> {
    let caps = provider.video_capabilities(provider.supported_video_models.first()?)?;
    let mut draft = fresh_video_draft();
    coerce_video(&mut draft, &caps);
    Some(draft)
}

pub fn default_audio_draft(provider: &ProviderDescriptor) -> Option<AudioDraft> {
    let caps = provider.audio_capabilities(provider.supported_audio_models.first()?)?;
    let mut draft = fresh_audio_draft();
    coerce_audio(&mut draft, &caps);
    Some(draft)
}

fn find_image_model(provider: &ProviderDescriptor, model: &ImageModel) -> Option<(usize, ModelCapabilities)> {
    let ix = provider.supported_image_models.iter().position(|m| m.id == model.id)?;
    Some((ix, provider.image_capabilities(&provider.supported_image_models[ix])?))
}

fn find_video_model(provider: &ProviderDescriptor, model: &VideoModel) -> Option<(usize, VideoModelCapabilities)> {
    let ix = provider.supported_video_models.iter().position(|m| m.id == model.id)?;
    Some((ix, provider.video_capabilities(&provider.supported_video_models[ix])?))
}

fn find_audio_model(provider: &ProviderDescriptor, model: &AudioModel) -> Option<(usize, AudioModelCapabilities)> {
    let ix = provider.supported_audio_models.iter().position(|m| m.id == model.id)?;
    Some((ix, provider.audio_capabilities(&provider.supported_audio_models[ix])?))
}

fn voice_by_id(caps: &AudioModelCapabilities, id: &str) -> Option<AudioVoice> {
    caps.supported_voices.iter().find(|v| v.id == id).cloned()
}

/// `validImageDraft`.
fn coerce_image(draft: &mut ImageDraft, caps: &ModelCapabilities) {
    if !draft.aspect_ratio.map(|a| caps.supported_aspect_ratios.contains(&a)).unwrap_or(false) {
        draft.aspect_ratio = caps.default_aspect_ratio();
    }
    if !draft.resolution.map(|r| caps.supported_resolutions.contains(&r)).unwrap_or(false) {
        draft.resolution = caps.default_resolution();
    }
    draft.count = draft.count.clamp(1, MAX_COUNT);
}

/// `validVideoDraft`.
fn coerce_video(draft: &mut VideoDraft, caps: &VideoModelCapabilities) {
    if !draft.aspect_ratio.map(|a| caps.aspect_ratios.contains(&a)).unwrap_or(false) {
        draft.aspect_ratio = caps.default_aspect_ratio();
    }
    if !draft.resolution.map(|r| caps.resolutions.contains(&r)).unwrap_or(false) {
        draft.resolution = caps.default_resolution();
    }
    if !caps.duration_range.contains(draft.duration) {
        draft.duration = caps.default_duration();
    }
    draft.count = draft.count.clamp(1, MAX_COUNT);
    if !caps.supports_audio {
        draft.audio = false;
    } else if caps.audio_always_on {
        draft.audio = true;
    }
}

/// `validAudioDraft`. Voices are matched by id so an outdated copy of a known voice is refreshed
/// from the catalog rather than replaced by the default.
fn coerce_audio(draft: &mut AudioDraft, caps: &AudioModelCapabilities) {
    draft.speaker1 = match draft.speaker1.as_ref().and_then(|v| voice_by_id(caps, &v.id)) {
        Some(v) => Some(v),
        None => caps.default_voice.clone().or_else(|| caps.supported_voices.first().cloned()),
    };
    draft.speaker2 = if caps.supports_two_speakers {
        draft.speaker2.as_ref().and_then(|v| voice_by_id(caps, &v.id).or_else(|| caps.secondary_default_voice.clone()))
    } else {
        None
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_providers::catalog::{audio, image, tool, video};
    use majik_providers::{fal, mock, openrouter, replicate, ProviderId};

    fn state(provider: &'static ProviderDescriptor) -> ComposerState {
        ComposerState::new(provider, &ProviderDraft::default())
    }

    fn asset(id: &str, role: AssetRole) -> DraftAsset {
        DraftAsset { asset: AssetId(id.into()), role }
    }

    fn image_request(model: &ImageModel, aspect_ratio: AspectRatio, resolution: ImageResolution) -> Request {
        Request::new(ProviderId::fal(), GenerationType::Image(ImageGenerationSettings { model: model.clone(), aspect_ratio, resolution }), "p", vec![])
    }

    fn video_request(model: &VideoModel, duration: u32) -> Request {
        Request::new(
            ProviderId::fal(),
            GenerationType::Video(VideoGenerationSettings { model: model.clone(), aspect_ratio: Some(VideoAspectRatio::Landscape), resolution: None, duration, audio_enabled: true }),
            "p",
            vec![],
        )
    }

    fn audio_request(model: &AudioModel, speaker1: &str) -> Request {
        Request::new(ProviderId::fal(), GenerationType::Audio(AudioGenerationSettings { model: model.clone(), speaker1: AudioVoice::new(speaker1, speaker1), speaker2: None }), "p", vec![])
    }

    fn loaded(outcome: RecreateOutcome) -> (ComposerState, Option<RecreateWarning>) {
        match outcome {
            RecreateOutcome::Loaded { state, warning } => (*state, warning),
            RecreateOutcome::Unsupported(t) => panic!("unexpected Unsupported({t:?})"),
        }
    }

    fn select_image_model(s: &mut ComposerState, id: &str) {
        let ix = s.provider.supported_image_models.iter().position(|m| m.id == id).expect("model in provider");
        s.select_model(ix);
    }

    fn select_video_model(s: &mut ComposerState, id: &str) {
        let ix = s.provider.supported_video_models.iter().position(|m| m.id == id).expect("model in provider");
        s.select_model(ix);
    }

    // ----- recreate -----

    #[test]
    fn recreate_default_model_for_unsupported_audio_model() {
        let before = state(replicate::descriptor());
        let (after, warning) = loaded(before.load_recreate(&audio_request(&audio::GEMINI_25_PRO, "Kore"), vec![]));
        assert_eq!(after.tab, ComposeTab::Media(MediaType::Audio));
        assert_eq!(after.audio_model().unwrap().id, "elevenlabs-v3");
        assert_eq!(after.audio.speaker1.as_ref().unwrap().id, "Rachel");
        assert_eq!(warning, Some(RecreateWarning::DefaultModel { tab: ComposeTab::Media(MediaType::Audio), original_model: "Gemini 2.5 Pro", replacement_model: "ElevenLabs v3" }));
        assert_eq!(after.image, before.image);
        assert_eq!(after.video, before.video);
    }

    #[test]
    fn recreate_unsupported_modality_leaves_state_untouched() {
        let mut before = state(openrouter::descriptor());
        assert!(before.add_asset(asset("/tmp/a.png", AssetRole::ReferenceImage)));
        let outcome = before.load_recreate(&audio_request(&audio::ELEVEN_LABS_V3, "Rachel"), vec![asset("/tmp/x.wav", AssetRole::Audio)]);
        assert!(matches!(outcome, RecreateOutcome::Unsupported(ComposeTab::Media(MediaType::Audio))));
        assert_eq!(before.tab, ComposeTab::Media(MediaType::Image));
        assert_eq!(before.assets.image.len(), 1);
    }

    #[test]
    fn recreate_default_model_discards_original_params() {
        let before = state(replicate::descriptor());
        let (after, warning) = loaded(before.load_recreate(&video_request(&video::KLING_30_STANDARD, 10), vec![]));
        assert_eq!(after.video, default_video_draft(replicate::descriptor()).unwrap());
        assert!(matches!(warning, Some(RecreateWarning::DefaultModel { tab: ComposeTab::Media(MediaType::Video), original_model: "Kling 3.0 Standard", .. })));
    }

    #[test]
    fn recreate_warns_default_settings_when_coerced() {
        let before = state(fal::descriptor());
        let (after, warning) = loaded(before.load_recreate(&image_request(&image::GEMINI_3_PRO, AspectRatio::Landscape, ImageResolution::Sd), vec![]));
        assert_eq!(after.image.resolution, Some(ImageResolution::Hd));
        assert_eq!(after.image.aspect_ratio, Some(AspectRatio::Landscape));
        assert_eq!(warning, Some(RecreateWarning::DefaultSettings { tab: ComposeTab::Media(MediaType::Image), model: "Nano Banana Pro" }));

        let (after, warning) = loaded(before.load_recreate(&video_request(&video::SORA_2, 5), vec![]));
        assert_eq!(after.video.duration, 4);
        assert_eq!(warning, Some(RecreateWarning::DefaultSettings { tab: ComposeTab::Media(MediaType::Video), model: "Sora 2" }));
    }

    #[test]
    fn recreate_without_coercion_has_no_warning() {
        let before = state(fal::descriptor());
        let (after, warning) = loaded(before.load_recreate(&image_request(&image::GEMINI_3_PRO, AspectRatio::Landscape, ImageResolution::Hd), vec![]));
        assert_eq!(warning, None);
        assert_eq!(after.tab, ComposeTab::Media(MediaType::Image));
        assert_eq!(after.image_model().unwrap().id, "gemini-3-pro");
        assert_eq!((after.image.aspect_ratio, after.image.resolution), (Some(AspectRatio::Landscape), Some(ImageResolution::Hd)));
    }

    #[test]
    fn recreate_ignores_stored_resolution_when_model_has_none() {
        let before = state(fal::descriptor());
        let (after, warning) = loaded(before.load_recreate(&image_request(&image::GEMINI_25_FLASH, AspectRatio::Square, ImageResolution::Hd), vec![]));
        assert_eq!(warning, None);
        assert_eq!(after.image.resolution, None);
    }

    #[test]
    fn recreate_inherits_image_count_from_current_draft() {
        let mut before = state(fal::descriptor());
        before.image.count = 4;
        let (after, _) = loaded(before.load_recreate(&image_request(&image::GEMINI_3_PRO, AspectRatio::Square, ImageResolution::Hd), vec![]));
        assert_eq!(after.image.count, 4);
    }

    #[test]
    fn recreate_inherits_video_count_from_current_draft() {
        let mut before = state(fal::descriptor());
        before.video.count = 3;
        let (after, _) = loaded(before.load_recreate(&video_request(&video::KLING_30_STANDARD, 10), vec![]));
        assert_eq!(after.video.count, 3);
    }

    #[test]
    fn recreate_refreshes_known_voices_without_warning() {
        let before = state(fal::descriptor());
        let (after, warning) = loaded(before.load_recreate(&audio_request(&audio::ELEVEN_LABS_V3, "Roger"), vec![]));
        assert_eq!(warning, None);
        let speaker1 = after.audio.speaker1.unwrap();
        assert_eq!(speaker1.id, "Roger");
        assert!(speaker1.preview_url.is_some(), "the catalog copy of the voice, not the stored stub");
    }

    #[test]
    fn recreate_replaces_only_target_tab_unfiltered() {
        let mut before = state(fal::descriptor());
        assert!(before.add_asset(asset("/tmp/ref.png", AssetRole::ReferenceImage)));
        assert!(before.set_tab(ComposeTab::Media(MediaType::Video)));
        assert!(before.add_asset(asset("/tmp/first.png", AssetRole::FirstFrame)));

        let stored = vec![asset("/tmp/first2.png", AssetRole::FirstFrame), asset("/tmp/last2.png", AssetRole::LastFrame)];
        let (after, _) = loaded(before.load_recreate(&video_request(&video::VEO_31, 8), stored.clone()));
        assert_eq!(after.assets.video, stored);
        assert_eq!(after.assets.image, before.assets.image, "the image tab keeps its own draft");

        // A role the target model can't use is kept in the tab but not offered for sending.
        let mask = vec![asset("/tmp/mask.png", AssetRole::MaskImage)];
        let (after, _) = loaded(before.load_recreate(&image_request(&image::GEMINI_3_PRO, AspectRatio::Square, ImageResolution::Hd), mask.clone()));
        assert_eq!(after.assets.image, mask);
        assert!(after.accepted_assets().is_empty());

        // Audio has no tab: stored inputs are dropped, both tabs untouched.
        let (after, _) = loaded(before.load_recreate(&audio_request(&audio::ELEVEN_LABS_V3, "Rachel"), vec![asset("/tmp/x.wav", AssetRole::Audio)]));
        assert_eq!(after.assets, before.assets);
        assert!(after.active_assets().is_empty());
    }

    #[test]
    fn recreate_warning_messages() {
        let default_model = RecreateWarning::DefaultModel { tab: ComposeTab::Media(MediaType::Video), original_model: "Kling 3.0 Standard", replacement_model: "Veo 3.1" };
        assert_eq!(default_model.message("Replicate"), "Replicate doesn't support the original video model, Kling 3.0 Standard. Using Veo 3.1 instead.");
        let default_settings = RecreateWarning::DefaultSettings { tab: ComposeTab::Media(MediaType::Image), model: "Nano Banana Pro" };
        assert_eq!(default_settings.message("fal.ai"), "Some original image settings for Nano Banana Pro aren't available from fal.ai. Using supported defaults instead.");
        assert_eq!(unsupported_message("OpenRouter", ComposeTab::Media(MediaType::Audio)), "OpenRouter doesn't support audio generation, so this item can't be recreated with the current provider.");
    }

    fn tool_request(provider: ProviderId, model: &ToolModel) -> Request {
        Request::new(provider, GenerationType::for_tool_model(model), "", vec![])
    }

    #[test]
    fn recreate_upscale_selects_tool_tab_model_and_asset() {
        let mut before = state(replicate::descriptor());
        assert!(before.add_asset(asset("/tmp/ref.png", AssetRole::ReferenceImage)));
        let stored = vec![asset("/tmp/small.png", AssetRole::ReferenceImage)];
        let (after, warning) = loaded(before.load_recreate(&tool_request(ProviderId::replicate(), &tool::CLARITY_UPSCALER), stored.clone()));
        assert_eq!(warning, None);
        assert_eq!(after.tab, ComposeTab::Tool(ToolId::Upscale));
        assert_eq!(after.active_tool_model().map(|m| m.id), Some("clarity-upscaler"));
        assert_eq!(after.assets.upscale, stored, "the one input the row was made from");
        assert_eq!(after.assets.image, before.assets.image, "the image tab keeps its own draft");
        assert_eq!((after.image, after.video, after.audio), (before.image, before.video, before.audio), "media drafts untouched");
    }

    #[test]
    fn recreate_tool_with_unknown_model_warns_and_uses_the_providers_first() {
        let before = state(fal::descriptor());
        let (after, warning) = loaded(before.load_recreate(&tool_request(ProviderId::replicate(), &tool::CLARITY_UPSCALER), vec![]));
        assert_eq!(after.tab, ComposeTab::Tool(ToolId::Upscale));
        assert_eq!(after.active_tool_model().map(|m| m.id), Some("topaz-upscale"));
        assert_eq!(warning, Some(RecreateWarning::DefaultModel { tab: ComposeTab::Tool(ToolId::Upscale), original_model: "Clarity Upscaler", replacement_model: "Topaz Upscale" }));
        assert_eq!(warning.unwrap().message("fal.ai"), "fal.ai doesn't support the original upscale model, Clarity Upscaler. Using Topaz Upscale instead.");
    }

    #[test]
    fn recreate_tool_on_provider_without_it_is_unsupported() {
        let mut before = state(openrouter::descriptor());
        assert!(before.add_asset(asset("/tmp/a.png", AssetRole::ReferenceImage)));
        let outcome = before.load_recreate(&tool_request(ProviderId::fal(), &tool::BRIA_BACKGROUND_REMOVE), vec![asset("/tmp/b.png", AssetRole::ReferenceImage)]);
        assert!(matches!(outcome, RecreateOutcome::Unsupported(ComposeTab::Tool(ToolId::RemoveBackground))), "{outcome:?}");
        assert_eq!(before.tab, ComposeTab::Media(MediaType::Image));
        assert_eq!(before.assets.image.len(), 1);
        assert_eq!(
            unsupported_message("OpenRouter", ComposeTab::Tool(ToolId::RemoveBackground)),
            "OpenRouter doesn't support background removal, so this item can't be recreated with the current provider."
        );
    }

    // ----- assets -----

    #[test]
    fn coerce_and_model_switch_never_prune_assets() {
        let mut s = state(fal::descriptor());
        select_image_model(&mut s, "gpt-image-2");
        assert!(s.add_asset(asset("/tmp/ref.png", AssetRole::ReferenceImage)));
        assert!(s.add_asset(asset("/tmp/mask.png", AssetRole::MaskImage)));
        select_image_model(&mut s, "gemini-3-pro");
        assert_eq!(s.assets.image.len(), 2);
        assert_eq!(s.accepted_assets().len(), 1, "the mask is hidden under a model without a mask slot");
        assert!(s.set_tab(ComposeTab::Media(MediaType::Video)));
        assert!(s.set_tab(ComposeTab::Media(MediaType::Audio)));
        assert!(s.set_tab(ComposeTab::Media(MediaType::Image)));
        assert_eq!(s.assets.image.len(), 2);
        select_image_model(&mut s, "gpt-image-2");
        assert_eq!(s.accepted_assets().len(), 2, "switching back reveals the mask again");
    }

    #[test]
    fn tab_assets_are_isolated_and_audio_swallows_writes() {
        let mut s = state(fal::descriptor());
        assert!(s.add_asset(asset("/tmp/ref.png", AssetRole::ReferenceImage)));
        assert!(s.set_tab(ComposeTab::Media(MediaType::Video)));
        assert!(s.active_assets().is_empty());
        assert!(s.add_asset(asset("/tmp/first.png", AssetRole::FirstFrame)));
        assert!(s.set_tab(ComposeTab::Media(MediaType::Image)));
        assert_eq!(s.active_assets(), &[asset("/tmp/ref.png", AssetRole::ReferenceImage)]);
        assert!(s.set_tab(ComposeTab::Media(MediaType::Audio)));
        assert!(s.active_assets().is_empty());
        assert!(!s.add_asset(asset("/tmp/x.wav", AssetRole::Audio)));
        assert_eq!(s.remove_asset(0), None);
        s.clear_active_assets();
        assert_eq!(s.assets.image.len(), 1);
        assert_eq!(s.assets.video.len(), 1);
    }

    #[test]
    fn set_provider_resets_drafts_but_keeps_assets() {
        let mut s = state(fal::descriptor());
        s.image.count = 3;
        assert!(s.add_asset(asset("/tmp/ref.png", AssetRole::ReferenceImage)));
        let assets = s.assets.clone();
        s.set_provider(mock::descriptor(), &ProviderDraft::default());
        assert_eq!(s.provider.id, ProviderId::mock());
        assert_eq!(s.image.count, 1);
        assert_eq!(s.assets, assets);
    }

    #[test]
    fn first_available_image_role_follows_declaration_order_and_capacity() {
        let mut s = state(fal::descriptor());
        assert!(s.set_tab(ComposeTab::Media(MediaType::Video)));
        select_video_model(&mut s, "veo-3.1");
        assert_eq!(s.first_available_image_role(), Some(AssetRole::FirstFrame));
        assert!(s.add_asset(asset("/tmp/a.png", AssetRole::FirstFrame)));
        assert_eq!(s.first_available_image_role(), Some(AssetRole::LastFrame));
        assert!(s.add_asset(asset("/tmp/b.png", AssetRole::LastFrame)));
        assert_eq!(s.first_available_image_role(), None);
        select_video_model(&mut s, "wan-2.7");
        assert_eq!(s.first_available_image_role(), None, "the audio slot never takes an image");

        assert!(s.set_tab(ComposeTab::Media(MediaType::Image)));
        select_image_model(&mut s, "gpt-5-image");
        assert_eq!(s.first_available_image_role(), Some(AssetRole::ReferenceImage));
        assert!(s.add_asset(asset("/tmp/c.png", AssetRole::ReferenceImage)));
        assert_eq!(s.first_available_image_role(), Some(AssetRole::MaskImage));

        assert!(s.set_tab(ComposeTab::Media(MediaType::Audio)));
        assert_eq!(s.first_available_image_role(), None);
    }

    #[test]
    fn add_asset_caps_by_range_and_dedups_by_path() {
        let mut s = state(fal::descriptor());
        select_image_model(&mut s, "flux-2-pro");
        assert!(s.add_asset(asset("/tmp/a.png", AssetRole::ReferenceImage)));
        assert!(!s.add_asset(asset("/tmp/a.png", AssetRole::ReferenceImage)), "same file twice");
        assert!(!s.add_asset(asset("/tmp/b.png", AssetRole::ReferenceImage)), "role is full at 1");
        assert!(!s.add_asset(asset("/tmp/m.png", AssetRole::MaskImage)), "model has no mask slot");
        assert!(s.role_is_full(AssetRole::ReferenceImage));
        assert_eq!(s.remove_asset(0).map(|a| a.role), Some(AssetRole::ReferenceImage));
        assert!(!s.role_is_full(AssetRole::ReferenceImage));
    }

    #[test]
    fn clear_active_assets_only_clears_current_tab() {
        let mut s = state(fal::descriptor());
        assert!(s.add_asset(asset("/tmp/ref.png", AssetRole::ReferenceImage)));
        assert!(s.set_tab(ComposeTab::Media(MediaType::Video)));
        assert!(s.add_asset(asset("/tmp/first.png", AssetRole::FirstFrame)));
        s.clear_active_assets();
        assert!(s.assets.video.is_empty());
        assert_eq!(s.assets.image.len(), 1);
    }

    #[test]
    fn new_picks_first_supported_type_and_restores_draft_type() {
        let s = state(openrouter::descriptor());
        assert_eq!(s.tab, ComposeTab::Media(MediaType::Image));
        assert_eq!(s.supported_types(), vec![MediaType::Image]);
        let draft = ProviderDraft { media_type: Some("video".into()), ..Default::default() };
        assert_eq!(ComposerState::new(openrouter::descriptor(), &draft).tab, ComposeTab::Media(MediaType::Image), "unsupported stored type is ignored");
        assert_eq!(ComposerState::new(fal::descriptor(), &draft).tab, ComposeTab::Media(MediaType::Video));
    }

    // ----- tool tabs -----

    #[test]
    fn tool_tabs_appear_only_for_supporting_providers() {
        let expected = [
            ComposeTab::Media(MediaType::Image),
            ComposeTab::Media(MediaType::Video),
            ComposeTab::Media(MediaType::Audio),
            ComposeTab::Tool(ToolId::Upscale),
            ComposeTab::Tool(ToolId::RemoveBackground),
        ];
        assert_eq!(state(fal::descriptor()).supported_tabs(), expected);
        assert_eq!(state(replicate::descriptor()).supported_tabs(), expected);
        assert_eq!(state(openrouter::descriptor()).supported_tabs(), vec![ComposeTab::Media(MediaType::Image)]);
        let mut s = state(openrouter::descriptor());
        assert!(!s.set_tab(ComposeTab::Tool(ToolId::Upscale)));
        assert_eq!(s.tab, ComposeTab::Media(MediaType::Image));
    }

    #[test]
    fn compose_tab_raw_round_trips() {
        for tab in state(fal::descriptor()).supported_tabs() {
            assert_eq!(ComposeTab::from_raw(tab.raw()), Some(tab));
        }
        assert_eq!(ComposeTab::from_raw("upscale"), Some(ComposeTab::Tool(ToolId::Upscale)));
        assert_eq!(ComposeTab::from_raw("nope"), None, "unknown keys don't silently become Image");
    }

    #[test]
    fn tool_tab_constraints_take_one_to_ten_reference_images() {
        let mut s = state(mock::descriptor());
        assert!(s.set_tab(ComposeTab::Tool(ToolId::Upscale)));
        let constraints = s.asset_constraints();
        assert_eq!(constraints.allowed.len(), 1);
        assert_eq!(constraints.range(AssetRole::ReferenceImage), Some(&(1..=TOOL_MAX_IMAGES)));
        assert!(constraints.validate(&[]).is_err(), "at least one image");
        assert!(constraints.validate(&[AssetRole::ReferenceImage; TOOL_MAX_IMAGES]).is_ok());
        assert!(s.prompt_optional());
        for i in 0..TOOL_MAX_IMAGES {
            assert_eq!(s.first_available_image_role(), Some(AssetRole::ReferenceImage));
            assert!(s.add_asset(asset(&format!("/tmp/{i}.png"), AssetRole::ReferenceImage)));
        }
        assert!(!s.add_asset(asset("/tmp/eleven.png", AssetRole::ReferenceImage)), "the eleventh is refused");
        assert!(s.role_is_full(AssetRole::ReferenceImage));
        assert_eq!(s.first_available_image_role(), None);
        assert_eq!(s.accepted_assets().len(), TOOL_MAX_IMAGES);
    }

    #[test]
    fn tool_tab_assets_are_isolated_from_media_tabs() {
        let mut s = state(mock::descriptor());
        assert!(s.add_asset(asset("/tmp/ref.png", AssetRole::ReferenceImage)));
        assert!(s.set_tab(ComposeTab::Tool(ToolId::Upscale)));
        assert!(s.active_assets().is_empty());
        assert!(s.add_asset(asset("/tmp/up.png", AssetRole::ReferenceImage)));
        assert!(s.set_tab(ComposeTab::Tool(ToolId::RemoveBackground)));
        assert!(s.active_assets().is_empty());
        assert!(s.add_asset(asset("/tmp/bg.png", AssetRole::ReferenceImage)));
        s.clear_active_assets();
        assert!(s.assets.remove_background.is_empty());
        assert_eq!(s.assets.upscale.len(), 1);
        assert_eq!(s.assets.image.len(), 1);
        // Switching provider keeps every tab's inputs, tool tabs included.
        s.set_provider(replicate::descriptor(), &ProviderDraft::default());
        assert_eq!(s.assets.upscale.len(), 1);
        assert_eq!(s.assets.image.len(), 1);
    }

    #[test]
    fn tool_model_index_clamps_to_available_models() {
        let mut s = state(mock::descriptor());
        assert!(s.set_tab(ComposeTab::Tool(ToolId::Upscale)));
        assert_eq!(s.active_tool_model().map(|m| m.id), Some("mock-upscale"));
        s.select_model(5);
        assert_eq!(s.model_index(), 0, "an out-of-range selection falls back to the first model");
        assert_eq!(s.tool_model(ToolId::Upscale).map(|m| m.id), Some("mock-upscale"));
        assert_eq!(s.tool_model(ToolId::RemoveBackground).map(|m| m.id), Some("mock-remove-background"));
        assert_eq!(state(openrouter::descriptor()).tool_model(ToolId::Upscale), None);
    }

    #[test]
    fn video_tab_has_its_own_count() {
        let mut s = state(mock::descriptor());
        s.image.count = 4;
        s.video.count = 2;
        assert!(s.set_tab(ComposeTab::Media(MediaType::Video)));
        assert_eq!(s.count(), 2, "the video tab multiplies by the video count");
        *s.count_mut().unwrap() = 5;
        assert_eq!(s.video.count, 5, "count_mut edits the active tab's count");
        assert_eq!(s.image.count, 4, "the image count is untouched");
        assert!(s.set_tab(ComposeTab::Media(MediaType::Audio)));
        assert_eq!(s.count(), 1);
        assert!(s.count_mut().is_none(), "audio has no count");
    }

    #[test]
    fn video_count_clamps_1_to_8() {
        let mut s = state(mock::descriptor());
        s.video.count = 0;
        s.coerce();
        assert_eq!(s.video.count, 1);
        s.video.count = 99;
        s.coerce();
        assert_eq!(s.video.count, MAX_COUNT);
    }

    #[test]
    fn video_count_round_trips_through_the_draft() {
        let mut s = state(mock::descriptor());
        s.video.count = 3;
        let draft = s.to_draft();
        assert_eq!(draft.video.count, Some(3));
        let mut restored = state(mock::descriptor());
        restored.restore(&draft);
        assert_eq!(restored.video.count, 3);
    }

    #[test]
    fn tool_tab_has_no_generation_type_and_count_one() {
        let mut s = state(mock::descriptor());
        s.image.count = 4;
        assert!(s.set_tab(ComposeTab::Tool(ToolId::Upscale)));
        assert!(s.generation_type().is_none());
        assert_eq!(s.count(), 1);
        assert!(s.active_tool_model().is_some());
        assert!(s.set_tab(ComposeTab::Media(MediaType::Image)));
        assert_eq!(s.count(), 4);
        assert!(s.active_tool_model().is_none());
    }

    #[test]
    fn draft_round_trips_tool_tab_and_model() {
        let mut s = state(fal::descriptor());
        assert!(s.set_tab(ComposeTab::Tool(ToolId::RemoveBackground)));
        let draft = s.to_draft();
        assert_eq!(draft.media_type.as_deref(), Some("removeBackground"));
        assert_eq!(draft.upscale.model_id.as_deref(), Some("topaz-upscale"));
        assert_eq!(draft.remove_background.model_id.as_deref(), Some("bria-background-remove"));
        let restored = ComposerState::new(fal::descriptor(), &draft);
        assert_eq!(restored.tab, ComposeTab::Tool(ToolId::RemoveBackground));
        assert_eq!(restored.active_tool_model().map(|m| m.id), Some("bria-background-remove"));
        assert_eq!(ComposerState::new(openrouter::descriptor(), &draft).tab, ComposeTab::Media(MediaType::Image), "a provider without the tool can't show its tab");
        let stale = ProviderDraft { media_type: Some("upscale".into()), upscale: ToolDraftState { model_id: Some("gone".into()), ..Default::default() }, ..Default::default() };
        let restored = ComposerState::new(replicate::descriptor(), &stale);
        assert_eq!(restored.tab, ComposeTab::Tool(ToolId::Upscale));
        assert_eq!(restored.active_tool_model().map(|m| m.id), Some("clarity-upscaler"), "an unknown model id means the provider's first");
    }

    #[test]
    fn recreate_from_tool_tab_switches_to_media_tab() {
        let mut s = state(fal::descriptor());
        assert!(s.set_tab(ComposeTab::Tool(ToolId::Upscale)));
        assert!(s.add_asset(asset("/tmp/up.png", AssetRole::ReferenceImage)));
        let (after, _) = loaded(s.load_recreate(&image_request(&image::GEMINI_3_PRO, AspectRatio::Square, ImageResolution::Hd), vec![asset("/tmp/r.png", AssetRole::ReferenceImage)]));
        assert_eq!(after.tab, ComposeTab::Media(MediaType::Image));
        assert_eq!(after.assets.image.len(), 1);
        assert_eq!(after.assets.upscale.len(), 1, "the tool tab keeps its draft");
    }

    // ----- pricing -------------------------------------------------------------------
    //
    // Mock's prices are synthetic and fixed ($0.01/image, $0.10/s video, $0.15/s with audio,
    // $0.0001/character, $0.02/tool run), so these assert the wiring and the arithmetic without
    // pinning a real provider's figures.

    fn micros(estimate: Estimate) -> Option<u64> {
        estimate.amount().map(|usd| usd.0)
    }

    #[test]
    fn unit_price_prices_the_selected_image_model() {
        let mut s = state(mock::descriptor());
        select_image_model(&mut s, "flux-2-pro");
        assert_eq!(micros(s.unit_price(0, ToolInput::default())), Some(10_000));
    }

    #[test]
    fn unit_price_is_unknown_for_a_model_the_provider_has_no_price_for() {
        let mut s = state(mock::descriptor());
        select_image_model(&mut s, mock::pricing::UNPRICED_MODEL_ID);
        assert_eq!(s.unit_price(0, ToolInput::default()), Estimate::Unknown);
    }

    #[test]
    fn unit_price_scales_video_with_duration_and_the_audio_toggle() {
        let mut s = state(mock::descriptor());
        s.set_tab(ComposeTab::Media(MediaType::Video));
        select_video_model(&mut s, "veo-3.1");
        s.video.duration = 8;
        s.video.audio = false;
        assert_eq!(micros(s.unit_price(0, ToolInput::default())), Some(800_000), "8 s at $0.10/s");
        s.video.audio = true;
        assert_eq!(micros(s.unit_price(0, ToolInput::default())), Some(1_200_000), "audio moves it to $0.15/s");
        s.video.duration = 4;
        assert_eq!(micros(s.unit_price(0, ToolInput::default())), Some(600_000), "half the duration, half the price");
    }

    #[test]
    fn unit_price_scales_audio_with_the_text_length() {
        let mut s = state(mock::descriptor());
        s.set_tab(ComposeTab::Media(MediaType::Audio));
        assert_eq!(micros(s.unit_price(0, ToolInput::default())), Some(0));
        assert_eq!(micros(s.unit_price(1_000, ToolInput::default())), Some(100_000));
        assert_eq!(micros(s.unit_price(2_000, ToolInput::default())), Some(200_000));
    }

    #[test]
    fn unit_price_prices_a_tool_tab_that_has_no_generation_type() {
        let mut s = state(mock::descriptor());
        s.set_tab(ComposeTab::Tool(ToolId::Upscale));
        assert!(s.generation_type().is_none(), "tools don't go through a generation request");
        assert_eq!(micros(s.unit_price(0, ToolInput::default())), Some(20_000));
        s.set_tab(ComposeTab::Tool(ToolId::RemoveBackground));
        assert_eq!(micros(s.unit_price(0, ToolInput::default())), Some(20_000));
    }
}
