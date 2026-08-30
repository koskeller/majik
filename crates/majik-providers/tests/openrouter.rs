//! OpenRouter: request bodies, response parsing, capabilities, plus wiremock end-to-end coverage
//! of the HTTP path.

use base64::Engine as _;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request as WmRequest, ResponseTemplate};

use majik_providers::openrouter::capabilities::{self, SUPPORTED_IMAGE_MODEL_IDS};
use majik_providers::ClientOptions;
use majik_providers::openrouter::models::{Choice, ChoiceError, ImageUrl, Response, ResponseImage, ResponseMessage};
use majik_providers::openrouter::provider::{build_request, check_for_embedded_errors, extract_image_data, parse_response};
use majik_providers::openrouter::{descriptor, OpenRouterClient, OpenRouterError};
use majik_providers::{AspectRatio, AssetRole, GenerationError, ImageModel, ImageProviderClient, ImageResolution, ProviderAsset, ProviderId, ToolId};

/// One runtime for every test in this binary. `http::client()` is a process-wide `reqwest::Client`
/// whose pooled connections each carry a dispatch task owned by the runtime that opened them. Give
/// every test its own runtime and that task dies with the test while the idle connection stays in
/// the pool — so when a later wiremock server binds the port the dead one released, the next test
/// handed that connection fails with "dispatch task is gone: runtime dropped the dispatch task".
/// Which test loses is a race, so it fails on one runner and passes on the others. One runtime for
/// the whole binary keeps every dispatch task alive as long as the pool that refers to it, the way
/// `e2e.rs` already shares its own.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("test runtime"))
}

// ----- helpers ----------------------------------------------------------------------------------

fn make_response(choices: Vec<Choice>) -> Response {
    Response { id: "gen-test".into(), choices, created: 0, model: "test-model".into(), object: "chat.completion".into(), usage: None, system_fingerprint: None }
}

fn make_choice(content: Option<&str>, images: Option<Vec<ResponseImage>>, finish_reason: Option<&str>, error: Option<ChoiceError>) -> Choice {
    Choice {
        message: ResponseMessage { role: Some("assistant".into()), content: content.map(str::to_string), images, tool_calls: None },
        finish_reason: finish_reason.map(str::to_string),
        native_finish_reason: None,
        error,
    }
}

fn make_image(url: &str) -> ResponseImage {
    ResponseImage { image_url: ImageUrl { url: url.into() } }
}

fn error_body(message: &str, metadata: Option<Value>) -> Vec<u8> {
    let mut error = json!({ "message": message });
    if let Some(m) = metadata {
        error["metadata"] = m;
    }
    serde_json::to_vec(&json!({ "error": error })).unwrap()
}

/// A catalog-independent model carrying an OpenRouter-supported id.
fn model(id: &'static str) -> ImageModel {
    ImageModel::new(id, "Test", "Test", "logo-test", "")
}

fn flux1_dev() -> ImageModel {
    model("flux-1-dev") // not on OpenRouter
}

fn png_bytes() -> Vec<u8> {
    vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
}

