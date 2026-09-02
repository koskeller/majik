//! OpenRouter's prices, per model.
//!
//! **Checked 2026-08-29** against each model's page on openrouter.ai. Re-check from the slug table
//! in [`super::capabilities::model_slug_for_id`]; the figures go out of date whenever a price
//! moves, which is why the app always labels the number an estimate.
//!
//! Do NOT guess a price. A model whose page publishes no figure we can convert belongs in
//! `UNPRICED` in `tests/shared.rs`, not in a made-up row here.

use crate::pricing::{flat, per_megapixel, Estimate, PricedJob};
use crate::settings::ImageGenerationSettings;

use super::capabilities::*;

/// What we ask OpenRouter for. The per-megapixel models declare no resolutions, so the request
/// carries the default `image_size` of "1K", one megapixel, which is the minimum rate for all of
/// them.
const OUTPUT: (u32, u32) = (1024, 1024);

pub fn pricing(job: &PricedJob<'_>) -> Estimate {
    match job {
        PricedJob::Image(settings) => image(settings),
        // OpenRouter routes no video, audio or tool model; see the guard tests in tests/e2e.rs.
        _ => Estimate::Unknown,
    }
}

fn image(settings: &ImageGenerationSettings) -> Estimate {
    let (width, height) = OUTPUT;
    match settings.model.id {
        // Flat per output image, "regardless of size" in OpenRouter's own words.
        SEEDREAM_45 => flat(40_000),
        RIVERFLOW_2_MAX => flat(75_000),
        RIVERFLOW_2_STD => flat(35_000),
        RIVERFLOW_2_FAST => flat(30_000),

        // Per output megapixel, as `(first, each extra)`. FLUX also bills input megapixels at its
        // own rate; only the output is estimated here.
        FLUX_2_MAX => per_megapixel(70_000, 30_000, width, height),
        FLUX_2_PRO => per_megapixel(30_000, 15_000, width, height),
        FLUX_2_FLEX => per_megapixel(60_000, 60_000, width, height),
        FLUX_2_KLEIN => per_megapixel(14_000, 1_000, width, height),

        // The Gemini and GPT image models are billed per image-output *token* ($120/M, $60/M,
        // $30/M, $40/M, $8/M). OpenRouter doesn't publish how many tokens an image comes to, and
        // it varies with resolution, so there is no figure here we could rely on.
        _ => Estimate::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::ToolInput;
    use crate::settings::ToolSettings;
    use crate::catalog;
    use crate::models::{AspectRatio, ImageResolution};

    fn price(id: &str) -> Estimate {
        let model = catalog::image::model(id).expect("catalog model").clone();
        image(&ImageGenerationSettings { model, aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Hd })
    }

    fn dollars(estimate: Estimate) -> String {
        estimate.amount().expect("priced").to_string()
    }

    #[test]
    fn flat_per_image_models() {
        assert_eq!(dollars(price("seedream-4.5")), "$0.04");
        assert_eq!(dollars(price("riverflow-2-max")), "$0.075");
        assert_eq!(dollars(price("riverflow-2-std")), "$0.035");
        assert_eq!(dollars(price("riverflow-2-fast")), "$0.03");
    }

    #[test]
    fn flux_bills_the_first_output_megapixel() {
        assert_eq!(dollars(price("flux-2-max")), "$0.07");
        assert_eq!(dollars(price("flux-2-pro")), "$0.03");
        assert_eq!(dollars(price("flux-2-flex")), "$0.06", "dearer here than on fal");
        assert_eq!(dollars(price("flux-2-klein")), "$0.014");
    }

    #[test]
    fn token_billed_image_models_have_no_figure_we_can_convert() {
        for id in ["gemini-3-pro", "gemini-3.1-flash", "gemini-2.5-flash", "gpt-5-image", "gpt-5-image-mini", "gpt-image-2"] {
            assert_eq!(price(id), Estimate::Unknown, "{id} is billed per image-output token");
        }
    }

    #[test]
    fn openrouter_routes_nothing_but_images() {
        assert_eq!(pricing(&PricedJob::Tool { settings: &ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone()), input: ToolInput::default() }), Estimate::Unknown);
    }
}
