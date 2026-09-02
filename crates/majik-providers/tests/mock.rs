//! The Mock provider: prompt directives and the deterministic image / video renderers.

use std::time::Instant;

use majik_providers::catalog;
use majik_providers::mock::{self, image_renderer, parse_directives, video_renderer, MockClient};
use majik_providers::{
    AspectRatio, AssetRole, AudioGenerationSettings, AudioModel, AudioProviderClient, AudioVoice, GenerationError, ImageModel, ImageProviderClient,
    ImageResolution, ProviderAsset, ProviderClient, ProviderId, ToolModel, ToolProviderClient, ToolSettings, VideoAspectRatio, VideoGenerationSettings,
    VideoModel, VideoProviderClient, VideoResolution,
};

/// One runtime for every test in this binary. `http::client()` is a process-wide `reqwest::Client`
/// whose pooled connections each carry a dispatch task owned by the runtime that opened them. Give
/// every test its own runtime and that task dies with the test while the idle connection stays in
/// the pool, so when a later wiremock server binds the port the dead one released, the next test
/// handed that connection fails with "dispatch task is gone: runtime dropped the dispatch task".
/// Which test fails is a race, so it breaks on one runner and passes on the others. One runtime for
/// the whole binary keeps every dispatch task alive as long as the pool that refers to it, the way
/// `e2e.rs` already shares its own.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("test runtime"))
}

fn gemini3_pro() -> ImageModel {
    catalog::image::model("gemini-3-pro")
        .cloned()
        .unwrap_or_else(|| ImageModel::new("gemini-3-pro", "Nano Banana Pro", "Google", "logo-google", "TBD"))
}

fn veo31() -> VideoModel {
    catalog::video::model("veo-3.1").cloned().unwrap_or_else(|| VideoModel::new("veo-3.1", "Veo 3.1", "Google", "logo-google", "TBD"))
}

fn audio_model() -> AudioModel {
    catalog::audio::ALL.first().cloned().unwrap_or_else(|| AudioModel::new("mock-tts", "Mock TTS", "Mock", "logo-mock", "TBD"))
}

fn video_settings(duration: u32) -> VideoGenerationSettings {
    VideoGenerationSettings {
        model: veo31(),
        aspect_ratio: Some(VideoAspectRatio::Landscape),
        resolution: Some(VideoResolution::Sd),
        duration,
        audio_enabled: false,
    }
}

fn provider() -> MockClient {
    MockClient::new("any")
}

fn png_dimensions(png: &[u8]) -> (u32, u32) {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).expect("decode PNG");
    (img.width(), img.height())
}

/// Reads the `mvhd` box and returns the movie duration in seconds.
fn mp4_duration_seconds(bytes: &[u8]) -> Option<f64> {
    let p = bytes.windows(4).position(|w| w == b"mvhd")?;
    let be32 = |at: usize| -> Option<u32> { bytes.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())) };
    let be64 = |at: usize| -> Option<u64> { bytes.get(at..at + 8).map(|b| u64::from_be_bytes(b.try_into().unwrap())) };
    let version = *bytes.get(p + 4)?;
    let (timescale, duration) = if version == 1 {
        (be32(p + 24)?, be64(p + 28)?)
    } else {
        (be32(p + 16)?, be32(p + 20)? as u64)
    };
    if timescale == 0 {
        return None;
    }
    Some(duration as f64 / timescale as f64)
}

// ----- MockProvider ---------------------------------------------------------------------------

mod mock_provider {
    use super::*;

    #[test]
    fn image_success() {
        crate::rt().block_on(image_success_inner());
    }

    async fn image_success_inner() {
        let data = provider().generate_image("red circle", &gemini3_pro(), &[], Some(AspectRatio::Square), Some(ImageResolution::Hd)).await.unwrap();
        assert!(!data.is_empty());
        assert!(data.starts_with(b"\x89PNG"));
    }

    #[test]
    fn image_fails_rate_limited() {
        crate::rt().block_on(image_fails_rate_limited_inner());
    }