fn data_uri(bytes: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn success_body(bytes: &[u8]) -> Value {
    json!({
        "id": "gen-1",
        "choices": [{
            "message": { "role": "assistant", "content": "", "images": [{ "image_url": { "url": data_uri(bytes) } }] },
            "finish_reason": "stop"
        }],
        "created": 1_700_000_000,
        "model": "google/gemini-2.5-flash-image",
        "object": "chat.completion"
    })
}

async fn server_and_client() -> (MockServer, OpenRouterClient) {
    let server = MockServer::start().await;
    let client = OpenRouterClient::with_endpoint("test-key", format!("{}/api/v1/chat/completions", server.uri()));
    (server, client)
}

#[test]
fn chat_completion_traces_one_submit_with_data_uris_elided_and_no_key() {
    crate::rt().block_on(chat_completion_traces_one_submit_with_data_uris_elided_and_no_key_inner());
}

async fn chat_completion_traces_one_submit_with_data_uris_elided_and_no_key_inner() {
    let (server, client) = server_and_client().await;
    let png = vec![0x89, b'P', b'N', b'G', 9, 9, 9];
    let response = json!({
        "id": "gen-1",
        "choices": [{
            "message": { "role": "assistant", "content": "", "images": [{ "image_url": { "url": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&png)) } }] },
            "finish_reason": "stop"
        }],
        "created": 1_700_000_000,
        "model": "google/gemini-2.5-flash-image",
        "object": "chat.completion"
    });
    Mock::given(method("POST")).and(path("/api/v1/chat/completions")).respond_with(ResponseTemplate::new(200).set_body_json(response)).mount(&server).await;
    let seen: std::sync::Arc<std::sync::Mutex<Vec<majik_core::model::JobTrace>>> = Default::default();
    let sink = seen.clone();
    let client = client.with_on_trace(std::sync::Arc::new(move |trace| sink.lock().unwrap().push(trace)));
    let model = majik_providers::catalog::image::model(SUPPORTED_IMAGE_MODEL_IDS[0]).unwrap().clone();
    let reference = ProviderAsset { role: AssetRole::ReferenceImage, media_type: "image/png".into(), data: png.clone(), attributes: None };
    client.generate_image("a cat", &model, &[reference], None, None).await.unwrap();
    let traces = seen.lock().unwrap().clone();
    assert_eq!(traces.len(), 1, "one synchronous POST is the whole exchange");
    let trace = &traces[0];
    assert_eq!((trace.label, trace.method.as_str(), trace.status), (majik_core::model::TraceLabel::Submit, "POST", Some(200)));
    assert!(trace.url.ends_with("/api/v1/chat/completions"));
    let request = trace.request_body.as_deref().unwrap_or_default();
    assert!(request.contains(r#""text":"a cat""#), "{request}");
    assert!(request.contains("data:image/png;base64,…[") && !request.contains(&base64::engine::general_purpose::STANDARD.encode(&png)), "the reference image is elided: {request}");
    assert!(trace.response_body.as_deref().unwrap_or_default().contains("bytes elided]"));
    let text = format!("{trace:?}");
    assert!(!text.contains("test-key") && !text.contains("Bearer"), "no header in the trail: {text}");
}

// ----- parse_response ---------------------------------------------------------------------------

#[test]
fn parse_response_valid_json() {
    let body = serde_json::to_vec(&json!({
        "id": "gen-123",
        "choices": [{ "message": { "role": "assistant", "content": "hello" }, "finish_reason": "stop" }],
        "created": 1_700_000_000,
        "model": "test/model",
        "object": "chat.completion"
    }))
    .unwrap();
    let response = parse_response(&body).unwrap();
    assert_eq!(response.id, "gen-123");
    assert_eq!(response.choices.len(), 1);
    assert_eq!(response.model, "test/model");
}

#[test]
fn parse_response_malformed_json_is_decoding_error() {
    assert!(matches!(parse_response(b"not json"), Err(OpenRouterError::DecodingError(_))));
}

// ----- check_for_embedded_errors ----------------------------------------------------------------

#[test]
fn choice_error_is_generation_error() {
    let choice = make_choice(None, None, Some("stop"), Some(ChoiceError { code: 42, message: "provider failed".into(), metadata: None }));
    let err = check_for_embedded_errors(&make_response(vec![choice])).unwrap_err();
    assert!(matches!(err, OpenRouterError::GenerationError { code: Some(42), .. }), "{err:?}");
}

#[test]
fn finish_reason_error_is_generation_error() {
    let choice = make_choice(None, None, Some("error"), None);
    let err = check_for_embedded_errors(&make_response(vec![choice])).unwrap_err();
    assert!(matches!(err, OpenRouterError::GenerationError { code: None, .. }), "{err:?}");
}

#[test]
fn finish_reason_content_filter_is_content_filtered() {
    let choice = make_choice(None, None, Some("content_filter"), None);
    assert_eq!(check_for_embedded_errors(&make_response(vec![choice])), Err(OpenRouterError::ContentFiltered));
}

#[test]
fn successful_response_has_no_embedded_error() {
    let choice = make_choice(Some("ok"), None, Some("stop"), None);
    assert_eq!(check_for_embedded_errors(&make_response(vec![choice])), Ok(()));
}

#[test]
fn empty_choices_has_no_embedded_error() {
    assert_eq!(check_for_embedded_errors(&make_response(vec![])), Ok(()));
}

// ----- extract_image_data -----------------------------------------------------------------------

#[test]
fn valid_data_url_returns_decoded_bytes() {
    let original = vec![0xFF, 0xD8, 0xFF];
    let choice = make_choice(None, Some(vec![make_image(&data_uri(&original))]), Some("stop"), None);
    assert_eq!(extract_image_data(&make_response(vec![choice])).unwrap(), original);
}

#[test]
fn empty_choices_is_no_image_generated() {
    assert_eq!(extract_image_data(&make_response(vec![])), Err(OpenRouterError::NoImageGenerated));
}

#[test]
fn message_without_images_is_no_image_generated() {
    let choice = make_choice(Some("text only"), None, Some("stop"), None);
    assert_eq!(extract_image_data(&make_response(vec![choice])), Err(OpenRouterError::NoImageGenerated));
}

#[test]
fn data_url_missing_comma_is_invalid_image_data() {
    let choice = make_choice(None, Some(vec![make_image("data:image/pngbase64AAAA")]), Some("stop"), None);
    assert_eq!(extract_image_data(&make_response(vec![choice])), Err(OpenRouterError::InvalidImageData));
}

#[test]
fn invalid_base64_after_comma_is_invalid_image_data() {
    let choice = make_choice(None, Some(vec![make_image("data:image/png;base64,!!!not-base64!!!")]), Some("stop"), None);
    assert_eq!(extract_image_data(&make_response(vec![choice])), Err(OpenRouterError::InvalidImageData));
}

// ----- from_http (handleHTTPError) --------------------------------------------------------------

#[test]
fn http_401_is_unauthorized() {
    assert_eq!(OpenRouterError::from_http(401, &error_body("invalid key", None)), OpenRouterError::Unauthorized("invalid key".into()));
}

#[test]
fn http_402_is_payment_required() {
    assert_eq!(OpenRouterError::from_http(402, &error_body("no credits", None)), OpenRouterError::PaymentRequired("no credits".into()));
}

#[test]
fn http_403_is_forbidden_with_moderation_reasons() {
    let body = error_body("flagged", Some(json!({ "reasons": ["violence"], "flagged_input": "bad prompt" })));
    assert_eq!(
        OpenRouterError::from_http(403, &body),
        OpenRouterError::Forbidden { moderation_reasons: Some(vec!["violence".into()]), flagged_input: Some("bad prompt".into()) }
    );
}

#[test]
fn http_429_is_rate_limited() {
    assert_eq!(OpenRouterError::from_http(429, &error_body("slow down", None)), OpenRouterError::RateLimited("slow down".into()));
}

#[test]
fn http_502_is_bad_gateway_with_provider_name() {
    let body = error_body("upstream", Some(json!({ "provider_name": "Together" })));
    assert_eq!(OpenRouterError::from_http(502, &body), OpenRouterError::BadGateway { provider_name: Some("Together".into()), raw_error: None });
}

#[test]
fn http_502_raw_metadata_is_stringified() {
    let body = error_body("upstream", Some(json!({ "provider_name": "Together", "raw": { "detail": "boom" } })));
    let err = OpenRouterError::from_http(502, &body);
    assert_eq!(err, OpenRouterError::BadGateway { provider_name: Some("Together".into()), raw_error: Some(r#"{"detail":"boom"}"#.into()) });
    assert_eq!(err.into_generation_error(), GenerationError::server(Some(502), r#"Together: {"detail":"boom"}"#));
}

#[test]
fn http_unknown_status_is_http_error() {
    assert_eq!(OpenRouterError::from_http(999, &error_body("weird", None)), OpenRouterError::HttpError { status_code: 999, message: "weird".into() });
}

#[test]
fn http_error_without_json_body_uses_unknown_error() {
    assert_eq!(OpenRouterError::from_http(400, b"<html>"), OpenRouterError::BadRequest("Unknown error".into()));
}

// ----- is_retriable -----------------------------------------------------------------------------

#[test]
fn retriable_errors() {
    assert!(OpenRouterError::RequestTimeout("test".into()).is_retriable());
    assert!(OpenRouterError::RateLimited("test".into()).is_retriable());
    assert!(OpenRouterError::BadGateway { provider_name: Some("test".into()), raw_error: None }.is_retriable());
    assert!(OpenRouterError::ServiceUnavailable("test".into()).is_retriable());
    assert!(OpenRouterError::NoImageGenerated.is_retriable());
}

#[test]
fn non_retriable_errors() {
    assert!(!OpenRouterError::Unauthorized("test".into()).is_retriable());
    assert!(!OpenRouterError::BadRequest("test".into()).is_retriable());
    assert!(!OpenRouterError::ContentFiltered.is_retriable());
}

// ----- into_generation_error --------------------------------------------------------------------

#[test]
fn generation_error_mapping() {
    assert_eq!(OpenRouterError::Unauthorized("bad key".into()).into_generation_error(), GenerationError::Unauthorized("bad key".into()));
    assert_eq!(OpenRouterError::RateLimited("slow".into()).into_generation_error(), GenerationError::RateLimited("slow".into()));
    assert!(matches!(OpenRouterError::ContentFiltered.into_generation_error(), GenerationError::ContentFiltered(_)));
    assert_eq!(OpenRouterError::PaymentRequired("no credits".into()).into_generation_error(), GenerationError::PaymentRequired("no credits".into()));
    assert_eq!(OpenRouterError::RequestTimeout("too slow".into()).into_generation_error(), GenerationError::Timeout);
    assert_eq!(OpenRouterError::NoImageGenerated.into_generation_error(), GenerationError::NoResultGenerated);
    assert_eq!(
        OpenRouterError::Forbidden { moderation_reasons: Some(vec!["violence".into(), "hate".into()]), flagged_input: None }.into_generation_error(),
        GenerationError::ContentFiltered("violence, hate".into())
    );
    assert_eq!(
        OpenRouterError::Forbidden { moderation_reasons: None, flagged_input: None }.into_generation_error(),
        GenerationError::ContentFiltered("Content flagged by moderation".into())
    );
    assert_eq!(OpenRouterError::ServiceUnavailable("busy".into()).into_generation_error(), GenerationError::server(Some(503), "busy"));
    assert_eq!(
        OpenRouterError::BadGateway { provider_name: None, raw_error: None }.into_generation_error(),
        GenerationError::server(Some(502), "Bad gateway")
    );
    assert_eq!(
        OpenRouterError::GenerationError { code: Some(42), message: "x".into(), metadata: None }.into_generation_error(),
        GenerationError::server(Some(42), "x")
    );
    assert_eq!(OpenRouterError::BadRequest("nope".into()).into_generation_error(), GenerationError::InvalidRequest("nope".into()));
    assert_eq!(
        OpenRouterError::HttpError { status_code: 999, message: "weird".into() }.into_generation_error(),
        GenerationError::Unknown("HTTP 999: weird".into())
    );
}

#[test]
fn error_descriptions() {
    assert_eq!(
        OpenRouterError::Forbidden { moderation_reasons: Some(vec!["violence".into()]), flagged_input: Some("bad".into()) }.to_string(),
        "Content flagged by moderation: violence (flagged: \"bad\")"
    );
    assert_eq!(OpenRouterError::BadGateway { provider_name: Some("Together".into()), raw_error: Some("boom".into()) }.to_string(), "Model unavailable (Together): boom");
    assert_eq!(OpenRouterError::GenerationError { code: Some(7), message: "m".into(), metadata: None }.to_string(), "Generation failed (7): m");
    assert_eq!(OpenRouterError::HttpError { status_code: 418, message: "m".into() }.to_string(), "HTTP 418 I'm a teapot: m");
    assert_eq!(OpenRouterError::HttpError { status_code: 999, message: "m".into() }.to_string(), "HTTP 999: m");
}

// ----- generate_image error mapping (no network) ------------------------------------------------

#[test]
fn unsupported_model_is_invalid_request() {
    crate::rt().block_on(unsupported_model_is_invalid_request_inner());
}

async fn unsupported_model_is_invalid_request_inner() {
    let client = OpenRouterClient::new("fake");
    let err = client.generate_image("x", &flux1_dev(), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("Model 'flux-1-dev' is not supported by OpenRouter".into()));
}

#[test]
fn unsupported_asset_role_is_invalid_request() {
    crate::rt().block_on(unsupported_asset_role_is_invalid_request_inner());
}

async fn unsupported_asset_role_is_invalid_request_inner() {
    let client = OpenRouterClient::new("fake");
    let asset = ProviderAsset::new(AssetRole::MaskImage, "image/png", png_bytes());
    let err = client.generate_image("x", &model(capabilities::GEMINI_25_FLASH), &[asset], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("Role 'mask_image' is not supported by OpenRouter".into()));
}

#[test]
fn upscale_and_remove_background_are_invalid_requests() {
    crate::rt().block_on(upscale_and_remove_background_are_invalid_requests_inner());
}

async fn upscale_and_remove_background_are_invalid_requests_inner() {
    let client = OpenRouterClient::new("fake");
    assert_eq!(client.upscale_image(&[]).await.unwrap_err(), GenerationError::InvalidRequest("Image upscaling is not supported by OpenRouter".into()));
    assert_eq!(client.remove_background(&[]).await.unwrap_err(), GenerationError::InvalidRequest("Background removal is not supported by OpenRouter".into()));
}

// ----- build_request ----------------------------------------------------------------------------

#[test]
fn build_request_body() {
    let img = png_bytes();
    let req = build_request("a cat", "google/gemini-2.5-flash-image", &[&img], Some("16:9"), Some("2K"));
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(
        v,
        json!({
            "model": "google/gemini-2.5-flash-image",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": data_uri(&img) } },
                    { "type": "text", "text": "a cat" }
                ]
            }],
            "modalities": ["image"],
            "image_config": { "aspect_ratio": "16:9", "image_size": "2K" }
        })
    );
}

#[test]
fn build_request_omits_empty_prompt_and_image_config() {
    let v = serde_json::to_value(build_request("", "m", &[], None, None)).unwrap();
    assert_eq!(v["messages"][0]["content"], json!([]));
    assert!(v.get("image_config").is_none());
}

#[test]
fn build_request_partial_image_config_omits_missing_key() {
    let v = serde_json::to_value(build_request("p", "m", &[], Some("1:1"), None)).unwrap();
    assert_eq!(v["image_config"], json!({ "aspect_ratio": "1:1" }));
}

// ----- capabilities -----------------------------------------------------------------------------

#[test]
fn model_slugs() {
    let expected = [
        ("gemini-3-pro", "google/gemini-3-pro-image-preview"),
        ("gemini-2.5-flash", "google/gemini-2.5-flash-image"),
        ("gemini-3.1-flash", "google/gemini-3.1-flash-image-preview"),
        ("gpt-5-image", "openai/gpt-5-image"),
        ("gpt-5-image-mini", "openai/gpt-5-image-mini"),
        ("gpt-image-2", "openai/gpt-5.4-image-2"),
        ("seedream-4.5", "bytedance-seed/seedream-4.5"),
        ("riverflow-2-max", "sourceful/riverflow-v2-max-preview"),
        ("riverflow-2-std", "sourceful/riverflow-v2-standard-preview"),
        ("riverflow-2-fast", "sourceful/riverflow-v2-fast-preview"),
        ("flux-2-max", "black-forest-labs/flux.2-max"),
        ("flux-2-pro", "black-forest-labs/flux.2-pro"),
        ("flux-2-flex", "black-forest-labs/flux.2-flex"),
        ("flux-2-klein", "black-forest-labs/flux.2-klein-4b"),
        ("seedream-5-pro", "bytedance-seed/seedream-5-0-pro"),
        ("seedream-5-lite", "bytedance-seed/seedream-5-0-lite"),
        ("muse-image", "meta/muse-image"),
        ("qwen-image-3", "qwen/qwen-image-3"),
        ("qwen-image-3-pro", "qwen/qwen-image-3-pro"),
        ("grok-imagine-image-2", "x-ai/grok-imagine-image-2.0"),
    ];
    assert_eq!(expected.len(), SUPPORTED_IMAGE_MODEL_IDS.len());
    for (id, slug) in expected {
        assert_eq!(capabilities::model_slug_for_id(id), Some(slug), "{id}");
        assert!(SUPPORTED_IMAGE_MODEL_IDS.contains(&id), "{id} missing from supported list");
    }
    assert_eq!(capabilities::model_slug_for_id("flux-1-dev"), None);
}

#[test]
fn image_capabilities() {
    use AspectRatio::*;
    use ImageResolution::*;
    let all = vec![Square, Standard, ThreeToFour, Portrait, Landscape, Tall, Wide];
    let five = vec![Square, ThreeToFour, Standard, Tall, Landscape];

    let caps = |id| capabilities::image_capabilities(&model(id)).unwrap_or_else(|| panic!("{id} has no capabilities"));

    let c = caps(capabilities::GEMINI_3_PRO);
    assert_eq!((c.supported_aspect_ratios.clone(), c.supported_resolutions.clone(), c.max_input_images), (all.clone(), vec![Hd, Fhd, Uhd], 14));
    let c = caps(capabilities::GEMINI_31_FLASH);
    assert_eq!((c.supported_aspect_ratios.clone(), c.supported_resolutions.clone(), c.max_input_images), (all.clone(), vec![Sd, Hd, Fhd, Uhd], 14));
    let c = caps(capabilities::GEMINI_25_FLASH);
    assert_eq!((c.supported_aspect_ratios.clone(), c.supported_resolutions.clone(), c.max_input_images), (all.clone(), vec![Hd, Fhd, Uhd], 14));
    for id in [capabilities::GPT_5, capabilities::GPT_5_MINI, capabilities::GPT_IMAGE_2] {
        let c = caps(id);
        assert_eq!((c.supported_aspect_ratios.clone(), c.supported_resolutions.clone(), c.max_input_images), (vec![Square], vec![], 1), "{id}");
        assert!(!c.supports_resolution());
    }
    for id in [capabilities::SEEDREAM_45, capabilities::FLUX_2_MAX, capabilities::FLUX_2_PRO, capabilities::FLUX_2_FLEX, capabilities::FLUX_2_KLEIN] {
        let c = caps(id);
        assert_eq!((c.supported_aspect_ratios.clone(), c.supported_resolutions.clone(), c.max_input_images), (five.clone(), vec![], 1), "{id}");
    }
    for id in [capabilities::RIVERFLOW_2_MAX, capabilities::RIVERFLOW_2_STD, capabilities::RIVERFLOW_2_FAST] {
        let c = caps(id);
        assert_eq!((c.supported_aspect_ratios.clone(), c.supported_resolutions.clone(), c.max_input_images), (all.clone(), vec![], 1), "{id}");
    }
    assert_eq!(capabilities::image_capabilities(&flux1_dev()), None);
}

// ----- descriptor -------------------------------------------------------------------------------

#[test]
fn descriptor_fields() {
    let d = descriptor();
    assert_eq!(d.id, ProviderId::open_router());
    assert_eq!(d.display_name, "OpenRouter");
    assert_eq!(d.api_key_placeholder, "sk-or-v1-...");
    assert_eq!(d.api_key_instructions, "Get your API key from openrouter.ai/keys");
    assert_eq!(d.api_key_url, "https://openrouter.ai/keys");
    assert_eq!(d.billing_url, Some("https://openrouter.ai/settings/credits"));
    assert!(d.requires_api_key);
    assert!(d.is_user_selectable);
    assert!(d.supported_video_models.is_empty());
    assert!(d.supported_audio_models.is_empty());
    assert!(d.supported_tool_models.is_empty());
    assert!(!d.supports_tool(ToolId::Upscale));
    assert!(!d.supports_tool(ToolId::RemoveBackground));
    assert!(!d.supports_video_generation());
    assert!(!d.supports_audio_generation());
    assert!((d.make_video_client)(&ClientOptions::new("k")).is_none());
    assert!((d.make_audio_client)(&ClientOptions::new("k")).is_none());
    // Supported list is built by catalog lookup; every entry must resolve to a supported id, in order.
    let ids: Vec<&str> = d.supported_image_models.iter().map(|m| m.id).collect();
    let expected: Vec<&str> = SUPPORTED_IMAGE_MODEL_IDS.iter().copied().filter(|id| majik_providers::catalog::image::model(id).is_some()).collect();
    assert_eq!(ids, expected);
    for m in &d.supported_image_models {
        assert!(d.image_capabilities(m).is_some(), "{} has no capabilities", m.id);
        assert!(capabilities::model_slug(m).is_some(), "{} has no slug", m.id);
    }
    assert_eq!(d.image_capabilities(&flux1_dev()), None);
}

#[test]
fn descriptor_supports_every_catalog_model_it_lists() {
    // Only meaningful once the image catalog is populated; skip until then so this test can't
    // false-fail during the concurrent catalog port.
    if majik_providers::catalog::image::ALL.is_empty() {
        eprintln!("catalog::image::ALL is empty; skipping");
        return;
    }
    let d = descriptor();
    assert_eq!(d.supported_image_models.len(), 20);
    let ids: Vec<&str> = d.supported_image_models.iter().map(|m| m.id).collect();
    assert_eq!(ids, SUPPORTED_IMAGE_MODEL_IDS.to_vec());
}

// ----- HTTP round trips (wiremock) --------------------------------------------------------------

#[test]
fn generate_image_sends_expected_request_and_decodes_data_uri() {
    crate::rt().block_on(generate_image_sends_expected_request_and_decodes_data_uri_inner());
}

async fn generate_image_sends_expected_request_and_decodes_data_uri_inner() {
    let (server, client) = server_and_client().await;
    let reference = png_bytes();
    let expected_ref_uri = data_uri(&reference);
    let out = vec![1u8, 2, 3, 4];

    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .and(header("Authorization", "Bearer test-key"))
        .and(header("HTTP-Referer", "https://majik.app"))
        .and(header("X-Title", "Majik"))
        .and(header("Content-Type", "application/json"))
        .and(body_partial_json(json!({
            "model": "google/gemini-2.5-flash-image",
            "modalities": ["image"],
            "image_config": { "aspect_ratio": "16:9", "image_size": "2K" },
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": expected_ref_uri } },
                    { "type": "text", "text": "a cat" }
                ]
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body(&out)))
        .expect(1)
        .mount(&server)
        .await;

    let asset = ProviderAsset::new(AssetRole::ReferenceImage, "image/png", reference.clone());
    let bytes = client
        .generate_image("a cat", &model(capabilities::GEMINI_25_FLASH), &[asset], Some(AspectRatio::Landscape), Some(ImageResolution::Fhd))
        .await
        .unwrap();
    assert_eq!(bytes, out);

    // Exactly one request, with no stray keys when nothing optional was supplied.
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
}

#[test]
fn generate_image_without_options_omits_image_config() {
    crate::rt().block_on(generate_image_without_options_omits_image_config_inner());
}

async fn generate_image_without_options_omits_image_config_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(success_body(&[9]))).mount(&server).await;

    client.generate_image("p", &model(capabilities::FLUX_2_PRO), &[], None, None).await.unwrap();

    let received: Vec<WmRequest> = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["model"], "black-forest-labs/flux.2-pro");
    assert!(body.get("image_config").is_none());
    assert_eq!(body["messages"][0]["content"], json!([{ "type": "text", "text": "p" }]));
}

#[test]
fn http_401_maps_to_unauthorized() {
    crate::rt().block_on(http_401_maps_to_unauthorized_inner());
}

async fn http_401_maps_to_unauthorized_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401).set_body_bytes(error_body("invalid key", None))).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::Unauthorized("invalid key".into()));
}

