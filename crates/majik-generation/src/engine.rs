//! Job runner: concurrency, retry and timeouts on a dedicated tokio runtime. Results arrive as [`Event`]s on an
//! `async-channel` receiver that the UI polls from its own executor.

use majik_core::model::{JobId, JobTrace, GenerationId, MediaType, ToolId};
use majik_providers::http::Timeouts;
use majik_providers::{ClientOptions, GenerationError, JobHandle, ProviderClient, ProviderId, ProviderRegistry};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::improve::{improve_channel, ImproveReceiver, TextRequest};
use crate::request::{GenerationType, Request};

/// How long a prompt rewrite may take before the composer gives up on it. The provider clients
/// bound their own request too; this also covers a provider that accepts and never answers.
pub const IMPROVE_DEADLINE: Duration = Duration::from_secs(30);

/// Headroom over the provider client's own poll budget. `Timeouts::total` bounds the poll loop
/// alone: fetching the result payload and then downloading the bytes both happen after it, each
/// bounded by that profile's `Timeouts::request` (120 s) rather than by `total`. The engine has to
/// cover them — its deadline wraps the whole sequence, so a shorter headroom fires mid-download and
/// writes off a render the provider has already finished and charged for. It still has to be the
/// outer bound: if it fired first the client's `QueueTimeout` would never be the error the user
/// sees. The one retry restarts the client's full poll budget, which no fixed headroom can cover —
/// a retried job that runs that long loses to this deadline, which is the intent.
const CLIENT_HEADROOM: Duration = Duration::from_secs(300);

/// The provider client's own budget for a job of this kind — the bound the engine has to outlast.
fn client_timeouts(media_type: MediaType) -> Timeouts {
    match media_type {
        MediaType::Image => Timeouts::IMAGE,
        MediaType::Video => Timeouts::VIDEO,
        MediaType::Audio => Timeouts::AUDIO,
    }
}

/// The deadline for a job whose length isn't known: the client's poll budget for that media type
/// plus [`CLIENT_HEADROOM`]. Video renders scale with the clip, so prefer [`stale_timeout_for`]
/// wherever the request is in hand — this is the floor it starts from.
pub fn stale_timeout(media_type: MediaType) -> Duration {
    client_timeouts(media_type).total + CLIENT_HEADROOM
}

/// How long a configured job may take before it is called stale. Video is the case that moves: a
/// 30 s Seedance 2.5 or WAN 3.0 render takes far longer than the 4 s clip beside it in the
/// catalog, so the budget comes from the requested duration rather than a flat per-type constant.
/// Derived from the provider client's own [`Timeouts::video`] budget so the two can't drift into
/// disagreeing about when a job is dead.
pub fn stale_timeout_for(generation_type: &GenerationType) -> Duration {
    match generation_type {
        GenerationType::Video(settings) => Timeouts::video(settings.duration).total + CLIENT_HEADROOM,
        other => stale_timeout(other.media_type()),
    }
}