    async fn image_fails_rate_limited_inner() {
        let err = provider()
            .generate_image("red circle #fail:rateLimited", &gemini3_pro(), &[], Some(AspectRatio::Square), Some(ImageResolution::Hd))
            .await
            .unwrap_err();
        assert!(matches!(err, GenerationError::RateLimited(_)), "got {err:?}");
    }

    #[test]
    fn image_delay() {
        crate::rt().block_on(image_delay_inner());
    }

    async fn image_delay_inner() {
        let start = Instant::now();
        provider().generate_image("red circle #delay:0.1", &gemini3_pro(), &[], Some(AspectRatio::Square), Some(ImageResolution::Hd)).await.unwrap();
        let elapsed = start.elapsed().as_secs_f64();
        assert!(elapsed >= 0.09, "elapsed {elapsed}");
        assert!(elapsed < 1.0, "elapsed {elapsed}");
    }

    #[test]
    fn directive_stripping() {
        crate::rt().block_on(directive_stripping_inner());
    }

    async fn directive_stripping_inner() {
        let a = provider().generate_image("red circle", &gemini3_pro(), &[], Some(AspectRatio::Square), Some(ImageResolution::Hd)).await.unwrap();
        let b = provider().generate_image("red circle #delay:0", &gemini3_pro(), &[], Some(AspectRatio::Square), Some(ImageResolution::Hd)).await.unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn video_success() {
        crate::rt().block_on(video_success_inner());
    }

    async fn video_success_inner() {
        let data = provider().generate_video("ocean", &[], &video_settings(2)).await.unwrap();
        assert!(!data.is_empty());
        assert!(data.windows(4).any(|w| w == b"ftyp"), "not an MP4");
    }

    #[test]
    fn video_fails_timeout() {
        crate::rt().block_on(video_fails_timeout_inner());
    }

    async fn video_fails_timeout_inner() {
        let err = provider().generate_video("ocean #fail:timeout", &[], &video_settings(2)).await.unwrap_err();
        assert_eq!(err, GenerationError::Timeout);
    }

    #[test]
    fn audio_success_is_silent_wav() {
        crate::rt().block_on(audio_success_is_silent_wav_inner());
    }

    async fn audio_success_is_silent_wav_inner() {
        let settings = AudioGenerationSettings { model: audio_model(), speaker1: AudioVoice::new("v1", "Voice"), speaker2: None };
        let data = provider().generate_audio("hello", &settings).await.unwrap();
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");
        assert_eq!(data, MockClient::silent_wav(250));
    }

    #[test]
    fn silent_wav_layout() {
        let wav = MockClient::silent_wav(250);
        let data_size = 8000 * 250 / 1000 * 2;
        assert_eq!(wav.len(), 44 + data_size);
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize, 36 + data_size);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // mono
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 8000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 16000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize, data_size);
        assert!(wav[44..].iter().all(|&b| b == 0));
        assert_eq!(MockClient::silent_wav(0).len(), 44 + 2); // max(1, …) samples
    }

    #[test]
    fn upscale_pass_through() {
        crate::rt().block_on(upscale_pass_through_inner());
    }

    async fn upscale_pass_through_inner() {
        let input = [0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(run(&catalog::tool::MOCK_UPSCALE, AssetRole::ReferenceImage, "image/png", &input).await, input);
    }

    /// The video upscaler is the same pass-through, and exists so the composer's video tool path
    /// has something to run headlessly.
    #[test]
    fn video_upscale_pass_through() {
        crate::rt().block_on(video_upscale_pass_through_inner());
    }

    async fn video_upscale_pass_through_inner() {
        let clip = majik_providers::mock::video_renderer::render_blocking(64, 64, 1, [0, 128, 0]).unwrap();
        assert_eq!(run(&catalog::tool::MOCK_UPSCALE_VIDEO, AssetRole::ReferenceVideo, "video/mp4", &clip).await, clip);
    }

    #[test]
    fn remove_background_keys_out_the_corner_colour() {
        crate::rt().block_on(remove_background_keys_out_the_corner_colour_inner());
    }

    async fn remove_background_keys_out_the_corner_colour_inner() {
        // Red canvas with a blue 2×2 subject in the middle.
        let mut canvas = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
        for (x, y) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
            canvas.put_pixel(x, y, image::Rgb([0, 0, 255]));
        }
        let mut input = std::io::Cursor::new(Vec::new());
        canvas.write_to(&mut input, image::ImageFormat::Png).unwrap();
        let output = run(&catalog::tool::MOCK_REMOVE_BACKGROUND, AssetRole::ReferenceImage, "image/png", &input.into_inner()).await;
        let matted = image::load_from_memory(&output).unwrap().into_rgba8();
        assert_eq!(matted.dimensions(), (4, 4));
        assert_eq!(matted.get_pixel(0, 0).0[3], 0, "corner colour is transparent");
        assert_eq!(matted.get_pixel(3, 3).0[3], 0);
        assert_eq!(matted.get_pixel(1, 1).0, [0, 0, 255, 255], "subject keeps its colour and stays opaque");
    }

    #[test]
    fn remove_background_passes_non_images_through() {
        crate::rt().block_on(remove_background_passes_non_images_through_inner());
    }

    async fn remove_background_passes_non_images_through_inner() {
        let input = [0x01, 0x02, 0x03];
        assert_eq!(run(&catalog::tool::MOCK_REMOVE_BACKGROUND, AssetRole::ReferenceImage, "image/png", &input).await, input);
    }

    /// One tool run on the mock client.
    async fn run(model: &ToolModel, role: AssetRole, content_type: &str, data: &[u8]) -> Vec<u8> {
        let settings = ToolSettings::new(model.clone());
        let input = ProviderAsset::new(role, content_type, data.to_vec());
        provider().run_tool(&settings, &input).await.unwrap()
    }
}