#[test]
fn http_402_maps_to_payment_required() {
    crate::rt().block_on(http_402_maps_to_payment_required_inner());
}

async fn http_402_maps_to_payment_required_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(402).set_body_bytes(error_body("no credits", None))).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::PaymentRequired("no credits".into()));
}

#[test]
fn http_403_maps_to_content_filtered_with_reasons() {
    crate::rt().block_on(http_403_maps_to_content_filtered_with_reasons_inner());
}

async fn http_403_maps_to_content_filtered_with_reasons_inner() {
    let (server, client) = server_and_client().await;
    let body = error_body("flagged", Some(json!({ "reasons": ["violence", "hate"], "flagged_input": "bad prompt" })));
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(403).set_body_bytes(body)).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::ContentFiltered("violence, hate".into()));
}

#[test]
fn http_408_maps_to_timeout() {
    crate::rt().block_on(http_408_maps_to_timeout_inner());
}

async fn http_408_maps_to_timeout_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(408).set_body_bytes(error_body("slow", None))).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::Timeout);
}

#[test]
fn http_429_maps_to_rate_limited() {
    crate::rt().block_on(http_429_maps_to_rate_limited_inner());
}

async fn http_429_maps_to_rate_limited_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(429).set_body_bytes(error_body("slow down", None))).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::RateLimited("slow down".into()));
    assert!(err.is_retriable());
}

