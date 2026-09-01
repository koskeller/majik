//! Live-API tests: the real fal.ai / Replicate / OpenRouter endpoints, with real keys and real money.
//!
//! Every test here is `#[ignore]`d, so `cargo test` (and CI) skips them and says how many, while
//! the file still compiles under `cargo clippy --all-targets` so it cannot rot. Keys come from the
//! environment, one per provider; a missing key skips loudly, so you can run with only the keys you
//! hold.
//!
//! ```sh
//! cargo test -p majik-providers --test e2e -- --ignored                 # everything (expensive)
//! cargo test -p majik-providers --test e2e -- --ignored smoke           # the cheap tier
//! cargo test -p majik-providers --test e2e -- --ignored fal::video      # one module
//! cargo test -p majik-providers --test e2e -- --ignored --test-threads=2
//! ```
//!
//! The one test that is *not* ignored is [`guard`], which makes no network call: it asserts the
//! matrix below still covers exactly what each provider claims to support.

use majik_providers::{
    catalog, AspectRatio, AssetRole, AudioGenerationSettings, AudioModel, AudioVoice, ImageModel, ProviderAsset, ProviderClient, ProviderDescriptor,
    VideoAspectRatio, VideoGenerationSettings, VideoModel,
};

// ----- keys -------------------------------------------------------------------------------------

/// The key for a provider, or `None` with a visible note. A missing key skips rather than fails:
/// you rarely hold all three.
fn key(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(k) if !k.trim().is_empty() => Some(k),
        _ => {
            eprintln!("SKIP: {var} is not set");
            None
        }
    }
}

/// One runtime for the whole binary. `majik_providers::http` keeps a process-wide `reqwest::Client`
/// whose connection pool is bound to the runtime that first used it, so the runtime-per-test that
/// `#[test]` creates poisons the client for every later test ("dispatch task is gone"). The app has
/// exactly one long-lived runtime, the engine's, so a shared one matches production.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("test runtime"))
}

fn client(descriptor: &'static ProviderDescriptor, key: &str) -> ProviderClient {
    ProviderClient::new(descriptor, key)
}

/// 512×512 solid red. fal's image loader rejects very small inputs (1×1 → `image_load_error`) and
/// several models enforce a minimum 384×384 (wan 2.7 → `image_too_small`), so this sits well above
/// every known threshold.
fn reference_png() -> Vec<u8> {
    majik_core::images::solid_png(512, 512, [255, 0, 0])
}

// ----- assertions -------------------------------------------------------------------------------

/// A provider can answer 200 with an error page; "not empty" would pass that, so sniff the bytes.
#[track_caller]
fn assert_image(what: &str, bytes: &[u8]) {
    let mime = majik_providers::transcode::sniff_image_mime(bytes);
    assert!(mime.is_some(), "{what}: {} bytes that are not an image: {:?}", bytes.len(), String::from_utf8_lossy(&bytes[..bytes.len().min(200)]));
}

/// Demuxes the MP4 for real: this is the check that catches a provider switching to a codec the app
/// cannot play.
#[track_caller]
fn assert_video(what: &str, bytes: &[u8]) {
    use std::io::Write as _;
    let mut file = tempfile::NamedTempFile::with_suffix(".mp4").expect("temp file");
    file.write_all(bytes).expect("write");
    let info = majik_core::video::probe(file.path()).unwrap_or_else(|e| panic!("{what}: {} bytes that do not demux: {e}", bytes.len()));
    assert!(info.width.is_some_and(|w| w > 0) && info.height.is_some_and(|h| h > 0), "{what}: {:?}x{:?}", info.width, info.height);
    assert!(info.duration_secs.is_some_and(|d| d > 0.0), "{what}: {:?} duration", info.duration_secs);
}

#[track_caller]
fn assert_audio(what: &str, bytes: &[u8]) {
    let mime = majik_providers::transcode::sniff_audio_mime(bytes);
    assert!(mime.is_some(), "{what}: {} bytes that are not audio: {:?}", bytes.len(), String::from_utf8_lossy(&bytes[..bytes.len().min(200)]));
    assert!(bytes.len() > 1000, "{what}: {} bytes is too short to be speech", bytes.len());
}

// ----- the calls --------------------------------------------------------------------------------

fn image_model(id: &str) -> &'static ImageModel {
    catalog::image::model(id).unwrap_or_else(|| panic!("{id} is not in the image catalog"))
}

fn video_model(id: &str) -> &'static VideoModel {
    catalog::video::model(id).unwrap_or_else(|| panic!("{id} is not in the video catalog"))
}

fn audio_model(id: &str) -> &'static AudioModel {
    catalog::audio::model(id).unwrap_or_else(|| panic!("{id} is not in the audio catalog"))
}

/// Square where the model takes it, else whatever it declares first.
fn image_ratio(descriptor: &'static ProviderDescriptor, model: &ImageModel) -> Option<AspectRatio> {
    let caps = descriptor.image_capabilities(model)?;
    if caps.supported_aspect_ratios.contains(&AspectRatio::Square) {
        Some(AspectRatio::Square)
    } else {
        caps.supported_aspect_ratios.first().copied()
    }
}