pub const RETRY_DELAY: Duration = Duration::from_secs(2);
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Work for the engine: `id` is the library row, `job` the attempt of it this run is (every
/// event of the run names both, so a stale attempt's events can be told from the current one's).
#[derive(Clone, Debug)]
pub enum Job {
    /// Run a request: a generation or a tool over its one input asset.
    Generate { id: GenerationId, job: JobId, request: Box<Request> },
    /// Re-attach to a job a previous run left in flight, for the `remaining` part of its deadline.
    Resume { id: GenerationId, job: JobId, provider: ProviderId, media_type: MediaType, handle: JobHandle, remaining: Duration, is_upscaled: bool },
}

impl Job {
    pub fn id(&self) -> &GenerationId {
        match self {
            Job::Generate { id, .. } | Job::Resume { id, .. } => id,
        }
    }

    /// The attempt this run is.
    pub fn job(&self) -> &JobId {
        match self {
            Job::Generate { job, .. } | Job::Resume { job, .. } => job,
        }
    }

    fn provider(&self) -> &ProviderId {
        match self {
            Job::Generate { request, .. } => &request.provider,
            Job::Resume { provider, .. } => provider,
        }
    }

    /// How long the whole job (attempt, retry, polling) may take.
    fn deadline(&self) -> Duration {
        match self {
            Job::Resume { remaining, .. } => *remaining,
            Job::Generate { request, .. } => stale_timeout_for(&request.generation_type),
        }
    }

    fn is_upscaled(&self) -> bool {
        match self {
            Job::Generate { request, .. } => request.generation_type.tool() == Some(ToolId::Upscale),
            Job::Resume { is_upscaled, .. } => *is_upscaled,
        }
    }
}

/// What the engine reports about a run. `id` / `job` are the row and the attempt it was
/// submitted as. Exactly one of Completed / Failed / Cancelled ends a run; Accepted and Trace
/// come before it, as the provider is talked to.
#[derive(Clone, Debug)]
pub enum Event {
    /// The provider accepted the job (its id / poll URL, when the provider exposes them).
    Accepted { id: GenerationId, job: JobId, external_id: Option<String>, poll_url: Option<String> },
    /// One HTTP exchange with the provider, as it happened.
    Trace { id: GenerationId, job: JobId, trace: JobTrace },
    Completed { id: GenerationId, job: JobId, bytes: Vec<u8>, is_upscaled: bool },
    Failed { id: GenerationId, job: JobId, error: Box<GenerationError> },
    Cancelled { id: GenerationId, job: JobId },
}

impl Event {
    pub fn id(&self) -> &GenerationId {
        match self {
            Event::Accepted { id, .. } | Event::Trace { id, .. } | Event::Completed { id, .. } | Event::Failed { id, .. } | Event::Cancelled { id, .. } => id,
        }
    }

    pub fn job(&self) -> &JobId {
        match self {
            Event::Accepted { job, .. } | Event::Trace { job, .. } | Event::Completed { job, .. } | Event::Failed { job, .. } | Event::Cancelled { job, .. } => job,
        }
    }

    /// Whether this event ends its run.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Event::Completed { .. } | Event::Failed { .. } | Event::Cancelled { .. })
    }
}

/// Resolves the API key for a provider (Keychain / settings in the app; a map in tests).
pub type ApiKeys = Arc<dyn Fn(&ProviderId) -> Option<String> + Send + Sync>;

pub struct Engine {
    runtime: tokio::runtime::Runtime,
    keys: ApiKeys,
    limit: Arc<Semaphore>,
    events: async_channel::Sender<Event>,
    active: Arc<Mutex<HashMap<GenerationId, CancellationToken>>>,
}

/// Abstract job runner so callers can depend on a trait rather than the concrete tokio
/// [`Engine`]. Production uses [`Engine`]; tests can substitute [`InertRunner`] to keep the
/// deterministic GPUI test scheduler free of foreign-thread activity.
pub trait JobRunner: Send + Sync + 'static {
    /// Queue a job. Implementations emit exactly one terminal event per job (or none, if inert).
    fn submit(&self, job: Job);
    /// Request cancellation of an in-flight job, if the runner tracks it.
    fn cancel(&self, id: &GenerationId);
    /// Whether a job with this id is currently in flight.
    fn is_active(&self, id: &GenerationId) -> bool;
    /// Number of in-flight jobs.
    fn active_count(&self) -> usize;

    /// Rewrite a prompt with the provider's text model. The single outcome arrives on the returned
    /// receiver; dropping it detaches the caller (the call is bounded by [`IMPROVE_DEADLINE`], and
    /// a rewrite touches no library row, so there is nothing to clean up).
    fn improve_prompt(&self, request: TextRequest) -> ImproveReceiver;
}

impl JobRunner for Engine {
    fn submit(&self, job: Job) {
        Engine::submit(self, job)
    }
    fn cancel(&self, id: &GenerationId) {
        Engine::cancel(self, id)
    }
    fn is_active(&self, id: &GenerationId) -> bool {
        Engine::is_active(self, id)
    }
    fn active_count(&self) -> usize {
        Engine::active_count(self)
    }
    fn improve_prompt(&self, request: TextRequest) -> ImproveReceiver {
        Engine::improve_prompt(self, request)
    }
}