// ----- MockDescriptor -------------------------------------------------------------------------

mod mock_descriptor {
    use super::*;

    #[test]
    fn identity_and_flags() {
        let d = mock::descriptor();
        assert_eq!(d.id, ProviderId::mock());
        assert_eq!(d.display_name, "Mock");
        assert!(d.requires_api_key);
        assert_eq!(d.is_user_selectable, cfg!(debug_assertions));
        assert_eq!(
            d.supported_tool_models,
            vec![catalog::tool::MOCK_UPSCALE.clone(), catalog::tool::MOCK_UPSCALE_VIDEO.clone(), catalog::tool::MOCK_REMOVE_BACKGROUND.clone()]
        );
        // A video upscaler on the Mock provider is what lets the composer's video tool path run
        // headlessly, with no key and no network.
        assert_eq!(
            d.tool_models_for(majik_core::model::ToolId::Upscale, majik_core::model::MediaType::Video),
            vec![&catalog::tool::MOCK_UPSCALE_VIDEO]
        );
        assert_eq!(d.api_key_placeholder, "mock-any-key");
    }

    #[test]
    fn claims_every_catalog_model() {
        let d = mock::descriptor();
        assert_eq!(d.supported_image_models, catalog::image::ALL.to_vec());
        assert_eq!(d.supported_video_models, catalog::video::ALL.to_vec());
        assert_eq!(d.supported_audio_models, catalog::audio::ALL.to_vec());
    }

    /// Mock claims the whole catalog but borrows its capability tables from the real providers, so a
    /// model on only one of them still has to resolve. Without this, a model the composer offers
    /// under Mock produces no requests at all and Generate fails with "Write a prompt first."
    #[test]
    fn every_claimed_model_resolves_capabilities() {
        let d = mock::descriptor();
        for model in catalog::image::ALL {
            assert!(d.image_capabilities(model).is_some(), "no Mock image capabilities for {}", model.id);
        }
        for model in catalog::video::ALL {
            assert!(d.video_capabilities(model).is_some(), "no Mock video capabilities for {}", model.id);
        }
        for model in catalog::audio::ALL {
            assert!(d.audio_capabilities(model).is_some(), "no Mock audio capabilities for {}", model.id);
        }
    }

    #[test]
    fn provider_client_facade_serves_all_three() {
        crate::rt().block_on(provider_client_facade_serves_all_three_inner());
    }

