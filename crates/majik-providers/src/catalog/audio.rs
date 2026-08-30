//! The global catalog of every audio model. Same pattern as [`super::image`] — each model is defined once
//! as a `const` in [`defs`] and exposed both as a named `pub static` and inside [`ALL`].
//! Ids, names and descriptions are persistence keys / UI copy.

use crate::models::AudioModel;

mod defs {
    use crate::logo;
    use crate::models::AudioModel;

    pub const ELEVEN_LABS_V3: AudioModel =
        AudioModel::new("elevenlabs-v3", "ElevenLabs v3", "ElevenLabs", logo::ELEVEN_LABS, "Expressive multi-speaker dialogue");
    pub const GEMINI_25_PRO: AudioModel =
        AudioModel::new("gemini-2.5-pro", "Gemini 2.5 Pro", "Google", logo::GOOGLE, "Studio-quality narration and dialogue");
}

pub static ELEVEN_LABS_V3: AudioModel = defs::ELEVEN_LABS_V3;
pub static GEMINI_25_PRO: AudioModel = defs::GEMINI_25_PRO;

/// Every audio model, in the same (UI) order as `AudioModelCatalog.all`.
pub static ALL: &[AudioModel] = &[defs::ELEVEN_LABS_V3, defs::GEMINI_25_PRO];

/// Looks a model up by its persistence id (`AudioModelCatalog.model(id:)`).
pub fn model(id: &str) -> Option<&'static AudioModel> {
    ALL.iter().find(|m| m.id == id)
}
