//! fal: capability tables, image masks, audio request bodies, plus wiremock-backed HTTP shape
//! tests for the queue / sync paths.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use majik_core::model::{MediaType, ToolId};
use majik_providers::catalog;
use majik_providers::ClientOptions;
use majik_providers::fal::capabilities::{self as caps, ids};
use majik_providers::fal::models::*;
use majik_providers::fal::{
    audio_routing, build_audio_request_body, handle_http_error, normalize_speaker_prefixes, AudioRouting, FalClient, FalError, VideoEndpointVariant,
};
use majik_providers::ReferenceAssets;
use majik_providers::{
    AspectRatio, AssetRole, AudioGenerationSettings, AudioModel, AudioProviderClient, AudioVoice, GenerationError, ImageModel, ImageProviderClient,
    ImageResolution, JobHandle, ProviderAsset, ProviderClient, ProviderId, ProviderRegistry, ToolProviderClient, ToolSettings, VideoAspectRatio,
    VideoDurationRange, VideoGenerationSettings, VideoModel, VideoProviderClient, VideoResolution,
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

// ----- helpers ------------------------------------------------------------------------------------

fn img(id: &'static str) -> ImageModel {
    catalog::image::model(id).cloned().unwrap_or(ImageModel::new(id, "x", "x", "x", "x"))
}

fn vid(id: &'static str) -> VideoModel {
    catalog::video::model(id).cloned().unwrap_or(VideoModel::new(id, "x", "x", "x", "x"))
}

fn aud(id: &'static str) -> AudioModel {
    catalog::audio::model(id).cloned().unwrap_or(AudioModel::new(id, "x", "x", "x", "x"))
}

fn client() -> FalClient {
    FalClient::new("test-key")
}

fn mock_client(server: &MockServer) -> FalClient {
    FalClient::new("test-key").with_base_urls(server.uri(), server.uri()).with_rest_base_url(server.uri())
}

/// One tool run against the mock server.
async fn run_tool(server: &MockServer, settings: &ToolSettings, role: AssetRole, content_type: &str, data: &[u8]) -> Result<Vec<u8>, GenerationError> {
    let input = ProviderAsset::new(role, content_type, data.to_vec());
    mock_client(server).run_tool(settings, &input).await
}

async fn upscale_image(server: &MockServer, settings: &ToolSettings, data: &[u8]) -> Result<Vec<u8>, GenerationError> {
    run_tool(server, settings, AssetRole::ReferenceImage, "image/png", data).await
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn json_bytes(v: Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}

const FRAME: &[u8] = &[0x89, 0x50, 0x4E, 0x47];

fn video_settings(model: &'static str, aspect: Option<VideoAspectRatio>, res: Option<VideoResolution>, duration: u32, audio: bool) -> VideoGenerationSettings {
    VideoGenerationSettings { model: vid(model), aspect_ratio: aspect, resolution: res, duration, audio_enabled: audio }
}

fn voice(id: &str) -> AudioVoice {
    AudioVoice::new(id, id)
}

fn tiny_jpeg() -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([200, 30, 30])));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

// ----- extract_image_data -------------------------------------------------------------------------

#[test]
fn extract_valid_png_data_uri_returns_bytes_verbatim() {
    crate::rt().block_on(extract_valid_png_data_uri_returns_bytes_verbatim_inner());
}

async fn extract_valid_png_data_uri_returns_bytes_verbatim_inner() {
    // content_type=image/png skips the transcode step, so any bytes round-trip.
    let original = vec![0xFF, 0xD8, 0xFF];
    let response = json_bytes(json!({
        "images": [{ "url": format!("data:image/png;base64,{}", b64(&original)), "content_type": "image/png" }]
    }));
    let result = client().extract_image_data(&response).await.unwrap();
    assert_eq!(result, original);
}

#[test]
fn extract_empty_images_is_no_image_generated() {
    crate::rt().block_on(extract_empty_images_is_no_image_generated_inner());
}

async fn extract_empty_images_is_no_image_generated_inner() {
    let response = json_bytes(json!({ "images": [] }));
    assert_eq!(client().extract_image_data(&response).await.unwrap_err(), FalError::NoImageGenerated);
}

#[test]
fn extract_missing_images_is_no_image_generated() {
    crate::rt().block_on(extract_missing_images_is_no_image_generated_inner());
}

async fn extract_missing_images_is_no_image_generated_inner() {
    let response = json_bytes(json!({ "seed": 12345 }));
    assert_eq!(client().extract_image_data(&response).await.unwrap_err(), FalError::NoImageGenerated);
}

#[test]
fn extract_data_uri_missing_comma_is_invalid_image_data() {
    crate::rt().block_on(extract_data_uri_missing_comma_is_invalid_image_data_inner());
}

async fn extract_data_uri_missing_comma_is_invalid_image_data_inner() {
    let response = json_bytes(json!({ "images": [{ "url": "data:image/pngbase64AAAA" }] }));
    assert_eq!(client().extract_image_data(&response).await.unwrap_err(), FalError::InvalidImageData);
}

#[test]
fn extract_invalid_base64_is_invalid_image_data() {
    crate::rt().block_on(extract_invalid_base64_is_invalid_image_data_inner());
}

async fn extract_invalid_base64_is_invalid_image_data_inner() {
    let response = json_bytes(json!({ "images": [{ "url": "data:image/png;base64,!!!not-base64!!!" }] }));
    assert_eq!(client().extract_image_data(&response).await.unwrap_err(), FalError::InvalidImageData);
}

#[test]
fn extract_malformed_json_is_decoding_error() {
    crate::rt().block_on(extract_malformed_json_is_decoding_error_inner());
}

async fn extract_malformed_json_is_decoding_error_inner() {
    let err = client().extract_image_data(b"not json").await.unwrap_err();
    assert!(matches!(err, FalError::DecodingError(_)), "{err:?}");
}

#[test]
fn extract_transcodes_non_png_data_uri_to_png() {
    crate::rt().block_on(extract_transcodes_non_png_data_uri_to_png_inner());
}

async fn extract_transcodes_non_png_data_uri_to_png_inner() {
    let jpeg = tiny_jpeg();
    let response = json_bytes(json!({
        "images": [{ "url": format!("data:image/jpeg;base64,{}", b64(&jpeg)), "content_type": "image/jpeg" }]
    }));
    let result = client().extract_image_data(&response).await.unwrap();
    assert!(result.starts_with(&[0x89, b'P', b'N', b'G']));
}

// ----- handle_http_error --------------------------------------------------------------------------

fn error_body(detail: &str) -> Vec<u8> {
    json_bytes(json!({ "detail": detail }))
}

#[test]
fn http_401_is_unauthorized() {
    assert_eq!(handle_http_error(401, &error_body("invalid key")), FalError::Unauthorized("invalid key".into()));
}

#[test]
fn http_403_is_unauthorized() {
    assert_eq!(handle_http_error(403, &error_body("forbidden")), FalError::Unauthorized("forbidden".into()));
}

#[test]
fn http_402_is_payment_required() {
    assert_eq!(handle_http_error(402, &error_body("no credits")), FalError::PaymentRequired("no credits".into()));
}

#[test]
fn http_422_is_bad_request() {
    assert_eq!(handle_http_error(422, &error_body("invalid field")), FalError::BadRequest("invalid field".into()));
}

#[test]
fn http_429_is_rate_limited() {
    assert_eq!(handle_http_error(429, &error_body("too many requests")), FalError::RateLimited("too many requests".into()));
}

#[test]
fn http_500_is_server_error() {
    assert_eq!(handle_http_error(500, &error_body("internal error")), FalError::ServerError { status_code: 500, message: "internal error".into() });
}

#[test]
fn http_unknown_code_is_http_error() {
    assert_eq!(handle_http_error(999, &error_body("weird")), FalError::HttpError { status_code: 999, message: "weird".into() });
}

#[test]
fn http_error_without_json_body_uses_unknown_error() {
    assert_eq!(handle_http_error(500, b"<html>oops</html>"), FalError::ServerError { status_code: 500, message: "Unknown error".into() });
}

#[test]
fn http_content_policy_violation_wins_over_status() {
    let body = json_bytes(json!({
        "detail": [
            { "loc": ["body", "prompt"], "msg": "Unsafe prompt", "type": "content_policy_violation", "url": "https://fal.ai/policy", "ctx": {} }
        ]
    }));
    assert_eq!(handle_http_error(422, &body), FalError::ContentFiltered("Unsafe prompt (https://fal.ai/policy)".into()));
}

#[test]
fn http_422_with_items_joins_messages_and_appends_first_url() {
    let body = json_bytes(json!({
        "detail": [
            { "loc": ["body", "image_size"], "msg": "bad size", "type": "value_error", "url": "https://errors.fal.ai/1" },
            { "loc": ["body", "prompt"], "msg": "too long", "type": "value_error" }
        ]
    }));
    assert_eq!(handle_http_error(422, &body), FalError::BadRequest("bad size; too long (https://errors.fal.ai/1)".into()));
}

// ----- FalErrorDetail decoding --------------------------------------------------------------------

#[test]
fn error_detail_string_form() {
    let d: FalErrorDetail = serde_json::from_value(json!({ "detail": "nope" })).unwrap();
    assert_eq!(d, FalErrorDetail { detail: Some("nope".into()), items: vec![] });
}

