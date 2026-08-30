//! Reference inputs a prompt can address by handle.
//!
//! Majik has one handle grammar — `@Image1`, `@Video2`, `@Audio1`, numbered from the asset's
//! position within its role — and every model spells it differently, so the handles are rewritten
//! into the model's own dialect when its request body is built. The canonical form is what the
//! library stores, so a prompt survives a model or provider switch.

use crate::{AssetRole, ProviderAsset};

/// How a model wants its references addressed. Taken from each endpoint's own schema; like the
/// price tables these drift, so each provider's table carries the date it was checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceTagStyle {
    /// `@Image1` — Majik's own form, so the prompt goes out unchanged (fal Seedance, fal Grok 1.0).
    At,
    /// `[Image1]` (Replicate Seedance).
    Bracketed,
    /// `[Image 1]` (Replicate Happy Horse).
    BracketedSpaced,
    /// `character1` … `character9`, images only (fal Happy Horse).
    Character,
    /// `<IMAGE_0>`, **zero-based** (fal Grok Imagine Video 1.5).
    AngleZeroBased,
    /// `Image 1` in prose — for a model that documents no syntax (fal Veo 3.1, Gemini Omni Flash,
    /// Wan 2.7) or asks for prose outright (Wan 3.0, H3). It reads as part of the sentence the user
    /// wrote either way, which a literal `@Image1` would not.
    Prose,
}

/// How many references of each kind a request carries. A handle past the end of its list is left
/// alone rather than rewritten into a reference the model doesn't have; `validation` rejects the
/// request before it is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReferenceCounts {
    pub images: usize,
    pub videos: usize,
    pub audio: usize,
}

impl ReferenceCounts {
    pub fn of(&self, role: AssetRole) -> usize {
        match role {
            AssetRole::ReferenceImage => self.images,
            AssetRole::ReferenceVideo => self.videos,
            AssetRole::Audio => self.audio,
            _ => 0,
        }
    }

    pub fn total(&self) -> usize {
        self.images + self.videos + self.audio
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// The reference media of one request, in the order the composer attached them — which is the
/// order their handles are numbered in, and the order they go into the provider's arrays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReferenceAssets<'a> {
    pub images: Vec<&'a ProviderAsset>,
    pub videos: Vec<&'a ProviderAsset>,
    pub audio: Vec<&'a ProviderAsset>,
}

impl<'a> ReferenceAssets<'a> {
    /// Sorts a request's assets into their reference lists, ignoring every other role.
    pub fn from_assets(assets: &'a [ProviderAsset]) -> Self {
        let of = |role: AssetRole| assets.iter().filter(move |a| a.role == role).collect();
        Self { images: of(AssetRole::ReferenceImage), videos: of(AssetRole::ReferenceVideo), audio: of(AssetRole::Audio) }
    }

    /// Each list with the role it fills, in handle order.
    pub fn lists(&self) -> [(AssetRole, &Vec<&'a ProviderAsset>); 3] {
        [(AssetRole::ReferenceImage, &self.images), (AssetRole::ReferenceVideo, &self.videos), (AssetRole::Audio, &self.audio)]
    }

    pub fn counts(&self) -> ReferenceCounts {
        ReferenceCounts { images: self.images.len(), videos: self.videos.len(), audio: self.audio.len() }
    }

    /// Whether anything the prompt could address by handle is attached.
    pub fn is_empty(&self) -> bool {
        self.counts().is_empty()
    }

    /// References other than audio. Audio alone is never a reference request: every provider that
    /// takes reference audio requires an image or a video with it.
    pub fn has_visual(&self) -> bool {
        !self.images.is_empty() || !self.videos.is_empty()
    }
}

/// The keyword each reference role is addressed by.
fn keyword(role: AssetRole) -> Option<&'static str> {
    match role {
        AssetRole::ReferenceImage => Some("Image"),
        AssetRole::ReferenceVideo => Some("Video"),
        AssetRole::Audio => Some("Audio"),
        _ => None,
    }
}

/// The handle for the `index`-th (1-based) reference in `role`, as the composer shows it and the
/// user types it: `@Image1`.
pub fn handle(role: AssetRole, index: usize) -> String {
    match keyword(role) {
        Some(word) => format!("@{word}{index}"),
        None => String::new(),
    }
}

/// Every handle in `prompt`, in the order it appears, as `(role, 1-based index)`. Used to tell the
/// user a handle points at a reference they haven't attached.
pub fn handles(prompt: &str) -> Vec<(AssetRole, usize)> {
    let bytes = prompt.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'@' {
            if let Some((role, index, end)) = parse_handle(bytes, at) {
                found.push((role, index));
                at = end;
                continue;
            }
        }
        at += 1;
    }
    found
}