    async fn provider_client_facade_serves_all_three_inner() {
        let client = ProviderClient::new(mock::descriptor(), "any");
        let img = client.generate_image("x", &gemini3_pro(), &[], None, None).await.unwrap();
        assert!(img.starts_with(b"\x89PNG"));
        let vid = client.generate_video("x", &[], &video_settings(1)).await.unwrap();
        assert!(!vid.is_empty());
        let settings = AudioGenerationSettings { model: audio_model(), speaker1: AudioVoice::new("v1", "Voice"), speaker2: None };
        let aud = client.generate_audio("x", &settings).await.unwrap();
        assert!(aud.starts_with(b"RIFF"));
        let settings = ToolSettings::new(catalog::tool::MOCK_UPSCALE.clone());
        let input = ProviderAsset::new(AssetRole::ReferenceImage, "image/png", vec![1, 2]);
        assert_eq!(client.run_tool(&settings, &input).await.unwrap(), vec![1, 2]);
    }
}

// ----- MockPromptDirectives.parse -------------------------------------------------------------

mod mock_prompt_directives {
    use super::*;

    #[test]
    fn plain_prompt() {
        let parsed = parse_directives("red circle on white");
        assert_eq!(parsed.clean_prompt, "red circle on white");
        assert_eq!(parsed.delay, 0.0);
        assert_eq!(parsed.failure, None);
    }

    #[test]
    fn empty_prompt() {
        let parsed = parse_directives("");
        assert_eq!(parsed.clean_prompt, "");
        assert_eq!(parsed.delay, 0.0);
        assert_eq!(parsed.failure, None);
    }

    #[test]
    fn delay_parsed() {
        let parsed = parse_directives("red circle #delay:3");
        assert_eq!(parsed.clean_prompt, "red circle");
        assert_eq!(parsed.delay, 3.0);
        assert_eq!(parsed.failure, None);
    }

    #[test]
    fn fractional_delay() {
        assert_eq!(parse_directives("circle #delay:0.5").delay, 0.5);
    }

    #[test]
    fn malformed_delay() {
        let parsed = parse_directives("circle #delay:abc");
        assert_eq!(parsed.delay, 0.0);
        assert_eq!(parsed.clean_prompt, "circle");
    }

    #[test]
    fn fail_outcomes() {
        type Matcher = fn(&GenerationError) -> bool;
        let outcomes: Vec<(&str, Matcher)> = vec![
            ("unauthorized", |e| matches!(e, GenerationError::Unauthorized(_))),
            ("rateLimited", |e| matches!(e, GenerationError::RateLimited(_))),
            ("contentFiltered", |e| matches!(e, GenerationError::ContentFiltered(_))),
            ("timeout", |e| matches!(e, GenerationError::Timeout)),
            ("noResult", |e| matches!(e, GenerationError::NoResultGenerated)),
            ("paymentRequired", |e| matches!(e, GenerationError::PaymentRequired(_))),
            ("serverError", |e| matches!(e, GenerationError::ServerError { status_code: Some(500), .. })),
            ("invalidRequest", |e| matches!(e, GenerationError::InvalidRequest(_))),
        ];
        for (name, matcher) in outcomes {
            let parsed = parse_directives(&format!("circle #fail:{name}"));
            let failure = parsed.failure.unwrap_or_else(|| panic!("Expected failure for #fail:{name}"));
            assert!(matcher(&failure), "Wrong error case for #fail:{name}, got {failure:?}");
        }
    }

    #[test]
    fn bare_fail() {
        let failure = parse_directives("circle #fail").failure.expect("Expected failure");
        assert!(matches!(failure, GenerationError::Unknown(_)), "Expected Unknown, got {failure:?}");
    }

    #[test]
    fn unknown_fail_value() {
        let failure = parse_directives("circle #fail:banana").failure.expect("Expected failure");
        assert!(matches!(failure, GenerationError::Unknown(_)), "Expected Unknown, got {failure:?}");
    }

