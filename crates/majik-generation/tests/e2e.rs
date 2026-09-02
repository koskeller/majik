//! Live-API tests for prompt improvement as the app actually performs it: the instruction
//! [`majik_generation::improve`] builds, carried by `Engine::improve_prompt` to a real provider.
//!
//! `majik-providers`' own `e2e.rs` covers `complete_text` at the client level; this covers what
//! sits above it: the system prompt we ship, the token budget derived from the model's cap, and the
//! engine's key handling. Same rules as that suite: every live test is `#[ignore]`d and reads its
//! key from the environment.
//!
//! ```sh
//! cargo test -p majik-generation --test e2e -- --ignored
//! ```

use std::sync::Arc;

use majik_generation::{improve, Engine, GenerationType};
use majik_providers::{catalog, AspectRatio, AssetRole, ImageGenerationSettings, ImageResolution, ProviderDescriptor, ProviderId, VideoGenerationSettings};

/// The key for a provider, or `None` with a visible note (a missing key skips, it does not fail).
fn key(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(k) if !k.trim().is_empty() => Some(k),
        _ => {
            eprintln!("SKIP: {var} is not set");
            None
        }
    }
}

/// The env var holding each provider's key, the way the app's `ApiKeys` closure maps them.
fn key_var(provider: &ProviderId) -> &'static str {
    match provider.as_str() {
        "fal.ai" => "FAL_API_KEY",
        "Replicate" => "REPLICATE_API_KEY",
        _ => "OPENROUTER_API_KEY",
    }
}

/// One engine for the whole binary, resolving keys from the environment. It owns a tokio runtime,
/// and `majik_providers::http` keeps a process-wide `reqwest::Client` bound to whichever runtime
/// first uses it, so an engine per test would poison that client for the next one ("dispatch task
/// is gone"). The app runs one engine too, so this matches production. Never dropped.
fn engine() -> &'static Engine {
    static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| {
        // Prompt rewriting emits no `Event`s, so the receiver has nothing to carry.
        let (engine, _events) = Engine::new(Arc::new(|p: &ProviderId| std::env::var(key_var(p)).ok().filter(|k| !k.trim().is_empty())), 1).expect("engine");
        engine
    })
}

/// A second long-lived engine that answers every provider with a key that cannot work.
fn engine_with_a_bad_key() -> &'static Engine {
    static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| {
        let (engine, _events) = Engine::new(Arc::new(|_: &ProviderId| Some("definitely-not-a-key".to_string())), 1).expect("engine");
        engine
    })
}

/// Ask the real provider to rewrite `prompt` for `generation_type`, exactly as the composer would.
fn improved(descriptor: &'static ProviderDescriptor, generation_type: &GenerationType, prompt: &str, roles: &[AssetRole]) -> String {
    let request = improve::text_request(prompt, generation_type, descriptor, roles);
    let text = engine()
        .improve_prompt(request)
        .recv_blocking()
        .expect("the engine answers exactly once")
        .unwrap_or_else(|e| panic!("{} improve_prompt: {e}", descriptor.display_name));
    // Structure is asserted below, but only a human can judge whether the rewrite is any good, so
    // print it: `--nocapture` turns this suite into a quality read-through.
    eprintln!("\n{} · {prompt:?}\n  → {text}\n", descriptor.display_name);
    text
}

/// What goes into the prompt field: the instruction demands the edited prompt alone. The prompts
/// the tests send carry a spelling or grammar slip, so a correct edit differs from the original
/// without having to be longer than it — the instruction asks for the smallest edit.
#[track_caller]
fn assert_is_a_bare_prompt(who: &str, original: &str, improved: &str) {
    let text = improved.trim();
    assert!(!text.is_empty(), "{who}: empty rewrite");
    assert!(text != original, "{who}: the model echoed the prompt back");
    assert!(text.len() <= original.len() * 2 + 40, "{who}: the edit grew the prompt into a rewrite: {text:?}");
    assert!(!text.starts_with('"') && !text.starts_with('\''), "{who}: wrapped in quotes: {text:?}");
    assert!(!text.contains("```"), "{who}: markdown fence: {text:?}");
    let lower = text.to_lowercase();
    for preamble in ["here's", "here is", "sure,", "certainly", "rewritten prompt", "improved prompt"] {
        assert!(!lower.starts_with(preamble), "{who}: preamble {preamble:?}: {text:?}");
    }
    for aside in ["i don't see", "i do not see", "please share", "please provide", "let me know", "could you", "i can rewrite", "i'll need", "i need the"] {
        assert!(!lower.contains(aside), "{who}: talking to the user, not writing a prompt ({aside:?}): {text:?}");
    }
}

fn image(model_id: &str) -> GenerationType {
    let model = catalog::image::model(model_id).expect("model in catalog").clone();
    GenerationType::Image(ImageGenerationSettings { model, aspect_ratio: AspectRatio::Portrait, resolution: ImageResolution::Hd })
}

