//! Integration tests for the shared (provider-independent) parts of `majik-providers`.
//!
//! The dialogue parser, error mapping, data URIs, the model and voice catalogs, and the
//! descriptor / shared-type invariants that need no provider implementation.

use std::collections::HashSet;
use std::sync::Arc;

use majik_providers::asset::{AssetConstraintError, AssetConstraints, AssetRole};
use majik_providers::ClientOptions;
use majik_providers::catalog::{audio, image, tool, video};
use majik_providers::client::ImageProviderClient;
use majik_providers::data_uri::{from_data_uri, to_data_uri};
use majik_providers::descriptor::ProviderDescriptor;
use majik_providers::dialogue::{parse_dialogue, parse_dialogue_with_voices, DialogueTurn, Speaker};
use majik_providers::error::GenerationError;
use majik_providers::http::Timeouts;
use majik_providers::models::{AspectRatio, AudioModel, AudioVoice, ImageModel, ImageResolution, ModelCapabilities, VideoAspectRatio, VideoModel, VideoResolution};
use majik_providers::pricing::{Estimate, PricedJob, ToolInput};
use majik_providers::settings::{AudioGenerationSettings, ImageGenerationSettings, ToolSettings, VideoGenerationSettings, DEFAULT_UPSCALE_FACTOR};
use majik_providers::voices::{elevenlabs, gemini};
use majik_providers::{logo, ProviderAsset, ProviderId, ProviderRegistry, ToolId, ToolModel};
use majik_core::model::MediaType;

// ===== AudioDialogueParser ==================================================================

fn alice() -> AudioVoice {
    AudioVoice::new("Alice", "Alice")
}

fn bob() -> AudioVoice {
    AudioVoice::new("Bob", "Bob")
}

fn texts(turns: &[DialogueTurn]) -> Vec<&str> {
    turns.iter().map(|t| t.text.as_str()).collect()
}

fn speakers(turns: &[DialogueTurn]) -> Vec<Speaker> {
    turns.iter().map(|t| t.speaker).collect()
}

#[test]
fn dialogue_basic_alternation() {
    let turns = parse_dialogue("Speaker 1: Hello there.\nSpeaker 2: General Kenobi.");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].speaker, Speaker::One);
    assert_eq!(turns[0].text, "Hello there.");
    assert_eq!(turns[1].speaker, Speaker::Two);
    assert_eq!(turns[1].text, "General Kenobi.");
}

#[test]
fn dialogue_case_insensitive() {
    let turns = parse_dialogue("SPEAKER 1: Top.\nspeaker 2: Bottom.\nSpeaker 1: Mid.");
    assert_eq!(texts(&turns), ["Top.", "Bottom.", "Mid."]);
    assert_eq!(speakers(&turns), [Speaker::One, Speaker::Two, Speaker::One]);
}

#[test]
fn dialogue_continuation_lines_attach_to_previous_turn() {
    let turns = parse_dialogue("Speaker 1: First line.\nStill speaker 1, no prefix.\nSpeaker 2: Now me.");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].text, "First line.\nStill speaker 1, no prefix.");
    assert_eq!(turns[1].text, "Now me.");
}

#[test]
fn dialogue_adjacent_same_speaker_merges() {
    let turns = parse_dialogue("Speaker 1: One.\nSpeaker 1: Two.\nSpeaker 2: Three.");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].speaker, Speaker::One);
    assert_eq!(turns[0].text, "One.\nTwo.");
    assert_eq!(turns[1].speaker, Speaker::Two);
}

#[test]
fn dialogue_unprefixed_falls_to_speaker_1() {
    let turns = parse_dialogue("Just narration, no labels at all.");
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].speaker, Speaker::One);
    assert_eq!(turns[0].text, "Just narration, no labels at all.");
}

#[test]
fn dialogue_leading_unlabeled_text_attaches_to_synthetic_speaker_1() {
    let turns = parse_dialogue("Pre-roll narration.\nSpeaker 2: Then me.");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].speaker, Speaker::One);
    assert_eq!(turns[0].text, "Pre-roll narration.");
    assert_eq!(turns[1].speaker, Speaker::Two);
    assert_eq!(turns[1].text, "Then me.");
}

#[test]
fn dialogue_empty_prompt_returns_empty() {
    assert!(parse_dialogue("").is_empty());
}

#[test]
fn dialogue_whitespace_only_returns_empty() {
    assert!(parse_dialogue("   \n\n  ").is_empty());
}

#[test]
fn dialogue_indented_labels_still_parse() {
    let turns = parse_dialogue("    Speaker 1: A.\n      Speaker 2: B.");
    assert_eq!(texts(&turns), ["A.", "B."]);
}

#[test]
fn dialogue_label_with_empty_body_followed_by_continuation() {
    let turns = parse_dialogue("Speaker 2:\nLate body.\nSpeaker 1:   ");
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].speaker, Speaker::Two);
    assert_eq!(turns[0].text, "Late body.");
}

#[test]
fn dialogue_windows_line_endings_are_trimmed_off() {
    // The parser splits on "\n" and trims spaces/tabs only, so a trailing "\r" stays on the line;
    // the label match still works because it is a prefix check.
    let turns = parse_dialogue("Speaker 1: Hi\r\nSpeaker 2: Yo\r");
    assert_eq!(speakers(&turns), [Speaker::One, Speaker::Two]);
    assert_eq!(texts(&turns), ["Hi\r", "Yo\r"]);
}

#[test]
fn dialogue_with_voices_resolves_speakers() {
    let turns = parse_dialogue_with_voices("Speaker 1: Hello there.\nSpeaker 2: General Kenobi.", &alice(), &bob());
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].voice, alice());
    assert_eq!(turns[0].text, "Hello there.");
    assert_eq!(turns[1].voice, bob());
    assert_eq!(turns[1].text, "General Kenobi.");
}

#[test]
fn dialogue_with_voices_merges_when_both_speakers_share_a_voice() {
    let turns = parse_dialogue_with_voices("Speaker 1: One.\nSpeaker 2: Two.\nSpeaker 1: Three.", &alice(), &alice());
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].voice, alice());
    assert_eq!(turns[0].text, "One.\nTwo.\nThree.");
}

// ===== GenerationError.isRetriable ==========================================================

#[test]
fn error_rate_limited_is_retriable() {
    assert!(GenerationError::RateLimited("test".into()).is_retriable());
}

#[test]
fn error_server_error_is_retriable() {
    assert!(GenerationError::server(Some(500), "test").is_retriable());
}

#[test]
fn error_server_error_without_status_is_retriable() {
    assert!(GenerationError::server(None, "test").is_retriable());
}

#[test]
fn error_timeout_is_retriable() {
    assert!(GenerationError::Timeout.is_retriable());
}

#[test]
fn error_no_result_generated_is_retriable() {
    assert!(GenerationError::NoResultGenerated.is_retriable());
}

#[test]
fn error_unauthorized_is_not_retriable() {
    assert!(!GenerationError::Unauthorized("test".into()).is_retriable());
}

#[test]
fn error_content_filtered_is_not_retriable() {
    assert!(!GenerationError::ContentFiltered("test".into()).is_retriable());
}

#[test]
fn error_invalid_request_is_not_retriable() {
    assert!(!GenerationError::InvalidRequest("test".into()).is_retriable());
}

#[test]
fn error_payment_required_is_not_retriable() {
    assert!(!GenerationError::PaymentRequired("test".into()).is_retriable());
}

#[test]
fn error_provider_failed_is_not_retriable() {
    assert!(!GenerationError::ProviderFailed("test".into()).is_retriable());
}