    #[test]
    fn unknown_directive() {
        let parsed = parse_directives("red #banana circle");
        assert_eq!(parsed.clean_prompt, "red circle");
        assert_eq!(parsed.failure, None);
        assert_eq!(parsed.delay, 0.0);
    }

    #[test]
    fn mixed_order() {
        let parsed = parse_directives("red #delay:1 circle #fail:contentFiltered on white");
        assert_eq!(parsed.clean_prompt, "red circle on white");
        assert_eq!(parsed.delay, 1.0);
        let failure = parsed.failure.expect("Expected failure");
        assert!(matches!(failure, GenerationError::ContentFiltered(_)), "Expected ContentFiltered, got {failure:?}");
    }

    #[test]
    fn directive_only() {
        let parsed = parse_directives("#fail:timeout");
        assert_eq!(parsed.clean_prompt, "");
        assert!(parsed.failure.is_some());
    }
}

// ----- MockImageRenderer ----------------------------------------------------------------------

mod mock_image_renderer {
    use super::*;

    #[test]
    fn determinism() {
        let a = image_renderer::render(&ProviderId::mock(), &gemini3_pro(), "red circle", Some(AspectRatio::Square), Some(ImageResolution::Hd));
        let b = image_renderer::render(&ProviderId::mock(), &gemini3_pro(), "red circle", Some(AspectRatio::Square), Some(ImageResolution::Hd));
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn different_prompts() {
        let a = image_renderer::render(&ProviderId::mock(), &gemini3_pro(), "red circle", Some(AspectRatio::Square), Some(ImageResolution::Hd));
        let b = image_renderer::render(&ProviderId::mock(), &gemini3_pro(), "blue circle", Some(AspectRatio::Square), Some(ImageResolution::Hd));
        assert_ne!(a, b);
    }

    #[test]
    fn square_pixel_size() {
        assert_eq!(image_renderer::pixel_size(Some(AspectRatio::Square), Some(ImageResolution::Hd)), (1024, 1024));
        assert_eq!(image_renderer::pixel_size(Some(AspectRatio::Square), Some(ImageResolution::Fhd)), (2048, 2048));
        assert_eq!(image_renderer::pixel_size(Some(AspectRatio::Square), Some(ImageResolution::Uhd)), (3840, 3840));
        assert_eq!(image_renderer::pixel_size(Some(AspectRatio::Square), Some(ImageResolution::Sd)), (512, 512));
    }

    #[test]
    fn landscape_pixel_size() {
        let (w, h) = image_renderer::pixel_size(Some(AspectRatio::Landscape), Some(ImageResolution::Hd));
        assert!(w > h);
        assert_eq!(w, 1024);
    }

    #[test]
    fn tall_pixel_size() {
        let (w, h) = image_renderer::pixel_size(Some(AspectRatio::Tall), Some(ImageResolution::Hd));
        assert!(h > w);
        assert_eq!(h, 1024);
    }

    #[test]
    fn nil_resolution_defaults_to_hd() {
        assert_eq!(image_renderer::pixel_size(Some(AspectRatio::Square), None), (1024, 1024));
        assert_eq!(image_renderer::pixel_size(None, None), (1024, 1024));
    }

    #[test]
    fn rendered_dimensions() {
        let data = image_renderer::render(&ProviderId::mock(), &gemini3_pro(), "test", Some(AspectRatio::Landscape), Some(ImageResolution::Hd));
        assert_eq!(png_dimensions(&data), (1024, 576));
    }

    #[test]
    fn rendered_pixels_match_color() {
        let data = image_renderer::render(&ProviderId::mock(), &gemini3_pro(), "test", Some(AspectRatio::Square), Some(ImageResolution::Sd));
        let expected = image_renderer::color(&ProviderId::mock(), "gemini-3-pro", "test", 512, 512);
        let img = image::load_from_memory(&data).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, expected);
        assert_eq!(img.get_pixel(511, 511).0, expected);
    }

    #[test]
    fn color_varies_by_provider() {
        let fal = image_renderer::color(&ProviderId::fal(), "m", "p", 100, 100);
        let mock = image_renderer::color(&ProviderId::mock(), "m", "p", 100, 100);
        assert_ne!(fal, mock);
    }