#[test]
fn http_502_maps_to_server_error_502() {
    crate::rt().block_on(http_502_maps_to_server_error_502_inner());
}

async fn http_502_maps_to_server_error_502_inner() {
    let (server, client) = server_and_client().await;
    let body = error_body("upstream", Some(json!({ "provider_name": "Together", "raw": "boom" })));
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(502).set_body_bytes(body)).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::server(Some(502), "Together: boom"));
}

#[test]
fn http_503_maps_to_server_error_503() {
    crate::rt().block_on(http_503_maps_to_server_error_503_inner());
}

async fn http_503_maps_to_server_error_503_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(503).set_body_bytes(error_body("no provider", None))).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::server(Some(503), "no provider"));
}

#[test]
fn http_400_maps_to_invalid_request() {
    crate::rt().block_on(http_400_maps_to_invalid_request_inner());
}

async fn http_400_maps_to_invalid_request_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400).set_body_bytes(error_body("bad params", None))).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::InvalidRequest("bad params".into()));
}

#[test]
fn http_unknown_status_maps_to_unknown() {
    crate::rt().block_on(http_unknown_status_maps_to_unknown_inner());
}

async fn http_unknown_status_maps_to_unknown_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(418).set_body_bytes(error_body("teapot", None))).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::Unknown("HTTP 418: teapot".into()));
}

