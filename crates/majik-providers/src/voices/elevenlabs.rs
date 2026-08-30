//! Two voice lists exist because the two providers accept different voice sets:
//! - [`replicate_voices`]: the voice enum from Replicate's `elevenlabs/v3` OpenAPI schema.
//!   Re-check with `scripts/replicate-audio-schema.py` when the model version changes.
//! - [`fal_voices`]: voice names accepted by fal's `elevenlabs/tts/eleven-v3` and
//!   `elevenlabs/text-to-dialogue/eleven-v3` endpoints. fal documents Rachel as the default and the
//!   remaining voices as examples or options.
//!
//! Ids are the literal wire values sent to the provider; keep them and every subtitle / preview
//! URL / category / gender / accent / language code exactly as the provider spells it.

use crate::models::AudioVoice;
use std::sync::OnceLock;

/// The voice fal documents as its default for the ElevenLabs v3 endpoints.
pub const FAL_DEFAULT_VOICE_ID: &str = "Rachel";

/// One catalog row; `None` preview/category/... only for `unmapped(...)` voices.
struct Row {
    actor: &'static str,
    subtitle: Option<&'static str>,
    preview_url: Option<&'static str>,
    category: Option<&'static str>,
    gender: Option<&'static str>,
    accent: Option<&'static str>,
    language_codes: &'static [&'static str],
}

/// A voice ElevenLabs still lists, with all its metadata.
const fn mapped(
    actor: &'static str,
    subtitle: Option<&'static str>,
    preview_url: &'static str,
    category: &'static str,
    gender: &'static str,
    accent: &'static str,
    language_codes: &'static [&'static str],
) -> Row {
    Row { actor, subtitle, preview_url: Some(preview_url), category: Some(category), gender: Some(gender), accent: Some(accent), language_codes }
}

