//! The library's domain types: generations, attempts, assets and the ids that name them.
//!
//! A [`Generation`] is one row the app made: its request and a pointer to its active attempt. It
//! stores no file. An [`Asset`] is a file the library holds and carries no role. The two are joined
//! by `output_asset_id` and by the inputs table, so re-using an output as an input shares the row
//! rather than copying bytes. Id strings are persistence keys: they reach `library.db` and the
//! filenames, so they are never rewritten in place.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Stable identifier of a library item: a UUID that is also the generated file's stem.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GenerationId(pub String);

impl GenerationId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for GenerationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for GenerationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlbumId(pub String);

/// Identifier of an asset row: a UUID, unrelated to the file's name or content.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssetId(pub String);

impl AssetId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for AssetId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Default for AlbumId {
    fn default() -> Self {
        Self::new()
    }
}

impl AlbumId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
    Audio,
}

impl MediaType {
    pub const ALL: [MediaType; 3] = [MediaType::Image, MediaType::Video, MediaType::Audio];

    pub fn label(self) -> &'static str {
        match self {
            MediaType::Image => "Image",
            MediaType::Video => "Video",
            MediaType::Audio => "Audio",
        }
    }

    /// What a set of these is called in the UI: the filter menu, empty states, messages about
    /// several rows. "Audio" is already a mass noun, so it is its own plural.
    pub fn plural(self) -> &'static str {
        match self {
            MediaType::Image => "Images",
            MediaType::Video => "Videos",
            MediaType::Audio => "Audio",
        }
    }

    /// Extension a generated file of this type is written with.
    pub fn file_extension(self) -> &'static str {
        match self {
            MediaType::Image => "png",
            MediaType::Video => "mp4",
            MediaType::Audio => "mp3",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" => Some(MediaType::Image),
            "mp4" | "mov" | "m4v" | "webm" => Some(MediaType::Video),
            "mp3" | "wav" | "m4a" | "flac" | "ogg" => Some(MediaType::Audio),
            _ => None,
        }
    }

    /// The kind of file a MIME type (or one of the legacy UTIs older libraries stored) describes.
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        let lower = content_type.to_ascii_lowercase();
        if lower.starts_with("image/") {
            return Some(MediaType::Image);
        }
        if lower.starts_with("video/") {
            return Some(MediaType::Video);
        }
        if lower.starts_with("audio/") {
            return Some(MediaType::Audio);
        }
        match lower.as_str() {
            "public.png" | "public.jpeg" | "public.webp" | "org.webmproject.webp" | "com.compuserve.gif" => Some(MediaType::Image),
            "public.mpeg-4" => Some(MediaType::Video),
            "public.mp3" | "public.wav" | "com.microsoft.waveform-audio" => Some(MediaType::Audio),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Generating,
    Completed,
    Failed,
    /// Completed, but the file is no longer in the library folder (removed or renamed outside the
    /// app). Derived on open and never stored; the row is written back as `Completed` so the item
    /// recovers by itself when the file returns.
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolId {
    Upscale,
    RemoveBackground,
}

impl ToolId {
    pub const ALL: [ToolId; 2] = [ToolId::Upscale, ToolId::RemoveBackground];

    pub fn label(self) -> &'static str {
        match self {
            ToolId::Upscale => "Upscale",
            ToolId::RemoveBackground => "Remove Background",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Generation {
    pub id: GenerationId,
    /// Absolute path of the media file. `None` while generating; for a [`Status::Missing`] row it is
    /// the path the file is expected at.
    pub path: Option<PathBuf>,
    pub media_type: MediaType,
    pub status: Status,
    pub created_at_ms: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_secs: Option<f64>,
    pub file_size: Option<u64>,
    pub is_favorite: bool,
    pub is_upscaled: bool,
    pub thumbnail: Option<PathBuf>,
    /// The asset holding the produced file. `path`, dimensions, duration, size and thumbnail above
    /// are copies of that asset's, filled in when the row is loaded; `None` until the generation
    /// completes (or once the row has failed after its file went missing).
    pub output_asset_id: Option<AssetId>,
    /// Serialized `majik_generation::Request` (without asset bytes); enough to "Recreate".
    pub request_json: Option<String>,
    pub model_name: Option<String>,
    pub provider: Option<String>,
    pub error: Option<String>,
    /// Stable error kind from `GenerationError::kind()` (drives failure-recovery actions).
    pub error_kind: Option<String>,
    pub tool: Option<ToolId>,
    /// The provider's handle for the attempt in flight (`generation_jobs.external_id` / `poll_url`
    /// of the active job while it is queued or running), so it can be resumed after a relaunch.
    /// `None` once the attempt has ended.
    pub job_id: Option<String>,
    pub poll_url: Option<String>,
    /// When the active attempt was asked for (`generation_jobs.created_at`; the row's own creation
    /// for a row without one). The clock a generating row's elapsed time runs from: a retry counts
    /// from the moment it was requested, not from the row's creation or from when the provider
    /// took it, so the timer never shows the previous attempt's time or jumps back to zero.
    pub queued_at_ms: u64,
    /// When the active attempt started at the provider (`None` while it is still queued).
    pub started_at_ms: Option<u64>,
    /// The attempt this row mirrors: its `status`, `error`, `error_kind`, `job_id`, `poll_url` and
    /// `started_at_ms` are the active job's (see [`GenerationJob`]).
    pub active_job_id: Option<JobId>,
}

impl Generation {
    /// The media file, once there is one to read: `None` while generating, after a failure, and
    /// for a [`Status::Missing`] row (whose `path` only says where the file should be). Anything
    /// that hands the file to the OS or reads it goes through here, not `path`.
    pub fn file(&self) -> Option<&Path> {
        if self.status == Status::Completed {
            self.path.as_deref()
        } else {
            None
        }
    }

    pub fn file_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{}.{}", self.id, self.media_type.file_extension()))
    }

    /// Prompt text pulled out of the stored request JSON; `None` when there is none to show (tool
    /// requests carry an empty prompt).
    pub fn prompt(&self) -> Option<String> {
        let json = self.request_json.as_ref()?;
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        v.get("prompt").and_then(|p| p.as_str()).filter(|p| !p.is_empty()).map(str::to_string)
    }

    /// Whether the composer can be put back into the state that made this row: every row made by
    /// this app stores its request, tools included (an upscale recreates as the upscale tab with
    /// its one input).
    pub fn can_recreate(&self) -> bool {
        self.request_json.is_some()
    }

    /// Whether a failed or missing row can be regenerated in place by replaying its stored
    /// request over its stored input assets.
    pub fn can_retry(&self) -> bool {
        self.can_recreate()
    }

    pub fn aspect_ratio_f32(&self) -> Option<f32> {
        match (self.width, self.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some(w as f32 / h as f32),
            _ => None,
        }
    }
}

/// Identifier of one provider attempt of a generation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub String);

impl JobId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, JobStatus::Completed | JobStatus::Failed | JobStatus::Canceled)
    }
}