#[test]
fn embedded_choice_error_maps_to_server_error() {
    crate::rt().block_on(embedded_choice_error_maps_to_server_error_inner());
}

async fn embedded_choice_error_maps_to_server_error_inner() {
    let (server, client) = server_and_client().await;
    let body = json!({
        "id": "gen-1",
        "choices": [{ "message": { "role": "assistant" }, "finish_reason": "stop", "error": { "code": 42, "message": "provider failed" } }],
        "created": 0, "model": "m", "object": "chat.completion"
    });
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body)).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::server(Some(42), "provider failed"));
}

#[test]
fn content_filter_finish_reason_maps_to_content_filtered() {
    crate::rt().block_on(content_filter_finish_reason_maps_to_content_filtered_inner());
}

async fn content_filter_finish_reason_maps_to_content_filtered_inner() {
    let (server, client) = server_and_client().await;
    let body = json!({
        "id": "gen-1",
        "choices": [{ "message": { "role": "assistant", "content": "" }, "finish_reason": "content_filter" }],
        "created": 0, "model": "m", "object": "chat.completion"
    });
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body)).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::ContentFiltered("Content filtered by provider's safety system".into()));
}

#[test]
fn text_only_response_maps_to_no_result_generated() {
    crate::rt().block_on(text_only_response_maps_to_no_result_generated_inner());
}