/// Kling declares a 2500-character prompt cap, which is what makes the budget path interesting.
fn capped_video() -> GenerationType {
    GenerationType::Video(VideoGenerationSettings {
        model: catalog::video::KLING_30_PRO.clone(),
        aspect_ratio: None,
        resolution: None,
        duration: 5,
        audio_enabled: false,
    })
}

macro_rules! improve_tests {
    ($provider:ident, $key_var:literal, $why:literal, $descriptor:expr) => {
        mod $provider {
            use super::*;

            /// The shipped instruction, on the shipped model, for a plain image prompt.
            #[test]
            #[ignore = $why]
            fn rewrites_an_image_prompt() {
                let Some(_key) = key($key_var) else { return };
                let prompt = "cat siting on windowsil in sun light";
                let text = improved($descriptor, &image("gemini-2.5-flash"), prompt, &[]);
                assert_is_a_bare_prompt($descriptor.display_name, prompt, &text);
            }

            /// The instruction says the aspect ratio and resolution are set elsewhere; a rewrite
            /// that restates them would be pushed into the provider as prompt text.
            #[test]
            #[ignore = $why]
            fn leaves_the_settings_out_of_the_prompt() {
                let Some(_key) = key($key_var) else { return };
                let text = improved($descriptor, &image("gemini-2.5-flash"), "cat siting on windowsil in sun light", &[]).to_lowercase();
                for banned in ["aspect ratio", "4:5", "resolution"] {
                    assert!(!text.contains(banned), "{}: the rewrite restates {banned:?}: {text:?}", $descriptor.display_name);
                }
            }

            /// With references attached the model is told not to invent what they already show.
            #[test]
            #[ignore = $why]
            fn rewrites_with_reference_images_attached() {
                let Some(_key) = key($key_var) else { return };
                let prompt = "make it snowy and the cat wear hat";
                let roles = [AssetRole::ReferenceImage, AssetRole::ReferenceImage];
                let text = improved($descriptor, &image("gemini-2.5-flash"), prompt, &roles);
                assert_is_a_bare_prompt($descriptor.display_name, prompt, &text);
            }

            /// A model that declares a prompt cap must get a rewrite that fits it, or Generate is
            /// disabled the moment the text reaches the field.
            #[test]
            #[ignore = $why]
            fn respects_the_models_prompt_cap() {
                let Some(_key) = key($key_var) else { return };
                let generation_type = capped_video();
                // OpenRouter runs no video models, so it has no capped model to check.
                let Some(limit) = majik_generation::validation::prompt_character_limit(&generation_type, $descriptor).ok().flatten() else {
                    eprintln!("SKIP: {} declares no prompt cap for this model", $descriptor.display_name);
                    return;
                };
                let prompt = "cat walk slow across the room";
                let text = improved($descriptor, &generation_type, prompt, &[]);
                assert_is_a_bare_prompt($descriptor.display_name, prompt, &text);
                assert!(text.chars().count() <= limit, "{}: {} characters over a {limit} cap", $descriptor.display_name, text.chars().count());
            }
        }
    };
}

improve_tests!(fal, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor());
improve_tests!(replicate, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor());
improve_tests!(openrouter, "OPENROUTER_API_KEY", "live API: needs OPENROUTER_API_KEY", majik_providers::openrouter::descriptor());

/// Needs the network but no key of your own: the engine must report a provider's rejection rather
/// than hang or answer with something the composer would paste into the prompt field.
#[test]
#[ignore = "live API: hits the network (no key needed)"]
fn a_bad_key_reaches_the_composer_as_unauthorized() {
    for descriptor in [majik_providers::fal::descriptor(), majik_providers::replicate::descriptor(), majik_providers::openrouter::descriptor()] {
        let request = improve::text_request("a cat", &image("gemini-2.5-flash"), descriptor, &[]);
        let error = engine_with_a_bad_key().improve_prompt(request).recv_blocking().expect("answered once").expect_err("a bogus key must not rewrite");
        assert!(matches!(error, majik_providers::GenerationError::Unauthorized(_)), "{}: {error:?}", descriptor.display_name);
    }
}

/// No network: with no key configured at all the engine refuses before any request, which is what
/// the composer's missing-key path depends on.
#[test]
fn no_key_configured_is_unauthorized_without_a_request() {
    let descriptor = majik_providers::fal::descriptor();
    let request = improve::text_request("a cat", &image("gemini-2.5-flash"), descriptor, &[]);
    // A third engine, but this one refuses before any HTTP, so it never touches the shared client.
    let (engine, _events) = Engine::new(Arc::new(|_: &ProviderId| None), 1).expect("engine");
    let error = engine.improve_prompt(request).recv_blocking().expect("answered once").expect_err("no key, no rewrite");
    assert!(matches!(error, majik_providers::GenerationError::Unauthorized(_)), "{error:?}");
}