#[test]
fn error_unknown_is_not_retriable() {
    assert!(!GenerationError::Unknown("test".into()).is_retriable());
}

// ===== GenerationError.errorDescription =====================================================

fn all_error_cases() -> Vec<GenerationError> {
    vec![
        GenerationError::Unauthorized("test".into()),
        GenerationError::RateLimited("test".into()),
        GenerationError::ContentFiltered("test".into()),
        GenerationError::server(Some(500), "test"),
        GenerationError::server(None, "test"),
        GenerationError::Timeout,
        GenerationError::NoResultGenerated,
        GenerationError::ProviderFailed("test".into()),
        GenerationError::InvalidRequest("test".into()),
        GenerationError::PaymentRequired("test".into()),
        GenerationError::Unknown("test".into()),
    ]
}

#[test]
fn error_all_cases_have_descriptions() {
    for error in all_error_cases() {
        assert!(!error.to_string().is_empty(), "{error:?} has an empty description");
    }
}

#[test]
fn error_all_cases_have_distinct_kinds() {
    let kinds: HashSet<&str> = all_error_cases().iter().map(|e| e.kind()).collect();
    // The two ServerError variants share a kind.
    assert_eq!(kinds.len(), all_error_cases().len() - 1);
}

#[test]
fn error_unauthorized_includes_message() {
    assert!(GenerationError::Unauthorized("bad key".into()).to_string().contains("bad key"));
}

#[test]
fn error_server_error_with_status_includes_code() {
    assert!(GenerationError::server(Some(503), "overloaded").to_string().contains("503"));
}

#[test]
fn error_server_error_without_status_omits_code() {
    let description = GenerationError::server(None, "failed").to_string();
    assert!(!description.contains('('));
    assert!(description.contains("failed"));
}

// ===== Data+DataURI =========================================================================

use base64::Engine as _;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn data_uri_valid_png() {
    let uri = format!("data:image/png;base64,{}", b64(&[0x89, 0x50, 0x4E, 0x47]));
    assert_eq!(from_data_uri(&uri), Some(vec![0x89, 0x50, 0x4E, 0x47]));
}

#[test]
fn data_uri_valid_jpeg() {
    let uri = format!("data:image/jpeg;base64,{}", b64(&[0xFF, 0xD8, 0xFF]));
    assert_eq!(from_data_uri(&uri), Some(vec![0xFF, 0xD8, 0xFF]));
}

#[test]
fn data_uri_missing_comma_returns_none() {
    assert_eq!(from_data_uri("data:image/png;base64AAAA"), None);
}

#[test]
fn data_uri_invalid_base64_returns_none() {
    assert_eq!(from_data_uri("data:image/png;base64,!!!invalid!!!"), None);
}

#[test]
fn data_uri_empty_string_returns_none() {
    assert_eq!(from_data_uri(""), None);
}

#[test]
fn data_uri_comma_only_returns_empty_data() {
    assert_eq!(from_data_uri(","), Some(Vec::new()));
}

#[test]
fn data_uri_minimal_valid() {
    let uri = format!(",{}", b64(&[0x01]));
    assert_eq!(from_data_uri(&uri), Some(vec![0x01]));
}

#[test]
fn data_uri_round_trip() {
    let original = vec![0xFF, 0xD8, 0xFF, 0xE0];
    let uri = to_data_uri(&original, "image/png");
    assert!(uri.starts_with("data:image/png;base64,"));
    assert_eq!(from_data_uri(&uri), Some(original));
}

// ===== AudioModelCatalog ====================================================================

#[test]
fn audio_catalog_all_contains_expected() {
    let ids: Vec<&str> = audio::ALL.iter().map(|m| m.id).collect();
    assert_eq!(ids, ["elevenlabs-v3", "gemini-2.5-pro"]);
}

#[test]
fn audio_catalog_lookup_by_id() {
    assert_eq!(audio::model("elevenlabs-v3"), Some(&audio::ELEVEN_LABS_V3));
    assert_eq!(audio::model("gemini-2.5-pro"), Some(&audio::GEMINI_25_PRO));
}

#[test]
fn audio_catalog_unknown_returns_none() {
    assert_eq!(audio::model("does-not-exist"), None);
}

#[test]
fn audio_model_encodes_as_id_and_decodes_via_catalog() {
    let original = audio::GEMINI_25_PRO.clone();
    let json = serde_json::to_string(&original).expect("serialize");
    assert_eq!(json, "\"gemini-2.5-pro\"");
    let decoded: AudioModel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, original);
}

#[test]
fn audio_model_metadata() {
    assert_eq!(audio::ELEVEN_LABS_V3.name, "ElevenLabs v3");
    assert_eq!(audio::ELEVEN_LABS_V3.manufacturer, "ElevenLabs");
    assert_eq!(audio::ELEVEN_LABS_V3.logo, logo::ELEVEN_LABS);
    assert_eq!(audio::ELEVEN_LABS_V3.short_description, "Expressive multi-speaker dialogue");
    assert_eq!(audio::GEMINI_25_PRO.name, "Gemini 2.5 Pro");
    assert_eq!(audio::GEMINI_25_PRO.manufacturer, "Google");
    assert_eq!(audio::GEMINI_25_PRO.logo, logo::GOOGLE);
    assert_eq!(audio::GEMINI_25_PRO.short_description, "Studio-quality narration and dialogue");
}

// ===== ImageModelCatalog ====================================================================

#[test]
fn image_catalog_ids_are_unique() {
    let ids: HashSet<&str> = image::ALL.iter().map(|m| m.id).collect();
    assert_eq!(ids.len(), image::ALL.len());
}

#[test]
fn image_catalog_names_are_unique() {
    let names: HashSet<&str> = image::ALL.iter().map(|m| m.name).collect();
    assert_eq!(names.len(), image::ALL.len());
}

#[test]
fn image_catalog_lookup_by_id_returns_same_instance() {
    for model in image::ALL {
        let found = image::model(model.id).expect("catalog entry is findable by id");
        assert_eq!(found, model);
        assert!(std::ptr::eq(found, model));
    }
}

#[test]
fn image_catalog_unknown_id_returns_none() {
    assert_eq!(image::model("does-not-exist"), None);
}

#[test]
fn image_catalog_order_and_ids() {
    let ids: Vec<&str> = image::ALL.iter().map(|m| m.id).collect();
    assert_eq!(
        ids,
        [
            "gemini-3-pro",
            "gemini-3.1-flash",
            "gemini-2.5-flash",
            "gpt-image-2",
            "gpt-5-image",
            "gpt-5-image-mini",
            "seedream-5-pro",
            "seedream-5-lite",
            "seedream-4.5",
            "muse-image",
            "riverflow-2-max",
            "riverflow-2-std",
            "riverflow-2-fast",
            "flux-2-max",
            "flux-2-pro",
            "flux-2-flex",
            "flux-2-klein",
            "flux-1-dev",
            "flux-1-schnell",
            "recraft-4-pro",
            "qwen-image-3-pro",
            "qwen-image-3",
            "wan-2.7-pro",
            "grok-imagine-image-2",
        ]
    );
}