/// One provider attempt of a generation (flarly's `generationJobs`): a retry is a new attempt of
/// the same row. The row's `status`, `output_asset_id` and `is_upscaled` are copies of its active
/// job's, written together; the handle, the error, the timestamps and what the provider said live
/// only here. Attempts are never deleted, so a soft-deleted generation keeps its history.
#[derive(Clone, Debug, PartialEq)]
pub struct GenerationJob {
    pub id: JobId,
    pub generation_id: GenerationId,
    /// 1 for the first run, +1 per retry.
    pub attempt: u32,
    pub status: JobStatus,
    /// The provider's own id for the job and where to poll it (resume after relaunch).
    pub external_id: Option<String>,
    pub poll_url: Option<String>,
    pub output_asset_id: Option<AssetId>,
    pub error: Option<String>,
    pub error_kind: Option<String>,
    /// The body the client submitted (no headers, data URIs elided), the provider's answer to it,
    /// and the last status / result body seen — each bounded to [`TRACE_BODY_LIMIT`]. The cost
    /// figures providers report live in the final response until they get columns of their own.
    pub provider_request_json: Option<String>,
    pub provider_create_response_json: Option<String>,
    pub provider_final_response_json: Option<String>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

/// Bytes of a traced request or response body kept per entry; the rest is cut with a marker.
pub const TRACE_BODY_LIMIT: usize = 64 * 1024;

const TRUNCATED_MARKER: &str = "…[__truncated__]";

/// Cut `body` to [`TRACE_BODY_LIMIT`] at a character boundary, marking the cut; idempotent.
pub fn bound_body(mut body: String) -> String {
    if body.len() <= TRACE_BODY_LIMIT {
        return body;
    }
    let mut cut = TRACE_BODY_LIMIT - TRUNCATED_MARKER.len();
    while !body.is_char_boundary(cut) {
        cut -= 1;
    }
    body.truncate(cut);
    body.push_str(TRUNCATED_MARKER);
    body
}

/// What an HTTP exchange of a job was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceLabel {
    /// The request that creates the provider job (or, for a synchronous provider, does it all).
    Submit,
    /// A status check while the provider works.
    Poll,
    /// Fetching the finished job's payload.
    Result,
    /// Fetching the produced file itself; only its size is recorded.
    Download,
}