#[test]
fn error_detail_array_form_keeps_only_strings() {
    let d: FalErrorDetail = serde_json::from_value(json!({
        "detail": [{ "msg": "m", "type": "t", "url": "u", "loc": ["a"], "ctx": { "x": 1 } }]
    }))
    .unwrap();
    assert_eq!(d.detail.as_deref(), Some("m"));
    assert_eq!(d.items, vec![FalErrorItem { msg: Some("m".into()), r#type: Some("t".into()), url: Some("u".into()) }]);
}

#[test]
fn error_detail_other_forms_are_empty() {
    let d: FalErrorDetail = serde_json::from_value(json!({ "detail": 42 })).unwrap();
    assert_eq!(d, FalErrorDetail::default());
    let d: FalErrorDetail = serde_json::from_value(json!({ "error": "x" })).unwrap();
    assert_eq!(d, FalErrorDetail::default());
    let d: FalErrorDetail = serde_json::from_value(json!({ "detail": ["not-an-object"] })).unwrap();
    assert_eq!(d, FalErrorDetail::default());
}

// ----- FalError → GenerationError -----------------------------------------------------------------

#[test]
fn fal_error_mapping() {
    assert!(matches!(FalError::RateLimited("test".into()).into_generation_error(), GenerationError::RateLimited(_)));
    assert!(matches!(FalError::Unauthorized("test".into()).into_generation_error(), GenerationError::Unauthorized(_)));
    assert!(matches!(FalError::PaymentRequired("test".into()).into_generation_error(), GenerationError::PaymentRequired(_)));
    assert!(matches!(FalError::ContentFiltered("test".into()).into_generation_error(), GenerationError::ContentFiltered(_)));
    assert_eq!(FalError::QueueTimeout.into_generation_error(), GenerationError::Timeout);
    assert_eq!(FalError::NoImageGenerated.into_generation_error(), GenerationError::NoResultGenerated);
    assert_eq!(FalError::NoVideoGenerated.into_generation_error(), GenerationError::NoResultGenerated);
    assert_eq!(FalError::NoResultGenerated.into_generation_error(), GenerationError::NoResultGenerated);
    assert_eq!(
        FalError::ServerError { status_code: 503, message: "overloaded".into() }.into_generation_error(),
        GenerationError::ServerError { status_code: Some(503), message: "overloaded".into() }
    );
    assert_eq!(FalError::QueueFailed("model crashed".into()).into_generation_error(), GenerationError::ProviderFailed("model crashed".into()));
    assert!(matches!(FalError::BadRequest("x".into()).into_generation_error(), GenerationError::InvalidRequest(_)));
    assert_eq!(
        FalError::UnsupportedModel("m".into()).into_generation_error(),
        GenerationError::InvalidRequest("Model 'm' is not supported by fal.ai".into())
    );
    assert!(matches!(FalError::HttpError { status_code: 418, message: "x".into() }.into_generation_error(), GenerationError::Unknown(_)));
    assert_eq!(FalError::Transport(GenerationError::Timeout).into_generation_error(), GenerationError::Timeout);
}

#[test]
fn all_fal_errors_have_descriptions() {
    let errors = [
        FalError::InvalidUrl("test".into()),
        FalError::InvalidResponse,
        FalError::BadRequest("test".into()),
        FalError::Unauthorized("test".into()),
        FalError::PaymentRequired("test".into()),
        FalError::RateLimited("test".into()),
        FalError::ServerError { status_code: 500, message: "test".into() },
        FalError::HttpError { status_code: 418, message: "test".into() },
        FalError::ContentFiltered("test".into()),
        FalError::NoImageGenerated,
        FalError::InvalidImageData,
        FalError::DecodingError("test".into()),
        FalError::UnsupportedModel("test".into()),
        FalError::NoVideoGenerated,
        FalError::NoResultGenerated,
        FalError::VideoDownloadFailed("test".into()),
        FalError::AudioDownloadFailed("test".into()),
        FalError::QueueTimeout,
        FalError::QueueFailed("test".into()),
        FalError::Transport(GenerationError::Timeout),
    ];
    for e in errors {
        assert!(!e.to_string().is_empty(), "{e:?}");
    }
    assert_eq!(FalError::HttpError { status_code: 418, message: "test".into() }.to_string(), "HTTP 418 I'm a teapot: test");
}

// ----- FalQueueStatus -----------------------------------------------------------------------------

fn decode_status(s: &str) -> FalQueueStatus {
    serde_json::from_value(Value::String(s.into())).unwrap()
}

fn encode_status(s: &FalQueueStatus) -> String {
    serde_json::to_value(s).unwrap().as_str().unwrap().to_string()
}

#[test]
fn queue_status_decodes() {
    assert_eq!(decode_status("IN_QUEUE"), FalQueueStatus::InQueue);
    assert_eq!(decode_status("IN_PROGRESS"), FalQueueStatus::InProgress);
    assert_eq!(decode_status("COMPLETED"), FalQueueStatus::Completed);
    assert_eq!(decode_status("FAILED"), FalQueueStatus::Failed);
    assert_eq!(decode_status("SOMETHING_ELSE"), FalQueueStatus::Unknown("SOMETHING_ELSE".into()));
}

#[test]
fn queue_status_encodes() {
    assert_eq!(encode_status(&FalQueueStatus::InQueue), "IN_QUEUE");
    assert_eq!(encode_status(&FalQueueStatus::InProgress), "IN_PROGRESS");
    assert_eq!(encode_status(&FalQueueStatus::Completed), "COMPLETED");
    assert_eq!(encode_status(&FalQueueStatus::Failed), "FAILED");
    assert_eq!(encode_status(&FalQueueStatus::Unknown("CUSTOM".into())), "CUSTOM");
}

// ----- error mapping through the trait ------------------------------------------------------------

#[test]
fn generate_image_unsupported_model_is_invalid_request() {
    crate::rt().block_on(generate_image_unsupported_model_is_invalid_request_inner());
}

async fn generate_image_unsupported_model_is_invalid_request_inner() {
    // riverflow-2-max is not on fal, so we fail before any network call.
    let err = client().generate_image("test", &img("riverflow-2-max"), &[], None, None).await.unwrap_err();
    assert!(matches!(err, GenerationError::InvalidRequest(_)), "{err:?}");
}

// ----- response decoding --------------------------------------------------------------------------

#[test]
fn decodes_video_response() {
    let r: FalVideoResponse = serde_json::from_str(r#"{"video": {"url": "https://cdn.fal.ai/video.mp4"}}"#).unwrap();
    assert_eq!(r.video.url, "https://cdn.fal.ai/video.mp4");
}

#[test]
fn decodes_submit_response_without_urls() {
    let r: FalQueueSubmitResponse = serde_json::from_str(r#"{"request_id": "abc-123"}"#).unwrap();
    assert_eq!(r.request_id, "abc-123");
    assert_eq!(r.status_url, None);
    assert_eq!(r.response_url, None);
}

#[test]
fn decodes_submit_response_with_urls() {
    let r: FalQueueSubmitResponse = serde_json::from_str(
        r#"{
          "request_id": "abc-123",
          "status_url": "https://queue.fal.run/fal-ai/veo3.1/requests/abc-123/status",
          "response_url": "https://queue.fal.run/fal-ai/veo3.1/requests/abc-123"
        }"#,
    )
    .unwrap();
    assert_eq!(r.request_id, "abc-123");
    assert_eq!(r.status_url.as_deref(), Some("https://queue.fal.run/fal-ai/veo3.1/requests/abc-123/status"));
    assert_eq!(r.response_url.as_deref(), Some("https://queue.fal.run/fal-ai/veo3.1/requests/abc-123"));
}

#[test]
fn decodes_wrapped_queue_video_response() {
    let r: FalQueuedVideoResponse = serde_json::from_str(r#"{"response": {"video": {"url": "https://cdn.fal.ai/video.mp4"}}}"#).unwrap();
    assert_eq!(r.response.unwrap().video.url, "https://cdn.fal.ai/video.mp4");
    assert_eq!(r.video, None);
}

#[test]
fn decodes_direct_queue_video_response() {
    let r: FalQueuedVideoResponse = serde_json::from_str(r#"{"video": {"url": "https://cdn.fal.ai/video.mp4"}}"#).unwrap();
    assert_eq!(r.video.unwrap().url, "https://cdn.fal.ai/video.mp4");
    assert_eq!(r.response, None);
}

// ----- resolve_video_endpoint ---------------------------------------------------------------------

#[test]
fn veo_both_frames_picks_first_last() {
    for (model, expected) in [
        (ids::VEO_31, "fal-ai/veo3.1/first-last-frame-to-video"),
        (ids::VEO_31_FAST, "fal-ai/veo3.1/fast/first-last-frame-to-video"),
        (ids::VEO_31_LITE, "fal-ai/veo3.1/lite/first-last-frame-to-video"),
    ] {
        let (id, variant) = FalClient::resolve_video_endpoint(&vid(model), Some(FRAME), Some(FRAME)).unwrap();
        assert_eq!(id, expected, "{model} endpoint");
        assert_eq!(variant, VideoEndpointVariant::FirstLast);
    }
}

#[test]
fn veo_first_frame_only_picks_i2v() {
    let (id, variant) = FalClient::resolve_video_endpoint(&vid(ids::VEO_31), Some(FRAME), None).unwrap();
    assert_eq!(id, "fal-ai/veo3.1/image-to-video");
    assert_eq!(variant, VideoEndpointVariant::I2v);
}

#[test]
fn veo_no_frames_picks_t2v() {
    let (id, variant) = FalClient::resolve_video_endpoint(&vid(ids::VEO_31), None, None).unwrap();
    assert_eq!(id, "fal-ai/veo3.1");
    assert_eq!(variant, VideoEndpointVariant::T2v);
}

#[test]
fn kling_both_frames_stays_on_i2v() {
    let (id, variant) = FalClient::resolve_video_endpoint(&vid(ids::KLING_30_PRO), Some(FRAME), Some(FRAME)).unwrap();
    assert_eq!(id, "fal-ai/kling-video/v3/pro/image-to-video");
    assert_eq!(variant, VideoEndpointVariant::I2v);
}

#[test]
fn seedance_both_frames_stays_on_i2v() {
    let (id, variant) = FalClient::resolve_video_endpoint(&vid(ids::SEEDANCE_20), Some(FRAME), Some(FRAME)).unwrap();
    assert_eq!(id, "bytedance/seedance-2.0/image-to-video");
    assert_eq!(variant, VideoEndpointVariant::I2v);
}

#[test]
fn happy_horse_endpoints() {
    let (t2v, v) = FalClient::resolve_video_endpoint(&vid(ids::HAPPY_HORSE_10), None, None).unwrap();
    assert_eq!(t2v, "alibaba/happy-horse/text-to-video");
    assert_eq!(v, VideoEndpointVariant::T2v);
    let (i2v, v) = FalClient::resolve_video_endpoint(&vid(ids::HAPPY_HORSE_10), Some(FRAME), None).unwrap();
    assert_eq!(i2v, "alibaba/happy-horse/image-to-video");
    assert_eq!(v, VideoEndpointVariant::I2v);
}

#[test]
fn unsupported_video_model_errors() {
    let err = FalClient::resolve_video_endpoint(&vid("not-on-fal"), None, None).unwrap_err();
    assert_eq!(err, FalError::UnsupportedModel("not-on-fal".into()));
}

// ----- build_video_request_body -------------------------------------------------------------------

#[test]
fn veo_first_last_body_keys() {
    let settings = video_settings(ids::VEO_31, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Hd), 6, true);
    let body = FalClient::build_video_request_body("p", Some(FRAME), Some(FRAME), None, VideoEndpointVariant::FirstLast, &settings);
    assert!(body["first_frame_url"].is_string());
    assert!(body["last_frame_url"].is_string());
    assert!(body.get("image_url").is_none());
    assert!(body.get("end_image_url").is_none());
    assert_eq!(body["duration"], json!("6s"));
    assert_eq!(body["generate_audio"], json!(true));
    assert_eq!(body["enable_safety_checker"], json!(false));
}

#[test]
fn veo_i2v_body_keys() {
    let settings = video_settings(ids::VEO_31, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Hd), 6, true);
    let body = FalClient::build_video_request_body("p", Some(FRAME), None, None, VideoEndpointVariant::I2v, &settings);
    assert!(body["image_url"].is_string());
    assert!(body.get("end_image_url").is_none());
    assert!(body.get("last_frame_url").is_none());
    assert!(body.get("first_frame_url").is_none());
}

#[test]
fn kling_i2v_body_keys() {
    let settings = video_settings(ids::KLING_30_PRO, Some(VideoAspectRatio::Landscape), None, 5, true);
    let body = FalClient::build_video_request_body("p", Some(FRAME), Some(FRAME), None, VideoEndpointVariant::I2v, &settings);
    assert!(body["start_image_url"].is_string());
    assert!(body["end_image_url"].is_string());
    assert!(body.get("first_frame_url").is_none());
    assert!(body.get("last_frame_url").is_none());
    assert_eq!(body["duration"], json!("5"));
    assert!(body.get("resolution").is_none());
}

#[test]
fn kling_turbo_i2v_uses_tail_image_url() {
    let settings = video_settings(ids::KLING_25_TURBO_PRO, Some(VideoAspectRatio::Landscape), None, 5, false);
    let body = FalClient::build_video_request_body("p", Some(FRAME), Some(FRAME), None, VideoEndpointVariant::I2v, &settings);
    assert!(body["image_url"].is_string());
    assert!(body["tail_image_url"].is_string());
    assert!(body.get("end_image_url").is_none());
    assert!(body.get("generate_audio").is_none());
}

#[test]
fn wan27_includes_audio_url() {
    let settings = video_settings(ids::WAN_27, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Hd), 5, true);
    let audio = ProviderAsset::new(AssetRole::Audio, "public.mp3", vec![0x01, 0x02]);
    let body = FalClient::build_video_request_body("p", Some(FRAME), None, Some(&audio), VideoEndpointVariant::I2v, &settings);
    assert!(body["audio_url"].as_str().unwrap().starts_with("data:audio/mpeg;base64,"));
    assert!(body["image_url"].is_string());
    assert_eq!(body["duration"], json!(5));
}

#[test]
fn happy_horse_body_keys() {
    let settings = video_settings(ids::HAPPY_HORSE_10, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 5, true);
    let t2v = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(t2v["aspect_ratio"], json!("16:9"));
    assert_eq!(t2v["resolution"], json!("1080p"));
    assert_eq!(t2v["duration"], json!(5));
    assert!(t2v.get("image_url").is_none());
    assert!(t2v.get("generate_audio").is_none());

    let i2v = FalClient::build_video_request_body("p", Some(FRAME), Some(FRAME), None, VideoEndpointVariant::I2v, &settings);
    assert!(i2v["image_url"].as_str().unwrap().starts_with("data:"));
    assert!(i2v.get("end_image_url").is_none());
    assert!(i2v.get("last_frame_url").is_none());
    assert_eq!(i2v["duration"], json!(5));
    assert_eq!(i2v["resolution"], json!("1080p"));
}

/// Muse Image and Grok Imagine Image 2 take a plain `aspect_ratio`, not fal's named `image_size`
/// presets, and Grok adds a lowercase 1k/2k `resolution` tier.
#[test]
fn new_image_models_size_and_resolution_keys() {
    assert_eq!(caps::api_image_size(&img(ids::MUSE_IMAGE), AspectRatio::Wide), Some(("aspect_ratio", "21:9".to_string())));
    assert_eq!(caps::api_image_resolution(&img(ids::MUSE_IMAGE), ImageResolution::Hd), None);

    assert_eq!(caps::api_image_size(&img(ids::GROK_IMAGINE_IMAGE_2), AspectRatio::Landscape), Some(("aspect_ratio", "16:9".to_string())));
    assert_eq!(caps::api_image_resolution(&img(ids::GROK_IMAGINE_IMAGE_2), ImageResolution::Hd), Some(("resolution", "1k".to_string())));
    assert_eq!(caps::api_image_resolution(&img(ids::GROK_IMAGINE_IMAGE_2), ImageResolution::Fhd), Some(("resolution", "2k".to_string())));

    // Qwen and Seedream 5 stay on the named presets.
    assert_eq!(caps::api_image_size(&img(ids::QWEN_IMAGE_3), AspectRatio::Square), Some(("image_size", "square_hd".to_string())));
    assert_eq!(caps::api_image_size(&img(ids::SEEDREAM_5_PRO), AspectRatio::Square), Some(("image_size", "square_hd".to_string())));
}

/// The aspect-ratio and resolution keys now come from the capability tables rather than being
/// written straight from `raw()`, so pin that an untouched model's body is exactly as it was.
#[test]
fn video_aspect_and_resolution_still_pass_through_unmapped() {
    let settings = video_settings(ids::VEO_31, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 8, true);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body["aspect_ratio"], json!("16:9"));
    assert_eq!(body["resolution"], json!("1080p"));
}

/// Wan 3.0 spells "let the model decide" `adaptive`, and its audio toggle is `audio`.
#[test]
fn wan_3_maps_auto_to_adaptive_and_toggles_plain_audio() {
    let settings = video_settings(ids::WAN_30, Some(VideoAspectRatio::Auto), Some(VideoResolution::Fhd), 5, true);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body["aspect_ratio"], json!("adaptive"));
    assert_eq!(body["audio"], json!(true));
    assert!(body.get("generate_audio").is_none());

    let named = video_settings(ids::WAN_30_PRIME, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Hd), 5, false);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &named);
    assert_eq!(body["aspect_ratio"], json!("16:9"), "only `auto` is renamed");
    assert_eq!(body["audio"], json!(false));
}