async fn text_only_response_maps_to_no_result_generated_inner() {
    let (server, client) = server_and_client().await;
    let body = json!({
        "id": "gen-1",
        "choices": [{ "message": { "role": "assistant", "content": "I cannot draw that" }, "finish_reason": "stop" }],
        "created": 0, "model": "m", "object": "chat.completion"
    });
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body)).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::NoResultGenerated);
    assert!(err.is_retriable());
}

#[test]
fn non_data_uri_image_maps_to_unknown() {
    crate::rt().block_on(non_data_uri_image_maps_to_unknown_inner());
}

async fn non_data_uri_image_maps_to_unknown_inner() {
    let (server, client) = server_and_client().await;
    let body = json!({
        "id": "gen-1",
        "choices": [{ "message": { "role": "assistant", "images": [{ "image_url": { "url": "https://cdn.example.com/out.png" } }] }, "finish_reason": "stop" }],
        "created": 0, "model": "m", "object": "chat.completion"
    });
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body)).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert_eq!(err, GenerationError::Unknown("Invalid image data received from the server".into()));
}

#[test]
fn malformed_success_body_maps_to_unknown_decoding_error() {
    crate::rt().block_on(malformed_success_body_maps_to_unknown_decoding_error_inner());
}

async fn malformed_success_body_maps_to_unknown_decoding_error_inner() {
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("not json")).mount(&server).await;
    let err = client.generate_image("p", &model(capabilities::GEMINI_3_PRO), &[], None, None).await.unwrap_err();
    assert!(matches!(&err, GenerationError::Unknown(m) if m.starts_with("Failed to decode response:")), "{err:?}");
}