/// The cheapest settings the model accepts: its lowest resolution, its shortest duration, sound off
/// unless the model always has it.
fn cheapest_video(descriptor: &'static ProviderDescriptor, model: &VideoModel) -> VideoGenerationSettings {
    let caps = descriptor.video_capabilities(model).unwrap_or_else(|| panic!("{} has no video capabilities", model.id));
    let aspect_ratio = if caps.aspect_ratios.contains(&VideoAspectRatio::Landscape) {
        Some(VideoAspectRatio::Landscape)
    } else {
        caps.aspect_ratios.first().copied()
    };
    VideoGenerationSettings {
        model: model.clone(),
        aspect_ratio,
        resolution: caps.lowest_resolution(),
        duration: caps.duration_range.min,
        audio_enabled: caps.audio_always_on,
    }
}

async fn text_to_image(descriptor: &'static ProviderDescriptor, key: &str, id: &str) {
    let model = image_model(id);
    let bytes = client(descriptor, key)
        .generate_image("A red circle on white background", model, &[], image_ratio(descriptor, model), None)
        .await
        .unwrap_or_else(|e| panic!("{}/{id} text-to-image: {e}", descriptor.display_name));
    assert_image(id, &bytes);
}

async fn image_to_image(descriptor: &'static ProviderDescriptor, key: &str, id: &str) {
    let model = image_model(id);
    let asset = ProviderAsset::new(AssetRole::ReferenceImage, "image/png", reference_png());
    let bytes = client(descriptor, key)
        .generate_image("Make the circle blue", model, &[asset], image_ratio(descriptor, model), None)
        .await
        .unwrap_or_else(|e| panic!("{}/{id} image-to-image: {e}", descriptor.display_name));
    assert_image(id, &bytes);
}

async fn text_to_video(descriptor: &'static ProviderDescriptor, key: &str, id: &str) {
    let settings = cheapest_video(descriptor, video_model(id));
    let bytes = client(descriptor, key)
        .generate_video("A calm ocean with gentle waves", &[], &settings)
        .await
        .unwrap_or_else(|e| panic!("{}/{id} text-to-video: {e}", descriptor.display_name));
    assert_video(id, &bytes);
}

async fn image_to_video(descriptor: &'static ProviderDescriptor, key: &str, id: &str) {
    let settings = cheapest_video(descriptor, video_model(id));
    let asset = ProviderAsset::new(AssetRole::FirstFrame, "image/png", reference_png());
    let bytes = client(descriptor, key)
        .generate_video("The circle slowly pulses", &[asset], &settings)
        .await
        .unwrap_or_else(|e| panic!("{}/{id} image-to-video: {e}", descriptor.display_name));
    assert_video(id, &bytes);
}

/// Two reference images the prompt addresses by handle: the request shape the composer builds when
/// the user attaches references and types `@Image1`. Each provider rewrites the handles into the
/// model's own dialect, so a failure here means a dialect has changed.
async fn reference_to_video(descriptor: &'static ProviderDescriptor, key: &str, id: &str) {
    let settings = cheapest_video(descriptor, video_model(id));
    let assets = [
        ProviderAsset::new(AssetRole::ReferenceImage, "image/png", reference_png()),
        ProviderAsset::new(AssetRole::ReferenceImage, "image/png", reference_png()),
    ];
    let bytes = client(descriptor, key)
        .generate_video("@Image1 slowly pulses beside @Image2", &assets, &settings)
        .await
        .unwrap_or_else(|e| panic!("{}/{id} reference-to-video: {e}", descriptor.display_name));
    assert_video(id, &bytes);
}

/// Voices come from the model's own capabilities, so adding a model needs no fixture here.
fn voices(descriptor: &'static ProviderDescriptor, model: &AudioModel) -> (AudioVoice, Option<AudioVoice>) {
    let caps = descriptor.audio_capabilities(model).unwrap_or_else(|| panic!("{} has no audio capabilities", model.id));
    let first = caps.default_voice.clone().or_else(|| caps.supported_voices.first().cloned()).unwrap_or_else(|| panic!("{} declares no voice", model.id));
    let second = caps.supports_two_speakers.then(|| caps.secondary_default_voice.clone().or_else(|| caps.supported_voices.get(1).cloned())).flatten();
    (first, second)
}

async fn monologue(descriptor: &'static ProviderDescriptor, key: &str, id: &str) {
    let model = audio_model(id);
    let (speaker1, _) = voices(descriptor, model);
    let settings = AudioGenerationSettings { model: model.clone(), speaker1, speaker2: None };
    let bytes = client(descriptor, key)
        .generate_audio("Hello from the integration suite.", &settings)
        .await
        .unwrap_or_else(|e| panic!("{}/{id} monologue: {e}", descriptor.display_name));
    assert_audio(id, &bytes);
}

async fn dialogue(descriptor: &'static ProviderDescriptor, key: &str, id: &str) {
    let model = audio_model(id);
    let (speaker1, speaker2) = voices(descriptor, model);
    let Some(speaker2) = speaker2 else {
        eprintln!("SKIP: {id} does not do two speakers");
        return;
    };
    let settings = AudioGenerationSettings { model: model.clone(), speaker1, speaker2: Some(speaker2) };
    let bytes = client(descriptor, key)
        .generate_audio("Speaker 1: How are you?\nSpeaker 2: Doing well, thanks.", &settings)
        .await
        .unwrap_or_else(|e| panic!("{}/{id} dialogue: {e}", descriptor.display_name));
    assert_audio(id, &bytes);
}