/// Wan 3.0's image-to-video endpoint names the opening frame `start_image_url`.
#[test]
fn wan_3_i2v_uses_start_image_url() {
    let settings = video_settings(ids::WAN_30, None, Some(VideoResolution::Hd), 5, true);
    let body = FalClient::build_video_request_body("p", Some(FRAME), Some(FRAME), None, VideoEndpointVariant::I2v, &settings);
    assert!(body["start_image_url"].as_str().unwrap().starts_with("data:"));
    assert!(body["end_image_url"].as_str().unwrap().starts_with("data:"));
    assert!(body.get("image_url").is_none());
}

/// H3 spells its tiers with an uppercase P and two names our enum doesn't share.
#[test]
fn minimax_h3_resolution_tiers() {
    for (resolution, expected) in [
        (VideoResolution::Sd, "480P"),
        (VideoResolution::Hd, "768P"),
        (VideoResolution::Fhd, "2K"),
        (VideoResolution::Uhd, "4K"),
    ] {
        let settings = video_settings(ids::MINIMAX_H3, None, Some(resolution), 5, false);
        let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
        assert_eq!(body["resolution"], json!(expected), "{expected}");
    }
    // H3 Max sells only the two lower tiers; the key is dropped rather than sent as a bad value.
    for resolution in [VideoResolution::Fhd, VideoResolution::Uhd] {
        let settings = video_settings(ids::MINIMAX_H3_MAX, None, Some(resolution), 5, false);
        let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
        assert!(body.get("resolution").is_none(), "{resolution:?}");
    }
    // H3 has no `auto` ratio at all, so the key is omitted rather than sent as "auto".
    let settings = video_settings(ids::MINIMAX_H3, Some(VideoAspectRatio::Auto), None, 5, false);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert!(body.get("aspect_ratio").is_none());
}

/// Both mix `auto` with the numbers in one duration enum, but disagree on the numbers' type:
/// Seedance 2.5 lists them as strings (`"30"`), FLUX 3 as integers (`20`).
#[test]
fn seedance_25_and_flux_3_duration_types() {
    let settings = video_settings(ids::SEEDANCE_25, None, Some(VideoResolution::Fhd), 30, true);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body["duration"], json!("30"));
    assert_eq!(body["resolution"], json!("1080p"));
    assert_eq!(body["generate_audio"], json!(true));

    let settings = video_settings(ids::FLUX_3, None, Some(VideoResolution::Hd), 20, true);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body["duration"], json!(20));
    assert_eq!(body["generate_audio"], json!(true));
}

/// Gemini Omni Flash takes an integer duration and exposes no audio toggle.
#[test]
fn gemini_omni_flash_body_keys() {
    let settings = video_settings(ids::GEMINI_OMNI_FLASH_11, Some(VideoAspectRatio::Tall), Some(VideoResolution::Uhd), 8, true);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body["aspect_ratio"], json!("9:16"));
    assert_eq!(body["resolution"], json!("4k"));
    assert_eq!(body["duration"], json!(8));
    assert!(body.get("generate_audio").is_none());
}

#[test]
fn pixverse_uses_generate_audio_switch() {
    let settings = video_settings(ids::PIXVERSE_V6, None, None, 5, false);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body["generate_audio_switch"], json!(false));
    assert!(body.get("generate_audio").is_none());
}

// ----- frame param variant matrix -----------------------------------------------------------------

#[test]
fn first_last_veo_mappings() {
    for model in [ids::VEO_31, ids::VEO_31_FAST, ids::VEO_31_LITE] {
        assert_eq!(caps::api_end_frame_param(&vid(model), VideoEndpointVariant::FirstLast), Some("last_frame_url"));
        assert_eq!(caps::api_start_frame_param(&vid(model), VideoEndpointVariant::FirstLast), Some("first_frame_url"));
    }
}

/// FLUX 3's first-last endpoint requires `start_image_url` / `end_image_url`; veo3.1's names would
/// drop both frames from a request that only makes sense with them.
#[test]
fn first_last_flux_3_mappings() {
    assert_eq!(caps::api_start_frame_param(&vid(ids::FLUX_3), VideoEndpointVariant::FirstLast), Some("start_image_url"));
    assert_eq!(caps::api_end_frame_param(&vid(ids::FLUX_3), VideoEndpointVariant::FirstLast), Some("end_image_url"));
}

/// The frame keys differ per endpoint, so a model added to the first-last table without its own
/// pair would silently send no frames at all.
#[test]
fn every_first_last_endpoint_has_frame_params() {
    for id in caps::SUPPORTED_VIDEO_MODEL_IDS {
        let model = vid(id);
        if caps::video_first_last_frame_endpoint(&model).is_none() {
            continue;
        }
        assert!(caps::api_start_frame_param(&model, VideoEndpointVariant::FirstLast).is_some(), "{id} has no start-frame param");
        assert!(caps::api_end_frame_param(&model, VideoEndpointVariant::FirstLast).is_some(), "{id} has no end-frame param");
    }
}

/// The body a both-frames FLUX 3 request actually sends.
#[test]
fn flux_3_first_last_body_keys() {
    let settings = video_settings(ids::FLUX_3, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 10, true);
    let body = FalClient::build_video_request_body("p", Some(FRAME), Some(FRAME), None, VideoEndpointVariant::FirstLast, &settings);
    assert!(body["start_image_url"].as_str().unwrap().starts_with("data:"));
    assert!(body["end_image_url"].as_str().unwrap().starts_with("data:"));
    assert!(body.get("first_frame_url").is_none());
    assert!(body.get("last_frame_url").is_none());
    assert_eq!(body["duration"], json!(10));
    assert_eq!(body["resolution"], json!("1080p"));
}

/// Wan 3.0 dropped 2.7's audio-conditioning input, so it advertises no audio role and the client
/// takes no conditioning track: its audio slot is the reference list of its reference endpoint.
#[test]
fn wan_3_takes_no_audio_input() {
    for id in [ids::WAN_30, ids::WAN_30_PRIME] {
        assert_eq!(caps::api_audio_input_param(&vid(id)), None, "{id}");
        let model_caps = caps::video_capabilities(&vid(id)).unwrap();
        assert_eq!(model_caps.asset_constraints.range(AssetRole::Audio), Some(&(0..=5)), "{id}: five reference clips");
        assert_eq!(caps::video_reference_params(&vid(id)).unwrap().audio, Some("reference_audio_urls"), "{id}");
    }
    assert_eq!(caps::api_audio_input_param(&vid(ids::WAN_27)), Some("audio_url"));
}

/// H3 Max's schema requires `prompt_expansion_mode`; plain H3 defaults it.
#[test]
fn minimax_h3_max_sends_its_required_prompt_expansion_mode() {
    let settings = video_settings(ids::MINIMAX_H3_MAX, None, Some(VideoResolution::Hd), 5, false);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert_eq!(body["prompt_expansion_mode"], json!("balanced"));

    let settings = video_settings(ids::MINIMAX_H3, None, Some(VideoResolution::Hd), 5, false);
    let body = FalClient::build_video_request_body("p", None, None, None, VideoEndpointVariant::T2v, &settings);
    assert!(body.get("prompt_expansion_mode").is_none());
}

#[test]
fn first_last_non_veo_mappings_are_none() {
    for model in [
        ids::SORA_2,
        ids::SORA_2_PRO,
        ids::KLING_30_PRO,
        ids::KLING_30_STANDARD,
        ids::KLING_25_TURBO_PRO,
        ids::KLING_26_PRO,
        ids::SEEDANCE_15_PRO,
        ids::SEEDANCE_20,
        ids::SEEDANCE_20_FAST,
        ids::HAPPY_HORSE_10,
        ids::WAN_27,
        ids::PIXVERSE_V6,
        ids::GROK_IMAGINE_VIDEO,
    ] {
        assert_eq!(caps::api_end_frame_param(&vid(model), VideoEndpointVariant::FirstLast), None, "{model}");
        assert_eq!(caps::api_start_frame_param(&vid(model), VideoEndpointVariant::FirstLast), None, "{model}");
    }
}

#[test]
fn i2v_end_frame_mappings() {
    for model in [ids::VEO_31, ids::VEO_31_FAST, ids::VEO_31_LITE, ids::SORA_2, ids::SORA_2_PRO, ids::HAPPY_HORSE_10, ids::PIXVERSE_V6, ids::GROK_IMAGINE_VIDEO] {
        assert_eq!(caps::api_end_frame_param(&vid(model), VideoEndpointVariant::I2v), None, "{model}");
    }
    for model in [ids::KLING_30_PRO, ids::KLING_30_STANDARD, ids::KLING_26_PRO, ids::SEEDANCE_15_PRO, ids::SEEDANCE_20, ids::SEEDANCE_20_FAST, ids::WAN_27] {
        assert_eq!(caps::api_end_frame_param(&vid(model), VideoEndpointVariant::I2v), Some("end_image_url"), "{model}");
    }
    assert_eq!(caps::api_end_frame_param(&vid(ids::KLING_25_TURBO_PRO), VideoEndpointVariant::I2v), Some("tail_image_url"));
}

#[test]
fn t2v_returns_none_for_every_model() {
    for model in catalog::video::ALL.iter() {
        assert_eq!(caps::api_start_frame_param(model, VideoEndpointVariant::T2v), None);
        assert_eq!(caps::api_end_frame_param(model, VideoEndpointVariant::T2v), None);
    }
    for id in caps::SUPPORTED_VIDEO_MODEL_IDS {
        assert_eq!(caps::api_start_frame_param(&vid(id), VideoEndpointVariant::T2v), None);
        assert_eq!(caps::api_end_frame_param(&vid(id), VideoEndpointVariant::T2v), None);
    }
}

// ----- capabilities -------------------------------------------------------------------------------

#[test]
fn happy_horse_capabilities() {
    let caps = caps::video_capabilities(&vid(ids::HAPPY_HORSE_10)).unwrap();
    assert_eq!(caps.duration_range, VideoDurationRange::new(3, 15, None));
    assert_eq!(
        caps.aspect_ratios,
        vec![VideoAspectRatio::Landscape, VideoAspectRatio::Tall, VideoAspectRatio::Square, VideoAspectRatio::Standard, VideoAspectRatio::Portrait]
    );
    assert_eq!(caps.resolutions, vec![VideoResolution::Hd, VideoResolution::Fhd]);
    assert_eq!(caps.max_input_images, 1);
    assert_eq!(caps.asset_constraints.range(AssetRole::FirstFrame), Some(&(0..=1)));
    assert_eq!(caps.asset_constraints.range(AssetRole::ReferenceImage), Some(&(0..=9)), "its reference endpoint takes nine");
    assert_eq!(caps.asset_constraints.allowed.len(), 2);
    assert!(caps.prompt_optional);
    assert!(caps.supports_audio);
    assert!(!caps.supports_audio_toggle());
}

/// A model's declared reference lists and the request keys that carry them have to agree: a count
/// with no key sends nothing, a key with no count offers the user no card.
/// A reference of `role`, with the MIME its kind implies.
fn reference_of(role: AssetRole) -> ProviderAsset {
    match role {
        AssetRole::ReferenceVideo => ProviderAsset::new(role, "video/mp4", vec![0x00, 0x01]),
        _ => ProviderAsset::new(role, "image/png", REFERENCE_PNG.to_vec()),
    }
}

#[test]
fn every_reference_model_agrees_with_its_request_keys() {
    for id in caps::SUPPORTED_VIDEO_MODEL_IDS {
        let model = vid(id);
        let declared = caps::video_capabilities(&model).unwrap().references;
        let params = caps::video_reference_params(&model);
        assert_eq!(declared.is_some(), params.is_some(), "{id}: capabilities and request keys disagree");
        assert_eq!(declared.is_some(), caps::video_reference_endpoint(&model).is_some(), "{id}: no reference endpoint");
        let (Some(declared), Some(params)) = (declared, params) else { continue };
        assert!(declared.images > 0, "{id}: a reference endpoint always takes images");
        for role in [AssetRole::ReferenceImage, AssetRole::ReferenceVideo, AssetRole::Audio] {
            assert_eq!(
                declared.max_for(role) > 0,
                params.param_for(role).is_some(),
                "{id}: {role:?} count and request key disagree"
            );
        }
    }
}

