//! What a configured job costs, before it runs.
//!
//! Prices are **per provider per model**: the same catalog `flux-2-pro` costs a different amount on
//! fal, Replicate and OpenRouter, so a price can't hang off the shared catalog model structs in
//! [`crate::models`] (which are id-only by design). Each provider owns a `pricing` table beside its
//! capability table and hands it to the registry through [`crate::ProviderDescriptor::price`].
//!
//! Everything here is an *estimate*. Provider prices change, some models bill on the size of an
//! output we haven't produced yet, and a model we have no figure for reports [`Estimate::Unknown`]
//! rather than guessing.

use std::fmt;

use crate::settings::{AudioGenerationSettings, ImageGenerationSettings, ToolSettings, VideoGenerationSettings};

/// USD in micro-dollars ($1 = 1_000_000). An integer so the arithmetic and the string the user
/// reads can never disagree through a float.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Usd(pub u64);

impl Usd {
    pub const ZERO: Usd = Usd(0);

    /// One dollar in micro-dollars, the unit every table entry is written in.
    pub const PER_DOLLAR: u64 = 1_000_000;

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Usd {
    /// Money, rounded on the integer micro-dollars so the result never depends on float
    /// formatting. Two decimals from a dime up; three below it, because prices really do run
    /// $0.003 an image and rounding those to whole cents (`$0.025` → `$0.03`) is 20% out on a
    /// per-item figure. A trailing zero is dropped, so $0.04 doesn't read as `$0.040`.
    /// `Usd::ZERO` is "Free".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let micros = self.0;
        if micros == 0 {
            return f.write_str("Free");
        }
        if micros < 500 {
            // Rounds to nothing even at three decimals.
            return f.write_str("<$0.001");
        }
        if micros < 100_000 {
            let mills = (micros + 500) / 1_000;
            let text = format!("${}.{:03}", mills / 1_000, mills % 1_000);
            return f.write_str(text.strip_suffix('0').unwrap_or(&text));
        }
        let cents = (micros + 5_000) / 10_000;
        write!(f, "${}.{:02}", cents / 100, cents % 100)
    }
}

/// What one output of a configured job costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Estimate {
    Exact(Usd),
    /// No price data for this model on this provider. This is deliberate: the
    /// `every_supported_model_is_priced_or_listed_as_unpriced` guard test lists every model allowed
    /// to be here, and the composer says so instead of showing a number.
    Unknown,
}

impl Estimate {
    /// The price of `n` outputs. Saturates rather than wrapping; `Unknown` stays unknown.
    pub fn times(self, n: usize) -> Estimate {
        match self {
            Estimate::Exact(usd) => Estimate::Exact(Usd(usd.0.saturating_mul(n as u64))),
            Estimate::Unknown => Estimate::Unknown,
        }
    }

    pub fn amount(self) -> Option<Usd> {
        match self {
            Estimate::Exact(usd) => Some(usd),
            Estimate::Unknown => None,
        }
    }
}