/// A runner that drops every job without executing it. Used in tests that assert on synchronous
/// bookkeeping (placeholder rows, album membership) and must not spawn real worker threads.
pub struct InertRunner;

impl JobRunner for InertRunner {
    fn submit(&self, _job: Job) {}
    fn cancel(&self, _id: &GenerationId) {}
    fn is_active(&self, _id: &GenerationId) -> bool {
        false
    }
    fn active_count(&self) -> usize {
        0
    }
    /// Nothing answers: the sender is dropped, so the caller's `recv` fails at once and it stops
    /// waiting rather than hanging.
    fn improve_prompt(&self, _request: TextRequest) -> ImproveReceiver {
        improve_channel().1
    }
}

impl Engine {
    pub fn new(keys: ApiKeys, concurrency: usize) -> anyhow::Result<(Self, async_channel::Receiver<Event>)> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("majik-generation").build()?;
        let (tx, rx) = async_channel::unbounded();
        Ok((Self { runtime, keys, limit: Arc::new(Semaphore::new(concurrency.max(1))), events: tx, active: Arc::new(Mutex::new(HashMap::new())) }, rx))
    }

    /// Rewrite `request.user` under `request.system` with the provider's text model. Unlike a
    /// generation this makes no row, emits no [`Event`] and is never retried: the composer is
    /// waiting on it, and the user can simply ask again.
    pub fn improve_prompt(&self, request: TextRequest) -> ImproveReceiver {
        self.improve_prompt_within(request, IMPROVE_DEADLINE)
    }

    /// [`Self::improve_prompt`] with an explicit deadline, so tests don't wait out the real one.
    pub fn improve_prompt_within(&self, request: TextRequest, deadline: Duration) -> ImproveReceiver {
        let (tx, rx) = improve_channel();
        let keys = self.keys.clone();
        self.runtime.spawn(async move {
            let outcome = match tokio::time::timeout(deadline, complete_text(&request, &keys)).await {
                Ok(result) => result,
                Err(_) => Err(GenerationError::Timeout),
            };
            if let Err(e) = tx.send(outcome).await {
                tracing::debug!(target: "majik", "the prompt rewrite was no longer wanted: {e}");
            }
        });
        rx
    }

    pub fn is_active(&self, id: &GenerationId) -> bool {
        self.active.lock().unwrap().contains_key(id)
    }

    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    pub fn cancel(&self, id: &GenerationId) {
        if let Some(token) = self.active.lock().unwrap().get(id) {
            token.cancel();
        }
    }

    /// Queue a job. Emits exactly one terminal event (Completed / Failed / Cancelled), preceded by
    /// `Accepted` once the provider reports a job handle and a `Trace` per HTTP exchange. One run
    /// per row: a job for a row that is already running is dropped, since two runs of one attempt
    /// would race for the row. The row counts as active until its terminal event has been sent.
    pub fn submit(&self, job: Job) {
        let id = job.id().clone();
        let attempt = job.job().clone();
        let token = CancellationToken::new();
        {
            let mut active = self.active.lock().unwrap();
            if active.contains_key(&id) {
                tracing::warn!(target: "majik", "{id}: already running, not submitting attempt {attempt} again");
                return;
            }
            active.insert(id.clone(), token.clone());
        }
        let keys = self.keys.clone();
        let limit = self.limit.clone();
        let events = self.events.clone();
        let active = self.active.clone();

        self.runtime.spawn(async move {
            let _permit = limit.acquire_owned().await;
            let outcome = tokio::select! {
                _ = token.cancelled() => Err(None),
                r = run_with_retry(&job, &keys, &events) => r.map_err(Some),
            };
            let event = match outcome {
                Ok(bytes) => Event::Completed { id: id.clone(), job: attempt, bytes, is_upscaled: job.is_upscaled() },
                Err(Some(error)) => Event::Failed { id: id.clone(), job: attempt, error: Box::new(error) },
                Err(None) => Event::Cancelled { id: id.clone(), job: attempt },
            };
            // Dropped from `active` before the outcome is announced, not after: a receiver that
            // reacts to a terminal event and then asks whether the row is still running must be
            // told no. Announcing first leaves a window where it is both finished and active, which
            // is a race the caller cannot avoid — and one that only ever lost on Windows.
            // Cancelling in that window was already a no-op; the outcome is decided by this point.
            active.lock().unwrap().remove(&id);
            if let Err(e) = events.send(event).await {
                tracing::warn!(target: "majik", "{id}: reporting the outcome: {e}");
            }
        });
    }
}