/// The reference endpoints take no frames at all, so the frame keys must stay out of their bodies.
#[test]
fn the_reference_variant_has_no_frame_params() {
    for id in caps::SUPPORTED_VIDEO_MODEL_IDS {
        let model = vid(id);
        assert_eq!(caps::api_start_frame_param(&model, VideoEndpointVariant::Reference), None, "{id}");
        assert_eq!(caps::api_end_frame_param(&model, VideoEndpointVariant::Reference), None, "{id}");
    }
}

/// Seedance spells the handles the way Majik does, so its prompt goes out as typed.
#[test]
fn seedance_reference_body_keeps_the_handles() {
    let settings = video_settings(ids::SEEDANCE_25, Some(VideoAspectRatio::Auto), Some(VideoResolution::Fhd), 10, true);
    let images = [reference_of(AssetRole::ReferenceImage), reference_of(AssetRole::ReferenceImage)];
    let video = reference_of(AssetRole::ReferenceVideo);
    let audio = ProviderAsset::new(AssetRole::Audio, "audio/mpeg", vec![9, 9]);
    let assets: Vec<ProviderAsset> = images.into_iter().chain([video, audio]).collect();
    let references = ReferenceAssets::from_assets(&assets);

    let body = FalClient::build_video_reference_body("@Image2 waves at @Video1 over @Audio1", &references, &settings);
    assert_eq!(body["prompt"], json!("@Image2 waves at @Video1 over @Audio1"));
    assert_eq!(body["image_urls"].as_array().unwrap().len(), 2);
    assert!(body["image_urls"][0].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(body["video_urls"][0].as_str().unwrap().starts_with("data:video/mp4;base64,"));
    assert!(body["audio_urls"][0].as_str().unwrap().starts_with("data:audio/mpeg;base64,"));
    assert_eq!(body["duration"], json!("10"));
    assert_eq!(body["generate_audio"], json!(true));
    assert!(body.get("image_url").is_none(), "no frame keys on the reference endpoint");
}

/// The dialects that need the prompt rewritten, on the models that use them.
#[test]
fn reference_bodies_speak_each_models_dialect() {
    let assets = [reference_of(AssetRole::ReferenceImage), reference_of(AssetRole::ReferenceImage)];
    let references = ReferenceAssets::from_assets(&assets);
    for (model, expected, key) in [
        (ids::HAPPY_HORSE_11, "character1 meets character2", "image_urls"),
        (ids::GROK_IMAGINE_VIDEO_15, "<IMAGE_0> meets <IMAGE_1>", "reference_image_urls"),
        (ids::VEO_31, "Image 1 meets Image 2", "image_urls"),
        (ids::MINIMAX_H3, "Image 1 meets Image 2", "reference_image_urls"),
        (ids::MINIMAX_H3_MAX, "Image 1 meets Image 2", "reference_image_urls"),
        (ids::GROK_IMAGINE_VIDEO, "@Image1 meets @Image2", "reference_image_urls"),
    ] {
        let settings = video_settings(model, None, Some(VideoResolution::Hd), 5, false);
        let body = FalClient::build_video_reference_body("@Image1 meets @Image2", &references, &settings);
        assert_eq!(body["prompt"], json!(expected), "{model}");
        assert_eq!(body[key].as_array().unwrap().len(), 2, "{model}");
    }
}

/// H3 Max carries all three reference kinds, each in its own array, alongside the
/// `prompt_expansion_mode` its schema requires and the resolution tier it spells its own way.
#[test]
fn minimax_h3_max_reference_body_carries_every_kind() {
    let settings = video_settings(ids::MINIMAX_H3_MAX, None, Some(VideoResolution::Hd), 5, false);
    let image = reference_of(AssetRole::ReferenceImage);
    let video = reference_of(AssetRole::ReferenceVideo);
    let audio = ProviderAsset::new(AssetRole::Audio, "audio/mpeg", vec![9, 9]);
    let assets = [image, video, audio];
    let references = ReferenceAssets::from_assets(&assets);

    let body = FalClient::build_video_reference_body("@Image1 dances to @Audio1 like @Video1", &references, &settings);
    assert_eq!(body["prompt"], json!("Image 1 dances to Audio 1 like Video 1"));
    assert!(body["reference_image_urls"][0].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(body["reference_video_urls"][0].as_str().unwrap().starts_with("data:video/mp4;base64,"));
    assert!(body["reference_audio_urls"][0].as_str().unwrap().starts_with("data:audio/mpeg;base64,"));
    assert_eq!(body["prompt_expansion_mode"], json!("balanced"));
    assert_eq!(body["resolution"], json!("768P"));
    assert!(body.get("image_url").is_none() && body.get("end_image_url").is_none(), "no frame keys on the reference endpoint");
}

/// Grok 1.5 renders references at 720p at most, where its text-to-video endpoint also sells 1080p.
#[test]
fn grok_15_references_are_capped_at_720p() {
    let references = caps::video_capabilities(&vid(ids::GROK_IMAGINE_VIDEO_15)).unwrap().references.unwrap();
    assert_eq!(references.resolutions, Some(&[VideoResolution::Sd, VideoResolution::Hd][..]));
    assert!(!references.allows_resolution(VideoResolution::Fhd) && references.allows_resolution(VideoResolution::Hd));
    for id in [ids::SEEDANCE_25, ids::VEO_31, ids::MINIMAX_H3, ids::MINIMAX_H3_MAX] {
        let references = caps::video_capabilities(&vid(id)).unwrap().references.unwrap();
        assert_eq!(references.resolutions, None, "{id}");
        assert!(references.allows_resolution(VideoResolution::Fhd), "{id}");
    }
}

#[test]
fn every_supported_model_has_capabilities_and_endpoints() {
    for id in caps::SUPPORTED_IMAGE_MODEL_IDS {
        let m = img(id);
        assert!(caps::image_capabilities(&m).is_some(), "{id}");
        assert!(caps::endpoint(&m).is_some(), "{id}");
        assert!(caps::api_edit_image_param(&m).is_some(), "{id}");
        assert!(caps::api_supports_output_format(&m).is_some(), "{id}");
    }
    for id in caps::SUPPORTED_VIDEO_MODEL_IDS {
        let m = vid(id);
        assert!(caps::video_capabilities(&m).is_some(), "{id}");
        assert!(caps::video_endpoint(&m).is_some(), "{id}");
        assert!(caps::video_i2v_endpoint(&m).is_some(), "{id}");
        assert!(caps::api_duration(&m, 5).is_some(), "{id}");
        assert!(caps::api_start_frame_param(&m, VideoEndpointVariant::I2v).is_some(), "{id}");
    }
    assert!(caps::image_capabilities(&img("riverflow-2-max")).is_none());
    assert!(caps::video_capabilities(&vid("nope")).is_none());
}

#[test]
fn image_size_mappings() {
    assert_eq!(caps::api_image_size(&img(ids::GEMINI_3_PRO), AspectRatio::Wide), Some(("aspect_ratio", "21:9".into())));
    assert_eq!(caps::api_image_size(&img(ids::GPT5), AspectRatio::Square), Some(("image_size", "1024x1024".into())));
    assert_eq!(caps::api_image_size(&img(ids::GPT5), AspectRatio::Landscape), None);
    assert_eq!(caps::api_image_size(&img(ids::FLUX_2_PRO), AspectRatio::Tall), Some(("image_size", "portrait_16_9".into())));
    assert_eq!(caps::api_image_size(&img(ids::FLUX_2_PRO), AspectRatio::Portrait), None);
    assert_eq!(caps::api_image_size(&img(ids::GPT_IMAGE_2), AspectRatio::Standard), Some(("image_size", "landscape_4_3".into())));
    assert_eq!(caps::api_image_resolution(&img(ids::GEMINI_3_PRO), ImageResolution::Uhd), Some(("resolution", "4K".into())));
    assert_eq!(caps::api_image_resolution(&img(ids::GPT_IMAGE_2), ImageResolution::Fhd), Some(("quality", "medium".into())));
    assert_eq!(caps::api_image_resolution(&img(ids::GPT_IMAGE_2), ImageResolution::Sd), None);
    assert_eq!(caps::api_image_resolution(&img(ids::FLUX_2_PRO), ImageResolution::Hd), None);
    assert_eq!(caps::image_capabilities(&img(ids::GPT_IMAGE_2)).unwrap().default_resolution(), Some(ImageResolution::Fhd));
}

// ----- mask param + constraints -------------------------------------------------------------------

const MODELS_WITHOUT_MASK: &[&str] = &[
    ids::GEMINI_3_PRO,
    ids::GEMINI_31_FLASH,
    ids::GEMINI_25_FLASH,
    ids::GPT5_MINI,
    ids::SEEDREAM_45,
    ids::FLUX_2_MAX,
    ids::FLUX_2_PRO,
    ids::FLUX_2_FLEX,
    ids::FLUX_2_KLEIN,
    ids::FLUX_1_DEV,
    ids::FLUX_1_SCHNELL,
    ids::RECRAFT_V4_PRO,
    ids::WAN_27_PRO,
];

#[test]
fn mask_param_mappings() {
    assert_eq!(caps::api_mask_param(&img(ids::GPT5)), Some("mask_image_url"));
    assert_eq!(caps::api_mask_param(&img(ids::GPT_IMAGE_2)), Some("mask_url"));
    for id in MODELS_WITHOUT_MASK {
        assert_eq!(caps::api_mask_param(&img(id)), None, "expected None mask param for {id}");
    }
}

#[test]
fn gpt5_mask_constraints() {
    let caps = caps::image_capabilities(&img(ids::GPT5)).unwrap();
    assert_eq!(caps.asset_constraints.range(AssetRole::ReferenceImage), Some(&(0..=1)));
    assert_eq!(caps.asset_constraints.range(AssetRole::MaskImage), Some(&(0..=1)));
    assert_eq!(caps.asset_constraints.range(AssetRole::ControlImage), None);
    assert_eq!(caps.asset_constraints.range(AssetRole::FirstFrame), None);
}

#[test]
fn gpt_image_2_mask_constraints() {
    let caps = caps::image_capabilities(&img(ids::GPT_IMAGE_2)).unwrap();
    assert_eq!(caps.asset_constraints.range(AssetRole::ReferenceImage), Some(&(0..=10)));
    assert_eq!(caps.asset_constraints.range(AssetRole::MaskImage), Some(&(0..=1)));
}

#[test]
fn models_without_native_mask_reject_mask_role() {
    for id in MODELS_WITHOUT_MASK {
        let caps = caps::image_capabilities(&img(id)).unwrap();
        assert!(caps.asset_constraints.range(AssetRole::MaskImage).is_none(), "{id} must not advertise mask support");
    }
}

// ----- build_request_body mask wiring -------------------------------------------------------------

const REFERENCE_PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const MASK_PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF];

#[test]
fn gpt5_emits_mask_image_url() {
    let body = FalClient::build_request_body("edit", &img(ids::GPT5), &[REFERENCE_PNG], Some(MASK_PNG), None, None);
    assert!(body["mask_image_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(body.get("mask_url").is_none());
    assert_eq!(body["image_urls"].as_array().unwrap().len(), 1);
}

#[test]
fn gpt_image_2_emits_mask_url() {
    let body = FalClient::build_request_body("edit", &img(ids::GPT_IMAGE_2), &[REFERENCE_PNG], Some(MASK_PNG), None, None);
    assert!(body["mask_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(body.get("mask_image_url").is_none());
    assert_eq!(body["image_urls"].as_array().unwrap().len(), 1);
}

#[test]
fn no_mask_no_key() {
    let body = FalClient::build_request_body("edit", &img(ids::GPT5), &[REFERENCE_PNG], None, None, None);
    assert!(body.get("mask_image_url").is_none());
    assert!(body.get("mask_url").is_none());
}

#[test]
fn unsupported_model_drops_mask_at_body_layer() {
    let body = FalClient::build_request_body("edit", &img(ids::FLUX_2_PRO), &[REFERENCE_PNG], Some(MASK_PNG), None, None);
    assert!(body.get("mask_image_url").is_none());
    assert!(body.get("mask_url").is_none());
}

#[test]
fn image_body_shape() {
    let body = FalClient::build_request_body("a cat", &img(ids::GEMINI_3_PRO), &[], None, Some(AspectRatio::Landscape), Some(ImageResolution::Fhd));
    assert_eq!(
        Value::Object(body),
        json!({ "prompt": "a cat", "enable_safety_checker": false, "aspect_ratio": "16:9", "resolution": "2K", "output_format": "png" })
    );

    // flux-1-dev's image-to-image endpoint takes a single `image_url`.
    let body = FalClient::build_request_body("edit", &img(ids::FLUX_1_DEV), &[REFERENCE_PNG, MASK_PNG], None, None, None);
    assert!(body["image_url"].is_string());
    assert!(body.get("image_urls").is_none());

    // seedream / recraft / wan do not take output_format.
    for id in [ids::SEEDREAM_45, ids::RECRAFT_V4_PRO, ids::WAN_27_PRO] {
        let body = FalClient::build_request_body("p", &img(id), &[], None, Some(AspectRatio::Square), None);
        assert!(body.get("output_format").is_none(), "{id}");
        assert_eq!(body["image_size"], json!("square_hd"), "{id}");
    }
}

// ----- generate_image mask rejections -------------------------------------------------------------

fn mask_asset() -> ProviderAsset {
    ProviderAsset::new(AssetRole::MaskImage, "image/png", vec![0x89, 0x50])
}

fn reference_asset() -> ProviderAsset {
    ProviderAsset::new(AssetRole::ReferenceImage, "image/png", vec![0x89, 0x50])
}

#[test]
fn mask_on_unsupported_model_throws() {
    crate::rt().block_on(mask_on_unsupported_model_throws_inner());
}

async fn mask_on_unsupported_model_throws_inner() {
    let err = client().generate_image("edit", &img(ids::FLUX_2_PRO), &[reference_asset(), mask_asset()], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("This model does not accept a mask input".into()));
}

#[test]
fn mask_without_reference_throws() {
    crate::rt().block_on(mask_without_reference_throws_inner());
}

async fn mask_without_reference_throws_inner() {
    let err = client().generate_image("edit", &img(ids::GPT_IMAGE_2), &[mask_asset()], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("A mask requires at least one reference image".into()));
}

#[test]
fn two_masks_throws() {
    crate::rt().block_on(two_masks_throws_inner());
}

async fn two_masks_throws_inner() {
    let err = client().generate_image("edit", &img(ids::GPT_IMAGE_2), &[reference_asset(), mask_asset(), mask_asset()], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("At most one mask is supported".into()));
}

#[test]
fn frame_role_on_image_endpoint_throws() {
    crate::rt().block_on(frame_role_on_image_endpoint_throws_inner());
}

async fn frame_role_on_image_endpoint_throws_inner() {
    let frame = ProviderAsset::new(AssetRole::FirstFrame, "image/png", vec![0x89]);
    let err = client().generate_image("p", &img(ids::GEMINI_3_PRO), &[frame], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("Role 'first_frame' is not supported by fal image endpoints".into()));
}

#[test]
fn video_input_validation() {
    crate::rt().block_on(video_input_validation_inner());
}

async fn video_input_validation_inner() {
    let c = client();
    let last = ProviderAsset::new(AssetRole::LastFrame, "image/png", FRAME.to_vec());
    let settings = video_settings(ids::VEO_31, None, None, 4, true);
    let err = c.generate_video("p", &[last], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("A last frame requires a first frame".into()));

    let audio = ProviderAsset::new(AssetRole::Audio, "audio/mpeg", vec![1]);
    let err = c.generate_video("p", &[audio], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("This model does not accept an audio input".into()));

    // Sora takes no references at all; Veo does, but not alongside a frame.
    let reference = || ProviderAsset::new(AssetRole::ReferenceImage, "image/png", vec![1]);
    let err = c.generate_video("p", &[reference()], &video_settings(ids::SORA_2, None, None, 4, true)).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("Sora 2 does not take reference inputs".into()));

    let first = ProviderAsset::new(AssetRole::FirstFrame, "image/png", FRAME.to_vec());
    let err = c.generate_video("p", &[reference(), first], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("References and a start or end frame can't be used together".into()));

    let err = c.generate_video("p", &[], &video_settings("nope", None, None, 4, true)).await.unwrap_err();
    assert!(matches!(err, GenerationError::InvalidRequest(_)));
}

// ----- audio capabilities -------------------------------------------------------------------------

#[test]
fn eleven_v3_audio_caps() {
    let caps = caps::audio_capabilities(&aud(ids::ELEVEN_LABS_V3)).unwrap();
    let ids: Vec<&str> = caps.supported_voices.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(caps.supported_voices.len(), 21);
    assert_eq!(ids.first().copied(), Some("Rachel"));
    assert!(ids.contains(&"Laura"));
    assert!(ids.contains(&"Bill"));
    assert!(!ids.contains(&"Drew"));
    assert!(!ids.contains(&"Grimblewood"));
    assert!(caps.supports_two_speakers);
    assert_eq!(caps.max_characters_monologue, 5000);
    assert_eq!(caps.max_characters_dialogue, 2000);
    assert_eq!(caps.default_voice.as_ref().map(|v| v.id.as_str()), Some("Rachel"));
    assert_eq!(caps.secondary_default_voice.as_ref().map(|v| v.id.as_str()), Some("Roger"));
}

#[test]
fn gemini_25_pro_audio_caps() {
    let caps = caps::audio_capabilities(&aud(ids::GEMINI_25_PRO)).unwrap();
    assert_eq!(caps.supported_voices.len(), 30);
    assert_eq!(caps.supported_voices.first().map(|v| v.id.as_str()), Some("Achernar"));
    assert!(caps.supports_two_speakers);
    assert_eq!(caps.max_characters_monologue, 50000);
    assert_eq!(caps.max_characters_dialogue, 50000);
    assert_eq!(caps.default_voice.as_ref().map(|v| v.id.as_str()), Some("Kore"));
    assert_eq!(caps.secondary_default_voice.as_ref().map(|v| v.id.as_str()), Some("Puck"));
}

#[test]
fn gemini_voices_include_preview_metadata() {
    let caps = caps::audio_capabilities(&aud(ids::GEMINI_25_PRO)).unwrap();
    for voice in &caps.supported_voices {
        assert!(voice.preview_url.is_some(), "{} is missing preview audio", voice.id);
        assert!(voice.gender.is_some(), "{} is missing gender", voice.id);
        assert_eq!(voice.language_codes.as_deref(), Some(&["multilingual".to_string()][..]), "{} is missing language metadata", voice.id);
    }
    let kore = caps.supported_voices.iter().find(|v| v.id == "Kore").unwrap();
    assert_eq!(kore.gender.as_deref(), Some("female"));
    assert_eq!(kore.preview_url.as_deref(), Some("https://docs.cloud.google.com/static/text-to-speech/docs/audio/chirp3-hd-kore.wav"));
    let puck = caps.supported_voices.iter().find(|v| v.id == "Puck").unwrap();
    assert_eq!(puck.gender.as_deref(), Some("male"));
    assert_eq!(puck.preview_url.as_deref(), Some("https://docs.cloud.google.com/static/text-to-speech/docs/audio/chirp3-hd-puck.wav"));
}

#[test]
fn unknown_audio_model_has_no_caps() {
    assert_eq!(caps::audio_capabilities(&AudioModel::new("made-up", "x", "x", "x", "x")), None);
}

#[test]
fn every_catalog_audio_model_mapped() {
    assert!(!catalog::audio::ALL.is_empty());
    for model in catalog::audio::ALL.iter() {
        assert!(caps::audio_capabilities(model).is_some(), "missing caps for {}", model.id);
    }
}

#[test]
fn fal_and_replicate_keep_separate_eleven_v3_voice_lists() {
    let fal_caps = caps::audio_capabilities(&aud(ids::ELEVEN_LABS_V3)).unwrap();
    let fal_ids: Vec<&str> = fal_caps.supported_voices.iter().map(|v| v.id.as_str()).collect();
    let replicate_ids: Vec<&str> = majik_providers::voices::elevenlabs::replicate_voices().iter().map(|v| v.id.as_str()).collect();
    assert_ne!(fal_ids, replicate_ids);
    assert_eq!(fal_ids.len(), 21);
    assert_eq!(replicate_ids.len(), 26);
    assert!(fal_ids.contains(&"Laura"));
    assert!(!replicate_ids.contains(&"Laura"));
    assert!(!fal_ids.contains(&"Drew"));
    assert!(replicate_ids.contains(&"Drew"));
}

#[test]
fn fal_eleven_v3_voice_metadata() {
    let caps = caps::audio_capabilities(&aud(ids::ELEVEN_LABS_V3)).unwrap();
    for voice in &caps.supported_voices {
        assert!(voice.subtitle.is_some(), "{} is missing official description", voice.id);
        assert!(voice.preview_url.is_some(), "{} is missing official preview audio", voice.id);
        assert!(voice.category.is_some(), "{} is missing official category", voice.id);
        assert!(voice.gender.is_some(), "{} is missing gender", voice.id);
        assert!(voice.accent.is_some(), "{} is missing official accent", voice.id);
        assert_eq!(voice.language_codes.as_deref(), Some(&["en".to_string()][..]), "{} is missing official language code", voice.id);
    }
    let find = |id: &str| caps.supported_voices.iter().find(|v| v.id == id).unwrap();

    let rachel = find("Rachel");
    assert_eq!(rachel.category.as_deref(), Some("professional"));
    assert_eq!(rachel.gender.as_deref(), Some("female"));
    assert_eq!(rachel.accent.as_deref(), Some("american"));
    assert_eq!(
        rachel.preview_url.as_deref(),
        Some("https://storage.googleapis.com/eleven-public-prod/database/workspace/1da06ea679a54975ad96a2221fe6530d/voices/eLDc7xhWxG2FElT3kUTj/aTInQG648LTH0oRjg54j.mp3")
    );

    let aria = find("Aria");
    assert_eq!(aria.category.as_deref(), Some("professional"));
    assert_eq!(aria.gender.as_deref(), Some("female"));
    assert_eq!(aria.accent.as_deref(), Some("african american"));

    let charlotte = find("Charlotte");
    assert_eq!(charlotte.category.as_deref(), Some("professional"));
    assert_eq!(charlotte.gender.as_deref(), Some("female"));
    assert_eq!(charlotte.accent.as_deref(), Some("british"));

    let laura = find("Laura");
    assert_eq!(laura.subtitle.as_deref(), Some("This young adult female voice delivers sunny enthusiasm with a quirky attitude."));
    assert_eq!(laura.category.as_deref(), Some("premade"));
    assert_eq!(laura.gender.as_deref(), Some("female"));
    assert_eq!(laura.accent.as_deref(), Some("american"));
    assert_eq!(
        laura.preview_url.as_deref(),
        Some("https://storage.googleapis.com/eleven-public-prod/premade/voices/FGY2WhTYpPnrIDTdsKH5/67341759-ad08-41a5-be6e-de12fe448618.mp3")
    );

    let river = find("River");
    assert_eq!(river.gender.as_deref(), Some("neutral"));
    assert_eq!(river.category.as_deref(), Some("premade"));
}

// ----- audio body shape ---------------------------------------------------------------------------

fn audio_settings(model: &'static str, speaker1: &str, speaker2: Option<&str>) -> AudioGenerationSettings {
    AudioGenerationSettings { model: aud(model), speaker1: voice(speaker1), speaker2: speaker2.map(voice) }
}

#[test]
fn eleven_monologue_routes_to_tts_endpoint() {
    let settings = audio_settings(ids::ELEVEN_LABS_V3, "Aria", None);
    let (endpoint, routing) = audio_routing(&settings, "Hello").unwrap();
    assert_eq!(endpoint, "fal-ai/elevenlabs/tts/eleven-v3");
    assert_eq!(routing, AudioRouting::ElevenLabsMonologue);
    let body = build_audio_request_body("Hello", &settings, &routing);
    assert_eq!(body["text"], json!("Hello"));
    assert_eq!(body["voice"], json!("Aria"));
    assert!(body.get("inputs").is_none());
}

#[test]
fn eleven_dialogue_routes_to_dialogue_endpoint() {
    let settings = audio_settings(ids::ELEVEN_LABS_V3, "Aria", Some("Roger"));
    let prompt = "Speaker 1: Hi.\nSpeaker 2: Hello.";
    let (endpoint, routing) = audio_routing(&settings, prompt).unwrap();
    assert_eq!(endpoint, "fal-ai/elevenlabs/text-to-dialogue/eleven-v3");
    assert!(matches!(routing, AudioRouting::ElevenLabsDialogue { .. }));
    let body = build_audio_request_body(prompt, &settings, &routing);
    let inputs = body["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0], json!({ "text": "Hi.", "voice": "Aria" }));
    assert_eq!(inputs[1], json!({ "text": "Hello.", "voice": "Roger" }));
    assert!(body.get("text").is_none());
}

#[test]
fn eleven_dialogue_with_empty_prompt_errors() {
    let settings = audio_settings(ids::ELEVEN_LABS_V3, "Aria", Some("Roger"));
    let err = audio_routing(&settings, "    \n  ").unwrap_err();
    assert_eq!(err, FalError::BadRequest("Add at least one Speaker 1 or Speaker 2 line.".into()));
}

#[test]
fn gemini_monologue_uses_voice_field() {
    let settings = audio_settings(ids::GEMINI_25_PRO, "Kore", None);
    let (endpoint, routing) = audio_routing(&settings, "Narrate.").unwrap();
    assert_eq!(endpoint, "fal-ai/gemini-tts");
    assert_eq!(routing, AudioRouting::GeminiMonologue);
    let body = build_audio_request_body("Narrate.", &settings, &routing);
    assert_eq!(body["model"], json!("gemini-2.5-pro-tts"));
    assert_eq!(body["prompt"], json!("Narrate."));
    assert_eq!(body["voice"], json!("Kore"));
    assert!(body.get("speakers").is_none());
    assert_eq!(body["output_format"], json!("mp3"));
}

#[test]
fn gemini_dialogue_uses_whitespace_free_speaker_ids() {
    let settings = audio_settings(ids::GEMINI_25_PRO, "Kore", Some("Puck"));
    let prompt = "Speaker 1: Yo.\nSpeaker 2: Sup.";
    let (_, routing) = audio_routing(&settings, prompt).unwrap();
    assert_eq!(routing, AudioRouting::GeminiDialogue);
    let body = build_audio_request_body(prompt, &settings, &routing);
    assert!(body.get("voice").is_none());
    let speakers = body["speakers"].as_array().unwrap();
    assert_eq!(speakers.len(), 2);
    assert_eq!(speakers[0], json!({ "speaker_id": "Speaker1", "voice": "Kore" }));
    assert_eq!(speakers[1], json!({ "speaker_id": "Speaker2", "voice": "Puck" }));
    assert_eq!(body["prompt"], json!("Speaker1: Yo.\nSpeaker2: Sup."));
    assert_eq!(body["output_format"], json!("mp3"));
}

#[test]
fn normalize_speaker_prefixes_is_idempotent_and_case_insensitive() {
    assert_eq!(normalize_speaker_prefixes("Speaker 1: Hi."), "Speaker1: Hi.");
    assert_eq!(normalize_speaker_prefixes("speaker 2: Yo."), "Speaker2: Yo.");
    assert_eq!(normalize_speaker_prefixes("Speaker1: A"), "Speaker1: A");
    assert_eq!(normalize_speaker_prefixes("Plain text"), "Plain text");
    assert_eq!(normalize_speaker_prefixes("  Speaker 1: A\n\nSPEAKER 2: B"), "  Speaker1: A\n\nSpeaker2: B");
}

#[test]
fn unsupported_audio_model_errors() {
    let settings = audio_settings("nope", "Kore", None);
    assert_eq!(audio_routing(&settings, "x").unwrap_err(), FalError::UnsupportedModel("nope".into()));
}

// ----- descriptor ---------------------------------------------------------------------------------

#[test]
fn descriptor_shape() {
    let d = majik_providers::fal::descriptor();
    assert_eq!(d.id, ProviderId::fal());
    assert_eq!(d.display_name, "fal.ai");
    assert_eq!(d.api_key_placeholder, "9...");
    assert_eq!(d.api_key_url, "https://fal.ai/dashboard/keys");
    assert_eq!(d.billing_url, Some("https://fal.ai/dashboard/usage-billing"));
    assert!(d.requires_api_key);
    assert!(d.is_user_selectable);
    assert_eq!(d.supported_image_models.len(), 20);
    assert_eq!(d.supported_video_models.len(), 25);
    assert_eq!(d.supported_audio_models.len(), 2);
    assert_eq!(d.supported_image_models[0].id, ids::GEMINI_3_PRO);
    assert_eq!(d.supported_video_models[0].id, ids::VEO_31);
    assert_eq!(
        d.supported_tool_models,
        vec![catalog::tool::TOPAZ_UPSCALE.clone(), catalog::tool::TOPAZ_UPSCALE_VIDEO.clone(), catalog::tool::BRIA_BACKGROUND_REMOVE.clone()]
    );
    // The Upscale tab offers one model per media, which is what lets it take either.
    assert_eq!(d.tool_models_for(ToolId::Upscale, MediaType::Image), vec![&catalog::tool::TOPAZ_UPSCALE]);
    assert_eq!(d.tool_models_for(ToolId::Upscale, MediaType::Video), vec![&catalog::tool::TOPAZ_UPSCALE_VIDEO]);
    assert!(d.tool_models_for(ToolId::RemoveBackground, MediaType::Video).is_empty());
    assert!((d.make_tool_client)(&ClientOptions::new("k")).is_some());
    for model in &d.supported_tool_models {
        assert!(d.tool_capabilities(model).is_some(), "{} has no capability row", model.id);
        assert!(caps::tool_endpoint(model).is_some(), "{} has no endpoint", model.id);
    }
    assert!(d.supports_video_generation());
    assert!(d.supports_audio_generation());
    assert!((d.make_video_client)(&ClientOptions::new("k")).is_some());
    assert!((d.make_audio_client)(&ClientOptions::new("k")).is_some());
    assert!(d.image_capabilities(&d.supported_image_models[0]).is_some());
    assert!(d.video_capabilities(&d.supported_video_models[0]).is_some());
    assert!(d.audio_capabilities(&d.supported_audio_models[0]).is_some());
    assert!(!d.supports_image_model(&img("riverflow-2-max")));

    let reg = ProviderRegistry::new(false);
    reg.register(d);
    assert!(reg.descriptor(&ProviderId::fal()).is_some());
    let _client = ProviderClient::new(d, "key");
}

// ----- HTTP shapes (wiremock) ---------------------------------------------------------------------

fn queue_result(server: &MockServer, endpoint: &str, request_id: &str) -> Value {
    json!({
        "request_id": request_id,
        "status_url": format!("{}/{endpoint}/requests/{request_id}/status", server.uri()),
        "response_url": format!("{}/{endpoint}/requests/{request_id}", server.uri()),
    })
}

#[test]
fn queue_image_flow_with_data_uri_result() {
    crate::rt().block_on(queue_image_flow_with_data_uri_result_inner());
}

async fn queue_image_flow_with_data_uri_result_inner() {
    let server = MockServer::start().await;
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    Mock::given(method("POST"))
        .and(path("/fal-ai/nano-banana-pro"))
        .and(header("Authorization", "Key test-key"))
        .and(header("Content-Type", "application/json"))
        .and(body_partial_json(json!({
            "prompt": "a cat",
            "enable_safety_checker": false,
            "aspect_ratio": "16:9",
            "resolution": "2K",
            "output_format": "png"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/nano-banana-pro", "req-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/nano-banana-pro/requests/req-1/status"))
        .and(header("Authorization", "Key test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/nano-banana-pro/requests/req-1"))
        .and(header("Authorization", "Key test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [{ "url": format!("data:image/png;base64,{}", b64(&png)), "content_type": "image/png" }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let result = mock_client(&server)
        .generate_image("a cat", &img(ids::GEMINI_3_PRO), &[], Some(AspectRatio::Landscape), Some(ImageResolution::Fhd))
        .await
        .unwrap();
    assert_eq!(result, png);
}

#[test]
fn queue_image_flow_uses_edit_endpoint_with_references_and_falls_back_to_constructed_urls() {
    crate::rt().block_on(queue_image_flow_uses_edit_endpoint_with_references_and_falls_back_to_constructed_urls_inner());
}

async fn queue_image_flow_uses_edit_endpoint_with_references_and_falls_back_to_constructed_urls_inner() {
    let server = MockServer::start().await;
    let png = vec![0x89, b'P', b'N', b'G'];
    let reference = ProviderAsset::new(AssetRole::ReferenceImage, "image/png", REFERENCE_PNG.to_vec());
    let expected_uri = format!("data:image/png;base64,{}", b64(REFERENCE_PNG));
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux-2-pro/edit"))
        .and(body_partial_json(json!({ "image_urls": [expected_uri] })))
        // No status_url / response_url: the client must build them from the queue base URL.
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "request_id": "req-2" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux-2-pro/edit/requests/req-2/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux-2-pro/edit/requests/req-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [{ "url": format!("data:image/png;base64,{}", b64(&png)), "content_type": "image/png" }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let result = mock_client(&server).generate_image("edit", &img(ids::FLUX_2_PRO), &[reference], None, None).await.unwrap();
    assert_eq!(result, png);
}

#[test]
fn queue_image_flow_downloads_cdn_url_and_transcodes_to_png() {
    crate::rt().block_on(queue_image_flow_downloads_cdn_url_and_transcodes_to_png_inner());
}

async fn queue_image_flow_downloads_cdn_url_and_transcodes_to_png_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/recraft/v4/pro/text-to-image"))
        .and(body_partial_json(json!({ "image_size": "square_hd" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/recraft/v4/pro/text-to-image", "req-3")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/recraft/v4/pro/text-to-image/requests/req-3/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/recraft/v4/pro/text-to-image/requests/req-3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [{ "url": format!("{}/cdn/out.jpg", server.uri()), "content_type": "image/jpeg" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn/out.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tiny_jpeg()))
        .expect(1)
        .mount(&server)
        .await;

    let result = mock_client(&server).generate_image("p", &img(ids::RECRAFT_V4_PRO), &[], Some(AspectRatio::Square), None).await.unwrap();
    assert!(result.starts_with(&[0x89, b'P', b'N', b'G']));
}

#[test]
fn queue_polls_while_in_queue_then_completes() {
    crate::rt().block_on(queue_polls_while_in_queue_then_completes_inner());
}

async fn queue_polls_while_in_queue_then_completes_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux/schnell"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/flux/schnell", "req-4")))
        .mount(&server)
        .await;
    // First poll: IN_QUEUE (3 s backoff), then COMPLETED.
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux/schnell/requests/req-4/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "IN_QUEUE" })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux/schnell/requests/req-4/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux/schnell/requests/req-4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [{ "url": format!("data:image/png;base64,{}", b64(&[1, 2, 3])), "content_type": "image/png" }]
        })))
        .mount(&server)
        .await;

    let started = std::time::Instant::now();
    let result = mock_client(&server).generate_image("p", &img(ids::FLUX_1_SCHNELL), &[], None, None).await.unwrap();
    assert_eq!(result, vec![1, 2, 3]);
    assert!(started.elapsed() >= std::time::Duration::from_secs(3), "expected the 3 s backoff");
}

#[test]
fn queue_failed_status_is_provider_failed() {
    crate::rt().block_on(queue_failed_status_is_provider_failed_inner());
}

async fn queue_failed_status_is_provider_failed_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux/dev"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/flux/dev", "req-5")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux/dev/requests/req-5/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "FAILED", "detail": "model crashed" })))
        .mount(&server)
        .await;

    let err = mock_client(&server).generate_image("p", &img(ids::FLUX_1_DEV), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::ProviderFailed("model crashed".into()));
}

