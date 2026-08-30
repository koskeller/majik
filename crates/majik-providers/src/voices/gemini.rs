//! The 30 Gemini / Chirp 3 HD voices accepted by fal's `gemini-tts` endpoint. Ids are the literal
//! wire values; every voice is multilingual and has a Google-hosted preview clip.

use crate::models::AudioVoice;
use std::sync::OnceLock;

/// Prefix of every preview URL; the row's file name is appended to it.
const PREVIEW_BASE_URL: &str = "https://docs.cloud.google.com/static/text-to-speech/docs/audio/";

/// `(name, gender, preview file name)`, in catalog order.
const ROWS: &[(&str, &str, &str)] = &[
    ("Achernar", "female", "chirp3-hd-achernar.wav"),
    ("Achird", "male", "chirp3-hd-achird.wav"),
    ("Algenib", "male", "chirp3-hd-algenib.wav"),
    ("Algieba", "male", "chirp3-hd-algieba.wav"),
    ("Alnilam", "male", "chirp3-hd-alnilam.wav"),
    ("Aoede", "female", "chirp3-hd-aoeda.wav"),
    ("Autonoe", "female", "chirp3-hd-autonoe.wav"),
    ("Callirrhoe", "female", "chirp3-hd-callirrhoe.wav"),
    ("Charon", "male", "chirp3-hd-charon.wav"),
    ("Despina", "female", "chirp3-hd-despina.wav"),
    ("Enceladus", "male", "chirp3-hd-enceladus.wav"),
    ("Erinome", "female", "chirp3-hd-erinome.wav"),
    ("Fenrir", "male", "chirp3-hd-fenrir.wav"),
    ("Gacrux", "female", "chirp3-hd-gacrux.wav"),
    ("Iapetus", "male", "chirp3-hd-iapetus.wav"),
    ("Kore", "female", "chirp3-hd-kore.wav"),
    ("Laomedeia", "female", "chirp3-hd-laomedeia.wav"),
    ("Leda", "female", "chirp3-hd-leda.wav"),
    ("Orus", "male", "chirp3-hd-orus.wav"),
    ("Pulcherrima", "female", "chirp3-hd-pulcherrima.wav"),
    ("Puck", "male", "chirp3-hd-puck.wav"),
    ("Rasalgethi", "male", "chirp3-hd-rasalgethi.wav"),
    ("Sadachbia", "male", "chirp3-hd-sadachbia.wav"),
    ("Sadaltager", "male", "chirp3-hd-sadaltager.wav"),
    ("Schedar", "male", "chirp3-hd-schedar.wav"),
    ("Sulafat", "female", "chirp3-hd-sulafat.wav"),
    ("Umbriel", "male", "chirp3-hd-umbriel.wav"),
    ("Vindemiatrix", "female", "chirp3-hd-vindemiatrix.wav"),
    ("Zephyr", "female", "chirp3-hd-zephyr.wav"),
    ("Zubenelgenubi", "male", "chirp3-hd-zubenelgenubi.wav"),
];

fn voice_from_row(name: &str, gender: &str, preview_path: &str) -> AudioVoice {
    AudioVoice {
        id: name.to_string(),
        display_name: name.to_string(),
        subtitle: None,
        preview_url: Some(format!("{PREVIEW_BASE_URL}{preview_path}")),
        category: None,
        gender: Some(gender.to_string()),
        accent: None,
        language_codes: Some(vec!["multilingual".to_string()]),
    }
}

/// Every Gemini TTS voice, in catalog order.
pub fn all() -> &'static [AudioVoice] {
    static ALL: OnceLock<Vec<AudioVoice>> = OnceLock::new();
    ALL.get_or_init(|| ROWS.iter().map(|(name, gender, path)| voice_from_row(name, gender, path)).collect())
}

/// Looks a voice up by its wire id (e.g. `"Kore"`).
pub fn voice(id: &str) -> Option<&'static AudioVoice> {
    all().iter().find(|v| v.id == id)
}
