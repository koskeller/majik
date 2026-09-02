//! Pre-flight validation of a request, with no blanket image / video prompt caps: a prompt is
//! only length-checked here when the model's capabilities declare a cap; otherwise the provider
//! rejects an overlong prompt itself and the row fails with its message.

use majik_core::model::{MediaType, ToolId};
use majik_providers::references::{self, ReferenceCounts};
use majik_providers::settings::VideoGenerationSettings;
use majik_providers::{AssetRole, ProviderDescriptor};
use thiserror::Error;

use crate::request::{AssetInput, GenerationType, Request};

pub const MAX_IMAGE_ASSET_BYTES: usize = 10_000_000;
pub const MAX_AUDIO_ASSET_BYTES: usize = 15_000_000;
/// The lowest cap any provider puts on a reference video (Seedance takes 200 MB, Wan 100 MB).
pub const MAX_VIDEO_ASSET_BYTES: usize = 100_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    fn display_name(self) -> &'static str {
        match self {
            MediaKind::Image => "Image",
            MediaKind::Video => "Video",
            MediaKind::Audio => "Audio",
        }
    }
    fn lowercase_name(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio file",
        }
    }
    fn limit_bytes(self) -> usize {
        match self {
            MediaKind::Image => MAX_IMAGE_ASSET_BYTES,
            MediaKind::Video => MAX_VIDEO_ASSET_BYTES,
            MediaKind::Audio => MAX_AUDIO_ASSET_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("Prompt is too long. {} prompts must be {limit} characters or fewer.", media_type.label())]
    PromptTooLong { media_type: MediaType, limit: usize },
    #[error("{} is too large. Use an {} under {}.", media_kind.display_name(), media_kind.lowercase_name(), format_file_size(*limit_bytes))]
    AssetTooLarge { media_kind: MediaKind, limit_bytes: usize },
    #[error("{} must be {allowed_formats} and {} or smaller.", media_kind.display_name(), format_file_size(media_kind.limit_bytes()))]
    UnsupportedFormat { media_kind: MediaKind, allowed_formats: &'static str },
    #[error("{} is missing file data. Remove it and attach the file again.", role.display_name())]
    MissingAssetData { role: AssetRole },
    #[error("Model {0} is not available for validation.")]
    UnsupportedModel(String),
    #[error("{model} does not take reference inputs. Remove them, or pick a model that does.")]
    ReferencesUnsupported { model: String },
    #[error("References and a start or end frame can't be used together.")]
    ReferencesWithFrames,
    #[error("{model} takes at most {max} {} reference(s).", role.display_name().to_lowercase())]
    TooManyReferences { model: String, role: AssetRole, max: usize },
    #[error("{model} takes at most {max} references in total.")]
    TooManyReferencesTotal { model: String, max: usize },
    #[error("An audio reference needs at least one image or video reference.")]
    AudioReferenceNeedsVisual,
    #[error("The prompt mentions {handle}, but {attached} {} reference(s) are attached.", role.display_name().to_lowercase())]
    UnknownHandle { handle: String, role: AssetRole, attached: usize },
    #[error("{model} renders references at {allowed} only.")]
    ReferenceResolution { model: String, allowed: String },
    #[error("{model} takes reference videos of {max_secs} seconds or shorter. Trim the clip, or pick a model that takes longer ones.")]
    ReferenceVideoTooLong { model: String, max_secs: u32 },
    #[error("{model} takes reference videos of {min_secs} seconds or longer. Attach a longer clip, or pick a model that takes shorter ones.")]
    ReferenceVideoTooShort { model: String, min_secs: u32 },
    #[error("{model} needs a reference video to go with reference images. Attach a clip, or use the images as a start or end frame.")]
    ReferenceNeedsVideo { model: String },
}

/// Mirrors `ByteCountFormatter` output for the limits we use ("10 MB", "15 MB").
pub fn format_file_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["bytes", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} bytes")
    } else if (v - v.round()).abs() < 0.05 {
        format!("{} {}", v.round() as u64, UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Validates every request of one submit (the same prompt, `count` times).
pub fn validate_requests(requests: &[Request], provider: &ProviderDescriptor) -> Result<(), ValidationError> {
    for request in requests {
        validate_request(request, provider)?;
    }
    Ok(())
}

pub fn validate_request(request: &Request, provider: &ProviderDescriptor) -> Result<(), ValidationError> {
    if let Some(tool) = request.generation_type.tool() {
        validate_tool_request(request, tool)?;
    }
    if let Some(limit) = prompt_character_limit(&request.generation_type, provider)? {
        if request.prompt.chars().count() > limit {
            return Err(ValidationError::PromptTooLong { media_type: request.media_type(), limit });
        }
    }
    for asset in &request.assets {
        validate_asset(asset)?;
    }
    if let GenerationType::Video(settings) = &request.generation_type {
        validate_references(request, settings, provider)?;
    }
    Ok(())
}

/// Everything a provider would reject about the reference lists, caught before the row is queued so
/// the composer can say it in a sentence. The provider clients check the same rules.
fn validate_references(request: &Request, settings: &VideoGenerationSettings, provider: &ProviderDescriptor) -> Result<(), ValidationError> {
    let Some(caps) = provider.video_capabilities(&settings.model) else {
        return Err(ValidationError::UnsupportedModel(settings.model.name.to_string()));
    };
    let model = settings.model.name.to_string();
    let count = |role: AssetRole| request.assets.iter().filter(|a| a.role == role).count();
    let counts = ReferenceCounts { images: count(AssetRole::ReferenceImage), videos: count(AssetRole::ReferenceVideo), audio: count(AssetRole::Audio) };
    // An audio asset is a reference only where the model has a reference audio list; on Wan 2.7 it
    // is the conditioning track the i2v endpoint takes.
    let audio_is_reference = caps.references.is_some_and(|r| r.audio > 0);
    let counts = ReferenceCounts { audio: if audio_is_reference { counts.audio } else { 0 }, ..counts };
    if counts.is_empty() {
        return Ok(());
    }
    let Some(references) = caps.references else {
        return Err(ValidationError::ReferencesUnsupported { model });
    };
    if request.assets.iter().any(|a| a.role.is_frame_input()) {
        return Err(ValidationError::ReferencesWithFrames);
    }
    for role in [AssetRole::ReferenceImage, AssetRole::ReferenceVideo, AssetRole::Audio] {
        let attached = counts.of(role);
        if attached > references.max_for(role) {
            return Err(ValidationError::TooManyReferences { model, role, max: references.max_for(role) });
        }
    }
    if let Some(max) = references.combined_max {
        if counts.total() > max {
            return Err(ValidationError::TooManyReferencesTotal { model, max });
        }
    }
    // The cap is on the clip the provider will measure, so the bytes are probed rather than trusted;
    // a clip that can't be read is refused the way a non-MP4 is.
    if references.requires_video && counts.videos == 0 {
        return Err(ValidationError::ReferenceNeedsVideo { model });
    }
    if references.limits_video_duration() {
        for clip in request.assets.iter().filter(|a| a.role == AssetRole::ReferenceVideo) {
            let info = majik_core::video::probe_bytes(&clip.data)
                .map_err(|_| ValidationError::UnsupportedFormat { media_kind: MediaKind::Video, allowed_formats: "MP4" })?;
            let duration = info.duration_secs.unwrap_or(0.0);
            if !references.allows_video_duration(duration) {
                let too_short = references.min_video_secs.filter(|min| duration < f64::from(*min));
                return Err(match (too_short, references.max_video_secs) {
                    (Some(min_secs), _) => ValidationError::ReferenceVideoTooShort { model, min_secs },
                    (None, Some(max_secs)) => ValidationError::ReferenceVideoTooLong { model, max_secs },
                    (None, None) => continue,
                });
            }
        }
    }
    if counts.audio > 0 && counts.images == 0 && counts.videos == 0 {
        return Err(ValidationError::AudioReferenceNeedsVisual);
    }
    // A handle past the end of its list would be sent to the model as literal text.
    for (role, index) in references::handles(&request.prompt) {
        if index == 0 || index > counts.of(role) {
            return Err(ValidationError::UnknownHandle { handle: references::handle(role, index), role, attached: counts.of(role) });
        }
    }
    if let Some(resolution) = settings.resolution {
        if !references.allows_resolution(resolution) {
            let allowed = references.resolutions.unwrap_or_default().iter().map(|r| r.display_name()).collect::<Vec<_>>().join(" or ");
            return Err(ValidationError::ReferenceResolution { model, allowed });
        }
    }
    Ok(())
}

pub fn validate_asset(asset: &AssetInput) -> Result<(), ValidationError> {
    match asset.role {
        AssetRole::ReferenceImage | AssetRole::MaskImage | AssetRole::ControlImage | AssetRole::FirstFrame | AssetRole::LastFrame => validate_image_asset(asset),
        AssetRole::ReferenceVideo => validate_video_asset(asset),
        AssetRole::Audio => validate_audio_asset(asset),
    }
}

fn validate_image_asset(asset: &AssetInput) -> Result<(), ValidationError> {
    if asset.data.len() > MAX_IMAGE_ASSET_BYTES {
        return Err(ValidationError::AssetTooLarge { media_kind: MediaKind::Image, limit_bytes: MAX_IMAGE_ASSET_BYTES });
    }
    if !is_supported_image_data(&asset.data) {
        return Err(ValidationError::UnsupportedFormat { media_kind: MediaKind::Image, allowed_formats: "PNG, JPEG, WebP, or GIF" });
    }
    Ok(())
}

/// A reference video: MP4 only, the only container the app decodes and every provider takes.
fn validate_video_asset(asset: &AssetInput) -> Result<(), ValidationError> {
    if asset.data.len() > MAX_VIDEO_ASSET_BYTES {
        return Err(ValidationError::AssetTooLarge { media_kind: MediaKind::Video, limit_bytes: MAX_VIDEO_ASSET_BYTES });
    }
    if !is_supported_video_data(&asset.data) {
        return Err(ValidationError::UnsupportedFormat { media_kind: MediaKind::Video, allowed_formats: "MP4" });
    }
    Ok(())
}

fn validate_audio_asset(asset: &AssetInput) -> Result<(), ValidationError> {
    if asset.data.len() > MAX_AUDIO_ASSET_BYTES {
        return Err(ValidationError::AssetTooLarge { media_kind: MediaKind::Audio, limit_bytes: MAX_AUDIO_ASSET_BYTES });
    }
    if !is_supported_audio_content_type(&asset.content_type) {
        return Err(ValidationError::UnsupportedFormat { media_kind: MediaKind::Audio, allowed_formats: "MP3 or WAV" });
    }
    Ok(())
}

/// A tool runs over exactly one input, in the role its model's media asks for, with a model of its
/// own kind (a hand-edited stored request could pair any of them wrongly).
fn validate_tool_request(request: &Request, tool: ToolId) -> Result<(), ValidationError> {
    let model = request.generation_type.tool_settings().map(|s| &s.model);
    let Some(model) = model.filter(|m| m.kind == tool) else {
        return Err(ValidationError::UnsupportedModel(model.map(|m| m.name).unwrap_or_default().to_string()));
    };
    let role = model.input_role();
    match request.assets.as_slice() {
        [asset] if asset.role == role => validate_asset(asset),
        _ => Err(ValidationError::MissingAssetData { role }),
    }
}

/// The model's prompt cap, `None` when the model declares none (the provider then enforces its
/// own). Audio models always have one; the Mock provider borrows fal's tables.
pub fn prompt_character_limit(generation_type: &GenerationType, provider: &ProviderDescriptor) -> Result<Option<usize>, ValidationError> {
    match generation_type {
        GenerationType::Image(settings) => Ok(provider.image_capabilities(&settings.model).and_then(|caps| caps.max_prompt_characters)),
        GenerationType::Video(settings) => Ok(provider.video_capabilities(&settings.model).and_then(|caps| caps.max_prompt_characters)),
        // Tools take no prompt.
        GenerationType::Upscale(_) | GenerationType::RemoveBackground(_) => Ok(None),
        GenerationType::Audio(settings) => {
            let caps = provider.audio_capabilities(&settings.model).ok_or_else(|| ValidationError::UnsupportedModel(settings.model.name.to_string()))?;
            if settings.speaker2.is_some() && caps.supports_two_speakers {
                Ok(Some(caps.max_characters_dialogue))
            } else {
                Ok(Some(caps.max_characters_monologue))
            }
        }
    }
}

/// An MP4 (`....ftyp`). H.264 in MP4 is the only video the app reads or writes.
pub fn is_supported_video_data(data: &[u8]) -> bool {
    data.len() >= 12 && &data[4..8] == b"ftyp"
}

pub fn is_supported_image_data(data: &[u8]) -> bool {
    is_png(data) || is_jpeg(data) || is_gif(data) || is_webp(data)
}

fn is_png(d: &[u8]) -> bool {
    d.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
}
fn is_jpeg(d: &[u8]) -> bool {
    d.starts_with(&[0xFF, 0xD8, 0xFF])
}
fn is_gif(d: &[u8]) -> bool {
    d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a")
}
fn is_webp(d: &[u8]) -> bool {
    d.len() >= 12 && &d[0..4] == b"RIFF" && &d[8..12] == b"WEBP"
}

/// Accepts MIME types and the legacy UTIs older libraries stored (`public.mp3`, `com.microsoft.waveform-audio`).
pub fn is_supported_audio_content_type(content_type: &str) -> bool {
    matches!(
        content_type.to_ascii_lowercase().as_str(),
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg-3" | "public.mp3" | "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" | "com.microsoft.waveform-audio" | "public.wav"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_providers::ToolSettings;

    /// A video request on `model` with `assets`, through the Mock provider (which mirrors fal's
    /// video capability tables).
    fn video_request(model_id: &str, assets: Vec<AssetInput>) -> Request {
        use majik_providers::{catalog, ProviderId, VideoGenerationSettings};
        let model = catalog::video::model(model_id).expect("model in catalog").clone();
        let caps = majik_providers::mock::descriptor().video_capabilities(&model).expect("mock knows it");
        let settings = VideoGenerationSettings {
            model,
            aspect_ratio: caps.default_aspect_ratio(),
            resolution: caps.default_resolution(),
            duration: caps.default_duration(),
            audio_enabled: false,
        };
        Request::new(ProviderId::mock(), GenerationType::Video(settings), "a clip", assets)
    }

    fn png(role: AssetRole) -> AssetInput {
        AssetInput::new(role, "image/png", vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0])
    }

    fn mp4() -> AssetInput {
        let mut data = vec![0u8; 16];
        data[4..8].copy_from_slice(b"ftyp");
        AssetInput::new(AssetRole::ReferenceVideo, "video/mp4", data)
    }

    fn check(request: &Request) -> Result<(), ValidationError> {
        validate_request(request, majik_providers::mock::descriptor())
    }

    #[test]
    fn references_are_accepted_by_a_model_that_takes_them() {
        let request = video_request("seedance-2.5", vec![png(AssetRole::ReferenceImage), png(AssetRole::ReferenceImage), mp4()]);
        assert_eq!(check(&request), Ok(()));
    }

    #[test]
    fn a_model_without_references_refuses_them() {
        let request = video_request("kling-3-pro", vec![png(AssetRole::ReferenceImage)]);
        assert_eq!(check(&request), Err(ValidationError::ReferencesUnsupported { model: "Kling 3.0 Pro".into() }));
    }

    /// The reference endpoints take no frames at all.
    #[test]
    fn references_and_frames_are_refused_together() {
        let request = video_request("seedance-2.5", vec![png(AssetRole::ReferenceImage), png(AssetRole::FirstFrame)]);
        assert_eq!(check(&request), Err(ValidationError::ReferencesWithFrames));
    }

    #[test]
    fn too_many_references_are_refused_per_kind_and_in_total() {
        let mut request = video_request("minimax-h3", vec![png(AssetRole::ReferenceImage); 10]);
        assert_eq!(
            check(&request),
            Err(ValidationError::TooManyReferences { model: "MiniMax H3".into(), role: AssetRole::ReferenceImage, max: 9 })
        );

        // Nine images and three videos are each within their own cap, but H3 takes twelve files in
        // total and the audio pushes it over.
        request.assets = vec![png(AssetRole::ReferenceImage); 9];
        request.assets.extend(std::iter::repeat_with(mp4).take(3));
        request.assets.push(AssetInput::new(AssetRole::Audio, "audio/wav", vec![0; 4]));
        assert_eq!(check(&request), Err(ValidationError::TooManyReferencesTotal { model: "MiniMax H3".into(), max: 12 }));
    }

    #[test]
    fn an_audio_reference_needs_something_to_go_with() {
        let request = video_request("seedance-2.5", vec![AssetInput::new(AssetRole::Audio, "audio/wav", vec![0; 4])]);
        assert_eq!(check(&request), Err(ValidationError::AudioReferenceNeedsVisual));
    }

    /// A handle past the end of its list would reach the model as literal text.
    #[test]
    fn a_handle_with_nothing_behind_it_is_refused() {
        let mut request = video_request("seedance-2.5", vec![png(AssetRole::ReferenceImage)]);
        request.prompt = "@Image1 waves at @Image2".into();
        assert_eq!(
            check(&request),
            Err(ValidationError::UnknownHandle { handle: "@Image2".into(), role: AssetRole::ReferenceImage, attached: 1 })
        );

        request.prompt = "@Image1 waves".into();
        assert_eq!(check(&request), Ok(()));
        request.prompt = "write to me@example.com".into();
        assert_eq!(check(&request), Ok(()), "an email address is not a handle");
    }

    /// Grok 1.5's reference endpoint stops at 720p where its text-to-video endpoint sells 1080p.
    #[test]
    fn a_resolution_the_reference_path_lacks_is_refused() {
        use majik_providers::VideoResolution;
        let mut request = video_request("grok-imagine-video-1.5", vec![png(AssetRole::ReferenceImage)]);
        let GenerationType::Video(settings) = &mut request.generation_type else { panic!("a video request") };
        settings.resolution = Some(VideoResolution::Fhd);
        assert_eq!(
            check(&request),
            Err(ValidationError::ReferenceResolution { model: "Grok Imagine Video 1.5".into(), allowed: "480p or 720p".into() })
        );

        let GenerationType::Video(settings) = &mut request.generation_type else { panic!("a video request") };
        settings.resolution = Some(VideoResolution::Hd);
        assert_eq!(check(&request), Ok(()));
    }

    /// Gemini Omni Flash takes reference clips of three seconds at most, and fal fails the whole
    /// request over a longer one.
    #[test]
    fn a_reference_clip_longer_than_the_model_takes_is_refused() {
        let clip = |seconds| AssetInput::new(AssetRole::ReferenceVideo, "video/mp4", majik_core::video::encode_solid_clip(64, 64, seconds, [0, 0, 255]).unwrap());
        assert_eq!(
            check(&video_request("gemini-omni-flash-1.1", vec![clip(4)])),
            Err(ValidationError::ReferenceVideoTooLong { model: "Gemini Omni Flash 1.1".into(), max_secs: 3 })
        );
        assert_eq!(check(&video_request("gemini-omni-flash-1.1", vec![clip(3)])), Ok(()));
        // A model that states no cap takes the same clip without opening it.
        assert_eq!(check(&video_request("seedance-2.5", vec![clip(4)])), Ok(()));
        // A capped model has to read the clip, so one that only looks like an MP4 is refused as such.
        assert_eq!(
            check(&video_request("gemini-omni-flash-1.1", vec![mp4()])),
            Err(ValidationError::UnsupportedFormat { media_kind: MediaKind::Video, allowed_formats: "MP4" })
        );
    }

    /// Kling O3 Pro's reference path is its video-to-video endpoint: the clip is required, and has
    /// to be between three and fifteen seconds long.
    #[test]
    fn a_video_to_video_reference_path_needs_its_clip_in_range() {
        let clip = |seconds| AssetInput::new(AssetRole::ReferenceVideo, "video/mp4", majik_core::video::encode_solid_clip(64, 64, seconds, [0, 0, 255]).unwrap());
        assert_eq!(
            check(&video_request("kling-o3-pro", vec![png(AssetRole::ReferenceImage)])),
            Err(ValidationError::ReferenceNeedsVideo { model: "Kling O3 Pro".into() })
        );
        assert_eq!(
            check(&video_request("kling-o3-pro", vec![clip(2)])),
            Err(ValidationError::ReferenceVideoTooShort { model: "Kling O3 Pro".into(), min_secs: 3 })
        );
        assert_eq!(check(&video_request("kling-o3-pro", vec![png(AssetRole::ReferenceImage), clip(3)])), Ok(()));
        assert_eq!(check(&video_request("kling-o3-pro", vec![clip(3)])), Ok(()), "the clip alone is enough");
        assert_eq!(
            check(&video_request("kling-o3-pro", vec![clip(3), clip(3)])),
            Err(ValidationError::TooManyReferences { model: "Kling O3 Pro".into(), role: AssetRole::ReferenceVideo, max: 1 })
        );
    }

    /// Nothing about references applies to a request that has none.
    #[test]
    fn a_plain_video_request_is_untouched() {
        assert_eq!(check(&video_request("kling-3-pro", vec![png(AssetRole::FirstFrame)])), Ok(()));
        assert_eq!(check(&video_request("seedance-2.5", vec![png(AssetRole::FirstFrame), png(AssetRole::LastFrame)])), Ok(()));
    }

    #[test]
    fn a_reference_video_must_be_an_mp4() {
        let bad = AssetInput::new(AssetRole::ReferenceVideo, "video/mp4", vec![0; 16]);
        assert_eq!(
            validate_asset(&bad),
            Err(ValidationError::UnsupportedFormat { media_kind: MediaKind::Video, allowed_formats: "MP4" })
        );
        assert_eq!(validate_asset(&mp4()), Ok(()));
    }

    #[test]
    fn tool_request_needs_exactly_one_image_of_its_own_kind() {
        use majik_providers::{catalog, ProviderId};
        let provider = majik_providers::mock::descriptor();
        let png = AssetInput::new(AssetRole::ReferenceImage, "image/png", vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]);
        let ok = Request::tool(ProviderId::mock(), ToolSettings::new(catalog::tool::MOCK_UPSCALE.clone()), png.clone());
        assert_eq!(validate_request(&ok, provider), Ok(()));

        let mut none = ok.clone();
        none.assets.clear();
        assert_eq!(validate_request(&none, provider), Err(ValidationError::MissingAssetData { role: AssetRole::ReferenceImage }));
        let mut two = ok.clone();
        two.assets.push(png.clone());
        assert_eq!(validate_request(&two, provider), Err(ValidationError::MissingAssetData { role: AssetRole::ReferenceImage }));
        let mut sound = ok.clone();
        sound.assets = vec![AssetInput::new(AssetRole::Audio, "audio/wav", vec![0; 4])];
        assert_eq!(validate_request(&sound, provider), Err(ValidationError::MissingAssetData { role: AssetRole::ReferenceImage }));

        // A hand-edited request pairing the upscale kind with a background remover.
        let mismatched = Request::new(ProviderId::mock(), GenerationType::Upscale(ToolSettings::new(catalog::tool::MOCK_REMOVE_BACKGROUND.clone())), "", vec![png]);
        assert_eq!(validate_request(&mismatched, provider), Err(ValidationError::UnsupportedModel("Mock Remove Background".into())));
    }

    /// The input has to be what the model works on: a video upscaler takes a clip and an image
    /// upscaler refuses one, both by role and by the bytes behind it.
    #[test]
    fn a_video_tool_takes_a_clip_and_an_image_tool_refuses_one() {
        use majik_providers::{catalog, ProviderId};
        let provider = majik_providers::mock::descriptor();
        let png = AssetInput::new(AssetRole::ReferenceImage, "image/png", vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]);
        let clip = mp4();

        let ok = Request::tool(ProviderId::mock(), ToolSettings::new(catalog::tool::MOCK_UPSCALE_VIDEO.clone()), clip.clone());
        assert_eq!(validate_request(&ok, provider), Ok(()));

        // An image on the video upscaler: the role is not the one its model asks for.
        let wrong_media = Request::tool(ProviderId::mock(), ToolSettings::new(catalog::tool::MOCK_UPSCALE_VIDEO.clone()), png.clone());
        assert_eq!(validate_request(&wrong_media, provider), Err(ValidationError::MissingAssetData { role: AssetRole::ReferenceVideo }));

        // And the other way round.
        let clip_on_image_tool = Request::tool(ProviderId::mock(), ToolSettings::new(catalog::tool::MOCK_UPSCALE.clone()), clip);
        assert_eq!(validate_request(&clip_on_image_tool, provider), Err(ValidationError::MissingAssetData { role: AssetRole::ReferenceImage }));

        // The bytes are checked too, not just the role: MP4 only, as everywhere else.
        let not_a_clip = Request::tool(
            ProviderId::mock(),
            ToolSettings::new(catalog::tool::MOCK_UPSCALE_VIDEO.clone()),
            AssetInput::new(AssetRole::ReferenceVideo, "video/mp4", vec![1, 2, 3, 4]),
        );
        assert_eq!(
            validate_request(&not_a_clip, provider),
            Err(ValidationError::UnsupportedFormat { media_kind: MediaKind::Video, allowed_formats: "MP4" })
        );
    }

    #[test]
    fn sniffs_formats() {
        assert!(is_supported_image_data(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]));
        assert!(is_supported_image_data(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_supported_image_data(b"GIF89a...."));
        assert!(is_supported_image_data(b"RIFF....WEBPVP8 "));
        assert!(!is_supported_image_data(b"not an image"));
    }

    #[test]
    fn audio_content_types() {
        assert!(is_supported_audio_content_type("audio/mpeg"));
        assert!(is_supported_audio_content_type("public.mp3"));
        assert!(is_supported_audio_content_type("audio/wav"));
        assert!(!is_supported_audio_content_type("audio/flac"));
    }

    #[test]
    fn prompt_length_is_only_checked_against_a_model_declared_cap() {
        use majik_providers::{catalog, AspectRatio, ImageGenerationSettings, ImageResolution, ProviderId, VideoGenerationSettings};
        let provider = majik_providers::mock::descriptor();
        let image = |prompt: String| {
            let model = catalog::image::ALL.first().expect("catalog populated").clone();
            Request::new(ProviderId::mock(), GenerationType::Image(ImageGenerationSettings { model, aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd }), prompt, vec![])
        };
        let video = |model: &majik_providers::VideoModel, prompt: String| {
            Request::new(ProviderId::mock(), GenerationType::Video(VideoGenerationSettings { model: model.clone(), aspect_ratio: None, resolution: None, duration: 5, audio_enabled: true }), prompt, vec![])
        };

        // No cap declared: any length goes through to the provider.
        assert_eq!(prompt_character_limit(&image(String::new()).generation_type, provider), Ok(None));
        assert_eq!(validate_request(&image("x".repeat(20_000)), provider), Ok(()));
        assert_eq!(validate_request(&video(&catalog::video::VEO_31, "x".repeat(20_000)), provider), Ok(()));

        // Kling documents 2500 characters; the check is inclusive.
        let kling = &catalog::video::KLING_30_PRO;
        assert_eq!(prompt_character_limit(&video(kling, String::new()).generation_type, provider), Ok(Some(2500)));
        assert_eq!(validate_request(&video(kling, "x".repeat(2500)), provider), Ok(()));
        assert_eq!(
            validate_request(&video(kling, "x".repeat(2501)), provider),
            Err(ValidationError::PromptTooLong { media_type: MediaType::Video, limit: 2500 })
        );
    }

    #[test]
    fn messages() {
        assert_eq!(format_file_size(10_000_000), "10 MB");
        assert_eq!(format_file_size(15_000_000), "15 MB");
        let e = ValidationError::PromptTooLong { media_type: MediaType::Image, limit: 1500 };
        assert_eq!(e.to_string(), "Prompt is too long. Image prompts must be 1500 characters or fewer.");
        let e = ValidationError::AssetTooLarge { media_kind: MediaKind::Image, limit_bytes: MAX_IMAGE_ASSET_BYTES };
        assert_eq!(e.to_string(), "Image is too large. Use an image under 10 MB.");
        let e = ValidationError::UnsupportedFormat { media_kind: MediaKind::Audio, allowed_formats: "MP3 or WAV" };
        assert_eq!(e.to_string(), "Audio must be MP3 or WAV and 15 MB or smaller.");
    }
}
