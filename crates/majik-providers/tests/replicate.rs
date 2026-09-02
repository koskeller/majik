//! Replicate: request bodies, capabilities and error mapping, plus wiremock coverage of the
//! predictions API flow.
//!
//! Models are constructed directly by id so the body/capability tests don't depend on the catalog;
//! only the descriptor-level tests go through `catalog::*`.

use std::collections::BTreeMap;

use majik_providers::asset::{AssetRole, ProviderAsset};
use majik_providers::catalog;
use majik_providers::client::{AudioProviderClient, ImageProviderClient, ToolProviderClient, VideoProviderClient};
use majik_providers::error::GenerationError;
use majik_providers::models::{
    AspectRatio, AudioModel, AudioVoice, ImageModel, ImageResolution, VideoAspectRatio, VideoDurationRange, VideoModel, VideoResolution,
};
use majik_providers::replicate::audio::{audio_capabilities, build_audio_request_body};
use majik_providers::replicate::capabilities::{
    api_end_frame_param, api_start_frame_param, resolve_video_endpoint, video_capabilities, video_reference_params, SUPPORTED_VIDEO_MODEL_IDS,
};
use majik_providers::replicate::provider::{
    build_remove_background_request_body, build_request_body, build_upscale_request_body, build_video_reference_body, build_video_request_body,
};
use majik_providers::replicate::{self, ReplicateClient, ReplicateError, VideoEndpointVariant};
use majik_providers::settings::{AudioGenerationSettings, ToolSettings, VideoGenerationSettings};
use majik_providers::ReferenceAssets;
use serde_json::{json, Map, Value};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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

// ----- helpers ----------------------------------------------------------------------------------

fn img(id: &'static str) -> ImageModel {
    ImageModel::new(id, id, "test", "logo-test", "")
}

fn vid(id: &'static str) -> VideoModel {
    VideoModel::new(id, id, "test", "logo-test", "")
}

fn aud(id: &'static str) -> AudioModel {
    AudioModel::new(id, id, "test", "logo-test", "")
}

fn s<'a>(body: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

fn image_body(prompt: &str, model: &ImageModel, images: &[&[u8]], ar: Option<AspectRatio>, res: Option<ImageResolution>) -> Map<String, Value> {
    build_request_body(prompt, model, images, None, ar, res)
}

fn video_settings(model: VideoModel, ar: Option<VideoAspectRatio>, res: Option<VideoResolution>, duration: u32, audio: bool) -> VideoGenerationSettings {
    VideoGenerationSettings { model, aspect_ratio: ar, resolution: res, duration, audio_enabled: audio }
}

fn client(server: &MockServer) -> ReplicateClient {
    ReplicateClient::with_base_url("test-key", format!("{}/v1", server.uri()))
}

fn png_bytes() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30])).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn jpeg_bytes() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30])).write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

// ----- image body shape -------------------------------------------------------------------------

#[test]
fn flux_schnell_t2i_body() {
    let body = image_body("a cat", &img("flux-1-schnell"), &[], Some(AspectRatio::Square), Some(ImageResolution::Hd));
    assert_eq!(s(&body, "prompt"), Some("a cat"));
    assert_eq!(s(&body, "aspect_ratio"), Some("1:1"));
    assert_eq!(s(&body, "megapixels"), Some("1"));
    assert_eq!(s(&body, "output_format"), Some("png"));
    assert!(body.get("image").is_none(), "schnell has no image input");
    assert!(body.get("images").is_none());
}

#[test]
fn flux_dev_edit_image_is_scalar() {
    let body = image_body("edit", &img("flux-1-dev"), &[&[0x01]], Some(AspectRatio::Landscape), Some(ImageResolution::Sd));
    let data_uri = s(&body, "image").expect("image is a string");
    assert!(data_uri.starts_with("data:image/png;base64,"));
    assert_eq!(s(&body, "aspect_ratio"), Some("16:9"));
    assert_eq!(s(&body, "megapixels"), Some("0.25"));
}

#[test]
fn flux2_pro_edit_input_images_is_array() {
    let body = image_body("edit", &img("flux-2-pro"), &[&[0x01], &[0x02]], Some(AspectRatio::Square), Some(ImageResolution::Uhd));
    let arr = body["input_images"].as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert_eq!(s(&body, "resolution"), Some("4 MP"));
    assert_eq!(s(&body, "aspect_ratio"), Some("1:1"));
    assert_eq!(s(&body, "output_format"), Some("png"));
}

#[test]
fn flux2_klein_edit_images_and_output_megapixels() {
    let body = image_body("edit", &img("flux-2-klein"), &[&[0x01]], Some(AspectRatio::Square), Some(ImageResolution::Fhd));
    assert_eq!(body["images"].as_array().map(Vec::len), Some(1));
    assert_eq!(s(&body, "output_megapixels"), Some("2"));
}

#[test]
fn seedream_image_input_and_size() {
    let body = image_body("p", &img("seedream-4.5"), &[&[0x01]], Some(AspectRatio::Standard), Some(ImageResolution::Uhd));
    assert_eq!(body["image_input"].as_array().map(Vec::len), Some(1));
    assert_eq!(s(&body, "size"), Some("4K"));
    assert!(body.get("output_format").is_none(), "seedream has no output_format");
}

#[test]
fn gpt_image_2_square_only_and_quality() {
    let square_uhd = image_body("p", &img("gpt-image-2"), &[], Some(AspectRatio::Square), Some(ImageResolution::Uhd));
    assert_eq!(s(&square_uhd, "aspect_ratio"), Some("1:1"));
    assert_eq!(s(&square_uhd, "quality"), Some("high"));
    assert_eq!(s(&square_uhd, "output_format"), Some("png"));

    let landscape_fhd = image_body("p", &img("gpt-image-2"), &[], Some(AspectRatio::Landscape), Some(ImageResolution::Fhd));
    assert!(landscape_fhd.get("aspect_ratio").is_none(), "gpt-image-2 only accepts 1:1");
    assert_eq!(s(&landscape_fhd, "quality"), Some("medium"));
}