/// A voice Replicate accepts but ElevenLabs no longer lists, so it has no metadata.
const fn unmapped(actor: &'static str, language_codes: &'static [&'static str]) -> Row {
    Row { actor, subtitle: None, preview_url: None, category: None, gender: None, accent: None, language_codes }
}

impl Row {
    fn to_voice(&self) -> AudioVoice {
        AudioVoice {
            id: self.actor.to_string(),
            display_name: self.actor.to_string(),
            subtitle: self.subtitle.map(str::to_string),
            preview_url: self.preview_url.map(str::to_string),
            category: self.category.map(str::to_string),
            gender: self.gender.map(str::to_string),
            accent: self.accent.map(str::to_string),
            language_codes: Some(self.language_codes.iter().map(|c| c.to_string()).collect()),
        }
    }
}

const EN: &[&str] = &["en"];

/// The Replicate `elevenlabs/v3` voice enum.
const REPLICATE_ROWS: &[Row] = &[
    mapped(
        "Rachel",
        Some("Clear, Calm, Natural, Neutral, Narrative"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/db14b36fd4854d3aa5f8ce2deefa6b50/voices/mDYJ5aI19GeZeL0uKqb3/AuuZUNwILPreDLyJD8Aq.mp3",
        "professional",
        "female",
        "canadian",
        EN,
    ),
    mapped(
        "Drew",
        None,
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/c865cfc145274d839d56bd675c041a36/voices/Z1XANW2S7adUEcqKCBlA/9Kt9AlFuaZ4mH6lAxUjQ.mp3",
        "professional",
        "male",
        "scottish",
        EN,
    ),
    mapped(
        "Clyde",
        Some("Full, Diplomatic and Inviting"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/T05GPpk2fvSBb625DXd4BJHZtu22/voices/wyWA56cQNU2KqUW4eCsI/RNPfnL6pqQi9eI02rhnL.mp3",
        "high_quality",
        "male",
        "british",
        EN,
    ),
    mapped(
        "Paul",
        Some("Deep & Warm - Yorkshire"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/c8391304779940b2892a28f2f14905e7/voices/oCXdm5WkYoKVEdlbPLev/cyTCwyJiIrMSzjUG42em.mp3",
        "professional",
        "male",
        "yorkshire",
        EN,
    ),
    mapped(
        "Aria",
        None,
        "https://storage.googleapis.com/eleven-public-prod/database/user/MJY8opk1DPc6wYLdHc0mH7HsGet2/voices/QeKcckTBICc3UuWL7ETc/tz2YmB6WDmy26JcuSI8D.mp3",
        "professional",
        "female",
        "american",
        EN,
    ),
    unmapped("Domi", EN),
    mapped(
        "Dave",
        None,
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/e25cb99b805f44e895d68f73a73a6f58/voices/0m71kiyu84bdUcKDzG0L/Z2W1NWE64g928pktt9TS.mp3",
        "professional",
        "male",
        "british",
        EN,
    ),
    mapped(
        "Roger",
        Some("Laid-Back, Casual & Resonant"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/e9f5485839c749cdae7fddf080cda345/voices/XAezqB2SuTKEhjCMe7Oy/ZgwbzKLYCuV09C41IXZn.mp3",
        "professional",
        "male",
        "american",
        EN,
    ),
    unmapped("Fin", EN),
    mapped(
        "Sarah",
        Some("Mature, Reassuring, Confident"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/d8198bd4b89c4185a6af8e40497140ab/voices/gJU2icYQsdEmbGJ65Z8W/067daf55-56bc-40b2-ac8f-e16e9bd69c56.mp3",
        "professional",
        "female",
        "american",
        EN,
    ),
    mapped(
        "James",
        Some("Deep Narrator"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/b2a9f1bbf7fb4a4d9a91669976e11a3f/voices/WrT2M515LQuE5m5EYh0W/89IUQBQ9QuGB9xJVfRIL.mp3",
        "professional",
        "male",
        "brazilian",
        &["pt"],
    ),
    mapped(
        "Jane",
        Some("NZ"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/0cd82c32f8b04414ae2b17a7ca4fa06d/voices/Br4MqB57uC425pYfq0mP/996b7779-4cd3-4dbf-a582-3a17ba44d1cb.mp3",
        "professional",
        "female",
        "new zealand",
        EN,
    ),
    mapped(
        "Juniper",
        Some("Grounded and Professional"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/1da06ea679a54975ad96a2221fe6530d/voices/aMSt68OGf4xUZAnLpTU8/J1MZWsaESdkAy9Offbd7.mp3",
        "high_quality",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Arabella",
        Some("Mysterious and Emotive"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/67adadb7d2a94f6ead64e95f45be2254/voices/Z3R5wn05IrDiVCyEkUrK/CBYKafo5onIe5234rAGS.mp3",
        "high_quality",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Hope",
        Some("Vibrant, Warm and Innocent"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/sD92HnMHS9WZLXKNTKxmnC8XmJ32/voices/cVd39cx0VtXNC13y5Y7z/ZmGEtFLzESmhA9TkCBUW.mp3",
        "high_quality",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Bradford",
        Some("Expressive and Articulate"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/CEZXxfdCfEfyXD7yUNKIPcuCKaB3/voices/NNl6r8mD7vthiJatiJt1/6adca4e2-0bc1-4ece-ac02-ff0bceac9c36.mp3",
        "high_quality",
        "male",
        "british",
        EN,
    ),
    mapped(
        "Reginald",
        Some("Intense Villian"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/Rf9A49797NP2PSelW0bbsI9to1i2/voices/QUOYMor9zfZEmffKPlfx/97yGokFCg5lnKQY5nvCV.mp3",
        "professional",
        "male",
        "american",
        EN,
    ),
    unmapped("Gaming", EN),
    mapped(
        "Austin",
        Some("Calm Leader"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/user_2301kezpxgzte7rbg6mjmm5cdtry/voices/fA2wlAJGF6MeyLM32my8/7764908d-9530-4f7b-b82a-815b36d15537.mp3",
        "professional",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Kuon",
        Some("Cheerful, Clear and Steady"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/5InJxTNw62QKlkwHZntu42lkOMk1/voices/B8gJV1IhpuegLxdpXFOE/38bca842-43a2-4be7-9fbd-0097cca97d45.mp3",
        "high_quality",
        "female",
        "standard",
        &["ja"],
    ),
    mapped(
        "Blondie",
        Some("Whispery"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/user_9001kfjw72adfajafjvmqzykzgcv/voices/NNlPuk2Pv2RnraA6G8yp/V12LjZ71FdbOGhwOfQ5B.mp3",
        "professional",
        "female",
        "british",
        EN,
    ),
    mapped(
        "Priyanka",
        None,
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/3625c28a7c2e4e7babbffbb8f98ab2fd/voices/T536A2SFCG4AEDVTRucQ/t2LLAGHLlzlgiKE3gSMx.mp3",
        "professional",
        "female",
        "standard",
        &["hi"],
    ),
    mapped(
        "Alexandra",
        Some("Confident, Clear and Steady"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/ttDSMlBjdZQQ1Gz383m2RHuuGhA3/voices/3dzJXoCYueSQiptQ6euE/QzkyqzqqpHUnPyycGTAo.mp3",
        "professional",
        "female",
        "american",
        EN,
    ),
    unmapped("Monika", EN),
    mapped(
        "Mark",
        Some("Midwest authority"),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/ab68028dc3284b0f9bbf5579200e0561/voices/TFNGY5E9nfgvDNh8jeHQ/96dc960e-ca3f-4764-a97d-dd2416d8668a.mp3",
        "professional",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Grimblewood",
        Some("Thornwhisker - Snarky Gnome & Magical Maintainer"),
        "https://storage.googleapis.com/eleven-public-prod/database/user/aWnKUw0pyjYMNaT9LJ1L6nH5r693/voices/ouL9IsyrSnUkCmfnD02u/21FjHRSMRihcHks1760n.mp3",
        "high_quality",
        "male",
        "british",
        EN,
    ),
];

/// The fal `elevenlabs/tts/eleven-v3` + `elevenlabs/text-to-dialogue/eleven-v3` voice names.
const FAL_ROWS: &[Row] = &[
    mapped(
        "Rachel",
        Some("A neutral-American accent woman with a reassuring and professional tone, perfect for both narrations and informative content."),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/1da06ea679a54975ad96a2221fe6530d/voices/eLDc7xhWxG2FElT3kUTj/aTInQG648LTH0oRjg54j.mp3",
        "professional",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Aria",
        Some("Southern, African-American female storyteller. Charismatic, engaging and gritty, with an intense cinematic feel."),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/1da06ea679a54975ad96a2221fe6530d/voices/M6ic45wruJGWAxLFEMNK/741a43cf-6965-4d85-bba2-d6f5db554c35.mp3",
        "professional",
        "female",
        "african american",
        EN,
    ),
    mapped(
        "Roger",
        Some("Easy going and perfect for casual conversations."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/CwhRBWXzGAHq8TQ4Fs17/58ee3ff5-f6f2-4628-93b8-e38eb31806b0.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Sarah",
        Some("Young adult woman with a confident and warm, mature quality and a reassuring, professional tone."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/EXAVITQu4vr4xnSDxMaL/01a3e33c-6e99-4ee7-8543-ff2216a32186.mp3",
        "premade",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Laura",
        Some("This young adult female voice delivers sunny enthusiasm with a quirky attitude."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/FGY2WhTYpPnrIDTdsKH5/67341759-ad08-41a5-be6e-de12fe448618.mp3",
        "premade",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Charlie",
        Some("A young Australian male with a confident and energetic voice."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/IKne3meq5aSn9XLyUdCD/102de6f2-22ed-43e0-a1f1-111fa75c5481.mp3",
        "premade",
        "male",
        "australian",
        EN,
    ),
    mapped(
        "George",
        Some("Warm resonance that instantly captivates listeners."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/JBFqnCBsd6RMkjVDRZzb/e6206d1a-0721-4787-aafb-06a6e705cac5.mp3",
        "premade",
        "male",
        "british",
        EN,
    ),
    mapped(
        "Callum",
        Some("Deceptively gravelly, yet unsettling edge."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/N2lVS1w4EtoT3dr4eOWO/ac833bd8-ffda-4938-9ebc-b0f99ca25481.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
    mapped(
        "River",
        Some("A relaxed, neutral voice ready for narrations or conversational projects."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/SAz9YHcvj6GT2YYXdXww/e6c95f0b-2227-491a-b3d7-2249240decb7.mp3",
        "premade",
        "neutral",
        "american",
        EN,
    ),
    mapped(
        "Liam",
        Some("A young adult with energy and warmth - suitable for reels and shorts."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/TX3LPaxmHKxFdv7VOQHJ/63148076-6363-42db-aea8-31424308b92c.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Charlotte",
        Some("A warm, soothing voice with an RP accent - perfect for a wide range of use-cases that require a professional, but natural tone."),
        "https://storage.googleapis.com/eleven-public-prod/database/workspace/1da06ea679a54975ad96a2221fe6530d/voices/wWX7KUejJFyJqUrufgrW/16693657-385d-446d-862e-af78141735a6.mp3",
        "professional",
        "female",
        "british",
        EN,
    ),
    mapped(
        "Alice",
        Some("Clear and engaging, friendly woman with a British accent suitable for e-learning."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/Xb7hH8MSUJpSbSDYk0k2/d10f7534-11f6-41fe-a012-2de1e482d336.mp3",
        "premade",
        "female",
        "british",
        EN,
    ),
    mapped(
        "Matilda",
        Some("A professional woman with a pleasing alto pitch. Suitable for many use cases."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/XrExE9yKIg1WjnnlVkGX/b930e18d-6b4d-466e-bab2-0ae97c6d8535.mp3",
        "premade",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Will",
        Some("Conversational and laid back."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/bIHbv24MWmeRgasZH58o/8caf8f3d-ad29-4980-af41-53f20c72d7a4.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Jessica",
        Some("Young and popular, this playful American female voice is perfect for trendy content."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/cgSgspJ2msm6clMCkdW9/56a97bf8-b69b-448f-846c-c3a11683d45a.mp3",
        "premade",
        "female",
        "american",
        EN,
    ),
    mapped(
        "Eric",
        Some("A smooth tenor pitch from a man in his 40s - perfect for agentic use cases."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/cjVigY5qzO86Huf0OWal/d098fda0-6456-4030-b3d8-63aa048c9070.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Chris",
        Some("Natural and real, this down-to-earth voice is great across many use-cases."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/iP95p4xoKVk53GoZ742B/3f4bde72-cc48-40dd-829f-57fbf906f4d7.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Brian",
        Some("Middle-aged man with a resonant and comforting tone. Great for narrations and advertisements."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/nPczCjzI2devNBz1zQrb/2dd3e72c-4fd3-42f1-93ea-abc5d4e5aa1d.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
    mapped(
        "Daniel",
        Some("A strong voice perfect for delivering a professional broadcast or news story."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/onwK4e9ZLuTAKqWW03F9/7eee0236-1a72-4b86-b303-5dcadc007ba9.mp3",
        "premade",
        "male",
        "british",
        EN,
    ),
    mapped(
        "Lily",
        Some("Velvety British female voice delivers news and narrations with warmth and clarity."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/pFZP5JQG7iQjIQuC4Bku/89b68b35-b3dd-4348-a84a-a3c13a3c2b30.mp3",
        "premade",
        "female",
        "british",
        EN,
    ),
    mapped(
        "Bill",
        Some("Friendly and comforting voice ready to narrate your stories."),
        "https://storage.googleapis.com/eleven-public-prod/premade/voices/pqHfZKP75CvOlQylNhV4/d782b3ff-84ba-4029-848c-acf01285524d.mp3",
        "premade",
        "male",
        "american",
        EN,
    ),
];

/// Voices accepted by Replicate's `elevenlabs/v3`.
pub fn replicate_voices() -> &'static [AudioVoice] {
    static VOICES: OnceLock<Vec<AudioVoice>> = OnceLock::new();
    VOICES.get_or_init(|| REPLICATE_ROWS.iter().map(Row::to_voice).collect())
}

/// Voices accepted by fal's ElevenLabs v3 endpoints.
pub fn fal_voices() -> &'static [AudioVoice] {
    static VOICES: OnceLock<Vec<AudioVoice>> = OnceLock::new();
    VOICES.get_or_init(|| FAL_ROWS.iter().map(Row::to_voice).collect())
}

/// Every catalog entry: [`replicate_voices`] followed by [`fal_voices`]. Note the two lists overlap
/// by name (e.g. `Rachel`) with different metadata, so ids are not unique across the whole slice.
pub fn all() -> &'static [AudioVoice] {
    static ALL: OnceLock<Vec<AudioVoice>> = OnceLock::new();
    ALL.get_or_init(|| replicate_voices().iter().chain(fal_voices()).cloned().collect())
}

/// First voice with the given id across [`all`] (Replicate list first).
pub fn voice(id: &str) -> Option<&'static AudioVoice> {
    all().iter().find(|v| v.id == id)
}

/// Looks a voice up in [`replicate_voices`].
pub fn replicate_voice(id: &str) -> Option<&'static AudioVoice> {
    replicate_voices().iter().find(|v| v.id == id)
}

/// Looks a voice up in [`fal_voices`].
pub fn fal_voice(id: &str) -> Option<&'static AudioVoice> {
    fal_voices().iter().find(|v| v.id == id)
}

/// The fal default voice (`Rachel`) from [`fal_voices`].
pub fn fal_default_voice() -> &'static AudioVoice {
    // The fal list is a compile-time constant that starts with Rachel; the fallback keeps this
    // total without panicking if the table is ever reordered.
    fal_voice(FAL_DEFAULT_VOICE_ID).unwrap_or(&fal_voices()[0])
}