impl TraceLabel {
    pub const ALL: [TraceLabel; 4] = [TraceLabel::Submit, TraceLabel::Poll, TraceLabel::Result, TraceLabel::Download];

    pub fn raw(self) -> &'static str {
        match self {
            TraceLabel::Submit => "submit",
            TraceLabel::Poll => "poll",
            TraceLabel::Result => "result",
            TraceLabel::Download => "download",
        }
    }

    pub fn from_raw(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|label| label.raw() == raw)
    }
}

/// One HTTP exchange of a job, as the provider client saw it. Never carries headers (the API key
/// lives there); bodies are bounded by [`bound_body`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobTrace {
    pub at_ms: u64,
    pub label: TraceLabel,
    pub method: String,
    pub url: String,
    /// `None` when the request never got an answer (see `error`).
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    /// The transport error, when there was no response.
    pub error: Option<String>,
}

impl JobTrace {
    /// Bound both bodies so a trace never grows past what the store keeps.
    pub fn bounded(mut self) -> Self {
        self.request_body = self.request_body.map(bound_body);
        self.response_body = self.response_body.map(bound_body);
        self
    }
}

/// One file the library holds: a generation's output, an input it was given, or an import. Assets
/// carry no role; what an asset *is* to a generation lives on that generation ([`GenerationInput`],
/// [`Generation::output_asset_id`]), so one asset can be the output of one generation and an input
/// of several others without copies.
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub id: AssetId,
    /// sha256 of the bytes, for import dedupe. Outputs migrated from older libraries have none.
    pub content_hash: Option<String>,
    pub kind: MediaType,
    /// MIME type (`image/png`) or a legacy UTI (`public.png`).
    pub content_type: String,
    /// Absolute path of the file; for a [`Asset::missing`] asset, where it is expected.
    pub path: PathBuf,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub file_size: Option<u64>,
    pub duration_secs: Option<f64>,
    pub created_at_ms: u64,
    pub thumbnail: Option<PathBuf>,
    /// The file is not in the library folder any more (removed outside the app). Derived on open,
    /// never stored; the row is kept so it recovers when the file returns.
    pub missing: bool,
}

impl Asset {
    /// The file, when there is one to read.
    pub fn file(&self) -> Option<&Path> {
        if self.missing {
            None
        } else {
            Some(&self.path)
        }
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn aspect_ratio_f32(&self) -> Option<f32> {
        match (self.width, self.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some(w as f32 / h as f32),
            _ => None,
        }
    }
}

/// What a grid cell or the detail shows: a generation (the Library, Favorites and album feeds) or
/// an asset (the Assets feed). Generations in flight have no asset yet, and an asset need not have
/// a generation, so the two stay distinct.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryId {
    Generation(GenerationId),
    Asset(AssetId),
}

impl EntryId {
    pub fn media(&self) -> Option<&GenerationId> {
        match self {
            EntryId::Generation(id) => Some(id),
            EntryId::Asset(_) => None,
        }
    }

    pub fn asset(&self) -> Option<&AssetId> {
        match self {
            EntryId::Generation(_) => None,
            EntryId::Asset(id) => Some(id),
        }
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryId::Generation(id) => write!(f, "m-{id}"),
            EntryId::Asset(id) => write!(f, "a-{id}"),
        }
    }
}

impl From<GenerationId> for EntryId {
    fn from(id: GenerationId) -> Self {
        EntryId::Generation(id)
    }
}

