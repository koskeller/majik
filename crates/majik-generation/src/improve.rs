//! Rewriting a prompt with a small text model: the instruction the composer sends and the request
//! that carries it. No HTTP here — [`crate::engine::Engine`] makes the call.

use majik_core::model::MediaType;
use majik_providers::{AssetRole, GenerationError, ProviderDescriptor, ProviderId};

use crate::request::GenerationType;
use crate::validation::prompt_character_limit;

/// Tokens to allow the rewrite when the model declares no prompt cap. Generous for a paragraph,
/// small enough that a model that starts rambling is cut off rather than billed for.
const DEFAULT_MAX_TOKENS: usize = 400;

/// Where a rewrite's single outcome arrives. Dropping it walks away from the call.
pub type ImproveReceiver = async_channel::Receiver<Result<String, GenerationError>>;
/// The other end, held by whatever is running the rewrite.
pub type ImproveSender = async_channel::Sender<Result<String, GenerationError>>;

/// The one-slot channel a rewrite reports through. A [`crate::engine::JobRunner`] implementation
/// makes one per ask; the caller awaits the receiver.
pub fn improve_channel() -> (ImproveSender, ImproveReceiver) {
    async_channel::bounded(1)
}

/// What the engine needs to run one rewrite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRequest {
    pub provider: ProviderId,
    pub system: String,
    pub user: String,
    pub max_tokens: usize,
}

/// The rewrite of `prompt` for what the composer currently has selected.
pub fn text_request(prompt: &str, generation_type: &GenerationType, provider: &ProviderDescriptor, reference_roles: &[AssetRole]) -> TextRequest {
    let limit = prompt_character_limit(generation_type, provider).ok().flatten();
    TextRequest {
        provider: provider.id.clone(),
        system: instruction(generation_type, provider, reference_roles),
        user: prompt.trim().to_string(),
        // Roughly four characters to a token, so a capped prompt gets the budget it can use.
        max_tokens: limit.map(|l| (l / 3).clamp(64, 2000)).unwrap_or(DEFAULT_MAX_TOKENS),
    }
}

/// The instruction the model rewrites under: what it is writing for, what is already decided
/// elsewhere (ratio, resolution, duration), what the user attached, and how to answer.
/// The handles the attached references are addressed by, in the order the roles arrive — the same
/// numbering the composer shows and the provider clients rewrite.
fn reference_handles(reference_roles: &[AssetRole]) -> Vec<String> {
    let mut seen: Vec<AssetRole> = Vec::new();
    reference_roles
        .iter()
        .filter(|role| role.is_reference())
        .map(|role| {
            seen.push(*role);
            majik_providers::references::handle(*role, seen.iter().filter(|r| *r == role).count())
        })
        .collect()
}

pub fn instruction(generation_type: &GenerationType, provider: &ProviderDescriptor, reference_roles: &[AssetRole]) -> String {
    let media = match generation_type.media_type() {
        MediaType::Image => "an image",
        MediaType::Video => "a video",
        MediaType::Audio => "an audio",
    };
    let manufacturer = manufacturer(generation_type);
    let mut lines = vec![
        "You rewrite prompts for AI media generation.".to_string(),
        format!("Rewrite the user's prompt into one strong prompt for {} by {manufacturer}, {media} generation model.", generation_type.model_name()),
        "Keep the user's subject, intent, named styles, people and any text to be rendered. Add concrete visual detail: composition, lighting, materials, colour, camera and lens, mood.".to_string(),
    ];

    if let GenerationType::Video(settings) = generation_type {
        lines.push(format!(
            "Describe motion and what changes over the {}s shot, including camera movement.",
            settings.duration
        ));
        if settings.audio_enabled {
            lines.push("The model also generates sound — describe it in one clause.".to_string());
        }
    }

    if !reference_roles.is_empty() {
        let roles = reference_roles.iter().map(|r| r.display_name().to_lowercase()).collect::<Vec<_>>().join(", ");
        // The rewriter is a text call: the images go to the generation model, not to it. Saying
        // they are "attached" makes a literal model answer "I don't see any images" — which would
        // land in the prompt field.
        lines.push(format!(
            "The generation will also receive {} reference image{} ({roles}), which you cannot see. Write the prompt so it refers to them (\"the subject in the reference\", \"the style of the reference\") instead of describing or inventing what they show.",
            reference_roles.len(),
            if reference_roles.len() == 1 { "" } else { "s" },
        ));
        // A handle is how the prompt points at one specific reference; paraphrasing it away breaks
        // the link the generation model resolves, and renaming it points at the wrong file.
        let handles = reference_handles(reference_roles);
        if !handles.is_empty() {
            lines.push(format!(
                "The prompt addresses the references by handle ({}). Keep every handle exactly as written — never rename, renumber, drop or explain one.",
                handles.join(", ")
            ));
        }
    }

    if let Some(fixed) = fixed_settings(generation_type) {
        lines.push(format!("The output is {fixed} — do not mention aspect ratio, resolution or duration."));
    }

    if let Some(limit) = prompt_character_limit(generation_type, provider).ok().flatten() {
        lines.push(format!("Stay under {limit} characters."));
    }

    // A terse prompt ("make it snowy") over references the rewriter cannot see invites a clarifying
    // question, and the answer goes straight into the prompt field. There is nobody to answer it.
    lines.push(
        "Reply with the rewritten prompt only: one paragraph, plain text, no quotes, no preamble, no alternatives. Never ask a question or request more detail — if the prompt is terse or leans on something you cannot see, write the best prompt you can from what it gives you."
            .to_string(),
    );
    lines.join("\n")
}