#[test]
fn image_catalog_named_statics_match_catalog_entries() {
    let named: [&ImageModel; 24] = [
        &image::GEMINI_3_PRO,
        &image::GEMINI_31_FLASH,
        &image::GEMINI_25_FLASH,
        &image::GPT_IMAGE_2,
        &image::GPT_5,
        &image::GPT_5_MINI,
        &image::SEEDREAM_5_PRO,
        &image::SEEDREAM_5_LITE,
        &image::SEEDREAM_45,
        &image::MUSE_IMAGE,
        &image::RIVERFLOW_2_MAX,
        &image::RIVERFLOW_2_STD,
        &image::RIVERFLOW_2_FAST,
        &image::FLUX_2_MAX,
        &image::FLUX_2_PRO,
        &image::FLUX_2_FLEX,
        &image::FLUX_2_KLEIN,
        &image::FLUX_1_DEV,
        &image::FLUX_1_SCHNELL,
        &image::RECRAFT_V4_PRO,
        &image::QWEN_IMAGE_3_PRO,
        &image::QWEN_IMAGE_3,
        &image::WAN_27_PRO,
        &image::GROK_IMAGINE_IMAGE_2,
    ];
    assert_eq!(named.len(), image::ALL.len());
    for model in named {
        assert_eq!(image::model(model.id), Some(model), "{} missing from ALL", model.id);
    }
}

#[test]
fn image_catalog_display_names_and_manufacturers() {
    assert_eq!(image::GEMINI_3_PRO.name, "Nano Banana Pro");
    assert_eq!(image::GEMINI_31_FLASH.name, "Nano Banana 2");
    assert_eq!(image::GEMINI_25_FLASH.name, "Nano Banana");
    assert_eq!(image::GPT_IMAGE_2.name, "GPT Image 2");
    assert_eq!(image::FLUX_2_KLEIN.name, "FLUX.2 Klein 4B");
    assert_eq!(image::RIVERFLOW_2_STD.name, "Riverflow V2 Standard");
    assert_eq!(image::FLUX_2_PRO.manufacturer, "Black Forest Labs");
    assert_eq!(image::FLUX_2_PRO.logo, logo::FLUX);
    assert_eq!(image::GPT_5.logo, logo::OPEN_AI);
    assert_eq!(image::SEEDREAM_45.logo, logo::BYTE_DANCE);
    assert_eq!(image::RIVERFLOW_2_MAX.logo, logo::SOURCEFUL);
    assert_eq!(image::RECRAFT_V4_PRO.logo, logo::RECRAFT);
    for model in image::ALL {
        assert!(!model.manufacturer.is_empty());
        assert!(model.logo.starts_with("logo-"), "{} has an unexpected logo {}", model.id, model.logo);
    }
}

// ===== VideoModelCatalog ====================================================================

#[test]
fn video_catalog_ids_are_unique() {
    let ids: HashSet<&str> = video::ALL.iter().map(|m| m.id).collect();
    assert_eq!(ids.len(), video::ALL.len());
}

#[test]
fn video_catalog_names_are_unique() {
    let names: HashSet<&str> = video::ALL.iter().map(|m| m.name).collect();
    assert_eq!(names.len(), video::ALL.len());
}

#[test]
fn video_catalog_lookup_by_id_returns_same_instance() {
    for model in video::ALL {
        let found = video::model(model.id).expect("catalog entry is findable by id");
        assert_eq!(found, model);
        assert!(std::ptr::eq(found, model));
    }
}

#[test]
fn video_catalog_unknown_id_returns_none() {
    assert_eq!(video::model("does-not-exist"), None);
}

#[test]
fn video_catalog_order_and_ids() {
    let ids: Vec<&str> = video::ALL.iter().map(|m| m.id).collect();
    assert_eq!(
        ids,
        [
            "veo-3.1",
            "veo-3.1-fast",
            "veo-3.1-lite",
            "gemini-omni-flash-1.1",
            "sora-2-pro",
            "sora-2",
            "kling-3-pro",
            "kling-3-standard",
            "kling-2.6-pro",
            "kling-2.5-turbo-pro",
            "seedance-2.5",
            "seedance-2",
            "seedance-2-fast",
            "seedance-1.5-pro",
            "minimax-h3-max",
            "minimax-h3",
            "flux-3",
            "happyhorse-1.1",
            "happyhorse-1.0",
            "wan-3.0-prime",
            "wan-3.0",
            "wan-2.7",
            "pixverse-6",
            "grok-imagine-video-1.5",
            "grok-imagine-video",
        ]
    );
}

#[test]
fn video_catalog_named_statics_match_catalog_entries() {
    let named: [&VideoModel; 25] = [
        &video::VEO_31,
        &video::VEO_31_FAST,
        &video::VEO_31_LITE,
        &video::GEMINI_OMNI_FLASH_11,
        &video::SORA_2,
        &video::SORA_2_PRO,
        &video::KLING_30_PRO,
        &video::KLING_30_STANDARD,
        &video::KLING_25_TURBO_PRO,
        &video::KLING_26_PRO,
        &video::SEEDANCE_25,
        &video::SEEDANCE_15_PRO,
        &video::SEEDANCE_20,
        &video::SEEDANCE_20_FAST,
        &video::MINIMAX_H3,
        &video::MINIMAX_H3_MAX,
        &video::FLUX_3,
        &video::HAPPY_HORSE_11,
        &video::HAPPY_HORSE_10,
        &video::WAN_30,
        &video::WAN_30_PRIME,
        &video::WAN_27,
        &video::PIXVERSE_V6,
        &video::GROK_IMAGINE_VIDEO_15,
        &video::GROK_IMAGINE_VIDEO,
    ];
    assert_eq!(named.len(), video::ALL.len());
    for model in named {
        assert_eq!(video::model(model.id), Some(model), "{} missing from ALL", model.id);
    }
}

// From `VideoCapabilitiesTests` — the parts that only need the catalogs.
#[test]
fn video_all_models_have_manufacturer() {
    for model in video::ALL {
        assert!(!model.manufacturer.is_empty(), "{} has no manufacturer", model.id);
    }
}

#[test]
fn video_specific_manufacturers() {
    assert_eq!(video::VEO_31.manufacturer, "Google");
    assert_eq!(video::SORA_2.manufacturer, "OpenAI");
    assert_eq!(video::KLING_30_PRO.manufacturer, "Kuaishou");
    assert_eq!(video::SEEDANCE_15_PRO.manufacturer, "ByteDance");
    assert_eq!(video::HAPPY_HORSE_10.manufacturer, "Alibaba");
    assert_eq!(video::WAN_27.manufacturer, "Alibaba");
    assert_eq!(image::WAN_27_PRO.manufacturer, "Alibaba");
    assert_eq!(video::HAPPY_HORSE_10.logo, logo::ALIBABA);
    assert_eq!(video::WAN_27.logo, logo::ALIBABA);
    assert_eq!(image::WAN_27_PRO.logo, logo::ALIBABA);
    assert_eq!(video::PIXVERSE_V6.manufacturer, "PixVerse");
    assert_eq!(video::PIXVERSE_V6.logo, logo::PIXVERSE);
    assert_eq!(video::GROK_IMAGINE_VIDEO.manufacturer, "xAI");
    assert_eq!(video::GROK_IMAGINE_VIDEO.logo, logo::GROK);
    assert_eq!(video::KLING_30_STANDARD.name, "Kling 3.0 Standard");
    assert_eq!(video::SEEDANCE_20_FAST.name, "Seedance 2.0 Fast");
}

// ===== Model Codable ========================================================================

#[test]
fn image_model_deserializes_unknown_id_as_error() {
    let err = serde_json::from_str::<ImageModel>("\"not-a-real-model\"").expect_err("unknown id must fail");
    assert!(err.to_string().contains("not-a-real-model"));
}

#[test]
fn video_model_round_trips_through_id() {
    let json = serde_json::to_string(&video::VEO_31).expect("serialize");
    assert_eq!(json, "\"veo-3.1\"");
    let decoded: VideoModel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, video::VEO_31);
}