/// One text completion for [`Engine::improve_prompt`], with the same key handling as a generation.
async fn complete_text(request: &TextRequest, keys: &ApiKeys) -> Result<String, GenerationError> {
    let descriptor = ProviderRegistry::shared()
        .descriptor(&request.provider)
        .ok_or_else(|| GenerationError::InvalidRequest(format!("Unknown provider {}", request.provider)))?;
    let key = keys(&request.provider).unwrap_or_default();
    if descriptor.requires_api_key && key.is_empty() {
        return Err(GenerationError::Unauthorized(format!("No API key configured for {}", descriptor.display_name)));
    }
    let client = ProviderClient::with_options(descriptor, &ClientOptions::new(key));
    client.complete_text(&request.system, &request.user, request.max_tokens).await
}

/// One retry after 2 s for retriable errors, all under the per-type deadline.
async fn run_with_retry(job: &Job, keys: &ApiKeys, events: &async_channel::Sender<Event>) -> Result<Vec<u8>, GenerationError> {
    let deadline = job.deadline();
    // A resumed job with no time left is already stale: don't poll at all.
    if deadline.is_zero() {
        return Err(GenerationError::Timeout);
    }
    // One deadline for the entire sequence (attempt + 2s + retry) so a hung provider can never
    // hold its concurrency permit for ~2x the per-type timeout the app relies on.
    match tokio::time::timeout(deadline, run_with_retry_inner(job, keys, events)).await {
        Ok(r) => r,
        Err(_) => Err(GenerationError::Timeout),
    }
}

async fn run_with_retry_inner(job: &Job, keys: &ApiKeys, events: &async_channel::Sender<Event>) -> Result<Vec<u8>, GenerationError> {
    match run_once(job, keys, events).await {
        Err(e) if e.is_retriable() => {
            tracing::info!(target: "majik", "retrying {} after {e}", job.id());
            tokio::time::sleep(RETRY_DELAY).await;
            run_once(job, keys, events).await
        }
        other => other,
    }
}