#[test]
fn recraft_v4_pro_no_resolution_no_image_input() {
    // The image should be ignored: there is no edit endpoint.
    let body = image_body("p", &img("recraft-4-pro"), &[&[0x01]], Some(AspectRatio::Standard), Some(ImageResolution::Fhd));
    assert_eq!(s(&body, "aspect_ratio"), Some("4:3"));
    assert!(body.get("resolution").is_none(), "recraft size enum doesn't map to our resolution");
    assert!(body.get("image").is_none());
    assert!(body.get("images").is_none());
    assert!(body.get("input_images").is_none());
}

#[test]
fn wan27_image_pro_size_only_images_array() {
    let body = image_body("p", &img("wan-2.7-pro"), &[&[0x01]], Some(AspectRatio::Square), Some(ImageResolution::Uhd));
    assert!(body.get("aspect_ratio").is_none(), "wan-2.7-image-pro uses combined size");
    assert_eq!(s(&body, "size"), Some("4K"));
    assert_eq!(body["images"].as_array().map(Vec::len), Some(1));
}

#[test]
fn safety_override_disable_safety_checker() {
    for id in ["flux-1-schnell", "flux-1-dev", "flux-2-klein", "seedream-4.5"] {
        let body = image_body("p", &img(id), &[], Some(AspectRatio::Square), None);
        assert_eq!(body.get("disable_safety_checker"), Some(&json!(true)), "missing for {id}");
    }
}

#[test]
fn safety_override_flux2_tolerance() {
    for id in ["flux-2-pro", "flux-2-max", "flux-2-flex"] {
        let body = image_body("p", &img(id), &[], Some(AspectRatio::Square), None);
        assert_eq!(body.get("safety_tolerance"), Some(&json!(5)), "missing for {id}");
    }
}

#[test]
fn safety_override_gpt_moderation() {
    for id in ["gpt-5-image", "gpt-image-2"] {
        let body = image_body("p", &img(id), &[], Some(AspectRatio::Square), None);
        assert_eq!(s(&body, "moderation"), Some("low"), "missing for {id}");
    }
}

#[test]
fn safety_override_nano_banana_pro() {
    let body = image_body("p", &img("gemini-3-pro"), &[], Some(AspectRatio::Square), None);
    assert_eq!(s(&body, "safety_filter_level"), Some("block_only_high"));
}

#[test]
fn safety_override_absent_for_server_side_only_models() {
    for id in ["gemini-3.1-flash", "gemini-2.5-flash", "recraft-4-pro", "wan-2.7-pro"] {
        let body = image_body("p", &img(id), &[], Some(AspectRatio::Square), None);
        assert!(body.get("disable_safety_checker").is_none());
        assert!(body.get("safety_tolerance").is_none());
        assert!(body.get("moderation").is_none());
        assert!(body.get("safety_filter_level").is_none(), "set unexpectedly for {id}");
    }
}

#[test]
fn mask_rejected_for_every_model() {
    crate::rt().block_on(mask_rejected_for_every_model_inner());
}