#[test]
fn queue_failed_status_without_detail_uses_kind_message() {
    crate::rt().block_on(queue_failed_status_without_detail_uses_kind_message_inner());
}

async fn queue_failed_status_without_detail_uses_kind_message_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux/dev"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/flux/dev", "req-6")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux/dev/requests/req-6/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "FAILED" })))
        .mount(&server)
        .await;

    let err = mock_client(&server).generate_image("p", &img(ids::FLUX_1_DEV), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::ProviderFailed("The model failed to generate the image".into()));
}

#[test]
fn submit_http_errors_map_to_generation_errors() {
    crate::rt().block_on(submit_http_errors_map_to_generation_errors_inner());
}

async fn submit_http_errors_map_to_generation_errors_inner() {
    let cases: Vec<(u16, Value, GenerationError)> = vec![
        (401, json!({ "detail": "bad key" }), GenerationError::Unauthorized("bad key".into())),
        (402, json!({ "detail": "no credits" }), GenerationError::PaymentRequired("no credits".into())),
        (403, json!({ "detail": "forbidden" }), GenerationError::Unauthorized("forbidden".into())),
        (429, json!({ "detail": "slow down" }), GenerationError::RateLimited("slow down".into())),
        (503, json!({ "detail": "overloaded" }), GenerationError::ServerError { status_code: Some(503), message: "overloaded".into() }),
        (422, json!({ "detail": "bad field" }), GenerationError::InvalidRequest("bad field".into())),
        (
            422,
            json!({ "detail": [{ "msg": "Unsafe prompt", "type": "content_policy_violation" }] }),
            GenerationError::ContentFiltered("Unsafe prompt".into()),
        ),
        (418, json!({ "detail": "teapot" }), GenerationError::Unknown("HTTP 418: teapot".into())),
    ];
    for (status, body, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fal-ai/flux/dev"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let err = mock_client(&server).generate_image("p", &img(ids::FLUX_1_DEV), &[], None, None).await.unwrap_err();
        assert_eq!(err, expected, "status {status}");
    }
}