async fn upscale(descriptor: &'static ProviderDescriptor, key: &str) {
    let bytes = client(descriptor, key).upscale_image(&reference_png()).await.unwrap_or_else(|e| panic!("{} upscale: {e}", descriptor.display_name));
    assert_image("upscale", &bytes);
}

async fn remove_background(descriptor: &'static ProviderDescriptor, key: &str) {
    let bytes = client(descriptor, key).remove_background(&reference_png()).await.unwrap_or_else(|e| panic!("{} remove background: {e}", descriptor.display_name));
    assert_image("remove background", &bytes);
}

/// What the composer puts straight into the prompt field, so the model has to have obeyed "the
/// rewritten prompt only": no wrapping quotes, no "Here's your prompt", no markdown fence. A
/// failure here means the instruction needs work.
#[track_caller]
pub fn assert_is_a_bare_prompt(who: &str, original: &str, improved: &str) {
    let text = improved.trim();
    assert!(!text.is_empty(), "{who}: empty rewrite");
    assert!(text != original, "{who}: the model echoed the prompt back");
    assert!(text.len() > original.len(), "{who}: the rewrite added nothing: {text:?}");
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

/// The prompt-improvement path. Nothing in this repo had ever run it against a live endpoint.
async fn complete_text(descriptor: &'static ProviderDescriptor, key: &str) {
    let prompt = "a cat";
    let improved = client(descriptor, key)
        .complete_text(
            "You rewrite prompts for image generation. Reply with the rewritten prompt only: one paragraph, plain text, no quotes, no preamble.",
            prompt,
            200,
        )
        .await
        .unwrap_or_else(|e| panic!("{} complete_text: {e}", descriptor.display_name));
    assert_is_a_bare_prompt(descriptor.display_name, prompt, &improved);
}

/// A tight `max_tokens` must still come back as usable text rather than an error or an empty body:
/// the composer derives the budget from the model's prompt cap, which can be small.
async fn complete_text_within_a_small_budget(descriptor: &'static ProviderDescriptor, key: &str) {
    let improved = client(descriptor, key)
        .complete_text("Rewrite the prompt. Reply with the prompt only.", "a cat", 64)
        .await
        .unwrap_or_else(|e| panic!("{} complete_text (64 tokens): {e}", descriptor.display_name));
    assert!(!improved.trim().is_empty(), "{}: a small budget returned nothing", descriptor.display_name);
}

// ----- the matrix -------------------------------------------------------------------------------

/// One `#[ignore]`d test per model, plus the id list [`guard`] checks against the descriptor.
macro_rules! live_tests {
    ($call:ident, $key_var:literal, $why:literal, $descriptor:expr, $($name:ident => $id:literal,)*) => {
        /// Every model this module covers. Compared with the provider's descriptor by the guard.
        pub const IDS: &[&str] = &[$($id),*];
        $(
            #[test]
            #[ignore = $why]
            fn $name() {
                let Some(key) = crate::key($key_var) else { return };
                crate::rt().block_on(crate::$call($descriptor, &key, $id));
            }
        )*
    };
}

/// The cheap tier: one model per provider per media type, so `-- --ignored smoke` answers "does any
/// of this still work" for a few cents. Everything here is also covered per-model above.
mod smoke {
    #[test]
    #[ignore = "live API: needs FAL_API_KEY"]
    fn fal_image() {
        let Some(key) = crate::key("FAL_API_KEY") else { return };
        crate::rt().block_on(crate::text_to_image(majik_providers::fal::descriptor(), &key, "flux-2-klein"));
    }

    #[test]
    #[ignore = "live API: needs FAL_API_KEY"]
    fn fal_video() {
        let Some(key) = crate::key("FAL_API_KEY") else { return };
        crate::rt().block_on(crate::text_to_video(majik_providers::fal::descriptor(), &key, "seedance-2-fast"));
    }

    #[test]
    #[ignore = "live API: needs FAL_API_KEY"]
    fn fal_audio() {
        let Some(key) = crate::key("FAL_API_KEY") else { return };
        crate::rt().block_on(crate::monologue(majik_providers::fal::descriptor(), &key, "elevenlabs-v3"));
    }

    #[test]
    #[ignore = "live API: needs REPLICATE_API_KEY"]
    fn replicate_image() {
        let Some(key) = crate::key("REPLICATE_API_KEY") else { return };
        crate::rt().block_on(crate::text_to_image(majik_providers::replicate::descriptor(), &key, "flux-1-schnell"));
    }

    #[test]
    #[ignore = "live API: needs REPLICATE_API_KEY"]
    fn replicate_video() {
        let Some(key) = crate::key("REPLICATE_API_KEY") else { return };
        crate::rt().block_on(crate::text_to_video(majik_providers::replicate::descriptor(), &key, "seedance-2-fast"));
    }

    #[test]
    #[ignore = "live API: needs OPENROUTER_API_KEY"]
    fn openrouter_image() {
        let Some(key) = crate::key("OPENROUTER_API_KEY") else { return };
        crate::rt().block_on(crate::text_to_image(majik_providers::openrouter::descriptor(), &key, "gemini-2.5-flash"));
    }
}

mod fal {
    pub mod image {
        pub mod t2i {
            live_tests!(text_to_image, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor(),
                flux_1_dev => "flux-1-dev",
                flux_1_schnell => "flux-1-schnell",
                flux_2_flex => "flux-2-flex",
                flux_2_klein => "flux-2-klein",
                flux_2_max => "flux-2-max",
                flux_2_pro => "flux-2-pro",
                gemini_2_5_flash => "gemini-2.5-flash",
                gemini_3_pro => "gemini-3-pro",
                gemini_3_1_flash => "gemini-3.1-flash",
                gpt_5_image => "gpt-5-image",
                gpt_5_image_mini => "gpt-5-image-mini",
                gpt_image_2 => "gpt-image-2",
                grok_imagine_image_2 => "grok-imagine-image-2",
                muse_image => "muse-image",
                qwen_image_3 => "qwen-image-3",
                recraft_4_pro => "recraft-4-pro",
                seedream_4_5 => "seedream-4.5",
                seedream_5_lite => "seedream-5-lite",
                seedream_5_pro => "seedream-5-pro",
                wan_2_7_pro => "wan-2.7-pro",
            );
        }
        pub mod i2i {
            live_tests!(image_to_image, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor(),
                flux_1_dev => "flux-1-dev",
                flux_2_flex => "flux-2-flex",
                flux_2_klein => "flux-2-klein",
                flux_2_max => "flux-2-max",
                flux_2_pro => "flux-2-pro",
                gemini_2_5_flash => "gemini-2.5-flash",
                gemini_3_pro => "gemini-3-pro",
                gemini_3_1_flash => "gemini-3.1-flash",
                gpt_5_image => "gpt-5-image",
                gpt_5_image_mini => "gpt-5-image-mini",
                gpt_image_2 => "gpt-image-2",
                grok_imagine_image_2 => "grok-imagine-image-2",
                muse_image => "muse-image",
                qwen_image_3 => "qwen-image-3",
                seedream_4_5 => "seedream-4.5",
                seedream_5_lite => "seedream-5-lite",
                seedream_5_pro => "seedream-5-pro",
                wan_2_7_pro => "wan-2.7-pro",
            );
        }
    }
    pub mod video {
        pub mod t2v {
            live_tests!(text_to_video, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor(),
                flux_3 => "flux-3",
                gemini_omni_flash_1_1 => "gemini-omni-flash-1.1",
                grok_imagine_video => "grok-imagine-video",
                grok_imagine_video_1_5 => "grok-imagine-video-1.5",
                happyhorse_1_0 => "happyhorse-1.0",
                happyhorse_1_1 => "happyhorse-1.1",
                kling_2_5_turbo_pro => "kling-2.5-turbo-pro",
                kling_2_6_pro => "kling-2.6-pro",
                kling_3_pro => "kling-3-pro",
                kling_3_standard => "kling-3-standard",
                minimax_h3 => "minimax-h3",
                minimax_h3_max => "minimax-h3-max",
                pixverse_6 => "pixverse-6",
                seedance_1_5_pro => "seedance-1.5-pro",
                seedance_2 => "seedance-2",
                seedance_2_fast => "seedance-2-fast",
                seedance_2_5 => "seedance-2.5",
                sora_2 => "sora-2",
                sora_2_pro => "sora-2-pro",
                veo_3_1 => "veo-3.1",
                veo_3_1_fast => "veo-3.1-fast",
                veo_3_1_lite => "veo-3.1-lite",
                wan_2_7 => "wan-2.7",
                wan_3_0 => "wan-3.0",
                wan_3_0_prime => "wan-3.0-prime",
            );
        }
        pub mod i2v {
            live_tests!(image_to_video, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor(),
                flux_3 => "flux-3",
                gemini_omni_flash_1_1 => "gemini-omni-flash-1.1",
                grok_imagine_video => "grok-imagine-video",
                grok_imagine_video_1_5 => "grok-imagine-video-1.5",
                happyhorse_1_0 => "happyhorse-1.0",
                happyhorse_1_1 => "happyhorse-1.1",
                kling_2_5_turbo_pro => "kling-2.5-turbo-pro",
                kling_2_6_pro => "kling-2.6-pro",
                kling_3_pro => "kling-3-pro",
                kling_3_standard => "kling-3-standard",
                minimax_h3 => "minimax-h3",
                minimax_h3_max => "minimax-h3-max",
                pixverse_6 => "pixverse-6",
                seedance_1_5_pro => "seedance-1.5-pro",
                seedance_2 => "seedance-2",
                seedance_2_fast => "seedance-2-fast",
                seedance_2_5 => "seedance-2.5",
                sora_2 => "sora-2",
                sora_2_pro => "sora-2-pro",
                veo_3_1 => "veo-3.1",
                veo_3_1_fast => "veo-3.1-fast",
                veo_3_1_lite => "veo-3.1-lite",
                wan_2_7 => "wan-2.7",
                wan_3_0 => "wan-3.0",
                wan_3_0_prime => "wan-3.0-prime",
            );
        }
        pub mod r2v {
            live_tests!(reference_to_video, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor(),
                gemini_omni_flash_1_1 => "gemini-omni-flash-1.1",
                grok_imagine_video => "grok-imagine-video",
                grok_imagine_video_1_5 => "grok-imagine-video-1.5",
                happyhorse_1_0 => "happyhorse-1.0",
                happyhorse_1_1 => "happyhorse-1.1",
                minimax_h3 => "minimax-h3",
                minimax_h3_max => "minimax-h3-max",
                seedance_2 => "seedance-2",
                seedance_2_fast => "seedance-2-fast",
                seedance_2_5 => "seedance-2.5",
                veo_3_1 => "veo-3.1",
                veo_3_1_fast => "veo-3.1-fast",
                wan_2_7 => "wan-2.7",
                wan_3_0 => "wan-3.0",
                wan_3_0_prime => "wan-3.0-prime",
            );
        }
    }
    pub mod audio {
        pub mod monologue {
            live_tests!(monologue, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor(),
                elevenlabs_v3 => "elevenlabs-v3",
                gemini_2_5_pro => "gemini-2.5-pro",
            );
        }
        pub mod dialogue {
            live_tests!(dialogue, "FAL_API_KEY", "live API: needs FAL_API_KEY", majik_providers::fal::descriptor(),
                elevenlabs_v3 => "elevenlabs-v3",
                gemini_2_5_pro => "gemini-2.5-pro",
            );
        }
    }
    mod tools {
        #[test]
        #[ignore = "live API: needs FAL_API_KEY"]
        fn upscale() {
            let Some(key) = crate::key("FAL_API_KEY") else { return };
            crate::rt().block_on(crate::upscale(majik_providers::fal::descriptor(), &key));
        }

        #[test]
        #[ignore = "live API: needs FAL_API_KEY"]
        fn remove_background() {
            let Some(key) = crate::key("FAL_API_KEY") else { return };
            crate::rt().block_on(crate::remove_background(majik_providers::fal::descriptor(), &key));
        }
    }
    mod text {
        #[test]
        #[ignore = "live API: needs FAL_API_KEY"]
        fn improves_a_prompt() {
            let Some(key) = crate::key("FAL_API_KEY") else { return };
            crate::rt().block_on(crate::complete_text(majik_providers::fal::descriptor(), &key));
        }

        #[test]
        #[ignore = "live API: needs FAL_API_KEY"]
        fn improves_a_prompt_within_a_small_token_budget() {
            let Some(key) = crate::key("FAL_API_KEY") else { return };
            crate::rt().block_on(crate::complete_text_within_a_small_budget(majik_providers::fal::descriptor(), &key));
        }
    }
}

mod replicate {
    pub mod image {
        pub mod t2i {
            live_tests!(text_to_image, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor(),
                flux_1_dev => "flux-1-dev",
                flux_1_schnell => "flux-1-schnell",
                flux_2_flex => "flux-2-flex",
                flux_2_klein => "flux-2-klein",
                flux_2_max => "flux-2-max",
                flux_2_pro => "flux-2-pro",
                gemini_2_5_flash => "gemini-2.5-flash",
                gemini_3_pro => "gemini-3-pro",
                gemini_3_1_flash => "gemini-3.1-flash",
                gpt_5_image => "gpt-5-image",
                gpt_image_2 => "gpt-image-2",
                grok_imagine_image_2 => "grok-imagine-image-2",
                qwen_image_3 => "qwen-image-3",
                recraft_4_pro => "recraft-4-pro",
                seedream_4_5 => "seedream-4.5",
                seedream_5_lite => "seedream-5-lite",
                seedream_5_pro => "seedream-5-pro",
                wan_2_7_pro => "wan-2.7-pro",
            );
        }
        pub mod i2i {
            live_tests!(image_to_image, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor(),
                flux_1_dev => "flux-1-dev",
                flux_2_flex => "flux-2-flex",
                flux_2_klein => "flux-2-klein",
                flux_2_max => "flux-2-max",
                flux_2_pro => "flux-2-pro",
                gemini_2_5_flash => "gemini-2.5-flash",
                gemini_3_pro => "gemini-3-pro",
                gemini_3_1_flash => "gemini-3.1-flash",
                gpt_5_image => "gpt-5-image",
                gpt_image_2 => "gpt-image-2",
                grok_imagine_image_2 => "grok-imagine-image-2",
                qwen_image_3 => "qwen-image-3",
                seedream_4_5 => "seedream-4.5",
                seedream_5_lite => "seedream-5-lite",
                seedream_5_pro => "seedream-5-pro",
                wan_2_7_pro => "wan-2.7-pro",
            );
        }
    }
    pub mod video {
        pub mod t2v {
            live_tests!(text_to_video, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor(),
                flux_3 => "flux-3",
                grok_imagine_video => "grok-imagine-video",
                grok_imagine_video_1_5 => "grok-imagine-video-1.5",
                happyhorse_1_0 => "happyhorse-1.0",
                happyhorse_1_1 => "happyhorse-1.1",
                kling_2_5_turbo_pro => "kling-2.5-turbo-pro",
                kling_2_6_pro => "kling-2.6-pro",
                kling_3_pro => "kling-3-pro",
                minimax_h3 => "minimax-h3",
                pixverse_6 => "pixverse-6",
                seedance_1_5_pro => "seedance-1.5-pro",
                seedance_2 => "seedance-2",
                seedance_2_fast => "seedance-2-fast",
                seedance_2_5 => "seedance-2.5",
                sora_2 => "sora-2",
                sora_2_pro => "sora-2-pro",
                veo_3_1 => "veo-3.1",
                veo_3_1_fast => "veo-3.1-fast",
                veo_3_1_lite => "veo-3.1-lite",
                wan_2_7 => "wan-2.7",
                wan_3_0 => "wan-3.0",
                wan_3_0_prime => "wan-3.0-prime",
            );
        }
        pub mod i2v {
            live_tests!(image_to_video, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor(),
                flux_3 => "flux-3",
                grok_imagine_video => "grok-imagine-video",
                grok_imagine_video_1_5 => "grok-imagine-video-1.5",
                happyhorse_1_0 => "happyhorse-1.0",
                happyhorse_1_1 => "happyhorse-1.1",
                kling_2_5_turbo_pro => "kling-2.5-turbo-pro",
                kling_2_6_pro => "kling-2.6-pro",
                kling_3_pro => "kling-3-pro",
                minimax_h3 => "minimax-h3",
                pixverse_6 => "pixverse-6",
                seedance_1_5_pro => "seedance-1.5-pro",
                seedance_2 => "seedance-2",
                seedance_2_fast => "seedance-2-fast",
                seedance_2_5 => "seedance-2.5",
                veo_3_1 => "veo-3.1",
                veo_3_1_fast => "veo-3.1-fast",
                veo_3_1_lite => "veo-3.1-lite",
                wan_2_7 => "wan-2.7",
                wan_3_0 => "wan-3.0",
                wan_3_0_prime => "wan-3.0-prime",
            );
        }
        pub mod r2v {
            live_tests!(reference_to_video, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor(),
                happyhorse_1_1 => "happyhorse-1.1",
                minimax_h3 => "minimax-h3",
                seedance_2_5 => "seedance-2.5",
                veo_3_1 => "veo-3.1",
            );
        }
    }
    pub mod audio {
        pub mod monologue {
            live_tests!(monologue, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor(),
                elevenlabs_v3 => "elevenlabs-v3",
            );
        }
        pub mod dialogue {
            live_tests!(dialogue, "REPLICATE_API_KEY", "live API: needs REPLICATE_API_KEY", majik_providers::replicate::descriptor(),
                elevenlabs_v3 => "elevenlabs-v3",
            );
        }
    }
    mod tools {
        #[test]
        #[ignore = "live API: needs REPLICATE_API_KEY"]
        fn upscale() {
            let Some(key) = crate::key("REPLICATE_API_KEY") else { return };
            crate::rt().block_on(crate::upscale(majik_providers::replicate::descriptor(), &key));
        }

        #[test]
        #[ignore = "live API: needs REPLICATE_API_KEY"]
        fn remove_background() {
            let Some(key) = crate::key("REPLICATE_API_KEY") else { return };
            crate::rt().block_on(crate::remove_background(majik_providers::replicate::descriptor(), &key));
        }
    }
    mod text {
        #[test]
        #[ignore = "live API: needs REPLICATE_API_KEY"]
        fn improves_a_prompt() {
            let Some(key) = crate::key("REPLICATE_API_KEY") else { return };
            crate::rt().block_on(crate::complete_text(majik_providers::replicate::descriptor(), &key));
        }

        #[test]
        #[ignore = "live API: needs REPLICATE_API_KEY"]
        fn improves_a_prompt_within_a_small_token_budget() {
            let Some(key) = crate::key("REPLICATE_API_KEY") else { return };
            crate::rt().block_on(crate::complete_text_within_a_small_budget(majik_providers::replicate::descriptor(), &key));
        }
    }
}

mod openrouter {
    pub mod image {
        pub mod t2i {
            live_tests!(text_to_image, "OPENROUTER_API_KEY", "live API: needs OPENROUTER_API_KEY", majik_providers::openrouter::descriptor(),
                flux_2_flex => "flux-2-flex",
                flux_2_klein => "flux-2-klein",
                flux_2_max => "flux-2-max",
                flux_2_pro => "flux-2-pro",
                gemini_2_5_flash => "gemini-2.5-flash",
                gemini_3_pro => "gemini-3-pro",
                gemini_3_1_flash => "gemini-3.1-flash",
                gpt_5_image => "gpt-5-image",
                gpt_5_image_mini => "gpt-5-image-mini",
                gpt_image_2 => "gpt-image-2",
                grok_imagine_image_2 => "grok-imagine-image-2",
                muse_image => "muse-image",
                qwen_image_3 => "qwen-image-3",
                qwen_image_3_pro => "qwen-image-3-pro",
                riverflow_2_fast => "riverflow-2-fast",
                riverflow_2_max => "riverflow-2-max",
                riverflow_2_std => "riverflow-2-std",
                seedream_4_5 => "seedream-4.5",
                seedream_5_lite => "seedream-5-lite",
                seedream_5_pro => "seedream-5-pro",
            );
        }
        pub mod i2i {
            live_tests!(image_to_image, "OPENROUTER_API_KEY", "live API: needs OPENROUTER_API_KEY", majik_providers::openrouter::descriptor(),
                flux_2_flex => "flux-2-flex",
                flux_2_klein => "flux-2-klein",
                flux_2_max => "flux-2-max",
                flux_2_pro => "flux-2-pro",
                gemini_2_5_flash => "gemini-2.5-flash",
                gemini_3_pro => "gemini-3-pro",
                gemini_3_1_flash => "gemini-3.1-flash",
                gpt_5_image => "gpt-5-image",
                gpt_5_image_mini => "gpt-5-image-mini",
                gpt_image_2 => "gpt-image-2",
                grok_imagine_image_2 => "grok-imagine-image-2",
                muse_image => "muse-image",
                qwen_image_3 => "qwen-image-3",
                qwen_image_3_pro => "qwen-image-3-pro",
                riverflow_2_fast => "riverflow-2-fast",
                riverflow_2_max => "riverflow-2-max",
                riverflow_2_std => "riverflow-2-std",
                seedream_4_5 => "seedream-4.5",
                seedream_5_lite => "seedream-5-lite",
                seedream_5_pro => "seedream-5-pro",
            );
        }
    }
    mod text {
        #[test]
        #[ignore = "live API: needs OPENROUTER_API_KEY"]
        fn improves_a_prompt() {
            let Some(key) = crate::key("OPENROUTER_API_KEY") else { return };
            crate::rt().block_on(crate::complete_text(majik_providers::openrouter::descriptor(), &key));
        }

        #[test]
        #[ignore = "live API: needs OPENROUTER_API_KEY"]
        fn improves_a_prompt_within_a_small_token_budget() {
            let Some(key) = crate::key("OPENROUTER_API_KEY") else { return };
            crate::rt().block_on(crate::complete_text_within_a_small_budget(majik_providers::openrouter::descriptor(), &key));
        }
    }
}

/// Invalid keys against the real endpoints. Needs the network but no key of your own, so these are
/// the tests to run when checking a change to auth mapping.
mod errors {
    use majik_providers::{catalog, GenerationError, ProviderClient, ProviderDescriptor};

    async fn assert_rejects_the_key(descriptor: &'static ProviderDescriptor, bogus_key: &str, model_id: &str) {
        let model = catalog::image::model(model_id).expect("model in catalog");
        let error = ProviderClient::new(descriptor, bogus_key).generate_image("test", model, &[], None, None).await.expect_err("a bogus key must not generate");
        // Name the variant, so a DNS failure can't pass as "unauthorized".
        assert!(matches!(error, GenerationError::Unauthorized(_)), "{}: {error:?}", descriptor.display_name);
    }

    /// The text path is a different endpoint per provider (fal's `any-llm`, a Replicate prediction
    /// for another model, an OpenRouter body without `modalities`), so a bad key exercises code the
    /// image tests never touch. An endpoint that has been withdrawn answers 404, not 401.
    async fn assert_rejects_the_key_for_text(descriptor: &'static ProviderDescriptor, bogus_key: &str) {
        let error = ProviderClient::new(descriptor, bogus_key)
            .complete_text("Rewrite the prompt.", "a cat", 64)
            .await
            .expect_err("a bogus key must not complete text");
        assert!(matches!(error, GenerationError::Unauthorized(_)), "{}: {error:?}", descriptor.display_name);
    }

    #[test]
    #[ignore = "live API: hits the network (no key needed)"]
    fn fal_rejects_an_invalid_key_for_text() {
        crate::rt().block_on(assert_rejects_the_key_for_text(majik_providers::fal::descriptor(), "fal-invalid-key-for-testing"));
    }

    #[test]
    #[ignore = "live API: hits the network (no key needed)"]
    fn replicate_rejects_an_invalid_key_for_text() {
        crate::rt().block_on(assert_rejects_the_key_for_text(majik_providers::replicate::descriptor(), "r8_invalid-key-for-testing"));
    }

    #[test]
    #[ignore = "live API: hits the network (no key needed)"]
    fn openrouter_rejects_an_invalid_key_for_text() {
        crate::rt().block_on(assert_rejects_the_key_for_text(majik_providers::openrouter::descriptor(), "sk-or-invalid-key-for-testing"));
    }

    #[test]
    #[ignore = "live API: hits the network (no key needed)"]
    fn fal_rejects_an_invalid_key() {
        crate::rt().block_on(assert_rejects_the_key(majik_providers::fal::descriptor(), "fal-invalid-key-for-testing", "flux-2-pro"));
    }

    #[test]
    #[ignore = "live API: hits the network (no key needed)"]
    fn replicate_rejects_an_invalid_key() {
        crate::rt().block_on(assert_rejects_the_key(majik_providers::replicate::descriptor(), "r8_invalid-key-for-testing", "flux-1-schnell"));
    }

    #[test]
    #[ignore = "live API: hits the network (no key needed)"]
    fn openrouter_rejects_an_invalid_key() {
        crate::rt().block_on(assert_rejects_the_key(majik_providers::openrouter::descriptor(), "sk-or-invalid-key-for-testing", "gemini-3-pro"));
    }
}

mod guard {
    //! Not ignored, and makes no network call: the matrix above must keep covering exactly what each
    //! provider says it supports. The matrix is generated from the catalogs, so it cannot diverge
    //! from them the way a hand-written list would.

    use majik_providers::{ProviderDescriptor, ToolId};

    fn sorted(ids: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    /// The models `descriptor` supports, and the subset that takes an input image.
    fn images(descriptor: &'static ProviderDescriptor) -> (Vec<String>, Vec<String>) {
        let all: Vec<&str> = descriptor.supported_image_models.iter().map(|m| m.id).collect();
        let with_inputs: Vec<&str> =
            descriptor.supported_image_models.iter().filter(|m| descriptor.image_capabilities(m).is_some_and(|c| c.max_input_images > 0)).map(|m| m.id).collect();
        (sorted(&all), sorted(&with_inputs))
    }

    fn videos(descriptor: &'static ProviderDescriptor) -> (Vec<String>, Vec<String>) {
        let all: Vec<&str> = descriptor.supported_video_models.iter().map(|m| m.id).collect();
        let with_frames: Vec<&str> =
            descriptor.supported_video_models.iter().filter(|m| descriptor.video_capabilities(m).is_some_and(|c| c.max_input_images > 0)).map(|m| m.id).collect();
        (sorted(&all), sorted(&with_frames))
    }

    /// The models that take references the prompt can address by handle.
    fn references(descriptor: &'static ProviderDescriptor) -> Vec<String> {
        let ids: Vec<&str> =
            descriptor.supported_video_models.iter().filter(|m| descriptor.video_capabilities(m).is_some_and(|c| c.references.is_some())).map(|m| m.id).collect();
        sorted(&ids)
    }

    fn audio(descriptor: &'static ProviderDescriptor) -> Vec<String> {
        sorted(&descriptor.supported_audio_models.iter().map(|m| m.id).collect::<Vec<_>>())
    }

    #[test]
    fn the_image_matrix_matches_every_provider() {
        let (all, with_inputs) = images(majik_providers::fal::descriptor());
        assert_eq!(sorted(crate::fal::image::t2i::IDS), all, "fal text-to-image");
        assert_eq!(sorted(crate::fal::image::i2i::IDS), with_inputs, "fal image-to-image");

        let (all, with_inputs) = images(majik_providers::replicate::descriptor());
        assert_eq!(sorted(crate::replicate::image::t2i::IDS), all, "Replicate text-to-image");
        assert_eq!(sorted(crate::replicate::image::i2i::IDS), with_inputs, "Replicate image-to-image");

        let (all, with_inputs) = images(majik_providers::openrouter::descriptor());
        assert_eq!(sorted(crate::openrouter::image::t2i::IDS), all, "OpenRouter text-to-image");
        assert_eq!(sorted(crate::openrouter::image::i2i::IDS), with_inputs, "OpenRouter image-to-image");
    }

    #[test]
    fn the_video_matrix_matches_every_provider() {
        let (all, with_frames) = videos(majik_providers::fal::descriptor());
        assert_eq!(sorted(crate::fal::video::t2v::IDS), all, "fal text-to-video");
        assert_eq!(sorted(crate::fal::video::i2v::IDS), with_frames, "fal image-to-video");

        let (all, with_frames) = videos(majik_providers::replicate::descriptor());
        assert_eq!(sorted(crate::replicate::video::t2v::IDS), all, "Replicate text-to-video");
        assert_eq!(sorted(crate::replicate::video::i2v::IDS), with_frames, "Replicate image-to-video");

        assert!(majik_providers::openrouter::descriptor().supported_video_models.is_empty(), "OpenRouter has gained video; give it a module");
    }

    #[test]
    fn the_reference_matrix_matches_every_provider() {
        assert_eq!(sorted(crate::fal::video::r2v::IDS), references(majik_providers::fal::descriptor()), "fal reference-to-video");
        assert_eq!(sorted(crate::replicate::video::r2v::IDS), references(majik_providers::replicate::descriptor()), "Replicate reference-to-video");
    }

    #[test]
    fn the_audio_matrix_matches_every_provider() {
        assert_eq!(sorted(crate::fal::audio::monologue::IDS), audio(majik_providers::fal::descriptor()), "fal audio");
        assert_eq!(sorted(crate::fal::audio::dialogue::IDS), audio(majik_providers::fal::descriptor()), "fal audio");
        assert_eq!(sorted(crate::replicate::audio::monologue::IDS), audio(majik_providers::replicate::descriptor()), "Replicate audio");
        assert_eq!(sorted(crate::replicate::audio::dialogue::IDS), audio(majik_providers::replicate::descriptor()), "Replicate audio");
        assert!(majik_providers::openrouter::descriptor().supported_audio_models.is_empty(), "OpenRouter has gained audio; give it a module");
    }

    /// Tools and prompt improvement have a module per provider rather than per model, so the guard
    /// only has to catch a provider gaining or losing one.
    #[test]
    fn the_tool_and_text_modules_match_every_provider() {
        for descriptor in [majik_providers::fal::descriptor(), majik_providers::replicate::descriptor()] {
            assert!(descriptor.supports_tool(ToolId::Upscale), "{} lost upscaling; drop its tools module", descriptor.display_name);
            assert!(descriptor.supports_tool(ToolId::RemoveBackground), "{} lost background removal", descriptor.display_name);
        }
        let openrouter = majik_providers::openrouter::descriptor();
        assert!(!openrouter.supports_tool(ToolId::Upscale) && !openrouter.supports_tool(ToolId::RemoveBackground), "OpenRouter has gained tools; give it a module");

        for descriptor in [majik_providers::fal::descriptor(), majik_providers::replicate::descriptor(), openrouter] {
            assert!(descriptor.supports_prompt_improvement(), "{} lost its text client; drop its text module", descriptor.display_name);
        }
    }
}