    #[test]
    fn color_stable() {
        let a = image_renderer::color(&ProviderId::mock(), "m", "p", 100, 100);
        let b = image_renderer::color(&ProviderId::mock(), "m", "p", 100, 100);
        assert_eq!(a, b);
    }

    #[test]
    fn ratio_helpers() {
        assert_eq!(image_renderer::parse_ratio("16:9", (1, 1)), (16, 9));
        assert_eq!(image_renderer::parse_ratio("auto", (16, 9)), (16, 9));
        assert_eq!(image_renderer::parse_ratio("1:2:3", (4, 5)), (4, 5));
        assert_eq!(image_renderer::parse_ratio("a:b", (4, 5)), (4, 5));
        assert_eq!(image_renderer::fit_longest_edge(1024, 16, 9), (1024, 576));
        assert_eq!(image_renderer::fit_longest_edge(1024, 9, 16), (576, 1024));
        assert_eq!(image_renderer::fit_longest_edge(480, 16, 9), (480, 270));
        assert_eq!(image_renderer::fit_longest_edge(1024, 21, 9), (1024, 438)); // 438.86 → 438 (even)
        assert_eq!(image_renderer::fit_longest_edge(1024, 4, 5), (818, 1024)); // 819.2 → 819 & ~1 = 818
        assert_eq!(image_renderer::fit_longest_edge(2, 1000, 1), (2, 2)); // max(2, …)
    }
}

// ----- MockVideoRenderer ----------------------------------------------------------------------

mod mock_video_renderer {
    use super::*;
    use majik_core::video;

    fn write_clip(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn assert_colour(px: &[u8], expected: [u8; 3]) {
        assert!(px.iter().zip(expected).all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= 8), "{px:?} vs {expected:?}");
    }

    #[test]
    fn renders_requested_duration_size_and_colour() {
        crate::rt().block_on(renders_requested_duration_size_and_colour_inner());
    }

    async fn renders_requested_duration_size_and_colour_inner() {
        let data = video_renderer::render(&ProviderId::mock(), &veo31(), "ocean", 3, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Sd)).await.unwrap();
        assert_eq!(&data[4..8], b"ftyp");
        assert_eq!(mp4_duration_seconds(&data), Some(3.0));

        let (_dir, path) = write_clip(&data);
        let info = video::probe(&path).unwrap();
        assert_eq!((info.width, info.height, info.duration_secs, info.has_audio), (Some(480), Some(270), Some(3.0), false));