async fn run_once(job: &Job, keys: &ApiKeys, events: &async_channel::Sender<Event>) -> Result<Vec<u8>, GenerationError> {
    let provider_id = job.provider();
    let descriptor = ProviderRegistry::shared().descriptor(provider_id).ok_or_else(|| GenerationError::InvalidRequest(format!("Unknown provider {provider_id}")))?;
    let key = keys(provider_id).unwrap_or_default();
    if descriptor.requires_api_key && key.is_empty() {
        return Err(GenerationError::Unauthorized(format!("No API key configured for {}", descriptor.display_name)));
    }
    // The provider reports its job handle and every exchange from inside the request; the app
    // persists them so the row can be resumed after a relaunch and its attempt has its trail.
    let (accepted_events, accepted_id, accepted_job) = (events.clone(), job.id().clone(), job.job().clone());
    let on_accepted = Arc::new(move |handle: JobHandle| {
        let event = Event::Accepted { id: accepted_id.clone(), job: accepted_job.clone(), external_id: Some(handle.job_id), poll_url: handle.poll_url };
        if let Err(e) = accepted_events.try_send(event) {
            tracing::warn!(target: "majik", "reporting an accepted job: {e}");
        }
    });
    let (trace_events, trace_id, trace_job) = (events.clone(), job.id().clone(), job.job().clone());
    let on_trace = Arc::new(move |trace: JobTrace| {
        let event = Event::Trace { id: trace_id.clone(), job: trace_job.clone(), trace };
        if let Err(e) = trace_events.try_send(event) {
            tracing::warn!(target: "majik", "reporting a provider exchange: {e}");
        }
    });
    let client = ProviderClient::with_options(descriptor, &ClientOptions { api_key: key, on_accepted: Some(on_accepted), on_trace: Some(on_trace) });
    match job {
        Job::Generate { request, .. } => {
            let assets: Vec<_> = request.assets.iter().map(|a| a.to_provider_asset()).collect();
            match &request.generation_type {
                GenerationType::Image(s) => client.generate_image(&request.prompt, &s.model, &assets, Some(s.aspect_ratio), Some(s.resolution)).await,
                GenerationType::Video(s) => client.generate_video(&request.prompt, &assets, s).await,
                GenerationType::Audio(s) => client.generate_audio(&request.prompt, s).await,
                GenerationType::Upscale(_) => client.upscale_image(tool_input(request)?).await,
                GenerationType::RemoveBackground(_) => client.remove_background(tool_input(request)?).await,
            }
        }
        Job::Resume { handle, media_type, .. } => client.resume(handle, *media_type).await,
    }
}

