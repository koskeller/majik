//! The [`Request`] type every generation is made from, and the batch expansion behind it.

use majik_core::model::{MediaType, ToolId};
use majik_providers::{
    AssetRole, AudioGenerationSettings, ImageGenerationSettings, ProviderAsset, ProviderDescriptor, ProviderId, ToolModel, VideoGenerationSettings,
};
use serde::{Deserialize, Serialize};

/// What a tool request carries besides its one input image: the implementation to run it with.
/// The clients take no parameters yet; when one gains an upscale factor or the like, it lives here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSettings {
    pub model: ToolModel,
}

/// The operation a request asks for and its settings: a generation of one media type, or a tool
/// over one image. The `kind` tag of the tool variants is the raw value of the matching [`ToolId`],
/// so a stored request and the row's `tool` column say the same thing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
// Variants differ in size (audio carries two voices); it's one value per request, so boxing to
// equalize would only complicate the serde representation stored in `request_json`.
#[allow(clippy::large_enum_variant)]
pub enum GenerationType {
    Image(ImageGenerationSettings),
    Video(VideoGenerationSettings),
    Audio(AudioGenerationSettings),
    Upscale(ToolSettings),
    #[serde(rename = "removeBackground")]
    RemoveBackground(ToolSettings),
}

impl GenerationType {
    /// The tool variant `model` implements.
    pub fn for_tool_model(model: &ToolModel) -> Self {
        let settings = ToolSettings { model: model.clone() };
        match model.kind {
            ToolId::Upscale => GenerationType::Upscale(settings),
            ToolId::RemoveBackground => GenerationType::RemoveBackground(settings),
        }
    }

    /// Tools produce an image from an image.
    pub fn media_type(&self) -> MediaType {
        match self {
            GenerationType::Image(_) | GenerationType::Upscale(_) | GenerationType::RemoveBackground(_) => MediaType::Image,
            GenerationType::Video(_) => MediaType::Video,
            GenerationType::Audio(_) => MediaType::Audio,
        }
    }

    /// The app tool this request runs, if it is one.
    pub fn tool(&self) -> Option<ToolId> {
        match self {
            GenerationType::Upscale(_) => Some(ToolId::Upscale),
            GenerationType::RemoveBackground(_) => Some(ToolId::RemoveBackground),
            GenerationType::Image(_) | GenerationType::Video(_) | GenerationType::Audio(_) => None,
        }
    }

    pub fn tool_settings(&self) -> Option<&ToolSettings> {
        match self {
            GenerationType::Upscale(s) | GenerationType::RemoveBackground(s) => Some(s),
            GenerationType::Image(_) | GenerationType::Video(_) | GenerationType::Audio(_) => None,
        }
    }

    /// Whether the request has a prompt to write or show (tools run over their input alone).
    pub fn takes_prompt(&self) -> bool {
        self.tool().is_none()
    }

    pub fn model_id(&self) -> &str {
        match self {
            GenerationType::Image(s) => s.model.id,
            GenerationType::Video(s) => s.model.id,
            GenerationType::Audio(s) => s.model.id,
            GenerationType::Upscale(s) | GenerationType::RemoveBackground(s) => s.model.id,
        }
    }

    pub fn model_name(&self) -> &str {
        match self {
            GenerationType::Image(s) => s.model.name,
            GenerationType::Video(s) => s.model.name,
            GenerationType::Audio(s) => s.model.name,
            GenerationType::Upscale(s) | GenerationType::RemoveBackground(s) => s.model.name,
        }
    }
}

/// An input file as captured by the composer. Bytes are not serialized with the request; the
/// library stores them as content-addressed assets.
#[derive(Clone, PartialEq, Eq)]
pub struct AssetInput {
    pub role: AssetRole,
    pub content_type: String,
    pub data: Vec<u8>,
    pub attributes: Option<Vec<u8>>,
}

impl std::fmt::Debug for AssetInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetInput").field("role", &self.role).field("content_type", &self.content_type).field("bytes", &self.data.len()).finish()
    }
}

impl AssetInput {
    pub fn new(role: AssetRole, content_type: impl Into<String>, data: Vec<u8>) -> Self {
        Self { role, content_type: content_type.into(), data, attributes: None }
    }

    pub fn to_provider_asset(&self) -> ProviderAsset {
        ProviderAsset { role: self.role, media_type: self.content_type.clone(), data: self.data.clone(), attributes: self.attributes.clone() }
    }
}

/// Everything needed to (re)run one generation. Serialized (without asset bytes) into the
/// library's `request_json` so "Recreate" works after relaunch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub provider: ProviderId,
    #[serde(flatten)]
    pub generation_type: GenerationType,
    pub prompt: String,
    #[serde(skip)]
    pub assets: Vec<AssetInput>,
}

impl Request {
    pub fn new(provider: ProviderId, generation_type: GenerationType, prompt: impl Into<String>, assets: Vec<AssetInput>) -> Self {
        Self { provider, generation_type, prompt: prompt.into(), assets }
    }

    /// A tool run with `model` over one image (an empty prompt: tools take none). The variant
    /// follows the model's kind, so a request can't name a model of the wrong tool.
    pub fn tool(provider: ProviderId, model: &ToolModel, image: AssetInput) -> Self {
        Self::new(provider, GenerationType::for_tool_model(model), "", vec![image])
    }