        let poster = video::poster(&path, 400).unwrap();
        assert_eq!((poster.width(), poster.height()), (400, 225));
        let expected = image_renderer::color(&ProviderId::mock(), veo31().id, "ocean", 480, 270);
        assert_colour(&poster.get_pixel(200, 112).0[..3], expected);
    }

    #[test]
    fn colour_follows_the_prompt() {
        crate::rt().block_on(colour_follows_the_prompt_inner());
    }

    async fn colour_follows_the_prompt_inner() {
        let a = video_renderer::render(&ProviderId::mock(), &veo31(), "ocean", 1, None, None).await.unwrap();
        let b = video_renderer::render(&ProviderId::mock(), &veo31(), "desert", 1, None, None).await.unwrap();
        let (_dir_a, path_a) = write_clip(&a);
        let (_dir_b, path_b) = write_clip(&b);
        let px_a = video::poster(&path_a, 64).unwrap().get_pixel(32, 18).0;
        let px_b = video::poster(&path_b, 64).unwrap().get_pixel(32, 18).0;
        assert_ne!(px_a, px_b);
        let again = video_renderer::render(&ProviderId::mock(), &veo31(), "ocean", 1, None, None).await.unwrap();
        assert_eq!(a, again, "deterministic for the same prompt");
    }

    #[test]
    fn tall_aspect_is_honoured() {
        crate::rt().block_on(tall_aspect_is_honoured_inner());
    }

    async fn tall_aspect_is_honoured_inner() {
        let data = video_renderer::render(&ProviderId::mock(), &veo31(), "ocean", 2, Some(VideoAspectRatio::Tall), Some(VideoResolution::Hd)).await.unwrap();
        let (_dir, path) = write_clip(&data);
        let info = video::probe(&path).unwrap();
        assert_eq!((info.width, info.height), (Some(404), Some(720)));
    }

    #[test]
    fn every_sample_is_a_keyframe_so_seeking_is_free() {
        crate::rt().block_on(every_sample_is_a_keyframe_so_seeking_is_free_inner());
    }

    async fn every_sample_is_a_keyframe_so_seeking_is_free_inner() {
        let data = video_renderer::render(&ProviderId::mock(), &veo31(), "ocean", 3, None, None).await.unwrap();
        let (_dir, path) = write_clip(&data);
        let mut source = video::Source::open(&path).unwrap();
        assert_eq!(source.frame_at(2.0).unwrap().unwrap().pts_secs, 2.0);
        assert_eq!(source.frame_at(0.0).unwrap().unwrap().pts_secs, 0.0);
        assert_eq!(source.frame_at(1.0).unwrap().unwrap().pts_secs, 1.0);
        assert_eq!(source.frame_interval(), std::time::Duration::from_secs(1));
    }

    #[test]
    fn duration_is_at_least_one_second() {
        let data = video_renderer::render_blocking(64, 64, 0, [1, 2, 3]).unwrap();
        assert_eq!(mp4_duration_seconds(&data), Some(1.0));
    }

    #[test]
    fn pixel_size_landscape() {
        assert_eq!(video_renderer::pixel_size(Some(VideoAspectRatio::Landscape), Some(VideoResolution::Sd)), (480, 270));
    }

    #[test]
    fn pixel_size_auto() {
        let (w, h) = video_renderer::pixel_size(Some(VideoAspectRatio::Auto), Some(VideoResolution::Sd));
        assert!(w > h);
        assert_eq!((w, h), (480, 270));
    }

    #[test]
    fn pixel_size_defaults_and_buckets() {
        assert_eq!(video_renderer::pixel_size(None, None), (480, 270));
        assert_eq!(video_renderer::pixel_size(Some(VideoAspectRatio::Tall), Some(VideoResolution::Hd)), (404, 720));
        assert_eq!(video_renderer::pixel_size(Some(VideoAspectRatio::Square), Some(VideoResolution::Fhd)), (1080, 1080));
        assert_eq!(video_renderer::pixel_size(Some(VideoAspectRatio::Landscape), Some(VideoResolution::Uhd)), (1920, 1080));
    }
}

mod resume {
    use super::*;
    use majik_core::model::MediaType;
    use majik_providers::{JobHandle, ResumableClient as _};
    use std::sync::{Arc, Mutex};

    #[test]
    fn generate_reports_a_mock_job_handle_before_the_delay() {
        crate::rt().block_on(generate_reports_a_mock_job_handle_before_the_delay_inner());
    }