/// The one image a tool request runs over. Validation refuses a request without it; a retry whose
/// input file vanished is caught before submission, so this is the last line.
fn tool_input(request: &Request) -> Result<&[u8], GenerationError> {
    request.assets.first().map(|a| a.data.as_slice()).ok_or_else(|| GenerationError::InvalidRequest("The tool has no image to run over.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_providers::{catalog, AspectRatio, ImageGenerationSettings, ImageResolution};

    fn engine() -> (Engine, async_channel::Receiver<Event>) {
        Engine::new(Arc::new(|_| Some("test-key".to_string())), 2).unwrap()
    }

    fn image_request(prompt: &str) -> Request {
        let model = catalog::image::ALL.first().expect("catalog populated").clone();
        Request::new(
            ProviderId::mock(),
            GenerationType::Image(ImageGenerationSettings { model, aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd }),
            prompt,
            vec![],
        )
    }

    fn video_request(duration: u32) -> Request {
        let model = catalog::video::ALL.first().expect("catalog populated").clone();
        Request::new(
            ProviderId::mock(),
            GenerationType::Video(majik_providers::VideoGenerationSettings {
                model,
                aspect_ratio: None,
                resolution: None,
                duration,
                audio_enabled: false,
            }),
            "a clip",
            vec![],
        )
    }

    /// A 30 s render takes far longer than a 4 s one, so it can't be judged stale on the same
    /// clock — that flat cap discarded finished renders the user had already paid for.
    #[test]
    fn a_longer_clip_gets_a_longer_deadline() {
        let short = stale_timeout_for(&video_request(4).generation_type);
        let long = stale_timeout_for(&video_request(30).generation_type);
        assert!(long > short, "{long:?} should outlast {short:?}");
        // Nothing that worked before gets a shorter budget than it had.
        for duration in 1..=Timeouts::MAX_VIDEO_OUTPUT_SECONDS {
            assert!(stale_timeout_for(&video_request(duration).generation_type) >= stale_timeout(MediaType::Video), "{duration}s shrank");
        }
    }

    /// The engine has to outlast the provider client, or the client's `QueueTimeout` — the error
    /// that says what actually went wrong — could never reach the user.
    #[test]
    fn the_engine_deadline_outlasts_the_client_poll_budget() {
        for duration in [1, 5, 15, 20, 30] {
            let engine = stale_timeout_for(&video_request(duration).generation_type);
            assert!(engine > Timeouts::video(duration).total, "{duration}s: engine {engine:?} must outlast the client");
        }
        for media_type in [MediaType::Image, MediaType::Video, MediaType::Audio] {
            assert!(stale_timeout(media_type) > client_timeouts(media_type).total, "{media_type:?}");
        }
    }

    /// The poll budget stops at the poll loop: the result fetch and the download run after it on
    /// the client's per-request timeout. The engine's deadline wraps all of it, so the headroom
    /// has to cover both or a finished render is discarded while it is still downloading.
    #[test]
    fn the_headroom_covers_the_work_that_follows_the_poll_budget() {
        for media_type in [MediaType::Image, MediaType::Video, MediaType::Audio] {
            let client = client_timeouts(media_type);
            // The result payload, then the bytes — two requests, plus the retry's delay.
            let tail = client.request * 2 + RETRY_DELAY;
            assert!(CLIENT_HEADROOM >= tail, "{media_type:?}: headroom {CLIENT_HEADROOM:?} must cover {tail:?}");
        }
        let longest = stale_timeout_for(&video_request(Timeouts::MAX_VIDEO_OUTPUT_SECONDS).generation_type);
        assert!(longest >= Timeouts::video(Timeouts::MAX_VIDEO_OUTPUT_SECONDS).total + Timeouts::VIDEO.request * 2);
    }

    /// A `Resume` is bounded by the time its attempt has left, not by the per-type default: the
    /// budget has to be the one the attempt started with.
    #[test]
    fn a_resume_is_bounded_by_its_remaining_time() {
        let job = Job::Resume {
            id: GenerationId::new(),
            job: majik_core::model::JobId::new(),
            provider: ProviderId::mock(),
            media_type: MediaType::Video,
            handle: JobHandle { job_id: "j".into(), poll_url: None },
            remaining: Duration::from_secs(42),
            is_upscaled: false,
        };
        assert_eq!(job.deadline(), Duration::from_secs(42));
    }

    /// The next Completed / Failed / Cancelled, skipping the `Trace`s and the `Accepted` that
    /// precede it.
    fn next_terminal(rx: &async_channel::Receiver<Event>) -> Event {
        loop {
            let event = rx.recv_blocking().unwrap();
            if event.is_terminal() {
                return event;
            }
        }
    }

    /// The next event that isn't a `Trace`.
    fn next_after_traces(rx: &async_channel::Receiver<Event>) -> Event {
        loop {
            match rx.recv_blocking().unwrap() {
                Event::Trace { .. } => continue,
                event => return event,
            }
        }
    }

    /// Every event of one run, up to and including its terminal one.
    fn run_events(rx: &async_channel::Receiver<Event>) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let event = rx.recv_blocking().unwrap();
            let terminal = event.is_terminal();
            events.push(event);
            if terminal {
                return events;
            }
        }
    }

    fn resume_job(id: &GenerationId, job_id: &str, remaining: Duration) -> Job {
        Job::Resume {
            id: id.clone(),
            job: JobId::new(),
            provider: ProviderId::mock(),
            media_type: MediaType::Image,
            handle: JobHandle { job_id: job_id.into(), poll_url: None },
            remaining,
            is_upscaled: false,
        }
    }

    fn text_request(prompt: &str) -> crate::improve::TextRequest {
        crate::improve::TextRequest { provider: ProviderId::mock(), system: "rewrite it".into(), user: prompt.into(), max_tokens: 200 }
    }

    #[test]
    fn improve_prompt_returns_the_rewritten_text() {
        let (engine, _rx) = engine();
        let rx = engine.improve_prompt(text_request("a cat"));
        assert_eq!(rx.recv_blocking().unwrap(), Ok("a cat, cinematic lighting, highly detailed".to_string()));
    }

    #[test]
    fn a_failing_rewrite_reports_the_provider_error() {
        let (engine, _rx) = engine();
        let rx = engine.improve_prompt(text_request("a cat #fail:rateLimited"));
        assert!(matches!(rx.recv_blocking().unwrap(), Err(GenerationError::RateLimited(_))));
    }

    #[test]
    fn a_rewrite_without_a_key_is_unauthorized() {
        let (engine, _rx) = Engine::new(Arc::new(|_| None), 2).unwrap();
        let rx = engine.improve_prompt(text_request("a cat"));
        assert!(matches!(rx.recv_blocking().unwrap(), Err(GenerationError::Unauthorized(_))));
    }

    #[test]
    fn a_rewrite_that_outlasts_the_deadline_times_out() {
        let (engine, _rx) = engine();
        // The mock is told to take longer than the deadline it is given.
        let rx = engine.improve_prompt_within(text_request("a cat #delay:5"), Duration::from_millis(50));
        assert!(matches!(rx.recv_blocking().unwrap(), Err(GenerationError::Timeout)));
    }

    fn tool_request(model: &majik_providers::ToolModel, with_image: bool) -> Request {
        let image = crate::request::AssetInput::new(majik_providers::AssetRole::ReferenceImage, "image/png", majik_core::images::solid_png(2, 2, [1, 2, 3]));
        let mut request = Request::tool(ProviderId::mock(), model, image);
        if !with_image {
            request.assets.clear();
        }
        request
    }

    #[test]
    fn upscale_request_completes_as_upscaled() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(tool_request(&catalog::tool::MOCK_UPSCALE, true)) });
        match next_terminal(&rx) {
            Event::Completed { id: done, bytes, is_upscaled, .. } => {
                assert_eq!(done, id);
                assert!(is_upscaled, "an upscale request marks its output upscaled");
                assert!(!bytes.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn remove_background_request_completes_without_the_upscaled_flag() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id, job: JobId::new(), request: Box::new(tool_request(&catalog::tool::MOCK_REMOVE_BACKGROUND, true)) });
        assert!(matches!(next_terminal(&rx), Event::Completed { is_upscaled: false, .. }));
    }

    #[test]
    fn tool_request_without_an_image_fails_with_invalid_request() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id, job: JobId::new(), request: Box::new(tool_request(&catalog::tool::MOCK_UPSCALE, false)) });
        match next_terminal(&rx) {
            Event::Failed { error, .. } => assert!(matches!(*error, GenerationError::InvalidRequest(_)), "{error}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn every_event_of_a_run_names_the_row_and_the_attempt_it_was_submitted_as() {
        let (engine, rx) = engine();
        let (id, attempt) = (GenerationId::new(), JobId::new());
        engine.submit(Job::Generate { id: id.clone(), job: attempt.clone(), request: Box::new(image_request("a cat #delay:0")) });
        let events = run_events(&rx);
        assert!(events.iter().all(|e| e.id() == &id && e.job() == &attempt), "{events:?}");
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                Event::Trace { .. } => "trace",
                Event::Accepted { .. } => "accepted",
                Event::Completed { .. } => "completed",
                Event::Failed { .. } => "failed",
                Event::Cancelled { .. } => "cancelled",
            })
            .collect();
        assert_eq!(kinds, ["trace", "accepted", "trace", "completed"], "the Mock's submit, its handle, its result, then the outcome");
        let Event::Trace { trace, .. } = &events[0] else { unreachable!() };
        assert_eq!(trace.label, majik_core::model::TraceLabel::Submit);
        assert!(trace.request_body.as_deref().unwrap_or_default().contains("a cat"));
    }

    #[test]
    fn retry_once_traces_both_submits_before_one_terminal_event() {
        let (engine, rx) = engine();
        engine.submit(Job::Generate { id: GenerationId::new(), job: JobId::new(), request: Box::new(image_request("x #delay:0 #fail:rateLimited")) });
        let events = run_events(&rx);
        let submits = events.iter().filter(|e| matches!(e, Event::Trace { trace, .. } if trace.label == majik_core::model::TraceLabel::Submit)).count();
        assert_eq!(submits, 2, "rate limiting is retriable: the engine submits again");
        assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
        assert!(matches!(events.last(), Some(Event::Failed { .. })));
    }

    #[test]
    fn accepted_event_carries_the_provider_job_handle_before_the_result() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request("a cat #delay:0")) });
        match next_after_traces(&rx) {
            Event::Accepted { id: got, external_id, poll_url, .. } => {
                assert_eq!(got, id);
                assert!(external_id.as_deref().unwrap_or("").starts_with("mock-image-"), "{external_id:?}");
                assert_eq!(poll_url, None);
            }
            other => panic!("expected Accepted first, got {other:?}"),
        }
        assert!(matches!(next_terminal(&rx), Event::Completed { .. }));
    }

    #[test]
    fn resume_job_completes_with_the_provider_result() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(resume_job(&id, "mock-image-abc", Duration::from_secs(30)));
        match next_terminal(&rx) {
            Event::Completed { id: got, bytes, is_upscaled, .. } => {
                assert_eq!(got, id);
                assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
                assert!(!is_upscaled);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn resume_with_no_time_left_times_out_without_polling() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(resume_job(&id, "mock-image-abc", Duration::ZERO));
        match next_terminal(&rx) {
            Event::Failed { error, .. } => assert_eq!(*error, GenerationError::Timeout),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resume_of_a_gone_job_fails_once_without_retry() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(resume_job(&id, "mock-image-gone", Duration::from_secs(30)));
        match next_terminal(&rx) {
            Event::Failed { error, .. } => assert_eq!(*error, GenerationError::JobGone),
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "not retried");
    }

    #[test]
    fn mock_generation_completes() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request("a cat #delay:0")) });
        match next_terminal(&rx) {
            Event::Completed { id: got, bytes, .. } => {
                assert_eq!(got, id);
                assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(engine.active_count(), 0);
    }

    /// A terminal event means the run is over, so anything the receiver does in response — asking
    /// whether the row is still running, starting the next attempt — must already see it gone. The
    /// engine used to announce the outcome before dropping the job, and the window between the two
    /// was wide enough to lose on Windows.
    #[test]
    fn a_row_is_no_longer_active_the_moment_its_outcome_is_announced() {
        for prompt in ["done #delay:0", "boom #delay:0 #fail:contentFiltered"] {
            let (engine, rx) = engine();
            let id = GenerationId::new();
            engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request(prompt)) });
            let event = next_terminal(&rx);
            assert!(!engine.is_active(&id), "{prompt}: still active after {event:?}");
            assert_eq!(engine.active_count(), 0, "{prompt}");
        }
    }

    #[test]
    fn non_retriable_failure_is_reported_once() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request("x #delay:0 #fail:contentFiltered")) });
        match next_terminal(&rx) {
            Event::Failed { error, .. } => assert!(matches!(*error, GenerationError::ContentFiltered(_))),
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_second_job_for_a_running_row_is_dropped() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request("slow #delay:30")) });
        std::thread::sleep(Duration::from_millis(100));
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request("again")) });
        assert_eq!(engine.active_count(), 1, "one run per row");
        engine.cancel(&id);
        assert!(matches!(next_terminal(&rx), Event::Cancelled { id: got, .. } if got == id));
        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.try_recv().is_err(), "the dropped job never runs, so no second terminal event");
        assert!(!engine.is_active(&id));
    }

    #[test]
    fn a_row_stays_active_until_its_terminal_event_is_out() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request("quick")) });
        // Every event before the terminal one is sent while the run is still in progress.
        loop {
            let event = rx.recv_blocking().unwrap();
            if event.is_terminal() {
                break;
            }
            assert!(engine.is_active(&id), "still running at {event:?}");
        }
        let gone_by = std::time::Instant::now() + Duration::from_secs(2);
        while engine.is_active(&id) {
            assert!(std::time::Instant::now() < gone_by, "the row is released once its outcome is out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn cancel_emits_cancelled() {
        let (engine, rx) = engine();
        let id = GenerationId::new();
        engine.submit(Job::Generate { id: id.clone(), job: JobId::new(), request: Box::new(image_request("slow #delay:30")) });
        std::thread::sleep(Duration::from_millis(100));
        engine.cancel(&id);
        match next_terminal(&rx) {
            Event::Cancelled { id: got, .. } => assert_eq!(got, id),
            other => panic!("unexpected {other:?}"),
        }
    }
}
