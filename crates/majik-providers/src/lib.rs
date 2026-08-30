//! Provider descriptors, model catalogs, capability tables and the HTTP clients for fal.ai,
//! Replicate and OpenRouter, plus an offline Mock provider.
//!
//! Nothing here knows about the UI or the library database.

pub mod asset;
pub mod catalog;
pub mod client;
pub mod constants;
pub mod data_uri;
pub mod descriptor;
pub mod dialogue;
pub mod error;
pub mod http;
pub mod id;
pub mod logo;
pub mod models;
pub mod pricing;
pub mod references;
pub mod registry;
pub mod settings;
pub mod transcode;
pub mod voices;

pub mod fal;
pub mod mock;
pub mod openrouter;
pub mod replicate;

pub use asset::{AssetConstraintError, AssetConstraints, AssetRole, ProviderAsset};
pub use client::{
    AudioProviderClient, ClientOptions, ImageProviderClient, JobHandle, OnAccepted, ProviderClient, ResumableClient, TextProviderClient, VideoProviderClient,
};
pub use descriptor::ProviderDescriptor;
pub use error::GenerationError;
pub use id::ProviderId;
pub use models::{
    AspectRatio, AudioModel, AudioModelCapabilities, AudioVoice, ImageModel, ImageResolution, ModelCapabilities, VideoAspectRatio,
    ToolId, ToolModel, VideoDurationRange, VideoModel, VideoModelCapabilities, VideoReferences, VideoResolution,
};
pub use pricing::{Estimate, PricedJob, Usd};
pub use references::{handle, rewrite_handles, ReferenceAssets, ReferenceCounts, ReferenceTagStyle};
pub use registry::ProviderRegistry;
pub use settings::{AudioGenerationSettings, ImageGenerationSettings, VideoGenerationSettings};

/// Bytes of a generated or input media file.
pub type Bytes = Vec<u8>;
