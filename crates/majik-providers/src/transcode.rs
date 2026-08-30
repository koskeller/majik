//! Transcoding to PNG, using the `image` crate.

pub fn transcode_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Sniffs MP3 / WAV / Ogg magic bytes → MIME type: enough to tell "the provider sent audio" from
/// "the provider sent an error page with a 200", without decoding.
pub fn sniff_audio_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"ID3") {
        Some("audio/mpeg")
    } else if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0 {
        // An MP3 frame sync with no ID3 tag in front of it.
        Some("audio/mpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if bytes.starts_with(b"OggS") {
        Some("audio/ogg")
    } else {
        None
    }
}

/// Sniffs PNG / JPEG / GIF / WebP magic bytes → MIME type.
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_the_audio_containers_a_provider_returns() {
        assert_eq!(sniff_audio_mime(b"ID3\x04\x00rest"), Some("audio/mpeg"));
        assert_eq!(sniff_audio_mime(&[0xFF, 0xFB, 0x90, 0x00]), Some("audio/mpeg"), "a bare frame sync");
        assert_eq!(sniff_audio_mime(&[0xFF, 0xE0]), Some("audio/mpeg"), "the lowest valid sync");
        assert_eq!(sniff_audio_mime(b"RIFF\x24\x08\x00\x00WAVEfmt "), Some("audio/wav"));
        assert_eq!(sniff_audio_mime(b"OggS\x00\x02"), Some("audio/ogg"));
    }

    #[test]
    fn anything_else_is_not_audio() {
        assert_eq!(sniff_audio_mime(b"{\"error\":\"nope\"}"), None, "a JSON error body served as 200");
        assert_eq!(sniff_audio_mime(b""), None);
        assert_eq!(sniff_audio_mime(&[0xFF]), None, "a truncated sync is not a match");
        assert_eq!(sniff_audio_mime(&[0xFF, 0x0F]), None, "the sync bits must be set");
        // A WebP is RIFF too, but it is not audio.
        assert_eq!(sniff_audio_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "), None);
        assert_eq!(sniff_audio_mime(&majik_core::images::solid_png(2, 2, [1, 2, 3])), None);
    }
}