/// The settings the composer has already fixed, so the rewrite doesn't restate or fight them.
fn fixed_settings(generation_type: &GenerationType) -> Option<String> {
    match generation_type {
        GenerationType::Image(s) => Some(format!("{}, {}", s.aspect_ratio.raw(), s.resolution.raw())),
        GenerationType::Video(s) => {
            let mut parts: Vec<&str> = Vec::new();
            if let Some(ratio) = s.aspect_ratio {
                parts.push(ratio.raw());
            }
            if let Some(resolution) = s.resolution {
                parts.push(resolution.raw());
            }
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        GenerationType::Audio(_) | GenerationType::Upscale(_) | GenerationType::RemoveBackground(_) => None,
    }
}

fn manufacturer(generation_type: &GenerationType) -> &str {
    match generation_type {
        GenerationType::Image(s) => s.model.manufacturer,
        GenerationType::Video(s) => s.model.manufacturer,
        GenerationType::Audio(s) => s.model.manufacturer,
        GenerationType::Upscale(s) | GenerationType::RemoveBackground(s) => s.model.manufacturer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_providers::{catalog, AspectRatio, ImageGenerationSettings, ImageResolution, VideoAspectRatio, VideoGenerationSettings, VideoResolution};

    fn provider() -> &'static ProviderDescriptor {
        majik_providers::mock::descriptor()
    }

    fn image() -> GenerationType {
        let model = catalog::image::ALL.first().expect("catalog populated").clone();
        GenerationType::Image(ImageGenerationSettings { model, aspect_ratio: AspectRatio::Portrait, resolution: ImageResolution::Fhd })
    }

    fn video(model: &majik_providers::VideoModel, audio: bool) -> GenerationType {
        GenerationType::Video(VideoGenerationSettings {
            model: model.clone(),
            aspect_ratio: Some(VideoAspectRatio::Landscape),
            resolution: Some(VideoResolution::Hd),
            duration: 8,
            audio_enabled: audio,
        })
    }

    #[test]
    fn an_image_instruction_names_the_model_and_the_settings_already_chosen() {
        let gt = image();
        let text = instruction(&gt, provider(), &[]);
        assert!(text.contains(gt.model_name()), "{text}");
        assert!(text.contains("an image generation model"), "{text}");
        assert!(text.contains("4:5") && text.contains("2K"), "the fixed settings are named: {text}");
        assert!(text.contains("do not mention aspect ratio"), "{text}");
        assert!(text.ends_with("write the best prompt you can from what it gives you."), "the answer format comes last: {text}");
        assert!(text.contains("Never ask a question"), "there is nobody to answer one: {text}");
        assert!(!text.contains("reference image"), "nothing attached, nothing said: {text}");
    }

    #[test]
    fn a_video_instruction_asks_for_motion_and_sound_only_when_there_is_sound() {
        let with_audio = instruction(&video(&catalog::video::VEO_31, true), provider(), &[]);
        assert!(with_audio.contains("over the 8s shot"), "{with_audio}");
        assert!(with_audio.contains("generates sound"), "{with_audio}");
        let silent = instruction(&video(&catalog::video::VEO_31, false), provider(), &[]);
        assert!(silent.contains("over the 8s shot"), "{silent}");
        assert!(!silent.contains("generates sound"), "{silent}");
    }

    #[test]
    fn attached_references_are_counted_and_named_by_role() {
        let one = instruction(&image(), provider(), &[AssetRole::ReferenceImage]);
        assert!(one.contains("receive 1 reference image (image)"), "{one}");
        let two = instruction(&video(&catalog::video::VEO_31, false), provider(), &[AssetRole::FirstFrame, AssetRole::LastFrame]);
        assert!(two.contains("receive 2 reference images (first frame, last frame)"), "{two}");
        assert!(two.contains("which you cannot see"), "the rewriter is a text call: {two}");
        assert!(two.contains("instead of describing or inventing what they show"), "{two}");
    }

    #[test]
    fn a_declared_prompt_cap_bounds_the_instruction_and_the_token_budget() {
        // Kling documents 2500 characters; image models declare no cap.
        let capped = text_request("a cat", &video(&catalog::video::KLING_30_PRO, false), provider(), &[]);
        assert!(capped.system.contains("Stay under 2500 characters."), "{}", capped.system);
        assert_eq!(capped.max_tokens, 833);

        let uncapped = text_request("a cat", &image(), provider(), &[]);
        assert!(!uncapped.system.contains("Stay under"), "{}", uncapped.system);
        assert_eq!(uncapped.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn the_request_carries_the_trimmed_prompt_and_the_provider() {
        let request = text_request("  a cat  ", &image(), provider(), &[]);
        assert_eq!(request.user, "a cat");
        assert_eq!(request.provider, provider().id);
    }
}