#[test]
fn status_poll_http_error_is_mapped() {
    crate::rt().block_on(status_poll_http_error_is_mapped_inner());
}

async fn status_poll_http_error_is_mapped_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux/dev"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/flux/dev", "req-7")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/flux/dev/requests/req-7/status"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "detail": "boom" })))
        .mount(&server)
        .await;
    let err = mock_client(&server).generate_image("p", &img(ids::FLUX_1_DEV), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::ServerError { status_code: Some(500), message: "boom".into() });
}

#[test]
fn upscale_uses_sync_endpoint_and_downloads_result() {
    crate::rt().block_on(upscale_uses_sync_endpoint_and_downloads_result_inner());
}

async fn upscale_uses_sync_endpoint_and_downloads_result_inner() {
    let server = MockServer::start().await;
    let input = vec![0x89, b'P', b'N', b'G'];
    let output = vec![0x89, b'P', b'N', b'G', 9, 9];
    Mock::given(method("POST"))
        .and(path("/fal-ai/topaz/upscale/image"))
        .and(header("Authorization", "Key test-key"))
        .and(body_partial_json(json!({
            "image_url": format!("data:image/png;base64,{}", b64(&input)),
            "model": "Standard V2",
            "upscale_factor": 2,
            "output_format": "png"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "image": { "url": format!("{}/cdn/up.png", server.uri()) } })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/up.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(output.clone())).expect(1).mount(&server).await;

    let settings = ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone());
    let result = upscale_image(&server, &settings, &input).await.unwrap();
    assert_eq!(result, output);
}