async fn mask_rejected_for_every_model_inner() {
    let client = ReplicateClient::new("test-key");
    let err = client
        .generate_image(
            "p",
            &img("gpt-image-2"),
            &[
                ProviderAsset::new(AssetRole::ReferenceImage, "image/png", vec![0x01]),
                ProviderAsset::new(AssetRole::MaskImage, "image/png", vec![0x02]),
            ],
            Some(AspectRatio::Square),
            None,
        )
        .await
        .unwrap_err();
    match err {
        GenerationError::InvalidRequest(msg) => assert!(msg.contains("mask"), "{msg}"),
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

// ----- ReplicateError ---------------------------------------------------------------------------

#[test]
fn prediction_failed_maps_to_provider_failed() {
    let err = ReplicateError::PredictionFailed("model crashed".into()).into_generation_error();
    assert_eq!(err, GenerationError::ProviderFailed("model crashed".into()));
}

// ----- audio ------------------------------------------------------------------------------------

#[test]
fn eleven_labs_v3_capabilities_mirror_replicate_schema() {
    let caps = audio_capabilities(&aud("elevenlabs-v3")).expect("capabilities");
    assert!(!caps.supports_two_speakers);
    assert_eq!(caps.max_characters_monologue, 5000);
    assert_eq!(caps.max_characters_dialogue, 0);
    assert_eq!(caps.default_voice.as_ref().map(|v| v.id.as_str()), Some("Rachel"));
    assert!(caps.secondary_default_voice.is_none());
    assert_eq!(caps.supported_voices.len(), 26);
    assert!(caps.supported_voices.iter().any(|v| v.id == "Rachel"));
    assert!(caps.supported_voices.iter().any(|v| v.id == "Grimblewood"));
}

#[test]
fn unknown_audio_model_has_no_capabilities() {
    assert!(audio_capabilities(&aud("gemini-2.5-pro")).is_none());
}

#[test]
fn eleven_labs_v3_body_uses_prompt_and_voice() {
    let settings = AudioGenerationSettings { model: aud("elevenlabs-v3"), speaker1: AudioVoice::new("Rachel", "Rachel"), speaker2: None };
    let body = build_audio_request_body("Hello from Replicate audio.", &settings);
    assert_eq!(s(&body, "prompt"), Some("Hello from Replicate audio."));
    assert_eq!(s(&body, "voice"), Some("Rachel"));
    assert!(body.get("text").is_none());
    assert!(body.get("inputs").is_none());
}

// ----- image processing bodies ------------------------------------------------------------------

#[test]
fn upscale_body() {
    let body = build_upscale_request_body(&[0x01, 0x02], 2);
    assert!(s(&body, "image").unwrap().starts_with("data:image/png;base64,"));
    assert_eq!(body.get("scale_factor"), Some(&json!(2)));
    assert_eq!(s(&body, "output_format"), Some("png"));
}

#[test]
fn remove_background_body() {
    let body = build_remove_background_request_body(&[0x01, 0x02]);
    assert!(s(&body, "image").unwrap().starts_with("data:image/png;base64,"));
    assert_eq!(s(&body, "format"), Some("png"));
    assert_eq!(s(&body, "background_type"), Some("rgba"));
}

// ----- video body shape -------------------------------------------------------------------------

#[test]
fn veo31_t2v_body() {
    let settings = video_settings(vid("veo-3.1"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Hd), 8, true);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(s(&body, "aspect_ratio"), Some("16:9"));
    assert_eq!(s(&body, "resolution"), Some("720p"));
    assert_eq!(body.get("duration"), Some(&json!(8)));
    assert_eq!(body.get("generate_audio"), Some(&json!(true)));
    assert!(body.get("image").is_none());
    assert!(body.get("last_frame").is_none());
}

#[test]
fn veo31_i2v_with_last_frame() {
    let settings = video_settings(vid("veo-3.1"), Some(VideoAspectRatio::Tall), Some(VideoResolution::Hd), 8, true);
    let body = build_video_request_body("p", Some(&[0x01]), Some(&[0x02]), None, VideoEndpointVariant::I2v, &settings);
    assert_eq!(s(&body, "aspect_ratio"), Some("9:16"));
    assert!(s(&body, "image").unwrap().starts_with("data:"));
    assert!(s(&body, "last_frame").unwrap().starts_with("data:"));
}

/// `minimax/h3` is the one slug that doesn't call the field `aspect_ratio`.
#[test]
fn minimax_h3_sends_ratio_not_aspect_ratio() {
    let settings = video_settings(vid("minimax-h3"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 6, false);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(s(&body, "ratio"), Some("16:9"));
    assert!(body.get("aspect_ratio").is_none());
    assert_eq!(s(&body, "resolution"), Some("2K"));
    assert_eq!(body.get("duration"), Some(&json!(6)));

    let auto = video_settings(vid("minimax-h3"), Some(VideoAspectRatio::Auto), Some(VideoResolution::Hd), 5, false);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &auto);
    assert_eq!(s(&body, "ratio"), Some("adaptive"));
    assert_eq!(s(&body, "resolution"), Some("768P"));
}

/// The wan-3 slugs spell `.auto` `adaptive` and expose no audio toggle.
#[test]
fn wan_3_body() {
    let settings = video_settings(vid("wan-3.0"), Some(VideoAspectRatio::Auto), Some(VideoResolution::Sd), 12, true);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(s(&body, "aspect_ratio"), Some("adaptive"));
    assert_eq!(s(&body, "resolution"), Some("480p"));
    assert_eq!(body.get("duration"), Some(&json!(12)));
    assert!(body.get("generate_audio").is_none());
    assert!(body.get("audio").is_none());
}

/// Replicate's flux-3 takes a string duration and its opening frame in an `images` array.
#[test]
fn flux_3_body() {
    let settings = video_settings(vid("flux-3"), Some(VideoAspectRatio::Auto), Some(VideoResolution::Fhd), 20, true);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body.get("duration"), Some(&json!("20")));
    assert_eq!(s(&body, "resolution"), Some("1080p"));
    assert_eq!(body.get("generate_audio"), Some(&json!(true)));

    let body = build_video_request_body("p", Some(&[0x01]), None, None, VideoEndpointVariant::I2v, &settings);
    assert!(body.get("images").is_some());
    assert!(body.get("last_frame_image").is_none());
}

/// A reference of `role`, with the MIME its kind implies.
fn reference_of(role: AssetRole) -> ProviderAsset {
    match role {
        AssetRole::ReferenceVideo => ProviderAsset::new(role, "video/mp4", vec![0x00, 0x01]),
        AssetRole::Audio => ProviderAsset::new(role, "audio/mpeg", vec![0x09]),
        _ => ProviderAsset::new(role, "image/png", vec![0x89, 0x50]),
    }
}

/// Replicate's dialects differ from fal's for the very same model: Seedance wants `[Image1]` here
/// and `@Image1` there, Happy Horse `[Image 1]` here and `character1` there.
#[test]
fn reference_bodies_speak_each_slugs_dialect() {
    let assets = [reference_of(AssetRole::ReferenceImage), reference_of(AssetRole::ReferenceImage)];
    let references = ReferenceAssets::from_assets(&assets);
    for (model, expected, key) in [
        ("seedance-2.5", "[Image1] meets [Image2]", "reference_images"),
        ("happyhorse-1.1", "[Image 1] meets [Image 2]", "images"),
        ("veo-3.1", "Image 1 meets Image 2", "reference_images"),
        ("minimax-h3", "Image 1 meets Image 2", "reference_image_urls"),
    ] {
        let settings = video_settings(vid(model), None, Some(VideoResolution::Hd), 5, false);
        let body = build_video_reference_body("@Image1 meets @Image2", &references, &settings);
        assert_eq!(s(&body, "prompt"), Some(expected), "{model}");
        assert_eq!(body[key].as_array().unwrap().len(), 2, "{model}");
        assert!(body.get("image").is_none(), "{model}: no frame key on a reference request");
    }
}

/// H3 carries all three kinds, each in its own array, in attach order.
#[test]
fn h3_reference_body_carries_every_kind() {
    let assets = [
        reference_of(AssetRole::ReferenceImage),
        reference_of(AssetRole::ReferenceVideo),
        reference_of(AssetRole::Audio),
    ];
    let references = ReferenceAssets::from_assets(&assets);
    let settings = video_settings(vid("minimax-h3"), None, Some(VideoResolution::Hd), 5, false);
    let body = build_video_reference_body("@Image1 dances to @Audio1 like @Video1", &references, &settings);
    assert_eq!(s(&body, "prompt"), Some("Image 1 dances to Audio 1 like Video 1"));
    assert!(body["reference_image_urls"][0].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(body["reference_video_urls"][0].as_str().unwrap().starts_with("data:video/mp4;base64,"));
    assert!(body["reference_audio_urls"][0].as_str().unwrap().starts_with("data:audio/mpeg;base64,"));
}

/// The same agreement fal's tables have to keep: a declared count needs a request key, and a
/// request key needs a declared count.
#[test]
fn every_reference_model_agrees_with_its_request_keys() {
    for id in SUPPORTED_VIDEO_MODEL_IDS {
        let model = vid(id);
        let declared = video_capabilities(&model).unwrap().references;
        let params = video_reference_params(&model);
        assert_eq!(declared.is_some(), params.is_some(), "{id}: capabilities and request keys disagree");
        let (Some(declared), Some(params)) = (declared, params) else { continue };
        assert!(declared.images > 0, "{id}");
        for role in [AssetRole::ReferenceImage, AssetRole::ReferenceVideo, AssetRole::Audio] {
            assert_eq!(declared.max_for(role) > 0, params.param_for(role).is_some(), "{id}: {role:?}");
        }
    }
}

#[test]
fn the_reference_variant_has_no_frame_params() {
    for id in SUPPORTED_VIDEO_MODEL_IDS {
        let model = vid(id);
        assert_eq!(api_start_frame_param(&model, VideoEndpointVariant::Reference), None, "{id}");
        assert_eq!(api_end_frame_param(&model, VideoEndpointVariant::Reference), None, "{id}");
    }
}

/// Seedance 2.5 stops at 720p on Replicate, so the key is dropped rather than sent as 1080p.
#[test]
fn seedance_25_drops_unsupported_resolution() {
    let settings = video_settings(vid("seedance-2.5"), None, Some(VideoResolution::Fhd), 30, true);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert!(body.get("resolution").is_none());
    assert_eq!(body.get("duration"), Some(&json!(30)));
    assert_eq!(body.get("generate_audio"), Some(&json!(true)));
}

#[test]
fn sora2_body() {
    let settings = video_settings(vid("sora-2"), Some(VideoAspectRatio::Tall), Some(VideoResolution::Hd), 8, true);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(s(&body, "aspect_ratio"), Some("portrait"));
    assert!(body.get("duration").is_none());
    assert!(body.get("resolution").is_none());
    assert!(body.get("image").is_none());
    assert!(body.get("generate_audio").is_none());
}

#[test]
fn sora2_pro_resolution_mapping() {
    let hd = video_settings(vid("sora-2-pro"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Hd), 8, true);
    let body_hd = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &hd);
    assert_eq!(s(&body_hd, "resolution"), Some("standard"));

    let fhd = video_settings(vid("sora-2-pro"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 8, true);
    let body_fhd = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &fhd);
    assert_eq!(s(&body_fhd, "resolution"), Some("high"));
}

#[test]
fn kling25_turbo_pro_i2v() {
    let settings = video_settings(vid("kling-2.5-turbo-pro"), Some(VideoAspectRatio::Square), Some(VideoResolution::Hd), 10, true);
    let body = build_video_request_body("p", Some(&[0x01]), Some(&[0x02]), None, VideoEndpointVariant::I2v, &settings);
    assert_eq!(s(&body, "aspect_ratio"), Some("1:1"));
    assert!(s(&body, "image").unwrap().starts_with("data:"));
    assert!(s(&body, "end_image").unwrap().starts_with("data:"));
    assert!(body.get("start_image").is_none());
    assert!(body.get("generate_audio").is_none(), "kling 2.5 turbo has no audio toggle");
    assert_eq!(body.get("duration"), Some(&json!(10)));
}

#[test]
fn pixverse_v6_quality_and_audio_switch() {
    let settings = video_settings(vid("pixverse-6"), Some(VideoAspectRatio::Tall), Some(VideoResolution::Fhd), 8, false);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert!(body.get("resolution").is_none(), "pixverse uses `quality`, not `resolution`");
    assert_eq!(s(&body, "quality"), Some("1080p"));
    assert_eq!(body.get("generate_audio_switch"), Some(&json!(false)));
    assert!(body.get("generate_audio").is_none());
}

#[test]
fn wan27_i2v_omits_aspect_ratio() {
    let settings = video_settings(vid("wan-2.7"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 8, true);
    let body = build_video_request_body("p", Some(&[0x01]), Some(&[0x02]), None, VideoEndpointVariant::I2v, &settings);
    assert!(body.get("aspect_ratio").is_none(), "wan-2.7-i2v has no aspect_ratio field");
    assert_eq!(s(&body, "resolution"), Some("1080p"));
    assert!(s(&body, "first_frame").unwrap().starts_with("data:"));
    assert!(s(&body, "last_frame").unwrap().starts_with("data:"));
}

#[test]
fn wan27_includes_audio_input() {
    let audio = ProviderAsset::new(AssetRole::Audio, "public.mp3", vec![0x01, 0x02]);
    let settings = video_settings(vid("wan-2.7"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 8, true);
    let body = build_video_request_body("p", Some(&[0x01]), None, Some(&audio), VideoEndpointVariant::I2v, &settings);
    assert!(s(&body, "audio").unwrap().starts_with("data:audio/mpeg;base64,"));
    assert!(s(&body, "first_frame").unwrap().starts_with("data:"));
}

#[test]
fn happy_horse_t2v_and_i2v() {
    let t2v_settings = video_settings(vid("happyhorse-1.0"), Some(VideoAspectRatio::Portrait), Some(VideoResolution::Hd), 5, true);
    let t2v = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &t2v_settings);
    assert_eq!(s(&t2v, "aspect_ratio"), Some("3:4"));
    assert_eq!(s(&t2v, "resolution"), Some("720p"));
    assert_eq!(t2v.get("duration"), Some(&json!(5)));
    assert!(t2v.get("image").is_none());
    assert!(t2v.get("generate_audio").is_none());

    let i2v_settings = video_settings(vid("happyhorse-1.0"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 6, true);
    let i2v = build_video_request_body("p", Some(&[0x01]), Some(&[0x02]), None, VideoEndpointVariant::I2v, &i2v_settings);
    assert!(i2v.get("aspect_ratio").is_none());
    assert_eq!(s(&i2v, "resolution"), Some("1080p"));
    assert_eq!(i2v.get("duration"), Some(&json!(6)));
    assert!(s(&i2v, "image").unwrap().starts_with("data:"));
    assert!(i2v.get("last_frame").is_none());
    assert!(i2v.get("last_frame_image").is_none());
}

#[test]
fn seedance20_auto_maps_to_adaptive() {
    let settings = video_settings(vid("seedance-2"), Some(VideoAspectRatio::Auto), Some(VideoResolution::Hd), 8, true);
    let body = build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(s(&body, "aspect_ratio"), Some("adaptive"));
    assert_eq!(s(&body, "resolution"), Some("720p"));
}

// ----- endpoint resolution ----------------------------------------------------------------------

#[test]
fn veo31_endpoint_resolution() {
    let (t2v, v_t2v) = resolve_video_endpoint(&vid("veo-3.1"), false, false).unwrap();
    assert_eq!(t2v, "google/veo-3.1");
    assert_eq!(v_t2v, VideoEndpointVariant::T2v);

    let (i2v, v_i2v) = resolve_video_endpoint(&vid("veo-3.1"), true, false).unwrap();
    assert_eq!(i2v, "google/veo-3.1");
    assert_eq!(v_i2v, VideoEndpointVariant::I2v);
}

#[test]
fn wan27_endpoint_split() {
    let (t2v, _) = resolve_video_endpoint(&vid("wan-2.7"), false, false).unwrap();
    assert_eq!(t2v, "wan-video/wan-2.7-t2v");
    let (i2v, _) = resolve_video_endpoint(&vid("wan-2.7"), true, false).unwrap();
    assert_eq!(i2v, "wan-video/wan-2.7-i2v");
}

#[test]
fn happy_horse_endpoint_same_slug() {
    let (t2v, t2v_variant) = resolve_video_endpoint(&vid("happyhorse-1.0"), false, false).unwrap();
    assert_eq!(t2v, "alibaba/happyhorse-1.0");
    assert_eq!(t2v_variant, VideoEndpointVariant::T2v);
    let (i2v, i2v_variant) = resolve_video_endpoint(&vid("happyhorse-1.0"), true, false).unwrap();
    assert_eq!(i2v, "alibaba/happyhorse-1.0");
    assert_eq!(i2v_variant, VideoEndpointVariant::I2v);
}

#[test]
fn sora2_with_first_frame_is_unsupported() {
    let err = resolve_video_endpoint(&vid("sora-2"), true, false).unwrap_err();
    assert!(matches!(err, ReplicateError::UnsupportedModel(_)), "{err:?}");
}

#[test]
fn kling3_standard_excluded() {
    assert!(!SUPPORTED_VIDEO_MODEL_IDS.contains(&"kling-3-standard"));
    assert!(video_capabilities(&vid("kling-3-standard")).is_none());
}

#[test]
fn kling3_standard_excluded_from_descriptor() {
    let descriptor = replicate::descriptor();
    let kling3_standard = majik_providers::catalog::video::model("kling-3-standard").expect("catalog has kling-3-standard");
    assert!(!descriptor.supports_video_model(kling3_standard));
    assert!(descriptor.video_capabilities(kling3_standard).is_none());
    assert_eq!(descriptor.supported_video_models.len(), SUPPORTED_VIDEO_MODEL_IDS.len());
    assert_eq!(descriptor.supported_image_models.len(), 18);
    assert_eq!(descriptor.supported_audio_models.len(), 1);
}

#[test]
fn happy_horse_capabilities() {
    let caps = video_capabilities(&vid("happyhorse-1.0")).expect("capabilities");
    assert_eq!(caps.duration_range, VideoDurationRange::new(3, 15, None));
    assert_eq!(
        caps.aspect_ratios,
        vec![VideoAspectRatio::Landscape, VideoAspectRatio::Tall, VideoAspectRatio::Square, VideoAspectRatio::Standard, VideoAspectRatio::Portrait]
    );
    assert_eq!(caps.resolutions, vec![VideoResolution::Hd, VideoResolution::Fhd]);
    assert_eq!(caps.max_input_images, 1);
    assert_eq!(caps.asset_constraints.allowed, BTreeMap::from([(AssetRole::FirstFrame, 0..=1)]));
    assert!(caps.prompt_optional);
    assert!(caps.supports_audio);
    assert!(!caps.supports_audio_toggle());
}

// ----- HTTP flow (wiremock) ---------------------------------------------------------------------

#[test]
fn generate_image_submits_to_official_slug_and_downloads_png() {
    crate::rt().block_on(generate_image_submits_to_official_slug_and_downloads_png_inner());
}

async fn generate_image_submits_to_official_slug_and_downloads_png_inner() {
    let server = MockServer::start().await;
    let png = png_bytes();

    Mock::given(method("POST"))
        .and(path("/v1/models/black-forest-labs/flux-schnell/predictions"))
        .and(header("Authorization", "Token test-key"))
        .and(header("Prefer", "wait=60"))
        .and(header("Content-Type", "application/json"))
        .and(body_partial_json(json!({ "input": { "prompt": "a cat", "aspect_ratio": "1:1", "megapixels": "1", "output_format": "png", "disable_safety_checker": true } })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "pred-1",
            "status": "succeeded",
            "output": [format!("{}/out.png", server.uri())]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/out.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone())).mount(&server).await;

    let bytes = client(&server)
        .generate_image("a cat", &img("flux-1-schnell"), &[], Some(AspectRatio::Square), Some(ImageResolution::Hd))
        .await
        .unwrap();
    assert_eq!(bytes, png);
}

#[test]
fn generate_image_transcodes_non_png_output() {
    crate::rt().block_on(generate_image_transcodes_non_png_output_inner());
}

async fn generate_image_transcodes_non_png_output_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/bytedance/seedream-4.5/predictions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "pred-1",
            "status": "succeeded",
            "output": format!("{}/out.jpg", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/out.jpg")).respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg_bytes())).mount(&server).await;

    let bytes = client(&server).generate_image("p", &img("seedream-4.5"), &[], Some(AspectRatio::Square), None).await.unwrap();
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "output should be transcoded to PNG");
}

#[test]
fn generate_image_decodes_data_uri_output() {
    crate::rt().block_on(generate_image_decodes_data_uri_output_inner());
}

async fn generate_image_decodes_data_uri_output_inner() {
    let server = MockServer::start().await;
    let png = png_bytes();
    Mock::given(method("POST"))
        .and(path("/v1/models/openai/gpt-image-2/predictions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "pred-1",
            "status": "succeeded",
            "output": majik_providers::data_uri::to_data_uri(&png, "image/png")
        })))
        .mount(&server)
        .await;

    let bytes = client(&server).generate_image("p", &img("gpt-image-2"), &[], Some(AspectRatio::Square), None).await.unwrap();
    assert_eq!(bytes, png);
}