// ===== AssetConstraints =====================================================================

#[test]
fn constraints_empty_roles_pass_none() {
    assert_eq!(AssetConstraints::none().validate(&[]), Ok(()));
}

#[test]
fn constraints_any_role_rejected_by_none() {
    assert!(AssetConstraints::none().validate(&[AssetRole::FirstFrame]).is_err());
}

#[test]
fn constraints_first_frame_accepted_by_first_last_frame() {
    assert_eq!(AssetConstraints::first_last_frame().validate(&[AssetRole::FirstFrame]), Ok(()));
}

#[test]
fn constraints_last_frame_accepted_by_first_last_frame() {
    assert_eq!(AssetConstraints::first_last_frame().validate(&[AssetRole::LastFrame]), Ok(()));
}

#[test]
fn constraints_both_frames_accepted_by_first_last_frame() {
    assert_eq!(AssetConstraints::first_last_frame().validate(&[AssetRole::FirstFrame, AssetRole::LastFrame]), Ok(()));
}

#[test]
fn constraints_empty_roles_pass_first_last_frame() {
    assert_eq!(AssetConstraints::first_last_frame().validate(&[]), Ok(()));
}

#[test]
fn constraints_unaccepted_role_rejected_by_first_last_frame() {
    assert_eq!(
        AssetConstraints::first_last_frame().validate(&[AssetRole::ReferenceImage]),
        Err(AssetConstraintError::UnacceptedRole(AssetRole::ReferenceImage))
    );
}

#[test]
fn constraints_too_many_of_a_role_rejected() {
    assert_eq!(
        AssetConstraints::first_last_frame().validate(&[AssetRole::FirstFrame, AssetRole::FirstFrame]),
        Err(AssetConstraintError::TooMany { role: AssetRole::FirstFrame, max: 1, actual: 2 })
    );
}

#[test]
fn constraints_reference_image_range_accepts_up_to_max() {
    let constraints = AssetConstraints::new([(AssetRole::ReferenceImage, 0..=3)]);
    assert_eq!(constraints.validate(&[]), Ok(()));
    assert_eq!(constraints.validate(&[AssetRole::ReferenceImage; 3]), Ok(()));
    assert!(constraints.validate(&[AssetRole::ReferenceImage; 4]).is_err());
}

#[test]
fn constraints_reference_images_helper_matches_manual_construction() {
    assert_eq!(AssetConstraints::reference_images(3), AssetConstraints::new([(AssetRole::ReferenceImage, 0..=3)]));
    assert!(AssetConstraints::reference_images(3).accepts(AssetRole::ReferenceImage));
    assert!(!AssetConstraints::reference_images(3).accepts(AssetRole::FirstFrame));
}

// ===== AspectRatio / ImageResolution / VideoAspectRatio / VideoResolution ===================

#[test]
fn aspect_ratio_raw_values() {
    assert_eq!(AspectRatio::Square.raw(), "1:1");
    assert_eq!(AspectRatio::ThreeToFour.raw(), "3:4");
    assert_eq!(AspectRatio::Standard.raw(), "4:3");
    assert_eq!(AspectRatio::Landscape.raw(), "16:9");
    assert_eq!(AspectRatio::Portrait.raw(), "4:5");
    assert_eq!(AspectRatio::Tall.raw(), "9:16");
    assert_eq!(AspectRatio::Wide.raw(), "21:9");
}

#[test]
fn aspect_ratio_round_trips_and_serializes_as_raw() {
    for ratio in AspectRatio::ALL {
        assert_eq!(AspectRatio::from_raw(ratio.raw()), Some(ratio));
        assert_eq!(serde_json::to_string(&ratio).expect("serialize"), format!("\"{}\"", ratio.raw()));
    }
}

#[test]
fn image_resolution_raw_values() {
    assert_eq!(ImageResolution::Sd.raw(), "0.5K");
    assert_eq!(ImageResolution::Hd.raw(), "1K");
    assert_eq!(ImageResolution::Fhd.raw(), "2K");
    assert_eq!(ImageResolution::Uhd.raw(), "4K");
}

#[test]
fn video_aspect_ratio_key_raw_values() {
    assert_eq!(VideoAspectRatio::Square.raw(), "1:1");
    assert_eq!(VideoAspectRatio::Landscape.raw(), "16:9");
    assert_eq!(VideoAspectRatio::Tall.raw(), "9:16");
    assert_eq!(VideoAspectRatio::Auto.raw(), "auto");
}

#[test]
fn video_aspect_ratio_round_trip() {
    for ratio in VideoAspectRatio::ALL {
        assert_eq!(VideoAspectRatio::from_raw(ratio.raw()), Some(ratio));
    }
    assert_eq!(VideoAspectRatio::Auto.ratio(), None);
    assert_eq!(VideoAspectRatio::Landscape.ratio(), Some((16, 9)));
}

#[test]
fn video_resolution_raw_values() {
    assert_eq!(VideoResolution::Sd.raw(), "480p");
    assert_eq!(VideoResolution::Hd.raw(), "720p");
    assert_eq!(VideoResolution::Fhd.raw(), "1080p");
    assert_eq!(VideoResolution::Uhd.raw(), "4k");
    assert_eq!(VideoResolution::Uhd.display_name(), "4K");
}

// ===== ImageGenerationSettings ==============================================================

#[test]
fn image_settings_all_fields_stored() {
    let settings = ImageGenerationSettings { model: image::GPT_5.clone(), aspect_ratio: AspectRatio::Landscape, resolution: ImageResolution::Fhd };
    assert_eq!(settings.model, image::GPT_5);
    assert_eq!(settings.aspect_ratio, AspectRatio::Landscape);
    assert_eq!(settings.resolution, ImageResolution::Fhd);
}

#[test]
fn image_settings_equatable() {
    let a = ImageGenerationSettings { model: image::GEMINI_3_PRO.clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Fhd };
    let b = ImageGenerationSettings { model: image::GEMINI_3_PRO.clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Fhd };
    let c = ImageGenerationSettings { model: image::GPT_5.clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Fhd };
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn image_settings_codable_round_trip_encodes_only_model_id() {
    let settings = ImageGenerationSettings { model: image::FLUX_2_PRO.clone(), aspect_ratio: AspectRatio::Landscape, resolution: ImageResolution::Fhd };
    let json = serde_json::to_string(&settings).expect("serialize");
    assert!(json.contains("\"flux-2-pro\""));
    assert!(!json.contains("short_description"));
    assert!(!json.contains("manufacturer"));

    let decoded: ImageGenerationSettings = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, settings);
    assert_eq!(decoded.model.name, "FLUX.2 Pro");
}

#[test]
fn image_settings_decoding_unknown_model_id_fails() {
    let json = r#"{"model":"not-a-real-model","aspect_ratio":"1:1","resolution":"2K"}"#;
    assert!(serde_json::from_str::<ImageGenerationSettings>(json).is_err());
}

// ===== VideoGenerationSettings ==============================================================

#[test]
fn video_settings_all_fields_stored() {
    let settings = VideoGenerationSettings {
        model: video::SORA_2.clone(),
        aspect_ratio: Some(VideoAspectRatio::Landscape),
        resolution: Some(VideoResolution::Hd),
        duration: 8,
        audio_enabled: true,
    };
    assert_eq!(settings.model, video::SORA_2);
    assert_eq!(settings.aspect_ratio, Some(VideoAspectRatio::Landscape));
    assert_eq!(settings.resolution, Some(VideoResolution::Hd));
    assert_eq!(settings.duration, 8);
}