/// The factor and Topaz variant the composer picked reach the request; the variant travels as a
/// stable slug and is mapped to fal's own wire string here.
#[test]
fn upscale_sends_the_selected_factor_and_variant() {
    crate::rt().block_on(upscale_sends_the_selected_factor_and_variant_inner());
}

async fn upscale_sends_the_selected_factor_and_variant_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/topaz/upscale/image"))
        .and(body_partial_json(json!({ "model": "High Fidelity V2", "upscale_factor": 4 })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "image": { "url": format!("{}/cdn/up.png", server.uri()) } })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/up.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7])).mount(&server).await;

    let settings = ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone()).with_factor(4).with_variant("high-fidelity-v2");
    assert_eq!(upscale_image(&server, &settings, &[1]).await.unwrap(), vec![7]);
}

/// A variant slug the table no longer knows falls back to the model's default rather than being
/// forwarded as-is, so a request stored before a variant was dropped still runs.
#[test]
fn upscale_falls_back_to_the_default_variant() {
    crate::rt().block_on(upscale_falls_back_to_the_default_variant_inner());
}

async fn upscale_falls_back_to_the_default_variant_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/topaz/upscale/image"))
        .and(body_partial_json(json!({ "model": "Standard V2" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "image": { "url": format!("{}/cdn/up.png", server.uri()) } })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/up.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7])).mount(&server).await;

    let settings = ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone()).with_variant("no-such-variant");
    assert_eq!(upscale_image(&server, &settings, &[1]).await.unwrap(), vec![7]);
}

#[test]
fn remove_background_uses_sync_endpoint() {
    crate::rt().block_on(remove_background_uses_sync_endpoint_inner());
}

async fn remove_background_uses_sync_endpoint_inner() {
    let server = MockServer::start().await;
    let input = vec![1, 2, 3];
    let output = vec![4, 5, 6];
    Mock::given(method("POST"))
        .and(path("/fal-ai/bria/background/remove"))
        .and(body_partial_json(json!({ "image_url": format!("data:image/png;base64,{}", b64(&input)) })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "image": { "url": format!("{}/cdn/bg.png", server.uri()) } })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/bg.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(output.clone())).mount(&server).await;

    let settings = ToolSettings::new(catalog::tool::BRIA_BACKGROUND_REMOVE.clone());
    let result = upscale_image(&server, &settings, &input).await.unwrap();
    assert_eq!(result, output);
}

#[test]
fn sync_endpoint_errors_and_bad_downloads_are_mapped() {
    crate::rt().block_on(sync_endpoint_errors_and_bad_downloads_are_mapped_inner());
}

async fn sync_endpoint_errors_and_bad_downloads_are_mapped_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/bria/background/remove"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({ "detail": "nope" })))
        .mount(&server)
        .await;
    let err = upscale_image(&server, &ToolSettings::new(catalog::tool::BRIA_BACKGROUND_REMOVE.clone()), &[1]).await.unwrap_err();
    assert_eq!(err, GenerationError::Unauthorized("nope".into()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/topaz/upscale/image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "image": { "url": format!("{}/cdn/missing.png", server.uri()) } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/missing.png")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    let err = upscale_image(&server, &ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone()), &[1]).await.unwrap_err();
    assert_eq!(err, GenerationError::Unknown("Invalid image data received from the server".into()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/topaz/upscale/image"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let err = upscale_image(&server, &ToolSettings::new(catalog::tool::TOPAZ_UPSCALE.clone()), &[1]).await.unwrap_err();
    assert!(matches!(err, GenerationError::Unknown(ref m) if m.starts_with("Failed to decode response")), "{err:?}");
}

/// A video upscale is a queue job, not a synchronous call, and its input goes through fal's storage
/// rather than a data URI. `H264_output` must be `true`: the endpoint defaults to H265, which
/// `majik_core::video` cannot decode, so the result would land in the library unplayable.
#[test]
fn upscale_video_uploads_the_clip_and_queues_h264() {
    crate::rt().block_on(upscale_video_uploads_the_clip_and_queues_h264_inner());
}

async fn upscale_video_uploads_the_clip_and_queues_h264_inner() {
    let server = MockServer::start().await;
    let clip = vec![0, 0, 0, 0x18, b'f', b't', b'y', b'p'];
    let output = vec![0, 0, 0, 0x18, b'f', b't', b'y', b'p', 9];

    Mock::given(method("POST"))
        .and(path("/storage/upload/initiate"))
        .and(header("Authorization", "Key test-key"))
        .and(body_partial_json(json!({ "content_type": "video/mp4", "file_name": "input.mp4" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "file_url": format!("{}/cdn/input.mp4", server.uri()),
            "upload_url": format!("{}/signed/put", server.uri()),
        })))
        .expect(1)
        .mount(&server)
        .await;
    // The signed URL is the auth; sending our key to whatever host it names would leak it.
    Mock::given(method("PUT"))
        .and(path("/signed/put"))
        .and(header("Content-Type", "video/mp4"))
        .and(wiremock::matchers::body_bytes(clip.clone()))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/topaz/upscale/video"))
        .and(body_partial_json(json!({
            "video_url": format!("{}/cdn/input.mp4", server.uri()),
            "model": "Proteus",
            "upscale_factor": 4,
            "H264_output": true,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "request_id": "vid-1" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/topaz/upscale/video/requests/vid-1/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/topaz/upscale/video/requests/vid-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "video": { "url": format!("{}/cdn/out.mp4", server.uri()) } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/out.mp4")).respond_with(ResponseTemplate::new(200).set_body_bytes(output.clone())).expect(1).mount(&server).await;

    let settings = ToolSettings::new(catalog::tool::TOPAZ_UPSCALE_VIDEO.clone()).with_factor(4);
    let result = run_tool(&server, &settings, AssetRole::ReferenceVideo, "video/mp4", &clip).await.unwrap();
    assert_eq!(result, output);
}

/// Going through the queue is what gives a video upscale a resumable handle: without one, a job
/// still running when the app is relaunched could not be re-attached to.
#[test]
fn upscale_video_reports_its_queue_handle() {
    crate::rt().block_on(upscale_video_reports_its_queue_handle_inner());
}

async fn upscale_video_reports_its_queue_handle_inner() {
    let server = MockServer::start().await;
    mount_upload(&server).await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/topaz/upscale/video"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "request_id": "vid-2" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/topaz/upscale/video/requests/vid-2/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/topaz/upscale/video/requests/vid-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "video": { "url": format!("{}/cdn/out.mp4", server.uri()) } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/out.mp4")).respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1])).mount(&server).await;

    let seen: Arc<Mutex<Vec<JobHandle>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let client = mock_client(&server).with_on_accepted(Arc::new(move |handle| sink.lock().unwrap().push(handle)));
    let settings = ToolSettings::new(catalog::tool::TOPAZ_UPSCALE_VIDEO.clone());
    let input = ProviderAsset::new(AssetRole::ReferenceVideo, "video/mp4", vec![0, 0, 0, 0x18, b'f', b't', b'y', b'p']);
    client.run_tool(&settings, &input).await.unwrap();

    let handles = seen.lock().unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].job_id, "vid-2");
    assert!(handles[0].poll_url.as_deref().is_some_and(|u| u.ends_with("/requests/vid-2/status")), "{:?}", handles[0].poll_url);
}

/// A failed upload has to surface as a real error rather than as a request with no video in it.
#[test]
fn upscale_video_maps_upload_failures() {
    crate::rt().block_on(upscale_video_maps_upload_failures_inner());
}

async fn upscale_video_maps_upload_failures_inner() {
    let settings = ToolSettings::new(catalog::tool::TOPAZ_UPSCALE_VIDEO.clone());
    let clip = vec![0, 0, 0, 0x18, b'f', b't', b'y', b'p'];

    // The initiate call itself is refused.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storage/upload/initiate"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({ "detail": "bad key" })))
        .mount(&server)
        .await;
    let err = run_tool(&server, &settings, AssetRole::ReferenceVideo, "video/mp4", &clip).await.unwrap_err();
    assert_eq!(err, GenerationError::Unauthorized("bad key".into()));

    // The signed PUT fails, so nothing was ever stored to point the job at.
    let server = MockServer::start().await;
    mount_upload_initiate(&server).await;
    Mock::given(method("PUT")).and(path("/signed/put")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    let err = run_tool(&server, &settings, AssetRole::ReferenceVideo, "video/mp4", &clip).await.unwrap_err();
    assert_eq!(err, GenerationError::Unknown("Failed to upload the input: HTTP 500".into()));
}

async fn mount_upload_initiate(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/storage/upload/initiate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "file_url": format!("{}/cdn/input.mp4", server.uri()),
            "upload_url": format!("{}/signed/put", server.uri()),
        })))
        .mount(server)
        .await;
}

async fn mount_upload(server: &MockServer) {
    mount_upload_initiate(server).await;
    Mock::given(method("PUT")).and(path("/signed/put")).respond_with(ResponseTemplate::new(200)).mount(server).await;
}

#[test]
fn queue_video_flow_downloads_video() {
    crate::rt().block_on(queue_video_flow_downloads_video_inner());
}

