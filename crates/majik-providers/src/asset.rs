//! Input assets: the roles a provider accepts, the constraints on each, and the asset payload.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AssetRole {
    #[serde(rename = "reference_image")]
    ReferenceImage,
    #[serde(rename = "reference_video")]
    ReferenceVideo,
    #[serde(rename = "first_frame")]
    FirstFrame,
    #[serde(rename = "last_frame")]
    LastFrame,
    #[serde(rename = "mask_image")]
    MaskImage,
    #[serde(rename = "control_image")]
    ControlImage,
    #[serde(rename = "audio")]
    Audio,
}

impl AssetRole {
    pub const ALL: [AssetRole; 7] = [
        AssetRole::ReferenceImage,
        AssetRole::ReferenceVideo,
        AssetRole::FirstFrame,
        AssetRole::LastFrame,
        AssetRole::MaskImage,
        AssetRole::ControlImage,
        AssetRole::Audio,
    ];

    /// Persistence key.
    pub fn raw(self) -> &'static str {
        match self {
            AssetRole::ReferenceImage => "reference_image",
            AssetRole::ReferenceVideo => "reference_video",
            AssetRole::FirstFrame => "first_frame",
            AssetRole::LastFrame => "last_frame",
            AssetRole::MaskImage => "mask_image",
            AssetRole::ControlImage => "control_image",
            AssetRole::Audio => "audio",
        }
    }

    pub fn from_raw(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.raw() == raw)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            AssetRole::ReferenceImage => "Image",
            AssetRole::ReferenceVideo => "Video",
            AssetRole::FirstFrame => "First frame",
            AssetRole::LastFrame => "Last frame",
            AssetRole::MaskImage => "Mask",
            AssetRole::ControlImage => "Control",
            AssetRole::Audio => "Audio",
        }
    }

    /// Icon name in the app's bundle (`packaging/icons.json`).
    pub fn icon(self) -> &'static str {
        match self {
            AssetRole::ReferenceImage => "image-upload",
            AssetRole::ReferenceVideo => "film",
            AssetRole::FirstFrame | AssetRole::LastFrame => "film",
            AssetRole::MaskImage => "square-dashed",
            AssetRole::ControlImage => "layers",
            AssetRole::Audio => "audio-lines",
        }
    }

    pub fn is_frame_input(self) -> bool {
        matches!(self, AssetRole::FirstFrame | AssetRole::LastFrame)
    }

    /// Whether a file of `kind` can play this role: audio for the audio role, video for a video
    /// reference, images for every other one.
    pub fn accepts_kind(self, kind: majik_core::MediaType) -> bool {
        match self {
            AssetRole::Audio => kind == majik_core::MediaType::Audio,
            AssetRole::ReferenceVideo => kind == majik_core::MediaType::Video,
            _ => kind == majik_core::MediaType::Image,
        }
    }

    /// Whether this role is one of the reference lists a prompt can address by handle
    /// (`@Image1` / `@Video1` / `@Audio1`), as opposed to a frame, a mask or a control image.
    pub fn is_reference(self) -> bool {
        matches!(self, AssetRole::ReferenceImage | AssetRole::ReferenceVideo | AssetRole::Audio)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AssetConstraints {
    pub allowed: BTreeMap<AssetRole, RangeInclusive<usize>>,
}

impl AssetConstraints {
    pub fn new(allowed: impl IntoIterator<Item = (AssetRole, RangeInclusive<usize>)>) -> Self {
        Self { allowed: allowed.into_iter().collect() }
    }

    pub fn none() -> Self {
        Self::default()
    }

    pub fn first_last_frame() -> Self {
        Self::new([(AssetRole::FirstFrame, 0..=1), (AssetRole::LastFrame, 0..=1)])
    }

    pub fn first_last_frame_and_audio() -> Self {
        Self::new([(AssetRole::FirstFrame, 0..=1), (AssetRole::LastFrame, 0..=1), (AssetRole::Audio, 0..=1)])
    }

    pub fn reference_images(max: usize) -> Self {
        Self::new([(AssetRole::ReferenceImage, 0..=max)])
    }

    /// Adds `role` to the set (replacing any range it already had), so a model's reference lists can
    /// be merged onto the frame constraints it already declares.
    pub fn with_role(mut self, role: AssetRole, range: RangeInclusive<usize>) -> Self {
        self.allowed.insert(role, range);
        self
    }

    pub fn range(&self, role: AssetRole) -> Option<&RangeInclusive<usize>> {
        self.allowed.get(&role)
    }

    pub fn accepts(&self, role: AssetRole) -> bool {
        self.allowed.contains_key(&role)
    }

    pub fn validate(&self, roles: &[AssetRole]) -> std::result::Result<(), AssetConstraintError> {
        let mut counts: BTreeMap<AssetRole, usize> = BTreeMap::new();
        for role in roles {
            if !self.allowed.contains_key(role) {
                return Err(AssetConstraintError::UnacceptedRole(*role));
            }
            *counts.entry(*role).or_default() += 1;
        }
        for (role, range) in &self.allowed {
            let count = counts.get(role).copied().unwrap_or(0);
            if count < *range.start() {
                return Err(AssetConstraintError::TooFew { role: *role, min: *range.start(), actual: count });
            }
            if count > *range.end() {
                return Err(AssetConstraintError::TooMany { role: *role, max: *range.end(), actual: count });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AssetConstraintError {
    #[error("This model does not accept a {} input.", .0.raw())]
    UnacceptedRole(AssetRole),
    #[error("This model requires at least {min} {} input(s) (got {actual}).", role.raw())]
    TooFew { role: AssetRole, min: usize, actual: usize },
    #[error("This model accepts at most {max} {} input(s) (got {actual}).", role.raw())]
    TooMany { role: AssetRole, max: usize, actual: usize },
}

/// One input file handed to a provider.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAsset {
    pub role: AssetRole,
    /// MIME type (`image/png`) or a legacy UTI (`public.png`); see [`ProviderAsset::mime_type`].
    pub media_type: String,
    pub data: Vec<u8>,
    pub attributes: Option<Vec<u8>>,
}

impl std::fmt::Debug for ProviderAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderAsset").field("role", &self.role).field("media_type", &self.media_type).field("bytes", &self.data.len()).finish()
    }
}

impl ProviderAsset {
    pub fn new(role: AssetRole, media_type: impl Into<String>, data: Vec<u8>) -> Self {
        Self { role, media_type: media_type.into(), data, attributes: None }
    }

    /// MIME type for data URIs; maps the legacy UTIs older libraries stored.
    pub fn mime_type(&self) -> String {
        if self.media_type.contains('/') {
            return self.media_type.clone();
        }
        match self.media_type.as_str() {
            "public.png" => "image/png",
            "public.jpeg" => "image/jpeg",
            "org.webmproject.webp" | "public.webp" => "image/webp",
            "com.compuserve.gif" => "image/gif",
            "public.mp3" => "audio/mpeg",
            "com.microsoft.waveform-audio" | "public.wav" => "audio/wav",
            "public.mpeg-4" => "video/mp4",
            other => other,
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_ranges() {
        let c = AssetConstraints::first_last_frame();
        assert!(c.validate(&[]).is_ok());
        assert!(c.validate(&[AssetRole::FirstFrame]).is_ok());
        assert_eq!(
            c.validate(&[AssetRole::FirstFrame, AssetRole::FirstFrame]),
            Err(AssetConstraintError::TooMany { role: AssetRole::FirstFrame, max: 1, actual: 2 })
        );
        assert_eq!(c.validate(&[AssetRole::ReferenceImage]), Err(AssetConstraintError::UnacceptedRole(AssetRole::ReferenceImage)));
    }

    #[test]
    fn roles_accept_their_kind() {
        use majik_core::MediaType;
        assert!(AssetRole::ReferenceImage.accepts_kind(MediaType::Image) && AssetRole::FirstFrame.accepts_kind(MediaType::Image));
        assert!(!AssetRole::ReferenceImage.accepts_kind(MediaType::Audio) && !AssetRole::MaskImage.accepts_kind(MediaType::Video));
        assert!(AssetRole::Audio.accepts_kind(MediaType::Audio) && !AssetRole::Audio.accepts_kind(MediaType::Image));
        assert!(AssetRole::ReferenceVideo.accepts_kind(MediaType::Video) && !AssetRole::ReferenceVideo.accepts_kind(MediaType::Image));
    }

    #[test]
    fn mime_from_uti() {
        assert_eq!(ProviderAsset::new(AssetRole::ReferenceImage, "public.png", vec![]).mime_type(), "image/png");
        assert_eq!(ProviderAsset::new(AssetRole::ReferenceImage, "image/webp", vec![]).mime_type(), "image/webp");
    }
}