#[test]
fn generate_image_polls_until_succeeded() {
    crate::rt().block_on(generate_image_polls_until_succeeded_inner());
}

async fn generate_image_polls_until_succeeded_inner() {
    let server = MockServer::start().await;
    let png = png_bytes();
    Mock::given(method("POST"))
        .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "pred-7",
            "status": "starting",
            "urls": { "get": format!("{}/v1/predictions/pred-7", server.uri()) }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/predictions/pred-7"))
        .and(header("Authorization", "Token test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "pred-7",
            "status": "succeeded",
            "output": [format!("{}/out.png", server.uri())]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/out.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone())).mount(&server).await;

    let bytes = client(&server).generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap();
    assert_eq!(bytes, png);
}

#[test]
fn generate_image_polls_canonical_endpoint_when_urls_missing() {
    crate::rt().block_on(generate_image_polls_canonical_endpoint_when_urls_missing_inner());
}

async fn generate_image_polls_canonical_endpoint_when_urls_missing_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "pred-8", "status": "processing" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/predictions/pred-8"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "pred-8", "status": "failed", "error": "CUDA out of memory" })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server).generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::ProviderFailed("CUDA out of memory".into()));
}

#[test]
fn failed_prediction_with_safety_message_is_content_filtered() {
    crate::rt().block_on(failed_prediction_with_safety_message_is_content_filtered_inner());
}