impl From<AssetId> for EntryId {
    fn from(id: AssetId) -> Self {
        EntryId::Asset(id)
    }
}

/// A borrowed grid entry.
#[derive(Clone, Copy, Debug)]
pub enum Entry<'a> {
    Generation(&'a Generation),
    Asset(&'a Asset),
}

impl Entry<'_> {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::Generation(item) => EntryId::Generation(item.id.clone()),
            Entry::Asset(asset) => EntryId::Asset(asset.id.clone()),
        }
    }

    pub fn kind(&self) -> MediaType {
        match self {
            Entry::Generation(item) => item.media_type,
            Entry::Asset(asset) => asset.kind,
        }
    }

    pub fn thumbnail(&self) -> Option<&Path> {
        match self {
            Entry::Generation(item) => item.thumbnail.as_deref(),
            Entry::Asset(asset) => asset.thumbnail.as_deref(),
        }
    }

    /// The file, when there is one to read (see [`Generation::file`] / [`Asset::file`]).
    pub fn file(&self) -> Option<&Path> {
        match self {
            Entry::Generation(item) => item.file(),
            Entry::Asset(asset) => asset.file(),
        }
    }

    pub fn aspect_ratio_f32(&self) -> Option<f32> {
        match self {
            Entry::Generation(item) => item.aspect_ratio_f32(),
            Entry::Asset(asset) => asset.aspect_ratio_f32(),
        }
    }
}

/// An asset handed to a generation in a given role (`reference_image`, `first_frame`, `audio`, …;
/// the role keys are `majik_providers::AssetRole::raw`). `position` orders the assets of one role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInput {
    pub generation_id: GenerationId,
    pub asset_id: AssetId,
    pub role: String,
    pub position: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub id: AlbumId,
    pub name: String,
    pub created_at_ms: u64,
    #[serde(default)]
    pub items: Vec<GenerationId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(request_json: Option<&str>, tool: Option<ToolId>) -> Generation {
        Generation {
            id: GenerationId::new(),
            path: None,
            media_type: MediaType::Image,
            status: Status::Completed,
            created_at_ms: 0,
            width: None,
            height: None,
            duration_secs: None,
            file_size: None,
            is_favorite: false,
            is_upscaled: tool == Some(ToolId::Upscale),
            thumbnail: None,
            output_asset_id: None,
            request_json: request_json.map(str::to_string),
            model_name: None,
            provider: None,
            error: None,
            error_kind: None,
            tool,
            job_id: None,
            poll_url: None,
            queued_at_ms: 0,
            started_at_ms: None,
            active_job_id: None,
        }
    }

    #[test]
    fn bound_body_cuts_once_at_the_limit() {
        let short = "x".repeat(10);
        assert_eq!(bound_body(short.clone()), short);
        let long = "é".repeat(TRACE_BODY_LIMIT);
        let bounded = bound_body(long);
        assert!(bounded.len() <= TRACE_BODY_LIMIT, "{}", bounded.len());
        assert!(bounded.ends_with(TRUNCATED_MARKER));
        assert_eq!(bound_body(bounded.clone()), bounded, "idempotent");
    }

    #[test]
    fn empty_prompt_reads_as_none() {
        assert_eq!(row(Some(r#"{"prompt":"a cat"}"#), None).prompt().as_deref(), Some("a cat"));
        assert_eq!(row(Some(r#"{"prompt":""}"#), Some(ToolId::Upscale)).prompt(), None);
        assert_eq!(row(Some("{not json"), None).prompt(), None);
        assert_eq!(row(None, None).prompt(), None);
    }

    #[test]
    fn a_row_with_a_request_can_recreate_and_retry_tools_included() {
        let tool = row(Some(r#"{"kind":"upscale","prompt":""}"#), Some(ToolId::Upscale));
        assert!(tool.can_recreate() && tool.can_retry());
        let generation = row(Some(r#"{"kind":"image","prompt":"p"}"#), None);
        assert!(generation.can_recreate() && generation.can_retry());
        let bare = row(None, Some(ToolId::Upscale));
        assert!(!bare.can_recreate() && !bare.can_retry(), "nothing stored, nothing to replay");
    }
}