#[test]
fn video_settings_none_optionals() {
    let settings = VideoGenerationSettings { model: video::VEO_31.clone(), aspect_ratio: None, resolution: None, duration: 5, audio_enabled: true };
    assert_eq!(settings.aspect_ratio, None);
    assert_eq!(settings.resolution, None);
    assert_eq!(settings.duration, 5);
}

#[test]
fn video_settings_equatable() {
    let make = |model: &VideoModel| VideoGenerationSettings {
        model: model.clone(),
        aspect_ratio: Some(VideoAspectRatio::Landscape),
        resolution: Some(VideoResolution::Hd),
        duration: 8,
        audio_enabled: true,
    };
    assert_eq!(make(&video::VEO_31), make(&video::VEO_31));
    assert_ne!(make(&video::VEO_31), make(&video::SORA_2));
}

#[test]
fn video_settings_codable_round_trip_encodes_only_model_id() {
    let settings = VideoGenerationSettings {
        model: video::VEO_31.clone(),
        aspect_ratio: Some(VideoAspectRatio::Landscape),
        resolution: Some(VideoResolution::Hd),
        duration: 8,
        audio_enabled: true,
    };
    let json = serde_json::to_string(&settings).expect("serialize");
    assert!(json.contains("\"veo-3.1\""));
    let decoded: VideoGenerationSettings = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, settings);
}

#[test]
fn video_settings_audio_enabled_defaults_to_true_when_missing() {
    // Rows persisted before the audio toggle existed have no `audio_enabled` key.
    let json = r#"{"model":"veo-3.1","aspect_ratio":"16:9","resolution":"720p","duration":8}"#;
    let decoded: VideoGenerationSettings = serde_json::from_str(json).expect("deserialize");
    assert!(decoded.audio_enabled);
}

// ===== ProviderDescriptor ===================================================================

struct StubImageClient;

#[async_trait::async_trait]
impl ImageProviderClient for StubImageClient {
    async fn generate_image(
        &self,
        _prompt: &str,
        _model: &ImageModel,
        _assets: &[ProviderAsset],
        _aspect_ratio: Option<AspectRatio>,
        _resolution: Option<ImageResolution>,
    ) -> majik_providers::error::Result<Vec<u8>> {
        Ok(Vec::new())
    }

}

struct StubToolClient;

#[async_trait::async_trait]
impl majik_providers::ToolProviderClient for StubToolClient {
    async fn run_tool(&self, _settings: &ToolSettings, _input: &ProviderAsset) -> majik_providers::error::Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

fn stub_descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        id: ProviderId::from("stub"),
        display_name: "Stub",
        logo_asset_name: "stub-logo",
        api_key_placeholder: "stub-...",
        api_key_instructions: "Get your stub key from example.com",
        api_key_url: "https://example.com/keys",
        billing_url: Some("https://example.com/billing"),
        requires_api_key: true,
        is_user_selectable: true,
        supported_image_models: vec![image::GPT_5.clone()],
        supported_video_models: vec![],
        supported_audio_models: vec![],
        image_capabilities: |_| Some(ModelCapabilities::new([AspectRatio::Square], [], 1)),
        video_capabilities: |_| None,
        audio_capabilities: |_| None,
        tool_capabilities: |_| None,
        pricing: |_| Estimate::Exact(majik_providers::Usd(12_345)),
        supported_tool_models: Vec::new(),
        make_image_client: |_| Arc::new(StubImageClient),
        make_video_client: |_| None,
        make_audio_client: |_| None,
        // A provider that routes no text model: the composer hides Improve Prompt for it.
        make_text_client: |_| None,
        make_tool_client: |_| Some(Arc::new(StubToolClient)),
        make_resume_client: |_| None,
    }
}

#[test]
fn descriptor_all_fields_are_stored() {
    let d = stub_descriptor();
    assert_eq!(d.id.as_str(), "stub");
    assert_eq!(d.display_name, "Stub");
    assert_eq!(d.logo_asset_name, "stub-logo");
    assert_eq!(d.billing_url, Some("https://example.com/billing"));
    assert!(d.requires_api_key);
    assert!(d.is_user_selectable);
    assert!(!d.supports_video_generation());
    assert!(!d.supports_audio_generation());
    assert_eq!(d.supported_image_models.len(), 1);
    assert!(d.supported_tool_models.is_empty());
    assert!(!d.supports_tool(ToolId::Upscale));
    let settings = ImageGenerationSettings { model: image::GPT_5.clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Hd };
    assert_eq!(d.price(&PricedJob::Image(&settings)), Estimate::Exact(majik_providers::Usd(12_345)));
}

#[test]
fn descriptor_supports_image_reflects_supported_image_models() {
    let d = stub_descriptor();
    assert!(d.supports_image_model(&image::GPT_5));
    assert!(!d.supports_image_model(&image::FLUX_1_DEV));
}

#[test]
fn descriptor_supports_video_is_false_when_empty() {
    let d = stub_descriptor();
    assert!(!d.supports_video_model(&video::VEO_31));
}

#[test]
fn descriptor_image_capabilities_hook_is_invoked() {
    let d = stub_descriptor();
    let caps = d.image_capabilities(&image::GPT_5).expect("stub always returns capabilities");
    assert_eq!(caps.max_input_images, 1);
    assert!(caps.supports_aspect_ratio());
    assert!(!caps.supports_resolution());
    assert_eq!(caps.asset_constraints, AssetConstraints::reference_images(1));
    assert!(d.video_capabilities(&video::VEO_31).is_none());
    assert!(d.audio_capabilities(&audio::ELEVEN_LABS_V3).is_none());
}

#[test]
fn descriptor_client_factories_are_invoked() {
    let d = stub_descriptor();
    let client = (d.make_tool_client)(&ClientOptions::new("key")).expect("stub routes a tool client");
    let settings = ToolSettings::new(majik_providers::catalog::tool::TOPAZ_UPSCALE.clone());
    let input = ProviderAsset::new(AssetRole::ReferenceImage, "image/png", vec![1, 2, 3]);
    let bytes = tokio::runtime::Runtime::new().expect("runtime").block_on(client.run_tool(&settings, &input)).expect("stub tool succeeds");
    assert!(bytes.is_empty());
    assert!((d.make_video_client)(&ClientOptions::new("key")).is_none());
    assert!((d.make_audio_client)(&ClientOptions::new("key")).is_none());
}

// ===== Voice catalogs =======================================================================

#[test]
fn elevenlabs_catalog_sizes() {
    assert_eq!(elevenlabs::replicate_voices().len(), 26);
    assert_eq!(elevenlabs::fal_voices().len(), 21);
    assert_eq!(elevenlabs::all().len(), 47);
}

#[test]
fn elevenlabs_ids_are_unique_within_each_list() {
    let replicate: HashSet<&str> = elevenlabs::replicate_voices().iter().map(|v| v.id.as_str()).collect();
    assert_eq!(replicate.len(), elevenlabs::replicate_voices().len());
    let fal: HashSet<&str> = elevenlabs::fal_voices().iter().map(|v| v.id.as_str()).collect();
    assert_eq!(fal.len(), elevenlabs::fal_voices().len());
}

#[test]
fn elevenlabs_replicate_order() {
    let ids: Vec<&str> = elevenlabs::replicate_voices().iter().map(|v| v.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "Rachel", "Drew", "Clyde", "Paul", "Aria", "Domi", "Dave", "Roger", "Fin", "Sarah", "James", "Jane", "Juniper", "Arabella", "Hope", "Bradford",
            "Reginald", "Gaming", "Austin", "Kuon", "Blondie", "Priyanka", "Alexandra", "Monika", "Mark", "Grimblewood",
        ]
    );
}