// ----- prompt improvement (text completions) ----------------------------------------------------

fn text_body(content: &str) -> serde_json::Value {
    json!({
        "id": "gen-text",
        "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }],
        "created": 1_700_000_000,
        "model": "anthropic/claude-haiku-4.5",
        "object": "chat.completion"
    })
}

#[test]
fn text_request_sends_the_instruction_and_the_prompt_without_modalities() {
    crate::rt().block_on(text_request_sends_the_instruction_and_the_prompt_without_modalities_inner());
}

async fn text_request_sends_the_instruction_and_the_prompt_without_modalities_inner() {
    use majik_providers::TextProviderClient as _;
    let (server, client) = server_and_client().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_body("  a red apple on a table  ")))
        .mount(&server)
        .await;

    let text = client.complete_text("rewrite it", "apple", 400).await.unwrap();
    assert_eq!(text, "a red apple on a table", "the completion is returned trimmed");

    let request = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "anthropic/claude-haiku-4.5");
    assert_eq!(body["max_tokens"], 400);
    assert!(body.get("modalities").is_none(), "a text completion asks for no image modality: {body}");
    assert!(body.get("image_config").is_none(), "{body}");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"][0]["text"], "rewrite it");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"][0]["text"], "apple");
}

#[test]
fn an_empty_text_completion_is_no_result_generated() {
    crate::rt().block_on(an_empty_text_completion_is_no_result_generated_inner());
}

async fn an_empty_text_completion_is_no_result_generated_inner() {
    use majik_providers::TextProviderClient as _;
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(text_body("   "))).mount(&server).await;
    assert!(matches!(client.complete_text("s", "u", 100).await, Err(GenerationError::NoResultGenerated)));
}

#[test]
fn a_text_http_error_maps_like_any_other() {
    crate::rt().block_on(a_text_http_error_maps_like_any_other_inner());
}

async fn a_text_http_error_maps_like_any_other_inner() {
    use majik_providers::TextProviderClient as _;
    let (server, client) = server_and_client().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401).set_body_bytes(error_body("invalid key", None))).mount(&server).await;
    assert!(matches!(client.complete_text("s", "u", 100).await, Err(GenerationError::Unauthorized(_))));
}