async fn failed_prediction_with_safety_message_is_content_filtered_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "p", "status": "failed", "error": "NSFW content detected" })))
        .mount(&server)
        .await;

    let err = client(&server).generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::ContentFiltered("NSFW content detected".into()));
}

#[test]
fn canceled_prediction_maps_to_unknown() {
    crate::rt().block_on(canceled_prediction_maps_to_unknown_inner());
}

async fn canceled_prediction_maps_to_unknown_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "p", "status": "canceled" })))
        .mount(&server)
        .await;

    let err = client(&server).generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::Unknown("Prediction canceled".into()));
}

#[test]
fn succeeded_without_output_is_no_result() {
    crate::rt().block_on(succeeded_without_output_is_no_result_inner());
}

async fn succeeded_without_output_is_no_result_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "p", "status": "succeeded", "output": null })))
        .mount(&server)
        .await;

    let err = client(&server).generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::NoResultGenerated);
}

#[test]
fn http_status_errors_map_to_generation_errors() {
    crate::rt().block_on(http_status_errors_map_to_generation_errors_inner());
}

async fn http_status_errors_map_to_generation_errors_inner() {
    let cases: Vec<(u16, Value, GenerationError)> = vec![
        (401, json!({ "detail": "bad token" }), GenerationError::Unauthorized("bad token".into())),
        (403, json!({ "detail": "forbidden" }), GenerationError::Unauthorized("forbidden".into())),
        (402, json!({ "detail": "no credits" }), GenerationError::PaymentRequired("no credits".into())),
        (429, json!({ "detail": "slow down" }), GenerationError::RateLimited("slow down".into())),
        (422, json!({ "detail": "bad input" }), GenerationError::InvalidRequest("bad input".into())),
        (503, json!({ "title": "unavailable" }), GenerationError::server(Some(503), "unavailable")),
        (418, json!({}), GenerationError::Unknown("HTTP 418: Unknown error".into())),
    ];
    for (status, body, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let err = client(&server).generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap_err();
        assert_eq!(err, expected, "status {status}");
    }
}