#[test]
fn elevenlabs_fal_order() {
    let ids: Vec<&str> = elevenlabs::fal_voices().iter().map(|v| v.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "Rachel", "Aria", "Roger", "Sarah", "Laura", "Charlie", "George", "Callum", "River", "Liam", "Charlotte", "Alice", "Matilda", "Will", "Jessica",
            "Eric", "Chris", "Brian", "Daniel", "Lily", "Bill",
        ]
    );
}

#[test]
fn elevenlabs_mapped_voice_fields() {
    let rachel = elevenlabs::replicate_voice("Rachel").expect("Rachel is in the Replicate list");
    assert_eq!(rachel.display_name, "Rachel");
    assert_eq!(rachel.subtitle.as_deref(), Some("Clear, Calm, Natural, Neutral, Narrative"));
    assert_eq!(
        rachel.preview_url.as_deref(),
        Some("https://storage.googleapis.com/eleven-public-prod/database/workspace/db14b36fd4854d3aa5f8ce2deefa6b50/voices/mDYJ5aI19GeZeL0uKqb3/AuuZUNwILPreDLyJD8Aq.mp3")
    );
    assert_eq!(rachel.category.as_deref(), Some("professional"));
    assert_eq!(rachel.gender.as_deref(), Some("female"));
    assert_eq!(rachel.accent.as_deref(), Some("canadian"));
    assert_eq!(rachel.language_codes.as_deref(), Some(["en".to_string()].as_slice()));

    let drew = elevenlabs::replicate_voice("Drew").expect("Drew");
    assert_eq!(drew.subtitle, None);
    assert_eq!(drew.accent.as_deref(), Some("scottish"));

    let james = elevenlabs::replicate_voice("James").expect("James");
    assert_eq!(james.language_codes.as_deref(), Some(["pt".to_string()].as_slice()));
    let kuon = elevenlabs::replicate_voice("Kuon").expect("Kuon");
    assert_eq!(kuon.language_codes.as_deref(), Some(["ja".to_string()].as_slice()));
    let priyanka = elevenlabs::replicate_voice("Priyanka").expect("Priyanka");
    assert_eq!(priyanka.language_codes.as_deref(), Some(["hi".to_string()].as_slice()));
}

#[test]
fn elevenlabs_unmapped_voices_have_only_id_name_and_language() {
    for id in ["Domi", "Fin", "Gaming", "Monika"] {
        let v = elevenlabs::replicate_voice(id).expect("unmapped voice present");
        assert_eq!(v.display_name, id);
        assert_eq!(v.subtitle, None);
        assert_eq!(v.preview_url, None);
        assert_eq!(v.category, None);
        assert_eq!(v.gender, None);
        assert_eq!(v.accent, None);
        assert_eq!(v.language_codes.as_deref(), Some(["en".to_string()].as_slice()));
    }
    let mapped_count = elevenlabs::replicate_voices().iter().filter(|v| v.preview_url.is_some()).count();
    assert_eq!(mapped_count, 22);
    assert!(elevenlabs::fal_voices().iter().all(|v| v.preview_url.is_some() && v.subtitle.is_some()));
}

#[test]
fn elevenlabs_fal_voice_fields() {
    let rachel = elevenlabs::fal_voice("Rachel").expect("Rachel is in the fal list");
    assert_eq!(rachel.accent.as_deref(), Some("american"));
    assert_eq!(rachel.category.as_deref(), Some("professional"));
    assert!(rachel.subtitle.as_deref().is_some_and(|s| s.starts_with("A neutral-American accent woman")));
    let river = elevenlabs::fal_voice("River").expect("River");
    assert_eq!(river.gender.as_deref(), Some("neutral"));
    assert_eq!(river.category.as_deref(), Some("premade"));
    let aria = elevenlabs::fal_voice("Aria").expect("Aria");
    assert_eq!(aria.accent.as_deref(), Some("african american"));
    assert_eq!(elevenlabs::fal_default_voice(), rachel);
    assert_eq!(elevenlabs::FAL_DEFAULT_VOICE_ID, "Rachel");
}

#[test]
fn elevenlabs_lookup_prefers_replicate_list_and_rejects_unknown() {
    assert_eq!(elevenlabs::voice("Rachel"), elevenlabs::replicate_voice("Rachel"));
    assert_ne!(elevenlabs::replicate_voice("Rachel"), elevenlabs::fal_voice("Rachel"));
    assert_eq!(elevenlabs::voice("Lily"), elevenlabs::fal_voice("Lily"));
    assert_eq!(elevenlabs::voice("does-not-exist"), None);
    assert_eq!(elevenlabs::replicate_voice("Lily"), None);
    assert_eq!(elevenlabs::fal_voice("Drew"), None);
}

#[test]
fn elevenlabs_preview_urls_are_absolute_mp3s() {
    for v in elevenlabs::all() {
        if let Some(url) = &v.preview_url {
            assert!(url.starts_with("https://storage.googleapis.com/eleven-public-prod/"), "{}: {url}", v.id);
            assert!(url.ends_with(".mp3"), "{}: {url}", v.id);
        }
    }
}

#[test]
fn gemini_catalog_ids() {
    let ids: Vec<&str> = gemini::all().iter().map(|v| v.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "Achernar", "Achird", "Algenib", "Algieba", "Alnilam", "Aoede", "Autonoe", "Callirrhoe", "Charon", "Despina", "Enceladus", "Erinome", "Fenrir",
            "Gacrux", "Iapetus", "Kore", "Laomedeia", "Leda", "Orus", "Pulcherrima", "Puck", "Rasalgethi", "Sadachbia", "Sadaltager", "Schedar", "Sulafat",
            "Umbriel", "Vindemiatrix", "Zephyr", "Zubenelgenubi",
        ]
    );
    assert_eq!(gemini::all().len(), 30);
    let unique: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), 30);
}

#[test]
fn gemini_voice_fields() {
    let kore = gemini::voice("Kore").expect("Kore");
    assert_eq!(kore.display_name, "Kore");
    assert_eq!(kore.gender.as_deref(), Some("female"));
    assert_eq!(kore.preview_url.as_deref(), Some("https://docs.cloud.google.com/static/text-to-speech/docs/audio/chirp3-hd-kore.wav"));
    assert_eq!(kore.language_codes.as_deref(), Some(["multilingual".to_string()].as_slice()));
    assert_eq!(kore.subtitle, None);
    assert_eq!(kore.category, None);
    assert_eq!(kore.accent, None);

    // Aoede keeps the (misspelled) upstream file name.
    let aoede = gemini::voice("Aoede").expect("Aoede");
    assert_eq!(aoede.preview_url.as_deref(), Some("https://docs.cloud.google.com/static/text-to-speech/docs/audio/chirp3-hd-aoeda.wav"));

    let females = gemini::all().iter().filter(|v| v.gender.as_deref() == Some("female")).count();
    let males = gemini::all().iter().filter(|v| v.gender.as_deref() == Some("male")).count();
    assert_eq!((females, males), (14, 16));
    assert_eq!(gemini::voice("nope"), None);
}

#[test]
fn audio_voice_serde_round_trip() {
    let voice = gemini::voice("Puck").expect("Puck").clone();
    let json = serde_json::to_string(&voice).expect("serialize");
    let decoded: AudioVoice = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, voice);
    // Minimal shape (only id + display_name) decodes with every optional defaulting to None.
    let minimal: AudioVoice = serde_json::from_str(r#"{"id":"X","display_name":"X"}"#).expect("minimal");
    assert_eq!(minimal, AudioVoice::new("X", "X"));
}