    pub fn media_type(&self) -> MediaType {
        self.generation_type.media_type()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

/// `count` requests for the one prompt the composer holds. Returns an empty list when the model is
/// unsupported or there is nothing to generate. Tools don't go through here: they take no prompt
/// and run one request per image ([`Request::tool`]).
pub fn build_requests(prompt_text: &str, assets: &[AssetInput], generation_type: GenerationType, provider: &ProviderDescriptor, count: usize) -> Vec<Request> {
    let prompt = prompt_text.trim();

    let prompt_optional = match &generation_type {
        GenerationType::Image(s) => match provider.image_capabilities(&s.model) {
            Some(c) => c.prompt_optional,
            None => return Vec::new(),
        },
        GenerationType::Video(s) => match provider.video_capabilities(&s.model) {
            Some(c) => c.prompt_optional,
            None => return Vec::new(),
        },
        GenerationType::Audio(s) => {
            if provider.audio_capabilities(&s.model).is_none() {
                return Vec::new();
            }
            false
        }
        GenerationType::Upscale(_) | GenerationType::RemoveBackground(_) => return Vec::new(),
    };

    // A model that takes assets instead of a prompt still generates from them alone.
    if prompt.is_empty() && !(prompt_optional && !assets.is_empty()) {
        return Vec::new();
    }

    (0..count.max(1)).map(|_| Request::new(provider.id.clone(), generation_type.clone(), prompt, assets.to_vec())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_providers::catalog;

    fn png() -> AssetInput {
        AssetInput::new(AssetRole::ReferenceImage, "image/png", vec![0x89, b'P', b'N', b'G'])
    }

    #[test]
    fn tool_request_round_trips_through_json() {
        let request = Request::tool(ProviderId::mock(), &catalog::tool::MOCK_UPSCALE, png());
        assert_eq!(request.generation_type.tool(), Some(ToolId::Upscale));
        assert_eq!(request.media_type(), MediaType::Image);
        assert!(!request.generation_type.takes_prompt());
        let json = request.to_json();
        assert!(json.contains(r#""kind":"upscale""#), "{json}");
        assert!(json.contains(r#""model":"mock-upscale""#), "{json}");
        assert!(json.contains(r#""prompt":"""#), "{json}");
        let parsed = Request::from_json(&json).expect("parses");
        assert_eq!(parsed.generation_type, request.generation_type);
        assert!(parsed.assets.is_empty(), "asset bytes are not stored");
        assert_eq!(parsed.generation_type.model_name(), "Mock Upscale");
    }

    #[test]
    fn remove_background_kind_is_the_tool_columns_raw_value() {
        let request = Request::tool(ProviderId::mock(), &catalog::tool::MOCK_REMOVE_BACKGROUND, png());
        assert_eq!(request.generation_type.tool(), Some(ToolId::RemoveBackground));
        let json = request.to_json();
        assert!(json.contains(r#""kind":"removeBackground""#), "{json}");
        assert_eq!(Request::from_json(&json).map(|r| r.generation_type), Some(request.generation_type));
    }

    #[test]
    fn unknown_tool_model_does_not_parse() {
        assert!(Request::from_json(r#"{"provider":"Mock","kind":"upscale","model":"gone","prompt":""}"#).is_none());
    }

    #[test]
    fn tools_are_not_expanded_by_build_requests() {
        let gt = GenerationType::for_tool_model(&catalog::tool::MOCK_UPSCALE);
        assert!(build_requests("p", &[png()], gt, majik_providers::mock::descriptor(), 3).is_empty());
    }

    fn image_type() -> GenerationType {
        use majik_providers::{AspectRatio, ImageGenerationSettings, ImageResolution};
        let model = catalog::image::ALL.first().expect("catalog populated").clone();
        GenerationType::Image(ImageGenerationSettings { model, aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd })
    }

    #[test]
    fn one_request_per_count_all_carrying_the_same_prompt() {
        let provider = majik_providers::mock::descriptor();
        let requests = build_requests("  a red apple  ", &[], image_type(), provider, 3);
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|r| r.prompt == "a red apple"), "the prompt is trimmed and shared");
        assert_eq!(build_requests("a red apple", &[], image_type(), provider, 0).len(), 1, "a count of zero still makes one");
    }

    #[test]
    fn separators_are_ordinary_prompt_text() {
        let provider = majik_providers::mock::descriptor();
        let requests = build_requests("one\n===\ntwo", &[], image_type(), provider, 1);
        assert_eq!(requests.len(), 1, "=== no longer splits a prompt");
        assert_eq!(requests[0].prompt, "one\n===\ntwo");
    }

    #[test]
    fn a_blank_prompt_generates_only_when_the_model_takes_assets_instead() {
        use majik_providers::VideoGenerationSettings;
        let provider = majik_providers::mock::descriptor();
        assert!(build_requests("   ", &[], image_type(), provider, 2).is_empty(), "nothing to generate from");
        assert!(build_requests("   ", &[png()], image_type(), provider, 2).is_empty(), "this model still wants a prompt");

        // Happy Horse animates a first frame with no prompt at all.
        let optional = GenerationType::Video(VideoGenerationSettings {
            model: catalog::video::HAPPY_HORSE_10.clone(),
            aspect_ratio: None,
            resolution: None,
            duration: 5,
            audio_enabled: false,
        });
        assert!(build_requests("", &[], optional.clone(), provider, 2).is_empty(), "no prompt and no asset makes nothing");
        let frame = AssetInput::new(AssetRole::FirstFrame, "image/png", vec![0x89, b'P', b'N', b'G']);
        let from_asset = build_requests("  ", &[frame], optional, provider, 2);
        assert_eq!(from_asset.len(), 2);
        assert!(from_asset.iter().all(|r| r.prompt.is_empty()));
    }
}