#[test]
fn unsupported_image_model_is_invalid_request() {
    crate::rt().block_on(unsupported_image_model_is_invalid_request_inner());
}

async fn unsupported_image_model_is_invalid_request_inner() {
    let err = ReplicateClient::new("k").generate_image("p", &img("riverflow-2-max"), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("Model 'riverflow-2-max' is not supported by Replicate".into()));
}

#[test]
fn upscale_posts_pinned_version_to_predictions() {
    crate::rt().block_on(upscale_posts_pinned_version_to_predictions_inner());
}

async fn upscale_posts_pinned_version_to_predictions_inner() {
    let server = MockServer::start().await;
    let png = png_bytes();
    Mock::given(method("POST"))
        .and(path("/v1/predictions"))
        .and(header("Prefer", "wait=60"))
        .and(body_partial_json(json!({
            "version": majik_providers::constants::replicate::UPSCALE_VERSION,
            "input": { "scale_factor": 4, "output_format": "png" }
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "up-1", "status": "succeeded", "output": [format!("{}/up.png", server.uri())]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/up.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone())).mount(&server).await;

    let settings = ToolSettings::new(catalog::tool::CLARITY_UPSCALER.clone()).with_factor(4);
    let bytes = run_tool(&server, &settings).await;
    assert_eq!(bytes, png);
}

#[test]
fn remove_background_posts_pinned_version_to_predictions() {
    crate::rt().block_on(remove_background_posts_pinned_version_to_predictions_inner());
}

async fn remove_background_posts_pinned_version_to_predictions_inner() {
    let server = MockServer::start().await;
    let png = png_bytes();
    Mock::given(method("POST"))
        .and(path("/v1/predictions"))
        .and(body_partial_json(json!({
            "version": majik_providers::constants::replicate::REMOVE_BACKGROUND_VERSION,
            "input": { "format": "png", "background_type": "rgba" }
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "bg-1", "status": "succeeded", "output": format!("{}/bg.png", server.uri())
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/bg.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone())).mount(&server).await;

    let bytes = run_tool(&server, &ToolSettings::new(catalog::tool::REMBG.clone())).await;
    assert_eq!(bytes, png);
}

/// One tool run against the mock server, over two bytes standing in for an image.
async fn run_tool(server: &MockServer, settings: &ToolSettings) -> Vec<u8> {
    let input = ProviderAsset::new(AssetRole::ReferenceImage, "image/png", vec![0x01, 0x02]);
    client(server).run_tool(settings, &input).await.unwrap()
}

#[test]
fn generate_video_i2v_uses_i2v_slug_and_downloads() {
    crate::rt().block_on(generate_video_i2v_uses_i2v_slug_and_downloads_inner());
}

async fn generate_video_i2v_uses_i2v_slug_and_downloads_inner() {
    let server = MockServer::start().await;
    let video = b"\x00\x00\x00\x18ftypmp42fake".to_vec();
    Mock::given(method("POST"))
        .and(path("/v1/models/wan-video/wan-2.7-i2v/predictions"))
        .and(header("Authorization", "Token test-key"))
        .and(body_partial_json(json!({ "input": { "prompt": "p", "resolution": "1080p", "duration": 5 } })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "v-1", "status": "succeeded", "output": format!("{}/out.mp4", server.uri())
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/out.mp4")).respond_with(ResponseTemplate::new(200).set_body_bytes(video.clone())).mount(&server).await;

    let settings = video_settings(vid("wan-2.7"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 5, true);
    let assets = [ProviderAsset::new(AssetRole::FirstFrame, "image/png", vec![0x01])];
    let bytes = client(&server).generate_video("p", &assets, &settings).await.unwrap();
    assert_eq!(bytes, video);

    let requests: Vec<Request> = server.received_requests().await.unwrap();
    let submit = requests.iter().find(|r| r.url.path().ends_with("/predictions")).unwrap();
    let body: Value = serde_json::from_slice(&submit.body).unwrap();
    assert!(body["input"].get("aspect_ratio").is_none(), "wan-2.7-i2v has no aspect_ratio");
    assert!(body["input"]["first_frame"].as_str().unwrap().starts_with("data:image/png;base64,"));
}

#[test]
fn generate_video_rejects_unsupported_roles_and_orphan_last_frame() {
    crate::rt().block_on(generate_video_rejects_unsupported_roles_and_orphan_last_frame_inner());
}

async fn generate_video_rejects_unsupported_roles_and_orphan_last_frame_inner() {
    let client = ReplicateClient::new("k");
    let settings = video_settings(vid("veo-3.1"), Some(VideoAspectRatio::Landscape), Some(VideoResolution::Hd), 8, true);

    // Veo takes references, Sora doesn't, and neither takes one beside a frame.
    let reference = || ProviderAsset::new(AssetRole::ReferenceImage, "image/png", vec![1]);
    let sora = video_settings(vid("sora-2"), None, None, 8, true);
    let err = client.generate_video("p", &[reference()], &sora).await.unwrap_err();
    assert!(matches!(err, GenerationError::InvalidRequest(ref m) if m.ends_with("does not take reference inputs")), "{err:?}");

    let with_frame = vec![reference(), ProviderAsset::new(AssetRole::FirstFrame, "image/png", vec![1])];
    let err = client.generate_video("p", &with_frame, &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("References and a start or end frame can't be used together".into()));

    let err = client.generate_video("p", &[ProviderAsset::new(AssetRole::LastFrame, "image/png", vec![1])], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("A last frame requires a first frame".into()));

    let err = client.generate_video("p", &[ProviderAsset::new(AssetRole::Audio, "audio/mpeg", vec![1])], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("This model does not accept an audio input".into()));
}

#[test]
fn generate_video_empty_download_is_no_result() {
    crate::rt().block_on(generate_video_empty_download_is_no_result_inner());
}

async fn generate_video_empty_download_is_no_result_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/openai/sora-2/predictions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "v-2", "status": "succeeded", "output": format!("{}/empty.mp4", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/empty.mp4")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let settings = video_settings(vid("sora-2"), Some(VideoAspectRatio::Landscape), None, 8, true);
    let err = client(&server).generate_video("p", &[], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::NoResultGenerated);
}

#[test]
fn generate_audio_uses_elevenlabs_v3_slug() {
    crate::rt().block_on(generate_audio_uses_elevenlabs_v3_slug_inner());
}

async fn generate_audio_uses_elevenlabs_v3_slug_inner() {
    let server = MockServer::start().await;
    let mp3 = b"ID3fake-mp3".to_vec();
    Mock::given(method("POST"))
        .and(path("/v1/models/elevenlabs/v3/predictions"))
        .and(header("Authorization", "Token test-key"))
        .and(header("Prefer", "wait=60"))
        .and(body_partial_json(json!({ "input": { "prompt": "Hello", "voice": "Rachel" } })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "a-1", "status": "succeeded", "output": format!("{}/out.mp3", server.uri())
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/out.mp3")).respond_with(ResponseTemplate::new(200).set_body_bytes(mp3.clone())).mount(&server).await;

    let settings = AudioGenerationSettings { model: aud("elevenlabs-v3"), speaker1: AudioVoice::new("Rachel", "Rachel"), speaker2: None };
    let bytes = client(&server).generate_audio("Hello", &settings).await.unwrap();
    assert_eq!(bytes, mp3);
}

#[test]
fn generate_audio_rejects_other_models() {
    crate::rt().block_on(generate_audio_rejects_other_models_inner());
}

async fn generate_audio_rejects_other_models_inner() {
    let settings = AudioGenerationSettings { model: aud("gemini-2.5-pro"), speaker1: AudioVoice::new("Kore", "Kore"), speaker2: None };
    let err = ReplicateClient::new("k").generate_audio("Hello", &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("Model 'gemini-2.5-pro' is not supported by Replicate".into()));
}

// ----- Prediction handles and resume (relaunch recovery) -----------------------------------------

mod resume {
    use super::*;
    use majik_core::model::MediaType;
    use majik_providers::{JobHandle, ResumableClient as _};
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::any;

    #[test]
    fn submit_reports_the_prediction_handle_even_when_it_finishes_inline() {
        crate::rt().block_on(submit_reports_the_prediction_handle_even_when_it_finishes_inline_inner());
    }

    async fn submit_reports_the_prediction_handle_even_when_it_finishes_inline_inner() {
        let server = MockServer::start().await;
        let png = png_bytes();
        Mock::given(method("POST"))
            .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "pred-1",
                "status": "succeeded",
                "output": majik_providers::data_uri::to_data_uri(&png, "image/png")
            })))
            .mount(&server)
            .await;
        let seen: Arc<Mutex<Vec<JobHandle>>> = Default::default();
        let sink = seen.clone();
        let client = client(&server).with_on_accepted(Arc::new(move |handle| sink.lock().unwrap().push(handle)));
        client.generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![JobHandle { job_id: "pred-1".into(), poll_url: Some(format!("{}/v1/predictions/pred-1", server.uri())) }],
            "no urls.get in the answer: the canonical endpoint is the poll URL"
        );
    }

    #[test]
    fn prediction_run_traces_submit_poll_and_download_without_the_key() {
        crate::rt().block_on(prediction_run_traces_submit_poll_and_download_without_the_key_inner());
    }

    async fn prediction_run_traces_submit_poll_and_download_without_the_key_inner() {
        let server = MockServer::start().await;
        let png = png_bytes();
        Mock::given(method("POST"))
            .and(path("/v1/models/black-forest-labs/flux-dev/predictions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "pred-1", "status": "succeeded", "output": majik_providers::data_uri::to_data_uri(&png, "image/png") })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/predictions/pred-5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "pred-5", "status": "succeeded", "output": [format!("{}/out.png", server.uri())] })))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone())).mount(&server).await;

        let seen: Arc<Mutex<Vec<majik_core::model::JobTrace>>> = Default::default();
        let sink = seen.clone();
        let client = client(&server).with_on_trace(Arc::new(move |trace| sink.lock().unwrap().push(trace)));
        use majik_core::model::TraceLabel::*;

        client.generate_image("p", &img("flux-1-dev"), &[], None, None).await.unwrap();
        let traces = seen.lock().unwrap().clone();
        assert_eq!(traces.iter().map(|t| t.label).collect::<Vec<_>>(), [Submit], "finished inline under Prefer: wait");
        assert_eq!((traces[0].method.as_str(), traces[0].status), ("POST", Some(201)));
        assert!(traces[0].request_body.as_deref().unwrap_or_default().contains(r#""input""#), "{:?}", traces[0].request_body);
        assert!(traces[0].response_body.as_deref().unwrap_or_default().contains("data:image/png;base64,…["), "data URIs are elided: {:?}", traces[0].response_body);
        seen.lock().unwrap().clear();

        let handle = JobHandle { job_id: "pred-5".into(), poll_url: Some(format!("{}/v1/predictions/pred-5", server.uri())) };
        client.resume(&handle, MediaType::Image).await.unwrap();
        let traces = seen.lock().unwrap().clone();
        assert_eq!(traces.iter().map(|t| t.label).collect::<Vec<_>>(), [Poll, Download]);
        assert!(traces[0].url.ends_with("/v1/predictions/pred-5"));
        assert!(traces[0].response_body.as_deref().unwrap_or_default().contains("succeeded"));
        assert_eq!(traces[1].response_body.as_deref(), Some(format!("{} bytes", png.len()).as_str()));
        for trace in &traces {
            let text = format!("{trace:?}");
            assert!(!text.contains("test-key") && !text.contains("Token"), "no header in the trail: {text}");
        }
    }

    #[test]
    fn resume_reads_a_finished_prediction_without_waiting() {
        crate::rt().block_on(resume_reads_a_finished_prediction_without_waiting_inner());
    }

    async fn resume_reads_a_finished_prediction_without_waiting_inner() {
        let server = MockServer::start().await;
        let png = png_bytes();
        Mock::given(method("GET"))
            .and(path("/v1/predictions/pred-5"))
            .and(header("Authorization", "Token test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "pred-5", "status": "succeeded", "output": [format!("{}/out.png", server.uri())] })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone())).mount(&server).await;
        let handle = JobHandle { job_id: "pred-5".into(), poll_url: Some(format!("{}/v1/predictions/pred-5", server.uri())) };
        let started = std::time::Instant::now();
        let bytes = client(&server).resume(&handle, MediaType::Image).await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "a finished prediction is read on the first GET, no backoff first");
        assert_eq!(image::load_from_memory(&bytes).unwrap().into_rgb8().get_pixel(0, 0).0, [10, 20, 30]);
    }

    #[test]
    fn resume_polls_a_running_prediction_until_it_succeeds() {
        crate::rt().block_on(resume_polls_a_running_prediction_until_it_succeeds_inner());
    }

    async fn resume_polls_a_running_prediction_until_it_succeeds_inner() {
        let server = MockServer::start().await;
        let clip = b"mp4 bytes".to_vec();
        Mock::given(method("GET"))
            .and(path("/v1/predictions/pred-6"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "pred-6", "status": "processing" })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/predictions/pred-6"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "pred-6", "status": "succeeded", "output": format!("{}/clip.mp4", server.uri()) })))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/clip.mp4")).respond_with(ResponseTemplate::new(200).set_body_bytes(clip.clone())).mount(&server).await;
        // No poll URL stored: the canonical endpoint is used.
        let handle = JobHandle { job_id: "pred-6".into(), poll_url: None };
        assert_eq!(client(&server).resume(&handle, MediaType::Video).await.unwrap(), clip);
    }

    #[test]
    fn resume_of_an_unknown_prediction_is_job_gone() {
        crate::rt().block_on(resume_of_an_unknown_prediction_is_job_gone_inner());
    }

    async fn resume_of_an_unknown_prediction_is_job_gone_inner() {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(ResponseTemplate::new(404).set_body_json(json!({ "detail": "Not found" }))).mount(&server).await;
        let handle = JobHandle { job_id: "pred-7".into(), poll_url: None };
        assert_eq!(client(&server).resume(&handle, MediaType::Audio).await.unwrap_err(), GenerationError::JobGone);
    }
}

// ----- prompt improvement (a text prediction) ---------------------------------------------------

#[test]
fn text_prediction_sends_the_prompts_and_concatenates_the_output_chunks() {
    crate::rt().block_on(text_prediction_sends_the_prompts_and_concatenates_the_output_chunks_inner());
}

async fn text_prediction_sends_the_prompts_and_concatenates_the_output_chunks_inner() {
    use majik_providers::TextProviderClient as _;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/anthropic/claude-sonnet-5/predictions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "p1",
            "status": "succeeded",
            "output": ["a red apple", " on a table"]
        })))
        .mount(&server)
        .await;

    let text = client(&server).complete_text("rewrite it", "apple", 400).await.unwrap();
    assert_eq!(text, "a red apple on a table", "streamed chunks are one answer");

    let request = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["input"]["prompt"], "apple");
    assert_eq!(body["input"]["system_prompt"], "rewrite it");
    assert!(body["input"].get("reasoning_effort").is_none(), "the rewriter's model takes no GPT-5 knobs: {body}");
    assert_eq!(body["input"]["effort"], "low", "thinking off: {body}");
    assert_eq!(body["input"]["max_tokens"], 400);
}

#[test]
fn an_empty_text_prediction_is_no_result_generated() {
    crate::rt().block_on(an_empty_text_prediction_is_no_result_generated_inner());
}

async fn an_empty_text_prediction_is_no_result_generated_inner() {
    use majik_providers::TextProviderClient as _;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": "p1", "status": "succeeded", "output": ["   "] })))
        .mount(&server)
        .await;
    assert!(matches!(client(&server).complete_text("s", "u", 100).await, Err(GenerationError::NoResultGenerated)));
}

#[test]
fn a_failed_text_prediction_surfaces_the_provider_error() {
    crate::rt().block_on(a_failed_text_prediction_surfaces_the_provider_error_inner());
}

async fn a_failed_text_prediction_surfaces_the_provider_error_inner() {
    use majik_providers::TextProviderClient as _;
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    assert!(matches!(client(&server).complete_text("s", "u", 100).await, Err(GenerationError::Unauthorized(_))));
}