    async fn generate_reports_a_mock_job_handle_before_the_delay_inner() {
        let seen: Arc<Mutex<Vec<JobHandle>>> = Default::default();
        let sink = seen.clone();
        let client = provider().with_on_accepted(Arc::new(move |handle| sink.lock().unwrap().push(handle)));
        client.generate_image("a cat #delay:0", &gemini3_pro(), &[], None, None).await.unwrap();
        let handles = seen.lock().unwrap().clone();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].job_id, MockClient::job_id("image", "a cat"), "directives are stripped from the hashed prompt");
        assert_eq!(handles[0].poll_url, None);
        assert!(handles[0].job_id.starts_with("mock-image-"));
    }

    #[test]
    fn generate_traces_a_submit_and_a_result_that_carries_the_failure() {
        crate::rt().block_on(generate_traces_a_submit_and_a_result_that_carries_the_failure_inner());
    }

    async fn generate_traces_a_submit_and_a_result_that_carries_the_failure_inner() {
        use majik_core::model::TraceLabel::*;
        let seen: Arc<Mutex<Vec<majik_core::model::JobTrace>>> = Default::default();
        let sink = seen.clone();
        let client = provider().with_on_trace(Arc::new(move |trace| sink.lock().unwrap().push(trace)));
        client.generate_image("a cat #delay:0", &gemini3_pro(), &[], None, None).await.unwrap();
        let traces = seen.lock().unwrap().clone();
        assert_eq!(traces.iter().map(|t| t.label).collect::<Vec<_>>(), [Submit, Result]);
        assert_eq!(traces[0].url, format!("mock://image/{}", MockClient::job_id("image", "a cat")));
        assert_eq!((traces[0].method.as_str(), traces[0].status), ("POST", Some(202)));
        assert!(traces[0].request_body.as_deref().unwrap_or_default().contains(r#""prompt":"a cat""#), "{:?}", traces[0].request_body);
        assert!(traces[0].response_body.as_deref().unwrap_or_default().contains("request_id"));
        assert_eq!((traces[1].method.as_str(), traces[1].status), ("GET", Some(200)));
        assert!(traces[1].response_body.as_deref().unwrap_or_default().contains("COMPLETED"));

        seen.lock().unwrap().clear();
        let error = client.generate_image("a dog #fail:rateLimited", &gemini3_pro(), &[], None, None).await.unwrap_err();
        let traces = seen.lock().unwrap().clone();
        assert_eq!(traces.iter().map(|t| t.label).collect::<Vec<_>>(), [Submit, Result]);
        assert_eq!(traces[1].status, Some(500));
        let body = traces[1].response_body.as_deref().unwrap_or_default();
        assert!(body.contains("FAILED") && body.contains(&error.to_string()), "{body}");
    }

    #[test]
    fn resume_renders_a_fixture_for_each_media_type() {
        crate::rt().block_on(resume_renders_a_fixture_for_each_media_type_inner());
    }

    async fn resume_renders_a_fixture_for_each_media_type_inner() {
        let handle = JobHandle { job_id: MockClient::job_id("image", "x"), poll_url: None };
        assert!(provider().resume(&handle, MediaType::Image).await.unwrap().starts_with(&[0x89, b'P', b'N', b'G']));
        let video = provider().resume(&handle, MediaType::Video).await.unwrap();
        assert!(video.len() > 100 && &video[4..8] == b"ftyp", "an MP4");
        assert!(provider().resume(&handle, MediaType::Audio).await.unwrap().starts_with(b"RIFF"));
    }

    #[test]
    fn resume_of_a_foreign_or_expired_job_is_job_gone() {
        crate::rt().block_on(resume_of_a_foreign_or_expired_job_is_job_gone_inner());
    }

    async fn resume_of_a_foreign_or_expired_job_is_job_gone_inner() {
        for id in ["pred-1", "mock-image-gone"] {
            let handle = JobHandle { job_id: id.into(), poll_url: None };
            assert_eq!(provider().resume(&handle, MediaType::Image).await.unwrap_err(), GenerationError::JobGone, "{id}");
        }
    }
}

// ----- prompt improvement -----------------------------------------------------------------------

#[test]
fn mock_rewrites_a_prompt_deterministically() {
    crate::rt().block_on(mock_rewrites_a_prompt_deterministically_inner());
}

async fn mock_rewrites_a_prompt_deterministically_inner() {
    use majik_providers::TextProviderClient as _;
    let client = MockClient::new("k");
    let text = client.complete_text("instruction", "a cat", 400).await.unwrap();
    assert_eq!(text, "a cat, cinematic lighting, highly detailed");
    assert_eq!(client.complete_text("instruction", "a cat", 400).await.unwrap(), text, "the same prompt rewrites the same way");
}

#[test]
fn a_fail_directive_fails_the_rewrite_too() {
    crate::rt().block_on(a_fail_directive_fails_the_rewrite_too_inner());
}

async fn a_fail_directive_fails_the_rewrite_too_inner() {
    use majik_providers::TextProviderClient as _;
    let error = MockClient::new("k").complete_text("instruction", "a cat #fail:rateLimited", 400).await.unwrap_err();
    assert!(matches!(error, GenerationError::RateLimited(_)), "{error:?}");
}