/// Rewrites Majik's handles into `style`. Anything that isn't a handle — an email address, a bare
/// `@`, a handle past the end of its list — is copied through untouched.
pub fn rewrite_handles(prompt: &str, counts: ReferenceCounts, style: ReferenceTagStyle) -> String {
    if style == ReferenceTagStyle::At || counts.is_empty() {
        return prompt.to_string();
    }
    let bytes = prompt.as_bytes();
    let mut out = String::with_capacity(prompt.len());
    let mut at = 0;
    let mut copied = 0;
    while at < bytes.len() {
        if bytes[at] == b'@' {
            if let Some((role, index, end)) = parse_handle(bytes, at) {
                if index >= 1 && index <= counts.of(role) {
                    out.push_str(&prompt[copied..at]);
                    out.push_str(&render(role, index, style));
                    at = end;
                    copied = end;
                    continue;
                }
            }
        }
        at += 1;
    }
    out.push_str(&prompt[copied..]);
    out
}

/// Parses `@Image12` at `start` (which must be the `@`), returning the role, the 1-based index and
/// the byte offset just past the digits. The keyword is matched case-insensitively; the handle ends
/// at the last digit, so `@Image10` is reference 10 rather than reference 1 followed by a `0`.
fn parse_handle(bytes: &[u8], start: usize) -> Option<(AssetRole, usize, usize)> {
    for role in [AssetRole::ReferenceImage, AssetRole::ReferenceVideo, AssetRole::Audio] {
        let word = keyword(role)?.as_bytes();
        let after_word = start + 1 + word.len();
        if after_word > bytes.len() || !bytes[start + 1..after_word].eq_ignore_ascii_case(word) {
            continue;
        }
        let mut end = after_word;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == after_word {
            return None;
        }
        let index: usize = std::str::from_utf8(&bytes[after_word..end]).ok()?.parse().ok()?;
        return Some((role, index, end));
    }
    None
}