// ===== Tool catalog ==========================================================================

#[test]
fn tool_catalog_ids_are_unique_and_lookup_works() {
    let ids: HashSet<&str> = tool::ALL.iter().map(|m| m.id).collect();
    assert_eq!(ids.len(), tool::ALL.len());
    for m in tool::ALL {
        assert_eq!(tool::model(m.id), Some(m));
    }
    assert!(tool::model("nope").is_none());
    assert!(tool::of_kind(ToolId::Upscale).all(|m| m.kind == ToolId::Upscale));
    assert!(tool::of_kind(ToolId::RemoveBackground).all(|m| m.kind == ToolId::RemoveBackground));
    assert!(tool::of_kind_and_media(ToolId::Upscale, MediaType::Video).all(|m| m.kind == ToolId::Upscale && m.media == MediaType::Video));
}

/// The composer's Upscale tab decides what it takes from the selected model's media, so a tool
/// model that claims audio (which no tool works on) would draw a card nothing can fill.
#[test]
fn every_tool_model_works_on_an_image_or_a_video() {
    for m in tool::ALL {
        assert_ne!(m.media, MediaType::Audio, "{}", m.id);
        let expected = if m.media == MediaType::Video { AssetRole::ReferenceVideo } else { AssetRole::ReferenceImage };
        assert_eq!(m.input_role(), expected, "{}", m.id);
    }
    // Only upscaling has a video implementation; background removal is an image operation.
    assert!(tool::of_kind_and_media(ToolId::RemoveBackground, MediaType::Video).next().is_none());
    assert!(tool::of_kind_and_media(ToolId::Upscale, MediaType::Video).next().is_some());
}

/// Every model a provider offers has to have a capability row, or its tab would draw no settings
/// and fall back to a factor the endpoint may not accept.
#[test]
fn every_provider_tool_model_has_capabilities() {
    for d in ProviderRegistry::shared().all() {
        for m in &d.supported_tool_models {
            let caps = d.tool_capabilities(m).unwrap_or_else(|| panic!("{}: {} has no capability row", d.display_name, m.id));
            assert!(caps.max_inputs >= 1, "{}: {}", d.display_name, m.id);
            // One clip per run: a video upscale is minutes of provider time and dollars a second.
            if m.media == MediaType::Video {
                assert_eq!(caps.max_inputs, 1, "{}: {}", d.display_name, m.id);
            }
            // A default has to exist for every list, since that is what an untouched draft sends.
            assert_eq!(caps.default_factor().is_some(), !caps.upscale_factors.is_empty(), "{}", m.id);
            assert_eq!(caps.default_variant().is_some(), !caps.variants.is_empty(), "{}", m.id);
            // Background removal has no scale to choose.
            if m.kind == ToolId::RemoveBackground {
                assert!(caps.upscale_factors.is_empty(), "{}: {}", d.display_name, m.id);
            }
        }
    }
}

#[test]
fn tool_model_serializes_as_id_and_round_trips() {
    let json = serde_json::to_string(&tool::TOPAZ_UPSCALE).unwrap();
    assert_eq!(json, "\"topaz-upscale\"");
    let back: ToolModel = serde_json::from_str(&json).unwrap();
    assert_eq!(back, tool::TOPAZ_UPSCALE);
    assert!(serde_json::from_str::<ToolModel>("\"unknown-tool\"").is_err());
}