async fn queue_video_flow_downloads_video_inner() {
    let server = MockServer::start().await;
    let video = vec![0, 0, 0, 0x18, b'f', b't', b'y', b'p'];
    let expected_frame = format!("data:image/png;base64,{}", b64(FRAME));
    Mock::given(method("POST"))
        .and(path("/fal-ai/veo3.1/image-to-video"))
        .and(header("Authorization", "Key test-key"))
        .and(body_partial_json(json!({
            "prompt": "zoom",
            "enable_safety_checker": false,
            "aspect_ratio": "16:9",
            "resolution": "1080p",
            "duration": "8s",
            "image_url": expected_frame,
            "generate_audio": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/veo3.1/image-to-video", "vid-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/image-to-video/requests/vid-1/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/image-to-video/requests/vid-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "response": { "video": { "url": format!("{}/cdn/v.mp4", server.uri()) } } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/v.mp4")).respond_with(ResponseTemplate::new(200).set_body_bytes(video.clone())).expect(1).mount(&server).await;

    let settings = video_settings(ids::VEO_31, Some(VideoAspectRatio::Landscape), Some(VideoResolution::Fhd), 8, false);
    let first = ProviderAsset::new(AssetRole::FirstFrame, "image/png", FRAME.to_vec());
    let result = mock_client(&server).generate_video("zoom", &[first], &settings).await.unwrap();
    assert_eq!(result, video);
}

#[test]
fn queue_video_flow_download_failures() {
    crate::rt().block_on(queue_video_flow_download_failures_inner());
}

async fn queue_video_flow_download_failures_inner() {
    // Video download HTTP error.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/veo3.1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/veo3.1", "vid-2")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/requests/vid-2/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/requests/vid-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "video": { "url": format!("{}/cdn/gone.mp4", server.uri()) } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/gone.mp4")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    let settings = video_settings(ids::VEO_31, None, None, 4, true);
    let err = mock_client(&server).generate_video("p", &[], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::Unknown("Failed to download video: HTTP 404".into()));

    // Result without a video → no result.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/veo3.1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/veo3.1", "vid-3")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/requests/vid-3/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/requests/vid-3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "seed": 1 })))
        .mount(&server)
        .await;
    let err = mock_client(&server).generate_video("p", &[], &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::NoResultGenerated);
}

#[test]
fn queue_audio_flow_downloads_audio() {
    crate::rt().block_on(queue_audio_flow_downloads_audio_inner());
}

async fn queue_audio_flow_downloads_audio_inner() {
    let server = MockServer::start().await;
    let mp3 = vec![b'I', b'D', b'3', 4, 0];
    Mock::given(method("POST"))
        .and(path("/fal-ai/elevenlabs/tts/eleven-v3"))
        .and(header("Authorization", "Key test-key"))
        .and(body_partial_json(json!({ "text": "Hello there", "voice": "Rachel" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/elevenlabs/tts/eleven-v3", "aud-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/elevenlabs/tts/eleven-v3/requests/aud-1/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/elevenlabs/tts/eleven-v3/requests/aud-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "audio": { "url": format!("{}/cdn/a.mp3", server.uri()), "content_type": "audio/mpeg" } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/a.mp3")).respond_with(ResponseTemplate::new(200).set_body_bytes(mp3.clone())).expect(1).mount(&server).await;

    let settings = audio_settings(ids::ELEVEN_LABS_V3, "Rachel", None);
    let result = mock_client(&server).generate_audio("Hello there", &settings).await.unwrap();
    assert_eq!(result, mp3);
}

#[test]
fn queue_audio_dialogue_flow_posts_inputs_and_maps_download_errors() {
    crate::rt().block_on(queue_audio_dialogue_flow_posts_inputs_and_maps_download_errors_inner());
}

async fn queue_audio_dialogue_flow_posts_inputs_and_maps_download_errors_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/elevenlabs/text-to-dialogue/eleven-v3"))
        .and(body_partial_json(json!({ "inputs": [{ "text": "Hi.", "voice": "Rachel" }, { "text": "Yo.", "voice": "Roger" }] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/elevenlabs/text-to-dialogue/eleven-v3", "aud-2")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/elevenlabs/text-to-dialogue/eleven-v3/requests/aud-2/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/elevenlabs/text-to-dialogue/eleven-v3/requests/aud-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "audio": { "url": format!("{}/cdn/missing.mp3", server.uri()) } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/missing.mp3")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

    let settings = audio_settings(ids::ELEVEN_LABS_V3, "Rachel", Some("Roger"));
    let err = mock_client(&server).generate_audio("Speaker 1: Hi.\nSpeaker 2: Yo.", &settings).await.unwrap_err();
    assert_eq!(err, GenerationError::Unknown("Failed to download audio: HTTP 500".into()));
}

#[test]
fn gemini_tts_flow_posts_speakers() {
    crate::rt().block_on(gemini_tts_flow_posts_speakers_inner());
}

async fn gemini_tts_flow_posts_speakers_inner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/gemini-tts"))
        .and(body_partial_json(json!({
            "model": "gemini-2.5-pro-tts",
            "prompt": "Speaker1: A\nSpeaker2: B",
            "speakers": [{ "speaker_id": "Speaker1", "voice": "Kore" }, { "speaker_id": "Speaker2", "voice": "Puck" }],
            "output_format": "mp3"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/gemini-tts", "aud-3")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/gemini-tts/requests/aud-3/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/gemini-tts/requests/aud-3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "audio": { "url": format!("{}/cdn/g.mp3", server.uri()) } })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/cdn/g.mp3")).respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7, 7])).mount(&server).await;

    let settings = audio_settings(ids::GEMINI_25_PRO, "Kore", Some("Puck"));
    let result = mock_client(&server).generate_audio("Speaker 1: A\nSpeaker 2: B", &settings).await.unwrap();
    assert_eq!(result, vec![7, 7]);
}

// ----- Queue handles and resume (relaunch recovery) ----------------------------------------------

mod resume {
    use super::*;
    use majik_core::model::MediaType;
    use majik_providers::{JobHandle, ResumableClient as _};
    use std::sync::{Arc, Mutex};

    fn status_url(server: &MockServer, request_id: &str) -> String {
        format!("{}/fal-ai/nano-banana-pro/requests/{request_id}/status", server.uri())
    }

    async fn mount_completed(server: &MockServer, request_id: &str, result: Value) {
        Mock::given(method("GET"))
            .and(path(format!("/fal-ai/nano-banana-pro/requests/{request_id}/status")))
            .and(header("Authorization", "Key test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "COMPLETED" })))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/fal-ai/nano-banana-pro/requests/{request_id}")))
            .and(header("Authorization", "Key test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(result))
            .expect(1)
            .mount(server)
            .await;
    }

    #[test]
    fn queue_submit_reports_the_accepted_handle_before_polling() {
        crate::rt().block_on(queue_submit_reports_the_accepted_handle_before_polling_inner());
    }

    async fn queue_submit_reports_the_accepted_handle_before_polling_inner() {
        let server = MockServer::start().await;
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        Mock::given(method("POST"))
            .and(path("/fal-ai/nano-banana-pro"))
            .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/nano-banana-pro", "req-9")))
            .mount(&server)
            .await;
        mount_completed(&server, "req-9", json!({ "images": [{ "url": format!("data:image/png;base64,{}", b64(&png)), "content_type": "image/png" }] })).await;

        let seen: Arc<Mutex<Vec<JobHandle>>> = Default::default();
        let sink = seen.clone();
        let client = mock_client(&server).with_on_accepted(Arc::new(move |handle| sink.lock().unwrap().push(handle)));
        client.generate_image("a cat", &img(ids::GEMINI_3_PRO), &[], None, None).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![JobHandle { job_id: "req-9".into(), poll_url: Some(status_url(&server, "req-9")) }]);
    }

    #[test]
    fn queue_run_traces_submit_poll_result_and_download_without_the_key() {
        crate::rt().block_on(queue_run_traces_submit_poll_result_and_download_without_the_key_inner());
    }

    async fn queue_run_traces_submit_poll_result_and_download_without_the_key_inner() {
        let server = MockServer::start().await;
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        Mock::given(method("POST"))
            .and(path("/fal-ai/nano-banana-pro"))
            .respond_with(ResponseTemplate::new(200).set_body_json(queue_result(&server, "fal-ai/nano-banana-pro", "req-9")))
            .mount(&server)
            .await;
        mount_completed(&server, "req-9", json!({ "images": [{ "url": format!("{}/out.png", server.uri()), "content_type": "image/png" }] })).await;
        Mock::given(method("GET")).and(path("/out.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone())).mount(&server).await;

        let seen: Arc<Mutex<Vec<majik_core::model::JobTrace>>> = Default::default();
        let sink = seen.clone();
        let client = mock_client(&server).with_on_trace(Arc::new(move |trace| sink.lock().unwrap().push(trace)));
        client.generate_image("a cat", &img(ids::GEMINI_3_PRO), &[], None, None).await.unwrap();
        let traces = seen.lock().unwrap().clone();
        use majik_core::model::TraceLabel::*;
        assert_eq!(traces.iter().map(|t| t.label).collect::<Vec<_>>(), [Submit, Poll, Result, Download]);
        assert_eq!((traces[0].method.as_str(), traces[0].status), ("POST", Some(200)));
        assert!(traces[0].url.ends_with("/fal-ai/nano-banana-pro"), "{}", traces[0].url);
        assert!(traces[0].request_body.as_deref().unwrap_or_default().contains(r#""prompt":"a cat""#), "{:?}", traces[0].request_body);
        assert!(traces[0].response_body.as_deref().unwrap_or_default().contains("req-9"), "{:?}", traces[0].response_body);
        assert_eq!(traces[1].url, status_url(&server, "req-9"));
        assert!(traces[2].response_body.as_deref().unwrap_or_default().contains("out.png"));
        assert_eq!(traces[3].response_body.as_deref(), Some("7 bytes"), "a download records its size");
        for trace in &traces {
            let text = format!("{trace:?}");
            assert!(!text.contains("test-key") && !text.contains("Authorization"), "no header in the trail: {text}");
        }
    }

    #[test]
    fn resume_polls_the_status_url_then_fetches_the_image() {
        crate::rt().block_on(resume_polls_the_status_url_then_fetches_the_image_inner());
    }

    async fn resume_polls_the_status_url_then_fetches_the_image_inner() {
        let server = MockServer::start().await;
        let png = vec![0x89, b'P', b'N', b'G', 4, 5, 6];
        mount_completed(&server, "req-1", json!({ "images": [{ "url": format!("data:image/png;base64,{}", b64(&png)), "content_type": "image/png" }] })).await;
        let handle = JobHandle { job_id: "req-1".into(), poll_url: Some(status_url(&server, "req-1")) };
        let bytes = mock_client(&server).resume(&handle, MediaType::Image).await.unwrap();
        assert_eq!(bytes, png, "no submit: the stored handle is enough");
    }

    #[test]
    fn resume_downloads_a_video_result() {
        crate::rt().block_on(resume_downloads_a_video_result_inner());
    }

    async fn resume_downloads_a_video_result_inner() {
        let server = MockServer::start().await;
        let clip = b"not really an mp4".to_vec();
        mount_completed(&server, "req-2", json!({ "video": { "url": format!("{}/clip.mp4", server.uri()) } })).await;
        Mock::given(method("GET")).and(path("/clip.mp4")).respond_with(ResponseTemplate::new(200).set_body_bytes(clip.clone())).expect(1).mount(&server).await;
        let handle = JobHandle { job_id: "req-2".into(), poll_url: Some(status_url(&server, "req-2")) };
        assert_eq!(mock_client(&server).resume(&handle, MediaType::Video).await.unwrap(), clip);
    }

    #[test]
    fn resume_downloads_an_audio_result() {
        crate::rt().block_on(resume_downloads_an_audio_result_inner());
    }

    async fn resume_downloads_an_audio_result_inner() {
        let server = MockServer::start().await;
        let audio = b"RIFF....".to_vec();
        mount_completed(&server, "req-3", json!({ "audio": { "url": format!("{}/out.wav", server.uri()) } })).await;
        Mock::given(method("GET")).and(path("/out.wav")).respond_with(ResponseTemplate::new(200).set_body_bytes(audio.clone())).expect(1).mount(&server).await;
        let handle = JobHandle { job_id: "req-3".into(), poll_url: Some(status_url(&server, "req-3")) };
        assert_eq!(mock_client(&server).resume(&handle, MediaType::Audio).await.unwrap(), audio);
    }

    #[test]
    fn resume_of_a_request_the_queue_forgot_is_job_gone() {
        crate::rt().block_on(resume_of_a_request_the_queue_forgot_is_job_gone_inner());
    }

    async fn resume_of_a_request_the_queue_forgot_is_job_gone_inner() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fal-ai/nano-banana-pro/requests/req-4/status"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "detail": "Request not found" })))
            .mount(&server)
            .await;
        let handle = JobHandle { job_id: "req-4".into(), poll_url: Some(status_url(&server, "req-4")) };
        assert_eq!(mock_client(&server).resume(&handle, MediaType::Image).await.unwrap_err(), GenerationError::JobGone);
    }

    #[test]
    fn resume_needs_the_status_url() {
        crate::rt().block_on(resume_needs_the_status_url_inner());
    }

    async fn resume_needs_the_status_url_inner() {
        let server = MockServer::start().await;
        let handle = JobHandle { job_id: "req-5".into(), poll_url: None };
        assert!(matches!(mock_client(&server).resume(&handle, MediaType::Image).await.unwrap_err(), GenerationError::InvalidRequest(_)));
    }
}

// ----- prompt improvement (fal-ai/any-llm) ------------------------------------------------------

#[test]
fn text_request_posts_any_llm_with_the_instruction_as_the_system_prompt() {
    crate::rt().block_on(text_request_posts_any_llm_with_the_instruction_as_the_system_prompt_inner());
}

async fn text_request_posts_any_llm_with_the_instruction_as_the_system_prompt_inner() {
    use majik_providers::TextProviderClient as _;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/any-llm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "output": "  a red apple on a table  ", "partial": false })))
        .mount(&server)
        .await;

    let text = mock_client(&server).complete_text("rewrite it", "apple", 400).await.unwrap();
    assert_eq!(text, "a red apple on a table");

    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(request.headers.get("authorization").unwrap(), "Key test-key");
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "anthropic/claude-haiku-4.5");
    assert_eq!(body["prompt"], "apple");
    assert_eq!(body["system_prompt"], "rewrite it");
    assert_eq!(body["max_tokens"], 400);
}

#[test]
fn an_error_or_empty_output_from_any_llm_fails_the_rewrite() {
    crate::rt().block_on(an_error_or_empty_output_from_any_llm_fails_the_rewrite_inner());
}

async fn an_error_or_empty_output_from_any_llm_fails_the_rewrite_inner() {
    use majik_providers::TextProviderClient as _;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/any-llm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "output": "", "error": "model unavailable" })))
        .mount(&server)
        .await;
    assert!(matches!(mock_client(&server).complete_text("s", "u", 100).await, Err(GenerationError::ProviderFailed(m)) if m == "model unavailable"));

    let empty = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/any-llm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "output": "   " })))
        .mount(&empty)
        .await;
    assert!(matches!(mock_client(&empty).complete_text("s", "u", 100).await, Err(GenerationError::NoResultGenerated)));
}

#[test]
fn a_text_http_error_maps_like_any_other_fal_call() {
    crate::rt().block_on(a_text_http_error_maps_like_any_other_fal_call_inner());
}

async fn a_text_http_error_maps_like_any_other_fal_call_inner() {
    use majik_providers::TextProviderClient as _;
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/fal-ai/any-llm")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    assert!(matches!(mock_client(&server).complete_text("s", "u", 100).await, Err(GenerationError::Unauthorized(_))));
}
