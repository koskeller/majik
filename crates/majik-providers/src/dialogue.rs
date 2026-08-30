//! Splits a dialogue prompt with `Speaker 1:` / `Speaker 2:` line prefixes into ordered turns.
//! Pure, no I/O.
//!
//! Rules:
//! - A line whose trimmed content starts with `Speaker 1:` or `Speaker 2:` (case-insensitive)
//!   opens a new turn.
//! - Lines without a recognized prefix attach to the most recent turn (or to a synthetic
//!   Speaker 1 turn at the very top of the prompt).
//! - Adjacent turns with the same speaker are merged.
//! - Empty / whitespace-only prompts return `[]`.
//!
//! The core parser knows nothing about voices ([`parse_dialogue`]); [`parse_dialogue_with_voices`]
//! resolves speakers to voices and then merges turns with equal voices, which merges everything
//! when both speakers share a voice.

use crate::models::AudioVoice;

/// Which of the two dialogue speakers a turn belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Speaker {
    One,
    Two,
}

impl Speaker {
    /// The line-leading label (lower-cased) that opens a turn for this speaker.
    fn label(self) -> &'static str {
        match self {
            Speaker::One => "speaker 1:",
            Speaker::Two => "speaker 2:",
        }
    }
}

/// One contiguous block of text spoken by a single speaker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogueTurn {
    pub speaker: Speaker,
    pub text: String,
}

/// A [`DialogueTurn`] resolved to a concrete voice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceTurn {
    pub voice: AudioVoice,
    pub text: String,
}

/// Split `text` into ordered turns, with the voices abstracted away.
pub fn parse_dialogue(text: &str) -> Vec<DialogueTurn> {
    let mut turns: Vec<(Speaker, Vec<&str>)> = Vec::new();

    for raw in text.split('\n') {
        let trimmed = trim_whitespace(raw);
        if trimmed.is_empty() {
            continue;
        }

        if let Some((speaker, body)) = match_speaker_label(trimmed) {
            turns.push((speaker, if body.is_empty() { Vec::new() } else { vec![body] }));
        } else if let Some(last) = turns.last_mut() {
            last.1.push(trimmed);
        } else {
            turns.push((Speaker::One, vec![trimmed]));
        }
    }

    // Merge adjacent same-speaker turns.
    let mut merged: Vec<(Speaker, Vec<&str>)> = Vec::new();
    for (speaker, lines) in turns {
        match merged.last_mut() {
            Some((last_speaker, last_lines)) if *last_speaker == speaker => last_lines.extend(lines),
            _ => merged.push((speaker, lines)),
        }
    }

    merged
        .into_iter()
        .filter_map(|(speaker, lines)| {
            let text = lines.join("\n");
            (!text.is_empty()).then_some(DialogueTurn { speaker, text })
        })
        .collect()
}

/// Resolve speakers to voices and merge adjacent turns that end up with the same voice (which also
/// collapses the whole prompt when `speaker1 == speaker2`).
pub fn parse_dialogue_with_voices(text: &str, speaker1: &AudioVoice, speaker2: &AudioVoice) -> Vec<VoiceTurn> {
    let mut merged: Vec<VoiceTurn> = Vec::new();
    for turn in parse_dialogue(text) {
        let voice = match turn.speaker {
            Speaker::One => speaker1,
            Speaker::Two => speaker2,
        };
        match merged.last_mut() {
            Some(last) if &last.voice == voice => {
                last.text.push('\n');
                last.text.push_str(&turn.text);
            }
            _ => merged.push(VoiceTurn { voice: voice.clone(), text: turn.text }),
        }
    }
    merged
}

/// The speaker and the label-less, trimmed body when `trimmed` starts (case-insensitively) with a
/// speaker label.
fn match_speaker_label(trimmed: &str) -> Option<(Speaker, &str)> {
    [Speaker::One, Speaker::Two].into_iter().find_map(|speaker| strip_label(trimmed, speaker.label()).map(|body| (speaker, body)))
}

/// Case-insensitive (Unicode lower-casing) prefix strip that advances character by character, so
/// the returned slice indexes into the original string.
fn strip_label<'a>(trimmed: &'a str, label: &str) -> Option<&'a str> {
    let mut rest = trimmed;
    for expected in label.chars() {
        let c = rest.chars().next()?;
        if !c.to_lowercase().eq(std::iter::once(expected)) {
            return None;
        }
        rest = &rest[c.len_utf8()..];
    }
    Some(trim_whitespace(rest))
}

/// Trims Unicode `Zs` (space separators) plus tab, but not newlines and not the other control
/// characters Rust's `char::is_whitespace` covers.
fn trim_whitespace(s: &str) -> &str {
    s.trim_matches(is_space_separator)
}

fn is_space_separator(c: char) -> bool {
    matches!(c, '\t' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}