/// The settings default, so a `ToolSettings` stored before the tools took parameters still reads
/// back rather than failing the whole request.
#[test]
fn tool_settings_fill_in_defaults_for_a_request_stored_without_them() {
    let settings: ToolSettings = serde_json::from_str(r#"{"model":"topaz-upscale"}"#).expect("a bare model still decodes");
    assert_eq!(settings.model, tool::TOPAZ_UPSCALE);
    assert_eq!(settings.upscale_factor, DEFAULT_UPSCALE_FACTOR);
    assert_eq!(settings.variant, None);

    let full = ToolSettings::new(tool::TOPAZ_UPSCALE_VIDEO.clone()).with_factor(4).with_variant("starlight-hq");
    let round_tripped: ToolSettings = serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
    assert_eq!(round_tripped, full);
}

#[test]
fn every_provider_tool_model_is_in_the_catalog() {
    for d in ProviderRegistry::shared().all() {
        for m in &d.supported_tool_models {
            assert_eq!(tool::model(m.id), Some(m), "{}: {}", d.display_name, m.id);
        }
    }
}

/// Models a provider offers with no price of their own. Every entry is deliberate, and the composer
/// shows "No estimate available" for them, so adding a model to a provider forces the decision
/// instead of letting it ship silently unpriced.
///
/// Mock is priced synthetically apart from one model it withholds on purpose, so the composer's
/// "no estimate" path stays reachable in the app and in tests.
/// The resume path polls on the budget of the longest clip we render, because it can't know the
/// length of the job it is re-attaching to. If a model is added that renders for longer, that
/// budget silently stops covering it, so this pins the constant to the capability tables.
#[test]
fn video_budget_covers_every_model() {
    let mut longest = 0;
    for descriptor in ProviderRegistry::shared().all() {
        for model in &descriptor.supported_video_models {
            let Some(caps) = descriptor.video_capabilities(model) else { continue };
            assert!(
                caps.duration_range.max <= Timeouts::MAX_VIDEO_OUTPUT_SECONDS,
                "{} / {} renders up to {} s, past the {} s resume budget in Timeouts",
                descriptor.id.as_str(),
                model.id,
                caps.duration_range.max,
                Timeouts::MAX_VIDEO_OUTPUT_SECONDS,
            );
            longest = longest.max(caps.duration_range.max);
        }
    }
    assert_eq!(longest, Timeouts::MAX_VIDEO_OUTPUT_SECONDS, "the constant should track the longest model, not sit above it");
}

/// The poll budget grows with the clip, and never drops below what a video had before it did.
#[test]
fn video_budget_scales_with_the_clip_and_never_shrinks() {
    for duration in 1..=Timeouts::MAX_VIDEO_OUTPUT_SECONDS {
        assert!(Timeouts::video(duration).total >= Timeouts::VIDEO.total, "{duration}s is shorter than the old flat budget");
    }
    // A 30 s render gets meaningfully longer than a 5 s one; a flat cap was the bug.
    assert!(Timeouts::video(30).total > Timeouts::video(5).total);
    assert_eq!(Timeouts::video_resume().total, Timeouts::video(Timeouts::MAX_VIDEO_OUTPUT_SECONDS).total);
    // The per-request timeout is about one HTTP call, so it doesn't move with the clip.
    assert_eq!(Timeouts::video(30).request, Timeouts::VIDEO.request);
}

/// Clips that land on each side of a per-resolution rate tier, so the sweep can't miss one.
const TOOL_VIDEO_INPUTS: &[(u32, u32, u32)] = &[(640, 360, 5), (1280, 720, 5), (1920, 1080, 10), (3840, 2160, 30)];

const UNPRICED: &[(&str, &str)] = &[
    (ProviderId::MOCK, "flux-1-schnell"),
    // Gemini TTS bills input tokens plus the tokens of the audio it generates, and how long that
    // audio runs doesn't follow from the character count we would have to estimate from.
    (ProviderId::FAL, "gemini-2.5-pro"),
    // OpenRouter bills these per image-output *token* and doesn't publish how many tokens an image
    // comes to, so there is no per-image figure to convert. Its `pricing.image` field is not that
    // figure either: it is per *input* image, which is why Nano Banana Pro reads $0.000002 there
    // against a real $0.15 an image.
    (ProviderId::OPEN_ROUTER, "gemini-3-pro"),
    (ProviderId::OPEN_ROUTER, "gemini-3.1-flash"),
    (ProviderId::OPEN_ROUTER, "gemini-2.5-flash"),
    (ProviderId::OPEN_ROUTER, "gpt-5-image"),
    (ProviderId::OPEN_ROUTER, "gpt-5-image-mini"),
    (ProviderId::OPEN_ROUTER, "gpt-image-2"),
    (ProviderId::OPEN_ROUTER, "muse-image"),
    (ProviderId::OPEN_ROUTER, "seedream-5-pro"),
    (ProviderId::OPEN_ROUTER, "seedream-5-lite"),
    (ProviderId::OPEN_ROUTER, "qwen-image-3"),
    (ProviderId::OPEN_ROUTER, "qwen-image-3-pro"),
    (ProviderId::OPEN_ROUTER, "grok-imagine-image-2"),
];

fn is_unpriced(provider: &ProviderId, model_id: &str) -> bool {
    UNPRICED.iter().any(|(p, m)| *p == provider.as_str() && *m == model_id)
}

/// Builds the jobs a provider would run for `model` across every setting the composer can reach:
/// each resolution, each side of the audio toggle, the shortest and longest clip. That way the
/// guard checks the pricing table covers the whole model, not just the row its defaults use. A
/// table missing one tier is otherwise invisible until the user picks it and the estimate reads
/// "No estimate available".
#[test]
fn every_supported_model_is_priced_or_listed_as_unpriced() {
    let mut gaps = Vec::new();
    let mut check = |d: &ProviderDescriptor, model_id: &str, at: String, job: PricedJob<'_>| {
        if d.price(&job) == Estimate::Unknown && !is_unpriced(&d.id, model_id) {
            gaps.push(format!("{} / {model_id} at {at}", d.id.as_str()));
        }
    };
    for d in ProviderRegistry::shared().all() {
        for model in &d.supported_image_models {
            let Some(caps) = d.image_capabilities(model) else { continue };
            let aspect_ratios = if caps.supported_aspect_ratios.is_empty() {
                vec![AspectRatio::Square]
            } else {
                caps.supported_aspect_ratios.clone()
            };
            let resolutions = if caps.supported_resolutions.is_empty() { vec![ImageResolution::Hd] } else { caps.supported_resolutions.clone() };
            for aspect_ratio in aspect_ratios {
                for resolution in &resolutions {
                    let settings = ImageGenerationSettings { model: model.clone(), aspect_ratio, resolution: *resolution };
                    check(d, model.id, format!("{aspect_ratio:?}/{resolution:?}"), PricedJob::Image(&settings));
                }
            }
        }
        for model in &d.supported_video_models {
            let Some(caps) = d.video_capabilities(model) else { continue };
            let resolutions: Vec<Option<VideoResolution>> =
                if caps.resolutions.is_empty() { vec![None] } else { caps.resolutions.iter().copied().map(Some).collect() };
            // Both sides of the toggle, unless the model has no toggle to offer.
            let audio_settings: &[bool] = match (caps.supports_audio, caps.audio_always_on) {
                (false, _) => &[false],
                (true, true) => &[true],
                (true, false) => &[false, true],
            };
            let durations = caps.duration_range.presets_or_range();
            let durations = [durations.first().copied(), durations.last().copied()];
            for resolution in resolutions {
                for audio_enabled in audio_settings.iter().copied() {
                    for duration in durations.into_iter().flatten() {
                        let settings = VideoGenerationSettings {
                            model: model.clone(),
                            aspect_ratio: caps.default_aspect_ratio(),
                            resolution,
                            duration,
                            audio_enabled,
                        };
                        check(d, model.id, format!("{resolution:?}/audio={audio_enabled}/{duration}s"), PricedJob::Video(&settings));
                    }
                }
            }
        }
        for model in &d.supported_audio_models {
            let Some(caps) = d.audio_capabilities(model) else { continue };
            let Some(voice) = caps.default_voice.clone().or_else(|| caps.supported_voices.first().cloned()) else { continue };
            let settings = AudioGenerationSettings { model: model.clone(), speaker1: voice, speaker2: None };
            check(d, model.id, "the default voice".into(), PricedJob::Audio { settings: &settings, characters: 1_000 });
        }
        for model in &d.supported_tool_models {
            // Sweep the input shapes a tool can be handed: a library image, and for a video tool
            // a clip on each side of every rate tier, at each factor it offers. A table missing one
            // tier is otherwise invisible until a user picks exactly that clip.
            let caps = d.tool_capabilities(model).unwrap_or_default();
            let factors = if caps.upscale_factors.is_empty() { vec![DEFAULT_UPSCALE_FACTOR] } else { caps.upscale_factors.clone() };
            for factor in factors {
                let settings = ToolSettings::new(model.clone()).with_factor(factor);
                let inputs: Vec<(String, ToolInput)> = match model.media {
                    MediaType::Video => TOOL_VIDEO_INPUTS.iter().map(|(w, h, d)| (format!("{factor}x over {w}x{h} for {d}s"), ToolInput::video(*w, *h, *d))).collect(),
                    _ => vec![(format!("{factor}x over 1024x1024"), ToolInput::image(1024, 1024))],
                };
                for (at, input) in inputs {
                    check(d, model.id, at, PricedJob::Tool { settings: &settings, input });
                }
            }
        }
    }
    assert!(gaps.is_empty(), "unpriced and not listed in UNPRICED:\n  {}", gaps.join("\n  "));
}

#[test]
fn descriptor_tool_models_filters_by_kind() {
    let mut d = stub_descriptor();
    d.supported_tool_models = vec![tool::REMBG.clone(), tool::TOPAZ_UPSCALE.clone(), tool::CLARITY_UPSCALER.clone()];
    assert!(d.supports_tool(ToolId::Upscale));
    assert!(d.supports_tool(ToolId::RemoveBackground));
    assert_eq!(d.tool_models(ToolId::Upscale), vec![&tool::TOPAZ_UPSCALE, &tool::CLARITY_UPSCALER]);
    d.supported_tool_models.clear();
    assert!(!d.supports_tool(ToolId::Upscale));
    assert!(d.tool_models(ToolId::Upscale).is_empty());
}

/// The Upscale tab has one model list but two kinds of input; this filter is how it knows whether
/// the provider can upscale a clip at all.
#[test]
fn descriptor_tool_models_filter_by_media() {
    let mut d = stub_descriptor();
    d.supported_tool_models = vec![tool::TOPAZ_UPSCALE.clone(), tool::TOPAZ_UPSCALE_VIDEO.clone(), tool::BRIA_BACKGROUND_REMOVE.clone()];
    assert_eq!(d.tool_models_for(ToolId::Upscale, MediaType::Image), vec![&tool::TOPAZ_UPSCALE]);
    assert_eq!(d.tool_models_for(ToolId::Upscale, MediaType::Video), vec![&tool::TOPAZ_UPSCALE_VIDEO]);
    assert_eq!(d.tool_models_for(ToolId::RemoveBackground, MediaType::Image), vec![&tool::BRIA_BACKGROUND_REMOVE]);
    assert!(d.tool_models_for(ToolId::RemoveBackground, MediaType::Video).is_empty());

    // A provider with only an image upscaler supports the tool, but not over a clip.
    d.supported_tool_models = vec![tool::CLARITY_UPSCALER.clone()];
    assert!(d.supports_tool(ToolId::Upscale));
    assert!(d.tool_models_for(ToolId::Upscale, MediaType::Video).is_empty());
}