/// The configured job to price.
///
/// This mirrors the settings structs rather than `majik_generation::GenerationType`, which sits
/// *above* this crate in the dependency direction (`app → generation → providers`).
#[derive(Clone, Copy, Debug)]
pub enum PricedJob<'a> {
    Image(&'a ImageGenerationSettings),
    Video(&'a VideoGenerationSettings),
    /// `characters` is the length of the text to speak; TTS bills per character.
    Audio { settings: &'a AudioGenerationSettings, characters: usize },
    /// A tool run. `input` is the asset it will run over: an image upscaler is flat per run, but a
    /// video one bills per second at a rate that depends on the *output* resolution, which follows
    /// from the input's and `settings.upscale_factor`.
    Tool { settings: &'a ToolSettings, input: ToolInput },
}

/// The size of the asset a tool will run over. Zeroes mean "not known yet" (nothing is attached, or
/// the asset was never probed), which prices as [`Estimate::Unknown`] rather than as free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolInput {
    pub width: u32,
    pub height: u32,
    /// Rounded up: a provider bills a part-second of video as a whole one.
    pub duration_secs: u32,
}

impl ToolInput {
    pub fn image(width: u32, height: u32) -> Self {
        Self { width, height, duration_secs: 0 }
    }

    pub fn video(width: u32, height: u32, duration_secs: u32) -> Self {
        Self { width, height, duration_secs }
    }

    /// The output's short edge after `factor`, which is what a per-resolution rate table is keyed
    /// on ("720p" means 720 lines, whichever way the clip is turned).
    pub fn output_lines(&self, factor: u32) -> u32 {
        self.width.min(self.height).saturating_mul(factor)
    }
}

impl PricedJob<'_> {
    /// The catalog id of the model being priced, which is the key every provider table matches on.
    pub fn model_id(&self) -> &str {
        match self {
            PricedJob::Image(s) => s.model.id,
            PricedJob::Video(s) => s.model.id,
            PricedJob::Audio { settings, .. } => settings.model.id,
            PricedJob::Tool { settings, .. } => settings.model.id,
        }
    }
}

// ----- table helpers ------------------------------------------------------------------------------
//
// Small constructors so a provider's table reads as data. They live here rather than in each
// provider module because the billing *shapes* are shared even though the numbers are not.

/// A flat price per output, whatever the settings.
pub fn flat(micros: u64) -> Estimate {
    Estimate::Exact(Usd(micros))
}

/// Billed by megapixels of output, with the first one charged at a floor rate.
///
/// Megapixels are rounded to the nearest, never below one, which reproduces the worked examples
/// fal publishes for this shape (1024×1024 = 1 MP, 1920×1080 = 2 MP, 512×512 still pays for one).
pub fn per_megapixel(first: u64, extra: u64, width: u32, height: u32) -> Estimate {
    let pixels = u64::from(width) * u64::from(height);
    let megapixels = ((pixels + 500_000) / 1_000_000).max(1);
    Estimate::Exact(Usd(first + extra * (megapixels - 1)))
}

/// Billed per second of output video.
pub fn per_second(micros_per_second: u64, duration: u32) -> Estimate {
    Estimate::Exact(Usd(micros_per_second * u64::from(duration)))
}

/// Billed per character of spoken text.
pub fn per_character(micros_per_character: u64, characters: usize) -> Estimate {
    Estimate::Exact(Usd(micros_per_character.saturating_mul(characters as u64)))
}

/// Picks a rate from a lookup table keyed on some setting, e.g. resolution or (resolution, audio).
/// `None` when the table has no row for `key`; the caller then returns [`Estimate::Unknown`], which
/// is the right answer for a combination we never priced.
pub fn rate<K: PartialEq>(table: &[(K, u64)], key: K) -> Option<u64> {
    table.iter().find(|(k, _)| *k == key).map(|(_, micros)| *micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollars_render_with_two_decimals() {
        assert_eq!(Usd(3_200_000).to_string(), "$3.20");
        assert_eq!(Usd(19_200_000).to_string(), "$19.20");
        assert_eq!(Usd(40_000).to_string(), "$0.04");
        assert_eq!(Usd(10_000).to_string(), "$0.01");
    }

    #[test]
    fn prices_under_a_dime_keep_a_third_decimal() {
        assert_eq!(Usd(3_000).to_string(), "$0.003", "flux-schnell really is this cheap");
        assert_eq!(Usd(25_000).to_string(), "$0.025", "not $0.03");
        assert_eq!(Usd(18_000).to_string(), "$0.018");
        assert_eq!(Usd(39_000).to_string(), "$0.039");
        assert_eq!(Usd(1_000).to_string(), "$0.001");
    }

    #[test]
    fn a_trailing_zero_is_dropped() {
        assert_eq!(Usd(40_000).to_string(), "$0.04", "not $0.040");
        assert_eq!(Usd(70_000).to_string(), "$0.07");
        assert_eq!(Usd(99_999).to_string(), "$0.10");
    }

    #[test]
    fn rounding_happens_on_the_micros_not_on_a_float() {
        // 0.575 is just under 0.575 as an f64, so `{:.2}` would render $0.57.
        assert_eq!(Usd(575_000).to_string(), "$0.58");
        assert_eq!(Usd(4_500).to_string(), "$0.005");
    }

    #[test]
    fn prices_below_a_tenth_of_a_cent_collapse() {
        assert_eq!(Usd(499).to_string(), "<$0.001");
        assert_eq!(Usd(1).to_string(), "<$0.001");
    }

    #[test]
    fn zero_is_free() {
        assert_eq!(Usd::ZERO.to_string(), "Free");
        assert!(Usd::ZERO.is_zero());
    }

    #[test]
    fn times_multiplies_the_batch() {
        assert_eq!(flat(40_000).times(4), Estimate::Exact(Usd(160_000)));
        assert_eq!(flat(40_000).times(1), Estimate::Exact(Usd(40_000)));
        assert_eq!(flat(40_000).times(0), Estimate::Exact(Usd::ZERO));
    }

    #[test]
    fn times_saturates_rather_than_wrapping() {
        assert_eq!(Estimate::Exact(Usd(u64::MAX)).times(2), Estimate::Exact(Usd(u64::MAX)));
    }

    #[test]
    fn unknown_stays_unknown_however_many_outputs() {
        assert_eq!(Estimate::Unknown.times(8), Estimate::Unknown);
        assert_eq!(Estimate::Unknown.amount(), None);
    }

    #[test]
    fn megapixels_round_to_nearest_with_a_one_megapixel_floor() {
        // $0.03 first megapixel, $0.015 each extra: fal's flux-2-pro shape, checked against the
        // worked examples on fal.ai/models/fal-ai/flux-2-pro.
        assert_eq!(per_megapixel(30_000, 15_000, 512, 512), Estimate::Exact(Usd(30_000)), "a part megapixel still pays for one");
        assert_eq!(per_megapixel(30_000, 15_000, 1024, 1024), Estimate::Exact(Usd(30_000)));
        assert_eq!(per_megapixel(30_000, 15_000, 1920, 1080), Estimate::Exact(Usd(45_000)), "2.07 MP is 2");
        assert_eq!(per_megapixel(30_000, 15_000, 3840, 2160), Estimate::Exact(Usd(135_000)), "8.29 MP is 8");
    }

    #[test]
    fn per_second_scales_with_duration() {
        assert_eq!(per_second(400_000, 8), Estimate::Exact(Usd(3_200_000)));
        assert_eq!(per_second(400_000, 0), Estimate::Exact(Usd::ZERO));
    }

    #[test]
    fn per_character_scales_with_text_length() {
        assert_eq!(per_character(300, 1_000), Estimate::Exact(Usd(300_000)));
        assert_eq!(per_character(300, 0), Estimate::Exact(Usd::ZERO));
    }

    #[test]
    fn rate_finds_a_row_or_reports_the_gap() {
        let table = [((true, 1u8), 10_u64), ((false, 1), 20)];
        assert_eq!(rate(&table, (true, 1)), Some(10));
        assert_eq!(rate(&table, (false, 1)), Some(20));
        assert_eq!(rate(&table, (true, 2)), None, "an unpriced combination is a gap, not a zero");
    }
}