fn render(role: AssetRole, index: usize, style: ReferenceTagStyle) -> String {
    let word = keyword(role).unwrap_or("Image");
    match style {
        ReferenceTagStyle::At => format!("@{word}{index}"),
        ReferenceTagStyle::Bracketed => format!("[{word}{index}]"),
        ReferenceTagStyle::BracketedSpaced => format!("[{word} {index}]"),
        // Happy Horse names only its images, and only up to nine of them.
        ReferenceTagStyle::Character => match role {
            AssetRole::ReferenceImage if index <= 9 => format!("character{index}"),
            _ => format!("{word} {index}"),
        },
        ReferenceTagStyle::AngleZeroBased => format!("<{}_{}>", word.to_uppercase(), index - 1),
        ReferenceTagStyle::Prose => format!("{word} {index}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(images: usize, videos: usize, audio: usize) -> ReferenceCounts {
        ReferenceCounts { images, videos, audio }
    }

    #[test]
    fn every_dialect_rewrites_the_same_prompt() {
        let prompt = "@Image1 walks past @Image2 while @Video1 plays over @Audio1";
        let c = counts(2, 1, 1);
        assert_eq!(rewrite_handles(prompt, c, ReferenceTagStyle::At), prompt, "fal's own form goes out unchanged");
        assert_eq!(
            rewrite_handles(prompt, c, ReferenceTagStyle::Bracketed),
            "[Image1] walks past [Image2] while [Video1] plays over [Audio1]"
        );
        assert_eq!(
            rewrite_handles(prompt, c, ReferenceTagStyle::BracketedSpaced),
            "[Image 1] walks past [Image 2] while [Video 1] plays over [Audio 1]"
        );
        assert_eq!(
            rewrite_handles(prompt, c, ReferenceTagStyle::Prose),
            "Image 1 walks past Image 2 while Video 1 plays over Audio 1"
        );
        assert_eq!(
            rewrite_handles(prompt, c, ReferenceTagStyle::AngleZeroBased),
            "<IMAGE_0> walks past <IMAGE_1> while <VIDEO_0> plays over <AUDIO_0>",
            "the only zero-based dialect"
        );
        assert_eq!(
            rewrite_handles(prompt, c, ReferenceTagStyle::Character),
            "character1 walks past character2 while Video 1 plays over Audio 1",
            "Happy Horse names images only"
        );
    }

    #[test]
    fn character_falls_back_past_nine() {
        let c = counts(12, 0, 0);
        assert_eq!(rewrite_handles("@Image9 and @Image10", c, ReferenceTagStyle::Character), "character9 and Image 10");
    }

    /// `@Image10` is the tenth reference, not the first followed by a zero.
    #[test]
    fn the_longest_number_wins() {
        let c = counts(12, 0, 0);
        assert_eq!(rewrite_handles("@Image10", c, ReferenceTagStyle::Prose), "Image 10");
        assert_eq!(rewrite_handles("@Image1 0", c, ReferenceTagStyle::Prose), "Image 1 0");
    }

    #[test]
    fn anything_that_is_not_a_handle_is_left_alone() {
        let c = counts(3, 0, 0);
        for prompt in ["write to me@example.com", "an @ sign", "@Image", "@Images1", "@ Image1", "email @imagemagick"] {
            assert_eq!(rewrite_handles(prompt, c, ReferenceTagStyle::Prose), prompt, "{prompt}");
        }
    }

    /// A handle past the end of its list would become a reference the model doesn't have;
    /// `validation` rejects the request, and until then the text stays as the user typed it.
    #[test]
    fn a_handle_past_the_count_is_untouched() {
        let c = counts(2, 0, 0);
        assert_eq!(rewrite_handles("@Image1 and @Image3", c, ReferenceTagStyle::Prose), "Image 1 and @Image3");
        assert_eq!(rewrite_handles("@Video1", c, ReferenceTagStyle::Prose), "@Video1", "no videos attached");
    }

    #[test]
    fn the_keyword_is_case_insensitive() {
        let c = counts(1, 0, 0);
        assert_eq!(rewrite_handles("@image1 and @IMAGE1", c, ReferenceTagStyle::Prose), "Image 1 and Image 1");
    }

    #[test]
    fn nothing_is_rewritten_without_references() {
        assert_eq!(rewrite_handles("@Image1", ReferenceCounts::default(), ReferenceTagStyle::Prose), "@Image1");
    }

    #[test]
    fn handles_are_listed_in_order() {
        assert_eq!(
            handles("@Video2 then @Image1, not me@example.com"),
            vec![(AssetRole::ReferenceVideo, 2), (AssetRole::ReferenceImage, 1)]
        );
    }

    #[test]
    fn handle_renders_what_the_scanner_reads() {
        for role in [AssetRole::ReferenceImage, AssetRole::ReferenceVideo, AssetRole::Audio] {
            let text = handle(role, 3);
            assert_eq!(handles(&text), vec![(role, 3)], "{text}");
        }
    }

    /// The rewrite walks bytes; a prompt with multibyte characters around a handle must not split
    /// one of them.
    #[test]
    fn multibyte_text_survives() {
        let c = counts(1, 0, 0);
        assert_eq!(rewrite_handles("café @Image1 — ok ✅", c, ReferenceTagStyle::Prose), "café Image 1 — ok ✅");
    }
}
