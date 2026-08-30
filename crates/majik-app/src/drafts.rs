//! Per-provider composer drafts (port of `ComposerStore` / `ProviderComposerState`), persisted next to
//! `config.json` as `drafts.json`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ImageDraftState {
    pub model_id: Option<String>,
    pub aspect_ratio: Option<majik_providers::AspectRatio>,
    pub resolution: Option<majik_providers::ImageResolution>,
    pub count: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VideoDraftState {
    pub model_id: Option<String>,
    pub aspect_ratio: Option<majik_providers::VideoAspectRatio>,
    pub resolution: Option<majik_providers::VideoResolution>,
    pub duration: Option<u32>,
    pub audio: Option<bool>,
    pub count: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioDraftState {
    pub model_id: Option<String>,
    pub speaker1: Option<String>,
    pub speaker2: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolDraftState {
    pub model_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderDraft {
    /// The selected composer tab: a media type (`image` / `video` / `audio`) or a tool
    /// (`upscale` / `removeBackground`). The key predates the tool tabs.
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub image: ImageDraftState,
    #[serde(default)]
    pub video: VideoDraftState,
    #[serde(default)]
    pub audio: AudioDraftState,
    #[serde(default)]
    pub upscale: ToolDraftState,
    #[serde(default)]
    pub remove_background: ToolDraftState,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Drafts {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDraft>,
}

fn path() -> Option<std::path::PathBuf> {
    crate::config::config_dir().map(|d| d.join("drafts.json"))
}

impl Drafts {
    pub fn load() -> Self {
        path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, json);
        }
    }

    pub fn get(&self, provider: &str) -> ProviderDraft {
        self.providers.get(provider).cloned().unwrap_or_default()
    }

    pub fn set(&mut self, provider: &str, draft: ProviderDraft) {
        if self.providers.get(provider) != Some(&draft) {
            self.providers.insert(provider.to_string(), draft);
            self.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_draft_without_tool_fields_deserializes() {
        let json = r#"{"media_type":"video","image":{"model_id":"gpt-5"},"video":{},"audio":{}}"#;
        let draft: ProviderDraft = serde_json::from_str(json).unwrap();
        assert_eq!(draft.media_type.as_deref(), Some("video"));
        assert_eq!(draft.image.model_id.as_deref(), Some("gpt-5"));
        assert_eq!(draft.upscale, ToolDraftState::default());
        assert_eq!(draft.remove_background, ToolDraftState::default());
    }

    #[test]
    fn provider_draft_without_video_count_deserializes_to_none() {
        let json = r#"{"video":{"model_id":"veo-3.1","duration":6}}"#;
        let draft: ProviderDraft = serde_json::from_str(json).unwrap();
        assert_eq!(draft.video.count, None, "an older draft leaves the count to the state's default");
    }

    #[test]
    fn provider_draft_round_trips_tool_tab() {
        let draft = ProviderDraft {
            media_type: Some("upscale".into()),
            upscale: ToolDraftState { model_id: Some("topaz-upscale".into()) },
            remove_background: ToolDraftState { model_id: Some("bria-background-remove".into()) },
            ..Default::default()
        };
        let json = serde_json::to_string(&draft).unwrap();
        assert_eq!(serde_json::from_str::<ProviderDraft>(&json).unwrap(), draft);
    }
}
