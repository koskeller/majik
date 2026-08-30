//! Shared application state: the library model over `majik_core::Library`, background thumbnails,
//! and the generation engine (real providers or Mock) with its event pump.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use majik_core::model::{AlbumId, Asset, AssetId, GenerationId, Generation, MediaType, Status, ToolId};
use majik_core::{thumbnails, video, Library};
use majik_generation::engine::{stale_timeout, stale_timeout_for, JobRunner};
#[cfg(test)]
use majik_generation::engine::InertRunner;
use majik_generation::{validation, AssetInput, Engine, Event, ImproveReceiver, Job, Request, TextRequest};
use crate::credentials::ApiKeys;
use majik_providers::{AssetRole, JobHandle, ProviderDescriptor, ProviderId, ProviderRegistry, ToolModel};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct AppState {
    pub library: Entity<LibraryModel>,
    pub keys: Arc<ApiKeys>,
}

impl Global for AppState {}

pub fn library(cx: &App) -> Entity<LibraryModel> {
    cx.global::<AppState>().library.clone()
}

pub fn keys(cx: &App) -> Arc<ApiKeys> {
    cx.global::<AppState>().keys.clone()
}

#[derive(Clone, Debug)]
pub enum LibraryEvent {
    Changed,
    /// A generation finished (cancellations excluded). Drives the completion notification.
    GenerationFinished { ok: bool },
    /// Something the user asked for couldn't be done and no row shows it; the window toasts it.
    Error { message: String },
}

pub struct LibraryModel {
    pub lib: Library,
    engine: Box<dyn JobRunner>,
    /// Large-tier thumbnails (`thumbnails::THUMB_LARGE`) that have been rendered, keyed by the
    /// standard-tier path they sit beside, so the feed can pick one during a render without
    /// touching disk. Keyed by path rather than by asset because a thumbnail's identity is its
    /// file's (path, mtime, size): a retried generation reuses its asset row but writes a new file,
    /// so keying by asset would keep drawing the previous attempt's image. Not persisted; it
    /// refills as the tiles that need it come into view.
    large_thumbnails: HashMap<PathBuf, PathBuf>,
    /// Standard-tier paths already queued, so a settled scroll doesn't ask twice.
    large_pending: HashSet<PathBuf>,
    /// Standard-tier paths whose large tier could not be rendered. Kept so an undecodable file
    /// isn't re-attempted on every scroll; cleared only by a relaunch.
    large_failed: HashSet<PathBuf>,
}

impl EventEmitter<LibraryEvent> for LibraryModel {}

/// What the feed/detail hand to the composer panel. The target album isn't part of it: the composer
/// follows the sidebar's selection live.
#[derive(Clone, Debug, Default)]
pub struct PendingCompose {
    /// The generation to recreate: the composer reads its stored request and its input assets from
    /// the library and becomes the state that made it (tool rows open their tool's tab).
    pub recreate: Option<GenerationId>,
}

/// What a drag out of a grid carries: the assets behind the dragged cells (a generation drags its
/// output). One payload serves both an in-app drop on the composer and, once the pointer leaves the
/// window, the native file drag GPUI promotes it to.
#[derive(Clone, Debug, PartialEq)]
pub struct DraggedAssets {
    pub assets: Vec<DraggedAsset>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DraggedAsset {
    pub id: AssetId,
    pub kind: MediaType,
    /// The file, resolved at drag start so drop targets need no library lookup.
    pub path: PathBuf,
    /// The generation behind the cell: the row itself, or the one whose output the asset is. An
    /// album drop adds these; an asset that no generation made (an import) has none.
    pub generation: Option<GenerationId>,
}

impl DraggedAssets {
    pub fn paths(&self) -> Vec<PathBuf> {
        self.assets.iter().map(|a| a.path.clone()).collect()
    }

    /// The generations behind the drag, each once, in drag order.
    pub fn generations(&self) -> Vec<GenerationId> {
        let mut ids: Vec<GenerationId> = Vec::new();
        for id in self.assets.iter().filter_map(|a| a.generation.clone()) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }
}

impl LibraryModel {
    /// Open the library with the real engine. Rows a previous run left in flight are untouched
    /// until [`Self::recover_in_flight`], which needs the API keys the app loads afterwards.
    pub fn open(root: PathBuf, keys: Arc<ApiKeys>, cx: &mut Context<Self>) -> anyhow::Result<Self> {
        let lib = Library::open(root)?;
        let key_fn: majik_generation::engine::ApiKeys = Arc::new(move |p: &ProviderId| keys.get(p.as_str()));
        let (engine, rx) = Engine::new(key_fn, majik_generation::engine::DEFAULT_CONCURRENCY)?;
        let this = Self { lib, engine: Box::new(engine), large_thumbnails: HashMap::new(), large_pending: HashSet::new(), large_failed: HashSet::new() };
        // Detach: the pump runs until the model entity is dropped (its weak upgrade then fails).
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this.update(cx, |m, cx| m.apply(event, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        Ok(this)
    }

    /// Test-only constructor with an [`InertRunner`]: submitted jobs are dropped and no event pump
    /// runs, so the deterministic GPUI test scheduler never sees the engine's worker threads. Tests
    /// that need real generation behaviour cover it in `majik-generation` directly.
    #[cfg(test)]
    pub fn open_inert(root: PathBuf, _keys: Arc<ApiKeys>, cx: &mut Context<Self>) -> anyhow::Result<Self> {
        Self::open_with_runner(root, Box::new(InertRunner), cx)
    }

    /// Test-only constructor over any [`JobRunner`] (a recording one, to assert what a relaunch
    /// submits). Like the app, recovers in-flight rows right away, since its keys are in memory.
    #[cfg(test)]
    pub fn open_with_runner(root: PathBuf, engine: Box<dyn JobRunner>, _cx: &mut Context<Self>) -> anyhow::Result<Self> {
        let lib = Library::open(root)?;
        let mut this = Self { lib, engine, large_thumbnails: HashMap::new(), large_pending: HashSet::new(), large_failed: HashSet::new() };
        this.recover_in_flight();
        Ok(this)
    }

    /// Rows a previous run left "generating", swept once the API keys are loaded (a resume needs
    /// them). A row whose attempt is past its deadline has timed out; one with a provider job
    /// handle is resumed for the time it has left, looking exactly like a live generation; the rest
    /// were interrupted and get Retry. The age used is the attempt's, not the row's: a retried row
    /// is as old as its first attempt, but its current attempt may have started seconds ago.
    pub fn recover_in_flight(&mut self) {
        let now = majik_core::now_ms();
        for item in self.lib.in_flight() {
            // A row this process is already running (generated before the keys finished loading)
            // is not a leftover: resuming it would run the same attempt twice.
            if self.engine.is_active(&item.id) {
                continue;
            }
            let attempt = self.lib.active_job(&item.id);
            let started_at = attempt.as_ref().map(|job| job.started_at_ms.unwrap_or(job.created_at_ms)).unwrap_or(item.created_at_ms);
            // The budget has to be the one the attempt started with, or a long video would be
            // judged timed out on relaunch against a shorter deadline than it was given. A row
            // whose request no longer parses (its model was dropped from the catalog) falls back
            // to the minimum for its media type.
            let deadline = match item.request_json.as_deref().and_then(Request::from_json) {
                Some(request) => stale_timeout_for(&request.generation_type),
                None => {
                    tracing::warn!(target: "majik", "{}: request no longer parses, judging it on the {:?} floor", item.id, item.media_type);
                    stale_timeout(item.media_type)
                }
            };
            let elapsed = Duration::from_millis(now.saturating_sub(started_at));
            if elapsed >= deadline {
                self.lib.fail_generation_kind(&item.id, "Generation timed out. Please try again.", Some("timeout"));
                continue;
            }
            let provider = item.provider.as_deref().map(|p| ProviderId(p.to_string()));
            let resumable = provider.as_ref().and_then(|p| ProviderRegistry::shared().descriptor(p)).is_some_and(|d| d.supports_resume());
            let handle = attempt.as_ref().and_then(|job| job.external_id.clone().map(|job_id| JobHandle { job_id, poll_url: job.poll_url.clone() }));
            match (handle, provider, attempt) {
                (Some(handle), Some(provider), Some(job)) if resumable => self.engine.submit(Job::Resume {
                    id: item.id.clone(),
                    job: job.id,
                    provider,
                    media_type: item.media_type,
                    handle,
                    remaining: deadline - elapsed,
                    is_upscaled: item.tool == Some(ToolId::Upscale),
                }),
                _ => self.lib.fail_generation_kind(&item.id, "Generation was interrupted. Try again.", Some("interrupted")),
            }
        }
    }

    /// The row's active attempt, for tests that feed the model engine events by hand.
    #[cfg(test)]
    pub(crate) fn attempt(&self, id: &GenerationId) -> majik_core::model::JobId {
        self.lib.get(id).and_then(|item| item.active_job_id.clone()).expect("the row has an attempt")
    }

    /// Persist and notify observers. Every mutation goes through here.
    pub(crate) fn changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        cx.emit(LibraryEvent::Changed);
    }

    // ----- engine events -----------------------------------------------------

    /// An engine event is applied to the row's active attempt, and only while that attempt is
    /// open. An event for another attempt (superseded by a retry), for a row since deleted, or for
    /// an attempt that already ended (a second run of it, or a relaunch's outcome) is dropped.
    pub(crate) fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        let id = event.id().clone();
        let active = self.lib.get(&id).filter(|item| item.status == Status::Generating).and_then(|item| item.active_job_id.clone());
        if active.as_ref() != Some(event.job()) {
            tracing::debug!(target: "majik", "{id}: dropping an event of attempt {} (open attempt: {active:?})", event.job());
            return;
        }
        let finished = match event {
            Event::Accepted { id, external_id, poll_url, .. } => {
                self.lib.mark_running(&id, external_id, poll_url);
                None
            }
            Event::Trace { job, trace, .. } => {
                // A trace is bookkeeping: nothing the feed shows changes, so no notification.
                if let Err(e) = self.lib.record_trace(&job, trace) {
                    tracing::warn!(target: "majik", "{id}: recording a provider exchange: {e:#}");
                }
                return;
            }
            Event::Completed { id, bytes, is_upscaled, .. } => {
                let ok = match self.lib.complete_generation(&id, &bytes, is_upscaled) {
                    Ok(_) => {
                        self.thumbnail_after_completion(&id, cx);
                        true
                    }
                    Err(e) => {
                        self.lib.fail_generation_kind(&id, e.to_string(), Some("io"));
                        false
                    }
                };
                Some(ok)
            }
            Event::Failed { id, error, .. } => {
                self.lib.fail_generation_kind(&id, error.to_string(), Some(error.kind()));
                Some(false)
            }
            Event::Cancelled { id, .. } => {
                self.lib.cancel_generation(&id);
                None
            }
        };
        self.changed(cx);
        if let Some(ok) = finished {
            cx.emit(LibraryEvent::GenerationFinished { ok });
        }
    }

    // ----- thumbnails ------------------------------------------------------------

    pub fn start_thumbnails(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<Asset> = self.lib.assets().iter().filter(|a| a.thumbnail.is_none() && !a.missing && a.kind != MediaType::Audio).cloned().collect();
        for asset in pending {
            self.thumbnail_for(asset, cx);
        }
        let audio: Vec<Generation> = self.lib.generations().iter().filter(|i| i.media_type == MediaType::Audio && i.duration_secs.is_none() && i.path.is_some()).cloned().collect();
        for item in audio {
            self.probe_audio(item, cx);
        }
    }

    /// The large tier beside a standard-tier thumbnail, if it has been rendered. A map lookup, safe
    /// to call from a render pass — [`Self::request_large_thumbnails`] is what fills it.
    pub fn large_thumbnail(&self, standard: &std::path::Path) -> Option<&std::path::Path> {
        self.large_thumbnails.get(standard).map(PathBuf::as_path)
    }

    /// Render the large tier for these assets, for cells too big to draw the standard one sharply.
    /// Called when the zoom changes and as new rows scroll in — never from a render pass. Assets
    /// already rendered, already queued, or without a standard thumbnail to sit beside are skipped;
    /// a tier that is already on disk costs one `exists` check and no decode.
    pub fn request_large_thumbnails(&mut self, assets: &[AssetId], cx: &mut Context<Self>) {
        let wanted: Vec<Asset> = assets
            .iter()
            .filter_map(|id| self.lib.asset(id))
            .filter(|asset| !asset.missing && asset.kind != MediaType::Audio)
            .filter(|asset| {
                // The large tier sits beside the standard one, so there is nothing to render
                // until that exists, and nothing to redo once it has been rendered, queued or
                // failed.
                asset.thumbnail.as_deref().is_some_and(|standard| {
                    !self.large_thumbnails.contains_key(standard) && !self.large_pending.contains(standard) && !self.large_failed.contains(standard)
                })
            })
            .cloned()
            .collect();
        for asset in wanted {
            let Some(standard) = asset.thumbnail.clone() else { continue };
            self.large_pending.insert(standard.clone());
            let blobs = self.lib.blobs();
            let id = asset.id.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(async move { thumbnails::ensure_thumbnail_sized(&asset.path, asset.kind, thumbnails::THUMB_LARGE, blobs.as_ref()) })
                    .await;
                this.update(cx, |m, cx| {
                    m.large_pending.remove(&standard);
                    match result {
                        Ok(path) => {
                            m.large_thumbnails.insert(standard, path);
                            cx.notify();
                        }
                        // The standard tier stays on screen; only sharpness is lost. Remembering
                        // the failure stops an undecodable file being re-attempted every scroll.
                        Err(e) => {
                            m.large_failed.insert(standard);
                            tracing::warn!(target: "majik", "large thumbnail for asset {id}: {e:#}");
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn probe_audio(&mut self, item: Generation, cx: &mut Context<Self>) {
        let Some(path) = item.path.clone() else { return };
        let id = item.id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { majik_audio::probe(&path) }).await;
            if let Ok(info) = result {
                this.update(cx, |m, cx| {
                    m.lib.set_media_info(&id, None, None, Some(info.duration_secs));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Render an asset's thumbnail off the UI thread and record it (every generation showing that
    /// asset picks it up).
    fn thumbnail_for(&mut self, asset: Asset, cx: &mut Context<Self>) {
        let blobs = self.lib.blobs();
        let id = asset.id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { thumbnails::ensure_thumbnail_for(&asset.path, asset.kind, blobs.as_ref()) }).await;
            match result {
                Ok(path) => {
                    this.update(cx, |m, cx| {
                        m.lib.set_asset_thumbnail(&id, path);
                        cx.notify();
                    })
                    .ok();
                }
                Err(e) => tracing::warn!(target: "majik", "thumbnail for asset {id}: {e:#}"),
            }
        })
        .detach();
    }

    fn thumbnail_output(&mut self, id: &GenerationId, cx: &mut Context<Self>) {
        let Some(asset) = self.lib.get(id).and_then(|it| it.output_asset_id.as_ref()).and_then(|a| self.lib.asset(a)).cloned() else { return };
        self.thumbnail_for(asset, cx);
    }

    fn thumbnail_after_completion(&mut self, id: &GenerationId, cx: &mut Context<Self>) {
        let Some(item) = self.lib.get(id).cloned() else { return };
        match item.media_type {
            MediaType::Audio => self.probe_audio(item, cx),
            MediaType::Video => {
                // Read dimensions and duration off the UI thread (it reads the whole sample table), then thumbnail.
                let id = item.id.clone();
                let path = item.path.clone();
                cx.spawn(async move |this, cx| {
                    let info = match path {
                        Some(p) => cx
                            .background_spawn(async move { video::probe(&p) })
                            .await
                            .map_err(|e| tracing::warn!(target: "majik", "probe video {id}: {e:#}"))
                            .ok(),
                        None => None,
                    };
                    this.update(cx, |m, cx| {
                        if let Some(info) = info {
                            m.lib.set_media_info(&id, info.width, info.height, info.duration_secs);
                        }
                        m.thumbnail_output(&id, cx);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            MediaType::Image => self.thumbnail_output(id, cx),
        }
    }

    /// Add a file to the library as an asset (a composer drop, a paste, an import) and start its
    /// thumbnail. Content addressed, so the same bytes again return the existing asset.
    pub fn import_asset(&mut self, content_type: &str, bytes: &[u8], cx: &mut Context<Self>) -> anyhow::Result<AssetId> {
        let id = self.lib.import_asset(content_type, bytes)?;
        if let Some(asset) = self.lib.asset(&id).cloned().filter(|a| a.thumbnail.is_none() && a.kind != MediaType::Audio) {
            self.thumbnail_for(asset, cx);
        }
        self.changed(cx);
        Ok(id)
    }

    /// Import files as assets (the Assets grid's Import… and drop). Images are sniffed, audio and
    /// video go by extension; images and audio are validated like composer inputs. Returns the
    /// imported ids and one message per file that couldn't be imported.
    pub fn import_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) -> (Vec<AssetId>, Vec<String>) {
        let mut ids = Vec::new();
        let mut failures = Vec::new();
        for path in paths {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Ok(bytes) = std::fs::read(path) else {
                failures.push(format!("{name} couldn't be read."));
                continue;
            };
            let extension = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
            let content_type = match majik_providers::transcode::sniff_image_mime(&bytes) {
                Some(mime) => mime,
                None => match extension.as_str() {
                    "mp3" => "audio/mpeg",
                    "wav" => "audio/wav",
                    "mp4" | "m4v" => "video/mp4",
                    "mov" => "video/quicktime",
                    _ => {
                        failures.push(format!("{name} isn't a supported image, video or audio file."));
                        continue;
                    }
                },
            };
            let role = match MediaType::from_content_type(content_type) {
                Some(MediaType::Image) => Some(AssetRole::ReferenceImage),
                Some(MediaType::Audio) => Some(AssetRole::Audio),
                _ => None,
            };
            let input = AssetInput::new(role.unwrap_or(AssetRole::ReferenceImage), content_type, bytes);
            if role.is_some() {
                if let Err(e) = validation::validate_asset(&input) {
                    failures.push(format!("{name}: {e}"));
                    continue;
                }
            }
            match self.import_asset(content_type, &input.data, cx) {
                Ok(id) => ids.push(id),
                Err(e) => failures.push(format!("{name}: {e:#}")),
            }
        }
        (ids, failures)
    }

    /// Trash assets (see `Library::delete_assets`); an asset a live generation references is
    /// refused and reported in the error.
    pub fn delete_assets(&mut self, ids: &[AssetId], cx: &mut Context<Self>) -> anyhow::Result<()> {
        let result = self.lib.delete_assets(ids);
        self.changed(cx);
        result
    }

    // ----- simple mutations ------------------------------------------------------

    pub fn set_favorite(&mut self, ids: &[GenerationId], favorite: bool, cx: &mut Context<Self>) {
        for id in ids {
            self.lib.set_favorite(id, favorite);
        }
        self.changed(cx);
    }

    pub fn delete(&mut self, ids: &[GenerationId], cx: &mut Context<Self>) {
        for id in ids {
            self.engine.cancel(id);
        }
        if let Err(e) = self.lib.delete_generations(ids) {
            tracing::error!(target: "majik", "delete: {e:#}");
        }
        self.changed(cx);
    }

    /// Ask the provider's text model to rewrite a prompt. The outcome arrives on the receiver; the
    /// composer awaits it, and drops the receiver to give up on it (a rewrite owns no row).
    pub fn improve_prompt(&self, request: TextRequest) -> ImproveReceiver {
        self.engine.improve_prompt(request)
    }

    pub fn cancel(&mut self, ids: &[GenerationId]) {
        for id in ids {
            self.engine.cancel(id);
        }
    }

    pub fn create_album(&mut self, name: String, cx: &mut Context<Self>) -> AlbumId {
        let id = self.lib.create_album(name);
        self.changed(cx);
        id
    }

    pub fn rename_album(&mut self, id: &AlbumId, name: String, cx: &mut Context<Self>) {
        self.lib.rename_album(id, name);
        self.changed(cx);
    }

    pub fn delete_album(&mut self, id: &AlbumId, cx: &mut Context<Self>) {
        self.lib.delete_album(id);
        self.changed(cx);
    }

    pub fn add_to_album(&mut self, album: &AlbumId, ids: &[GenerationId], cx: &mut Context<Self>) {
        self.lib.add_to_album(album, ids);
        self.changed(cx);
    }

    pub fn remove_from_album(&mut self, album: &AlbumId, ids: &[GenerationId], cx: &mut Context<Self>) {
        self.lib.remove_from_album(album, ids);
        self.changed(cx);
    }

    // ----- generation ------------------------------------------------------------

    /// Insert one placeholder row per request, link the input assets to each and queue the jobs.
    /// `inputs` are the assets the requests' bytes came from; the rows reference them, nothing is
    /// copied.
    pub fn generate(&mut self, requests: Vec<Request>, inputs: &[(AssetId, AssetRole)], album: Option<AlbumId>, cx: &mut Context<Self>) -> Vec<GenerationId> {
        let links: Vec<(AssetId, &str)> = inputs.iter().map(|(asset, role)| (asset.clone(), role.raw())).collect();
        let ids = requests.into_iter().map(|request| self.queue_request(request, &links, album.as_ref())).collect();
        self.changed(cx);
        ids
    }

    /// One placeholder row for `request` — its request stored, `links` referenced as its inputs,
    /// filed in `album` — plus the engine job. The only way a row is created; the caller notifies.
    fn queue_request(&mut self, request: Request, links: &[(AssetId, &str)], album: Option<&AlbumId>) -> GenerationId {
        let id = self.lib.add_generating(
            request.media_type(),
            Some(request.to_json()),
            Some(request.generation_type.model_name().to_string()),
            Some(request.provider.to_string()),
            request.generation_type.tool(),
        );
        if let Err(e) = self.lib.attach_inputs(&id, links) {
            tracing::warn!(target: "majik", "linking inputs of {id}: {e:#}");
        }
        if let Some(album) = album {
            self.lib.add_to_album(album, std::slice::from_ref(&id));
        }
        let Some(job) = self.lib.get(&id).and_then(|item| item.active_job_id.clone()) else {
            tracing::warn!(target: "majik", "{id}: no attempt to run");
            return id;
        };
        self.engine.submit(Job::Generate { id: id.clone(), job, request: Box::new(request) });
        id
    }

    /// Re-run failed rows from their stored request and assets; a tool row replays its request
    /// over its stored input.
    pub fn retry(&mut self, ids: &[GenerationId], cx: &mut Context<Self>) {
        for id in ids {
            let Some(item) = self.lib.get(id).cloned() else { continue };
            // A missing file is regenerated in place: the new file is written under the same id.
            if !matches!(item.status, Status::Failed | Status::Missing) {
                continue;
            }
            let Some(mut request) = item.request_json.as_deref().and_then(Request::from_json) else {
                if item.request_json.is_some() {
                    self.lib.fail_generation(id, "Can't retry: the stored request can't be read.");
                }
                continue;
            };
            let inputs = self.linked_inputs(id);
            request.assets = inputs.iter().filter_map(|(role, asset)| self.asset_input(asset, *role)).collect();
            // The row offered Retry, so say why nothing happens: a request missing an input it
            // was made with would silently become a different generation (a text-only video), and
            // a tool has nothing to run over.
            if request.assets.len() < inputs.len() || (request.generation_type.tool().is_some() && request.assets.is_empty()) {
                self.lib.fail_generation(id, "Can't retry: an input is no longer available.");
                continue;
            }
            match self.lib.start_attempt(id) {
                Ok(job) => self.engine.submit(Job::Generate { id: id.clone(), job, request: Box::new(request) }),
                Err(e) => {
                    tracing::warn!(target: "majik", "retrying {id}: {e:#}");
                    cx.emit(LibraryEvent::Error { message: format!("Couldn't retry: {e:#}") });
                }
            }
        }
        self.changed(cx);
    }

    /// A generation's input assets with their roles, in the stored order. The only place
    /// `generation_inputs` is read, for both Retry and Recreate. A link whose role this build
    /// doesn't know (written by a newer one) is left out and logged.
    pub fn linked_inputs(&self, id: &GenerationId) -> Vec<(AssetRole, Asset)> {
        self.lib
            .inputs(id)
            .into_iter()
            .filter_map(|(link, asset)| match AssetRole::from_raw(&link.role) {
                Some(role) => Some((role, asset)),
                None => {
                    tracing::warn!(target: "majik", "{id}: input {} has an unknown role {:?}", asset.id, link.role);
                    None
                }
            })
            .collect()
    }

    /// `asset`'s bytes as a provider input in `role`. The only place a request's bytes come from.
    /// `None` when the file is gone (the row still references the asset, so a retry picks it up
    /// once the file is back) or can't be read. Image roles report the sniffed type, so validation
    /// sees what the bytes are rather than what the import claimed.
    pub(crate) fn asset_input(&self, asset: &Asset, role: AssetRole) -> Option<AssetInput> {
        if asset.missing {
            return None;
        }
        let bytes = match self.lib.asset_bytes(asset) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(target: "majik", "reading input {}: {e:#}", asset.id);
                return None;
            }
        };
        let content_type = match role {
            AssetRole::Audio | AssetRole::ReferenceVideo => asset.content_type.clone(),
            _ => majik_providers::transcode::sniff_image_mime(&bytes).map(str::to_string).unwrap_or_else(|| asset.content_type.clone()),
        };
        Some(AssetInput::new(role, content_type, bytes))
    }

    /// Run a tool with the composer's selected model over library assets, one row per asset. This
    /// is what the composer's tool tabs use. Assets that aren't readable images are skipped.
    pub fn run_tool_on_assets(&mut self, model: &ToolModel, assets: &[AssetId], provider: ProviderId, album: Option<AlbumId>, cx: &mut Context<Self>) -> usize {
        let mut n = 0;
        for id in assets {
            let Some(input) = self.lib.asset(id).filter(|a| a.kind == MediaType::Image).and_then(|a| self.asset_input(a, AssetRole::ReferenceImage)) else { continue };
            if majik_providers::transcode::sniff_image_mime(&input.data).is_none() {
                continue;
            }
            self.queue_request(Request::tool(provider.clone(), model, input), &[(id.clone(), AssetRole::ReferenceImage.raw())], album.as_ref());
            n += 1;
        }
        self.changed(cx);
        n
    }

    /// Run a tool over completed library images with the provider's default model. This is what
    /// the context menus use. The tool row references the source's output asset directly.
    pub fn run_tool(&mut self, tool: ToolId, ids: &[GenerationId], provider: ProviderId, album: Option<AlbumId>, cx: &mut Context<Self>) -> usize {
        // Without a model the job could only fail at the provider; the menus disable the entry
        // (`tool_available`), so this only catches the menu bar's unconditional actions.
        let Some(model) = ProviderRegistry::shared().descriptor(&provider).and_then(|d| d.default_tool_model(tool)).cloned() else {
            tracing::warn!(target: "majik", "{}: {provider} has no model for it", tool.label());
            return 0;
        };
        let sources: Vec<Asset> = ids
            .iter()
            .filter_map(|id| self.lib.get(id))
            .filter(|it| tool.is_eligible(it))
            .filter_map(|it| self.lib.asset(it.output_asset_id.as_ref()?).cloned())
            .collect();
        let mut n = 0;
        for asset in sources {
            let Some(input) = self.asset_input(&asset, AssetRole::ReferenceImage) else { continue };
            self.queue_request(Request::tool(provider.clone(), &model, input), &[(asset.id, AssetRole::ReferenceImage.raw())], album.as_ref());
            n += 1;
        }
        self.changed(cx);
        n
    }
}

/// The providers the composer can pick from: user-selectable and either keyless or with a key
/// stored. Keys live per provider (Settings → Providers), so several can be ready at once.
pub fn available_providers(cx: &App) -> Vec<&'static ProviderDescriptor> {
    let keys = keys(cx);
    ProviderRegistry::shared().user_selectable().into_iter().filter(|d| !d.requires_api_key || keys.get(d.id.as_str()).is_some()).collect()
}

/// The provider generations and tools go to: the one picked in the composer (`Config::provider`)
/// when it is available, else the first available one (its key was removed, or a fresh install
/// whose first key is for another provider), else the picked one anyway. In that last case nothing
/// can run, and `generate` sends the user to Settings for the key.
pub fn selected_provider(cx: &App) -> &'static ProviderDescriptor {
    let picked = cx.global::<crate::config::Config>().provider_id();
    let available = available_providers(cx);
    if let Some(d) = available.iter().find(|d| d.id == picked) {
        return d;
    }
    if let Some(d) = available.first() {
        return d;
    }
    ProviderRegistry::shared().descriptor(&picked).unwrap_or_else(majik_providers::fal::descriptor)
}

/// Whether the selected provider has a model for `tool`.
pub fn tool_supported(tool: ToolId, cx: &App) -> bool {
    selected_provider(cx).supports_tool(tool)
}

/// Whether `tool` can run on any of `items` right now: the selected provider has a model for it
/// and at least one item is eligible. Every menu entry that offers a tool uses this.
pub fn tool_available(tool: ToolId, items: &[Generation], cx: &App) -> bool {
    tool_supported(tool, cx) && items.iter().any(|item| tool.is_eligible(item))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env, seed_item, Seed};
    use gpui::TestAppContext;
    use majik_core::model::{Status, ToolId};
    use majik_generation::{GenerationType, Request};
    use majik_providers::{catalog, AspectRatio, ImageGenerationSettings, ImageResolution};

    fn image_request(prompt: &str) -> Request {
        Request::new(
            ProviderId::mock(),
            GenerationType::Image(ImageGenerationSettings { model: catalog::image::ALL[0].clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd }),
            prompt,
            vec![],
        )
    }

    fn video_request(duration: u32) -> Request {
        Request::new(
            ProviderId::mock(),
            GenerationType::Video(majik_providers::VideoGenerationSettings {
                model: catalog::video::ALL[0].clone(),
                aspect_ratio: None,
                resolution: None,
                duration,
                audio_enabled: false,
            }),
            "a clip",
            vec![],
        )
    }

    #[gpui::test]
    fn video_completion_probes_dimensions_duration_and_renders_a_poster(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Video, None, Some("mock".into()), Some("Mock".into()), None);
            m.apply(majik_generation::Event::Completed { id: id.clone(), job: m.attempt(&id.clone()), bytes: crate::test_support::mock_clip().to_vec(), is_upscaled: false }, cx);
            id
        });
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!(item.status, Status::Completed);
            assert_eq!((item.width, item.height, item.duration_secs), (Some(64), Some(64), Some(2.0)));
            let poster = item.thumbnail.as_ref().expect("poster rendered without ffmpeg");
            assert_eq!(poster.extension().and_then(|e| e.to_str()), Some("jpg"));
            assert_eq!(&std::fs::read(poster).unwrap()[..2], &[0xFF, 0xD8]);
        });
    }

    #[gpui::test]
    fn undecodable_video_completes_without_a_poster(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Video, None, None, None, None);
            m.apply(majik_generation::Event::Completed { id: id.clone(), job: m.attempt(&id.clone()), bytes: b"not really media".to_vec(), is_upscaled: false }, cx);
            id
        });
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!(item.status, Status::Completed, "the file is kept; only its metadata is missing");
            assert!(item.thumbnail.is_none() && item.duration_secs.is_none());
        });
    }

    #[gpui::test]
    fn a_large_thumbnail_is_rendered_on_request_and_only_once(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        e.library.update(cx, |m, cx| m.start_thumbnails(cx));
        cx.run_until_parked();
        let (asset, standard) = e.library.read_with(cx, |m, _| {
            let asset = m.lib.assets()[0].clone();
            (asset.id.clone(), asset.thumbnail.clone().expect("thumbnailed"))
        });
        e.library.read_with(cx, |m, _| assert!(m.large_thumbnail(&standard).is_none(), "not until a big cell asks"));

        e.library.update(cx, |m, cx| m.request_large_thumbnails(std::slice::from_ref(&asset), cx));
        cx.run_until_parked();

        let large = e.library.read_with(cx, |m, _| m.large_thumbnail(&standard).map(std::path::Path::to_path_buf)).expect("rendered");
        assert!(large.exists(), "{} is on disk", large.display());
        assert_ne!(large, standard, "a file of its own, beside the standard tier");
        assert_eq!(thumbnails::sized_thumb_path(&standard, thumbnails::THUMB_LARGE), Some(large.clone()));

        // Asking again is a no-op: the map already has it.
        e.library.update(cx, |m, cx| m.request_large_thumbnails(std::slice::from_ref(&asset), cx));
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| assert_eq!(m.large_thumbnail(&standard).map(std::path::Path::to_path_buf), Some(large)));
    }

    #[gpui::test]
    fn an_asset_without_a_standard_thumbnail_is_not_asked_for_a_large_one(cx: &mut TestAppContext) {
        // Nothing has been thumbnailed yet (an inert library never does it on its own), and the
        // large tier sits beside the standard one, so there is nothing to render yet.
        let e = env(cx, 1, "Mock");
        let asset = e.library.read_with(cx, |m, _| m.lib.assets()[0].id.clone());
        e.library.update(cx, |m, cx| m.request_large_thumbnails(std::slice::from_ref(&asset), cx));
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| assert_eq!(m.large_thumbnails.len(), 0));
    }

    /// A retry reuses the asset row but writes a new file, so its thumbnails are new files too.
    /// Keying the large tier by asset would keep drawing the previous attempt's image.
    #[gpui::test]
    fn regenerating_an_asset_does_not_keep_the_old_large_tier(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        e.library.update(cx, |m, cx| m.start_thumbnails(cx));
        cx.run_until_parked();
        let (id, asset, standard) = e.library.read_with(cx, |m, _| {
            let generation = m.lib.generations()[0].clone();
            let asset = m.lib.assets()[0].clone();
            (generation.id, asset.id.clone(), asset.thumbnail.clone().expect("thumbnailed"))
        });
        e.library.update(cx, |m, cx| m.request_large_thumbnails(std::slice::from_ref(&asset), cx));
        cx.run_until_parked();
        let stale = e.library.read_with(cx, |m, _| m.large_thumbnail(&standard).map(std::path::Path::to_path_buf)).expect("rendered");

        // Regenerate in place, the way a retry does, then thumbnail again.
        e.library.update(cx, |m, cx| {
            m.lib.complete_generation(&id, &majik_core::images::gradient_png(80, 60, 9), false).unwrap();
            m.start_thumbnails(cx);
        });
        cx.run_until_parked();
        let fresh_standard = e.library.read_with(cx, |m, _| m.lib.asset(&asset).unwrap().thumbnail.clone()).expect("re-thumbnailed");
        assert_ne!(fresh_standard, standard, "a new file, so a new thumbnail path");

        e.library.read_with(cx, |m, _| assert!(m.large_thumbnail(&fresh_standard).is_none(), "the old tier is not served for the new image"));
        e.library.update(cx, |m, cx| m.request_large_thumbnails(std::slice::from_ref(&asset), cx));
        cx.run_until_parked();
        let fresh = e.library.read_with(cx, |m, _| m.large_thumbnail(&fresh_standard).map(std::path::Path::to_path_buf)).expect("rendered again");
        assert_ne!(fresh, stale, "and a fresh one was rendered for it");
    }

    #[gpui::test]
    fn start_thumbnails_fills_a_missing_video_poster_on_relaunch(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = seed_item(&e.library, cx, Seed { media_type: MediaType::Video, ..Seed::default() });
        e.library.read_with(cx, |m, _| assert!(m.lib.get(&id).unwrap().thumbnail.is_none(), "seeded without a poster"));
        e.library.update(cx, |m, cx| m.start_thumbnails(cx));
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| assert!(m.lib.get(&id).unwrap().thumbnail.is_some()));
    }

    #[gpui::test]
    fn thumbnails_are_rendered_off_the_ui_thread(cx: &mut TestAppContext) {
        let e = env(cx, 3, "Mock");
        e.library.read_with(cx, |m, _| assert!(m.lib.generations().iter().all(|i| i.thumbnail.is_none()), "seeded without thumbnails"));
        e.library.update(cx, |m, cx| m.start_thumbnails(cx));
        e.library.read_with(cx, |m, _| assert!(m.lib.generations().iter().all(|i| i.thumbnail.is_none()), "nothing is rendered synchronously on the UI thread"));
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| assert!(m.lib.generations().iter().all(|i| i.thumbnail.as_ref().is_some_and(|t| t.is_file()))));
    }

    #[gpui::test]
    fn video_completion_probes_and_renders_the_poster_off_the_ui_thread(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Video, None, Some("mock".into()), Some("Mock".into()), None);
            m.apply(majik_generation::Event::Completed { id: id.clone(), job: m.attempt(&id.clone()), bytes: crate::test_support::mock_clip().to_vec(), is_upscaled: false }, cx);
            id
        });
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!(item.status, Status::Completed, "the row completes immediately");
            assert!(item.width.is_none() && item.thumbnail.is_none(), "the demux and the poster wait for the background task");
        });
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!((item.width, item.duration_secs), (Some(64), Some(2.0)));
            assert!(item.thumbnail.is_some());
        });
    }

    #[gpui::test]
    fn generate_inserts_placeholders_and_delete_removes(cx: &mut TestAppContext) {
        let e = env(cx, 2, "Mock");
        let before = e.library.read_with(cx, |m, _| m.lib.generations().len());
        let ids = e.library.update(cx, |m, cx| m.generate(vec![image_request("a"), image_request("b")], &[], None, cx));
        assert_eq!(ids.len(), 2);
        e.library.read_with(cx, |m, _| {
            assert_eq!(m.lib.generations().len(), before + 2);
            assert!(ids.iter().all(|id| m.lib.get(id).unwrap().status == Status::Generating));
        });
        // Deleting a generating row cancels it and removes it.
        e.library.update(cx, |m, cx| m.delete(&[ids[0].clone()], cx));
        e.library.read_with(cx, |m, _| assert!(m.lib.get(&ids[0]).is_none()));
    }

    #[gpui::test]
    fn retry_regenerates_a_missing_file_in_place(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let id = e.library.update(cx, |m, cx| {
            let ids = m.generate(vec![image_request("a")], &[], None, cx);
            let path = m.lib.complete_generation(&ids[0], &majik_core::images::solid_png(2, 2, [1, 1, 1]), false).unwrap();
            std::fs::remove_file(path).unwrap();
            m.lib.reload().unwrap();
            ids[0].clone()
        });
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.get(&id).unwrap().status, Status::Missing));
        e.library.update(cx, |m, cx| m.retry(std::slice::from_ref(&id), cx));
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!(item.status, Status::Generating);
            assert!(item.error.is_none());
        });
    }

    #[gpui::test]
    fn retry_ignores_a_missing_row_without_a_request(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        // `env` images carry no request, so a retry has nothing to replay.
        let id = e.library.update(cx, |m, _| {
            let id = m.lib.generations()[0].id.clone();
            std::fs::remove_file(m.lib.get(&id).unwrap().path.clone().unwrap()).unwrap();
            m.lib.reload().unwrap();
            id
        });
        e.library.update(cx, |m, cx| m.retry(std::slice::from_ref(&id), cx));
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.get(&id).unwrap().status, Status::Missing));
    }

    #[gpui::test]
    fn retry_of_a_tool_row_without_its_input_reports_the_failure(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let ids: Vec<_> = e.library.read_with(cx, |m, _| m.lib.generations().iter().map(|i| i.id.clone()).collect());
        e.library.update(cx, |m, cx| m.run_tool(ToolId::Upscale, &ids, ProviderId::mock(), None, cx));
        let row = e.library.read_with(cx, |m, _| m.lib.generations().iter().find(|i| i.tool.is_some()).unwrap().id.clone());
        // The job failed and the source image vanished from the folder behind the app's back.
        e.library.update(cx, |m, _| {
            m.lib.fail_generation(&row, "boom");
            for (_, asset) in m.lib.inputs(&row) {
                std::fs::remove_file(asset.path).unwrap();
            }
            m.lib.reload().unwrap();
        });
        e.library.update(cx, |m, cx| m.retry(std::slice::from_ref(&row), cx));
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&row).unwrap();
            assert!(item.can_retry(), "the menu offered Retry");
            assert_eq!(item.status, Status::Failed, "not silently left as it was");
            assert!(item.error.as_deref().unwrap_or_default().contains("no longer available"), "{:?}", item.error);
        });
    }

    #[gpui::test]
    fn retry_of_an_unreadable_request_reports_the_failure(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, _| {
            let id = m.lib.add_generating(MediaType::Image, Some("{not json".into()), Some("Mock".into()), Some("Mock".into()), None);
            m.lib.fail_generation(&id, "boom");
            id
        });
        e.library.update(cx, |m, cx| m.retry(std::slice::from_ref(&id), cx));
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!(item.status, Status::Failed);
            assert!(item.error.as_deref().unwrap_or_default().contains("can't be read"), "{:?}", item.error);
        });
    }

    fn submit_trace(response: &str) -> majik_core::model::JobTrace {
        majik_core::model::JobTrace {
            at_ms: 1,
            label: majik_core::model::TraceLabel::Submit,
            method: "POST".into(),
            url: "mock://image/run".into(),
            status: Some(202),
            duration_ms: 3,
            request_body: Some(r#"{"prompt":"a"}"#.into()),
            response_body: Some(response.into()),
            error: None,
        }
    }

    #[gpui::test]
    fn completed_event_writes_the_attempt_with_its_handle_trail_and_output(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| m.generate(vec![image_request("a")], &[], None, cx).remove(0));
        e.library.update(cx, |m, cx| {
            let job = m.attempt(&id);
            m.apply(Event::Trace { id: id.clone(), job: job.clone(), trace: submit_trace(r#"{"request_id":"mock-1"}"#) }, cx);
            m.apply(Event::Accepted { id: id.clone(), job: job.clone(), external_id: Some("mock-1".into()), poll_url: None }, cx);
            m.apply(Event::Completed { id: id.clone(), job, bytes: majik_core::images::solid_png(4, 4, [1, 2, 3]), is_upscaled: false }, cx);
        });
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            let job = m.lib.active_job(&id).expect("attempt 1");
            assert_eq!((item.status, job.status, job.attempt), (Status::Completed, majik_core::model::JobStatus::Completed, 1));
            assert_eq!(job.external_id.as_deref(), Some("mock-1"));
            assert!(job.started_at_ms.is_some() && job.finished_at_ms.is_some());
            assert_eq!(job.output_asset_id, item.output_asset_id);
            assert_eq!(job.provider_create_response_json.as_deref(), Some(r#"{"request_id":"mock-1"}"#));
            assert_eq!(job.provider_request_json.as_deref(), Some(r#"{"prompt":"a"}"#));
            assert_eq!(m.lib.traces(&job.id).len(), 1);
        });
    }

    #[gpui::test]
    fn failed_and_cancelled_events_end_the_attempt_accordingly(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let ids = e.library.update(cx, |m, cx| m.generate(vec![image_request("a"), image_request("b")], &[], None, cx));
        e.library.update(cx, |m, cx| {
            m.apply(Event::Failed { id: ids[0].clone(), job: m.attempt(&ids[0]), error: Box::new(majik_providers::GenerationError::Timeout) }, cx);
            m.apply(Event::Cancelled { id: ids[1].clone(), job: m.attempt(&ids[1]) }, cx);
        });
        e.library.read_with(cx, |m, _| {
            let failed = m.lib.active_job(&ids[0]).unwrap();
            let item = m.lib.get(&ids[0]).unwrap();
            assert_eq!((failed.status, item.status), (majik_core::model::JobStatus::Failed, Status::Failed));
            assert_eq!((failed.error.as_deref(), failed.error_kind.as_deref()), (item.error.as_deref(), item.error_kind.as_deref()));
            assert!(failed.error.is_some() && failed.error_kind.is_some() && failed.finished_at_ms.is_some());
            let cancelled = m.lib.active_job(&ids[1]).unwrap();
            assert_eq!(cancelled.status, majik_core::model::JobStatus::Canceled);
            assert_eq!(m.lib.get(&ids[1]).unwrap().error.as_deref(), Some("Cancelled."));
        });
    }

    #[gpui::test]
    fn trace_event_is_recorded_without_notifying(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| m.generate(vec![image_request("a")], &[], None, cx).remove(0));
        let changes = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = changes.clone();
        cx.update(|cx| {
            cx.subscribe(&e.library, move |_, event: &LibraryEvent, _| {
                if matches!(event, LibraryEvent::Changed) {
                    counter.set(counter.get() + 1);
                }
            })
            .detach();
        });
        e.library.update(cx, |m, cx| {
            let job = m.attempt(&id);
            m.apply(Event::Trace { id: id.clone(), job: job.clone(), trace: submit_trace("{}") }, cx);
            m.apply(Event::Trace { id: id.clone(), job, trace: submit_trace("{}") }, cx);
        });
        assert_eq!(changes.get(), 0, "a trace changes nothing the feed shows");
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.traces(&m.attempt(&id)).len(), 2));
    }

    #[gpui::test]
    fn events_of_a_superseded_attempt_are_ignored(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| m.generate(vec![image_request("a")], &[], None, cx).remove(0));
        let first = e.library.update(cx, |m, cx| {
            let first = m.attempt(&id);
            m.apply(Event::Failed { id: id.clone(), job: first.clone(), error: Box::new(majik_providers::GenerationError::Timeout) }, cx);
            m.retry(std::slice::from_ref(&id), cx);
            first
        });
        e.library.read_with(cx, |m, _| {
            assert_eq!(m.lib.active_job(&id).unwrap().attempt, 2);
            assert_eq!(m.lib.get(&id).unwrap().status, Status::Generating);
        });
        e.library.update(cx, |m, cx| m.apply(Event::Completed { id: id.clone(), job: first, bytes: majik_core::images::solid_png(4, 4, [1, 2, 3]), is_upscaled: false }, cx));
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.get(&id).unwrap().status, Status::Generating, "the stale attempt's result is dropped"));
        e.library.update(cx, |m, cx| m.apply(Event::Completed { id: id.clone(), job: m.attempt(&id), bytes: majik_core::images::solid_png(4, 4, [1, 2, 3]), is_upscaled: false }, cx));
        e.library.read_with(cx, |m, _| {
            assert_eq!(m.lib.get(&id).unwrap().status, Status::Completed);
            let attempts: Vec<_> = m.lib.jobs(&id).iter().map(|j| (j.attempt, j.status)).collect();
            assert_eq!(attempts, [(1, majik_core::model::JobStatus::Failed), (2, majik_core::model::JobStatus::Completed)]);
        });
    }

    #[gpui::test]
    fn retry_submits_the_request_as_attempt_two(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, _| {
            let id = m.lib.add_generating(MediaType::Image, Some(image_request("again").to_json()), Some("Mock".into()), Some("Mock".into()), None);
            m.lib.fail_generation(&id, "boom");
            id
        });
        let (library, jobs) = crate::test_support::reopen_recording(&e, cx);
        library.update(cx, |m, cx| m.retry(std::slice::from_ref(&id), cx));
        let jobs = jobs.lock().unwrap();
        let [Job::Generate { id: submitted, job, .. }] = jobs.as_slice() else { panic!("one Generate job, got {jobs:?}") };
        assert_eq!(submitted, &id);
        library.read_with(cx, |m, _| {
            let active = m.lib.active_job(&id).unwrap();
            assert_eq!((&active.id, active.attempt), (job, 2), "the job runs as the new attempt");
        });
    }

    #[gpui::test]
    fn deleting_a_generation_keeps_its_attempts(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| m.generate(vec![image_request("a")], &[], None, cx).remove(0));
        e.library.update(cx, |m, cx| {
            m.apply(Event::Failed { id: id.clone(), job: m.attempt(&id), error: Box::new(majik_providers::GenerationError::Timeout) }, cx);
            m.delete(std::slice::from_ref(&id), cx);
        });
        e.library.read_with(cx, |m, _| {
            assert!(m.lib.get(&id).is_none());
            assert_eq!(m.lib.jobs(&id).len(), 1, "history outlives the row in the feed");
        });
    }

    #[gpui::test]
    fn deleting_a_generation_in_flight_cancels_its_attempt(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| m.generate(vec![image_request("a")], &[], None, cx).remove(0));
        e.library.update(cx, |m, cx| m.delete(std::slice::from_ref(&id), cx));
        e.library.read_with(cx, |m, _| {
            let job = &m.lib.jobs(&id)[0];
            assert_eq!(job.status, majik_core::model::JobStatus::Canceled, "nothing is left running in the history");
            assert!(job.finished_at_ms.is_some());
        });
        // The engine's own outcome arrives afterwards and is dropped.
        let job = e.library.read_with(cx, |m, _| m.lib.jobs(&id)[0].id.clone());
        e.library.update(cx, |m, cx| m.apply(Event::Cancelled { id: id.clone(), job }, cx));
        e.library.read_with(cx, |m, _| assert!(m.lib.get(&id).is_none()));
    }

    #[gpui::test]
    fn a_second_outcome_for_an_attempt_that_ended_is_dropped(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| m.generate(vec![image_request("a")], &[], None, cx).remove(0));
        let job = e.library.update(cx, |m, cx| {
            let job = m.attempt(&id);
            m.apply(Event::Completed { id: id.clone(), job: job.clone(), bytes: majik_core::images::solid_png(4, 4, [1, 2, 3]), is_upscaled: false }, cx);
            job
        });
        // Resuming the same attempt (a relaunch racing the outcome) reports as well.
        e.library.update(cx, |m, cx| m.apply(Event::Failed { id: id.clone(), job, error: Box::new(majik_providers::GenerationError::Timeout) }, cx));
        e.library.read_with(cx, |m, _| {
            assert_eq!(m.lib.get(&id).unwrap().status, Status::Completed, "a finished row is not failed by a late word");
            assert_eq!(m.lib.active_job(&id).unwrap().status, majik_core::model::JobStatus::Completed);
        });
    }

    #[gpui::test]
    fn retry_of_a_row_whose_input_is_gone_reports_the_failure(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let source = e.library.read_with(cx, |m, _| m.lib.assets()[0].clone());
        let id = e.library.update(cx, |m, cx| m.generate(vec![image_request("from a reference")], &[(source.id.clone(), AssetRole::ReferenceImage)], None, cx).remove(0));
        e.library.update(cx, |m, cx| {
            m.apply(Event::Failed { id: id.clone(), job: m.attempt(&id), error: Box::new(majik_providers::GenerationError::Timeout) }, cx);
            std::fs::remove_file(&source.path).unwrap();
            m.lib.reload().unwrap();
        });
        let (library, jobs) = crate::test_support::reopen_recording(&e, cx);
        library.update(cx, |m, cx| m.retry(std::slice::from_ref(&id), cx));
        assert!(jobs.lock().unwrap().is_empty(), "not resubmitted as a text-only request");
        library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!(item.status, Status::Failed);
            assert!(item.error.as_deref().unwrap_or_default().contains("no longer available"), "{:?}", item.error);
            let attempts: Vec<_> = m.lib.jobs(&id).iter().map(|j| (j.attempt, j.status)).collect();
            assert_eq!(attempts, [(1, majik_core::model::JobStatus::Failed), (2, majik_core::model::JobStatus::Failed)], "the refusal is its own attempt");
            assert_eq!(m.lib.jobs(&id)[0].error_kind.as_deref(), Some("timeout"), "the provider's verdict stands");
        });
    }

    #[gpui::test]
    fn retry_that_cannot_be_written_tells_the_user_and_keeps_the_row_failed(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, _| {
            let id = m.lib.add_generating(MediaType::Image, Some(image_request("again").to_json()), Some("Mock".into()), Some("Mock".into()), None);
            m.lib.fail_generation_kind(&id, "boom", Some("server_error"));
            id
        });
        let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = errors.clone();
        cx.update(|cx| {
            cx.subscribe(&e.library, move |_, event: &LibraryEvent, _| {
                if let LibraryEvent::Error { message } = event {
                    seen.borrow_mut().push(message.clone());
                }
            })
            .detach();
        });
        e.library.update(cx, |m, cx| {
            m.lib.db().set_read_only(true).unwrap();
            m.retry(std::slice::from_ref(&id), cx);
            m.lib.db().set_read_only(false).unwrap();
        });
        assert_eq!(errors.borrow().len(), 1, "the refusal reaches the UI: {:?}", errors.borrow());
        e.library.read_with(cx, |m, _| {
            let item = m.lib.get(&id).unwrap();
            assert_eq!((item.status, item.error.as_deref()), (Status::Failed, Some("boom")), "no spinner for a job that doesn't exist");
            assert!(item.can_retry());
        });
    }

    #[gpui::test]
    fn run_tool_refuses_a_provider_without_a_model_for_it(cx: &mut TestAppContext) {
        let e = env(cx, 1, "OpenRouter");
        let ids: Vec<_> = e.library.read_with(cx, |m, _| m.lib.generations().iter().map(|i| i.id.clone()).collect());
        let n = e.library.update(cx, |m, cx| m.run_tool(ToolId::Upscale, &ids, ProviderId::open_router(), None, cx));
        assert_eq!(n, 0, "OpenRouter has no upscaler");
        e.library.read_with(cx, |m, _| assert!(m.lib.generations().iter().all(|i| i.tool.is_none()), "no doomed row was queued"));
    }

    #[gpui::test]
    fn available_providers_are_those_with_a_key_or_needing_none(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let names = |cx: &mut TestAppContext| cx.update(|cx| available_providers(cx).iter().map(|d| d.display_name).collect::<Vec<_>>());
        assert_eq!(names(cx), vec!["Mock", "OpenRouter", "Replicate", "fal.ai"], "every provider is seeded with a key");
        cx.update(|cx| keys(cx).delete("Replicate", cx).detach());
        cx.run_until_parked();
        assert_eq!(names(cx), vec!["Mock", "OpenRouter", "fal.ai"]);
    }

    #[gpui::test]
    fn selected_provider_is_the_picked_one_while_it_has_a_key(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        cx.update(|cx| assert_eq!(selected_provider(cx).id, ProviderId::mock()));
        cx.update(|cx| cx.global_mut::<crate::config::Config>().provider = "Replicate".into());
        cx.update(|cx| assert_eq!(selected_provider(cx).id, ProviderId::replicate()));
    }

    #[gpui::test]
    fn selected_provider_falls_back_to_one_with_a_key(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Replicate");
        cx.update(|cx| keys(cx).delete("Replicate", cx).detach());
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(selected_provider(cx).display_name, "Mock", "the first available provider, alphabetically"));
        for provider in ["Mock", "OpenRouter", "fal.ai"] {
            cx.update(|cx| keys(cx).delete(provider, cx).detach());
        }
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(available_providers(cx).is_empty());
            assert_eq!(selected_provider(cx).id, ProviderId::replicate(), "with no key anywhere the picked provider stands; generate asks for its key");
        });
    }

    #[gpui::test]
    fn tool_available_needs_a_provider_model_and_an_eligible_item(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let item = e.library.read_with(cx, |m, _| m.lib.generations()[0].clone());
        cx.update(|cx| {
            assert!(tool_available(ToolId::Upscale, std::slice::from_ref(&item), cx));
            assert!(!tool_available(ToolId::Upscale, &[], cx), "nothing eligible");
            cx.global_mut::<crate::config::Config>().provider = "OpenRouter".into();
            assert!(!tool_available(ToolId::Upscale, std::slice::from_ref(&item), cx), "OpenRouter has no upscaler");
        });
    }

    #[gpui::test]
    fn run_tool_creates_upscale_placeholder_for_eligible_images(cx: &mut TestAppContext) {
        let e = env(cx, 3, "Mock");
        // Seeded images are completed images → eligible for upscale.
        let ids: Vec<_> = e.library.read_with(cx, |m, _| m.lib.generations().iter().map(|i| i.id.clone()).collect());
        let n = e.library.update(cx, |m, cx| m.run_tool(ToolId::Upscale, &ids, ProviderId::mock(), None, cx));
        assert_eq!(n, ids.len(), "one tool row per eligible image");
        let tool_rows = e.library.read_with(cx, |m, _| m.lib.generations().iter().filter(|i| i.tool == Some(ToolId::Upscale) && i.status == Status::Generating).count());
        assert_eq!(tool_rows, ids.len());
    }

    #[gpui::test]
    fn run_tool_on_assets_stores_selected_tool_model_name_and_skips_non_images(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (image, sound) = e.library.update(cx, |m, cx| {
            let image = m.import_asset("image/png", &majik_core::images::solid_png(3, 3, [7, 7, 7]), cx).unwrap();
            let sound = m.import_asset("audio/wav", b"RIFF....WAVE", cx).unwrap();
            (image, sound)
        });
        let model = &catalog::tool::MOCK_REMOVE_BACKGROUND;
        let n = e.library.update(cx, |m, cx| m.run_tool_on_assets(model, &[image.clone(), sound], ProviderId::mock(), None, cx));
        assert_eq!(n, 1, "only image assets are queued");
        let rows: Vec<_> = e.library.read_with(cx, |m, _| m.lib.generations().iter().filter(|i| i.tool == Some(ToolId::RemoveBackground)).cloned().collect());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_name.as_deref(), Some("Mock Remove Background"));
        assert_eq!(rows[0].status, Status::Generating);
        let stored = rows[0].request_json.as_deref().and_then(Request::from_json).expect("a tool row stores its request");
        assert_eq!(stored.generation_type, GenerationType::for_tool_model(model));
        assert!(rows[0].can_recreate());
        e.library.read_with(cx, |m, _| {
            let inputs = m.lib.inputs(&rows[0].id);
            assert_eq!(inputs.len(), 1);
            assert_eq!(inputs[0].1.id, image, "the row references the asset it was run over");
        });
    }

    #[gpui::test]
    fn import_files_sorts_kinds_validates_and_reports_failures(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let dir = e.dir.path();
        let png = dir.join("photo.png");
        std::fs::write(&png, majik_core::images::solid_png(3, 3, [1, 2, 3])).unwrap();
        let wav = dir.join("voice.wav");
        std::fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
        let clip = dir.join("clip.mp4");
        std::fs::write(&clip, crate::test_support::mock_clip()).unwrap();
        let text = dir.join("notes.txt");
        std::fs::write(&text, b"not media").unwrap();
        let gone = dir.join("gone.png");
        let (ids, failures) = e.library.update(cx, |m, cx| m.import_files(&[png, wav, clip, text, gone], cx));
        assert_eq!(ids.len(), 3);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].contains("notes.txt") && failures[1].contains("gone.png"), "{failures:?}");
        e.library.read_with(cx, |m, _| {
            let kinds: Vec<MediaType> = ids.iter().map(|id| m.lib.asset(id).unwrap().kind).collect();
            assert_eq!(kinds, [MediaType::Image, MediaType::Audio, MediaType::Video]);
            let clip = m.lib.asset(&ids[2]).unwrap();
            assert_eq!((clip.width, clip.height), (Some(64), Some(64)), "video imports are probed");
        });
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| assert!(m.lib.asset(&ids[0]).unwrap().thumbnail.is_some() && m.lib.asset(&ids[2]).unwrap().thumbnail.is_some()));
    }

    #[gpui::test]
    fn delete_assets_refuses_referenced_ones_and_trashes_the_rest(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let output = e.library.read_with(cx, |m, _| m.lib.generations()[0].output_asset_id.clone().unwrap());
        let import = e.library.update(cx, |m, cx| m.import_asset("image/png", &majik_core::images::solid_png(2, 2, [4, 4, 4]), cx).unwrap());
        let result = e.library.update(cx, |m, cx| m.delete_assets(&[output.clone(), import.clone()], cx));
        assert!(result.is_err(), "the referenced one is reported");
        e.library.read_with(cx, |m, _| {
            assert!(m.lib.asset(&output).is_some(), "the generation's output stays");
            assert!(m.lib.asset(&import).is_none(), "the unreferenced import went to the trash");
        });
        assert_eq!(std::fs::read_dir(e.dir.path().join(".majik/trash")).unwrap().count(), 1);
    }

    #[gpui::test]
    fn tool_rows_carry_their_request_with_the_providers_default_model(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let source = e.library.read_with(cx, |m, _| m.lib.generations()[0].id.clone());
        e.library.update(cx, |m, cx| m.run_tool(ToolId::Upscale, std::slice::from_ref(&source), ProviderId::mock(), None, cx));
        e.library.read_with(cx, |m, _| {
            let row = m.lib.generations().iter().find(|i| i.tool == Some(ToolId::Upscale)).expect("the tool row");
            let stored = row.request_json.as_deref().and_then(Request::from_json).expect("stores its request");
            assert_eq!(stored.generation_type.tool(), Some(ToolId::Upscale));
            assert_eq!(stored.generation_type.model_id(), "mock-upscale", "the provider's default upscaler");
            assert_eq!(stored.provider, ProviderId::mock());
            assert!(stored.prompt.is_empty() && row.prompt().is_none(), "tools have no prompt to show");
            assert!(row.can_recreate() && row.can_retry());
        });
    }

    #[gpui::test]
    fn retry_of_a_tool_row_replays_its_request_over_its_input(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let source = e.library.read_with(cx, |m, _| m.lib.generations()[0].id.clone());
        let row = e.library.update(cx, |m, cx| {
            m.run_tool(ToolId::RemoveBackground, std::slice::from_ref(&source), ProviderId::mock(), None, cx);
            let row = m.lib.generations().iter().find(|i| i.tool.is_some()).unwrap().id.clone();
            m.lib.fail_generation(&row, "boom");
            row
        });
        let (library, jobs) = crate::test_support::reopen_recording(&e, cx);
        library.update(cx, |m, cx| m.retry(std::slice::from_ref(&row), cx));
        let jobs = jobs.lock().unwrap();
        let [Job::Generate { id, request, .. }] = jobs.as_slice() else { panic!("one Generate job, got {jobs:?}") };
        assert_eq!(id, &row, "regenerated in place");
        assert_eq!(request.generation_type.tool(), Some(ToolId::RemoveBackground));
        assert_eq!(request.assets.len(), 1, "the stored input's bytes travel with the request");
        assert_eq!(request.assets[0].role, AssetRole::ReferenceImage);
        assert_eq!(request.assets[0].content_type, "image/png");
        library.read_with(cx, |m, _| assert_eq!(m.lib.get(&row).unwrap().status, Status::Generating));
    }

    #[gpui::test]
    fn run_tool_on_a_library_image_references_its_output_asset(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let (source, output) = e.library.read_with(cx, |m, _| {
            let item = &m.lib.generations()[0];
            (item.id.clone(), item.output_asset_id.clone().unwrap())
        });
        e.library.update(cx, |m, cx| m.run_tool(ToolId::Upscale, std::slice::from_ref(&source), ProviderId::mock(), None, cx));
        e.library.read_with(cx, |m, _| {
            let row = m.lib.generations().iter().find(|i| i.tool.is_some()).unwrap();
            assert_eq!(m.lib.inputs(&row.id)[0].1.id, output, "no copy: the tool row points at the same asset");
            assert_eq!(m.lib.assets().len(), 1);
        });
    }

    #[gpui::test]
    fn generate_links_the_input_assets_to_every_row(cx: &mut TestAppContext) {
        let e = env(cx, 1, "Mock");
        let asset = e.library.update(cx, |m, cx| m.import_asset("image/png", &majik_core::images::solid_png(3, 3, [1, 2, 3]), cx).unwrap());
        let ids = e.library.update(cx, |m, cx| m.generate(vec![image_request("a"), image_request("b")], &[(asset.clone(), AssetRole::ReferenceImage)], None, cx));
        e.library.read_with(cx, |m, _| {
            for id in &ids {
                let inputs = m.lib.inputs(id);
                assert_eq!(inputs.len(), 1);
                assert_eq!((inputs[0].0.role.as_str(), &inputs[0].1.id), ("reference_image", &asset));
            }
            assert_eq!(m.lib.generations_using(&asset).len(), 2);
            assert!(m.lib.is_referenced(&asset));
        });
    }

    #[gpui::test]
    fn import_asset_thumbnails_off_the_ui_thread_and_dedupes(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let bytes = majik_core::images::solid_png(5, 5, [9, 8, 7]);
        let a = e.library.update(cx, |m, cx| m.import_asset("image/png", &bytes, cx).unwrap());
        e.library.read_with(cx, |m, _| assert!(m.lib.asset(&a).unwrap().thumbnail.is_none(), "nothing rendered synchronously"));
        cx.run_until_parked();
        e.library.read_with(cx, |m, _| assert!(m.lib.asset(&a).unwrap().thumbnail.as_ref().is_some_and(|t| t.is_file())));
        let b = e.library.update(cx, |m, cx| m.import_asset("image/png", &bytes, cx).unwrap());
        assert_eq!(a, b);
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.assets().len(), 1));
        assert!(e.library.update(cx, |m, cx| m.import_asset("text/plain", b"x", cx)).is_err());
    }

    #[gpui::test]
    fn run_tool_skips_ineligible_and_already_upscaled(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        // A generating (not completed) row is not eligible.
        let id = e.library.update(cx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Image, None, None, Some("Mock".into()), None);
            let _ = &cx;
            id
        });
        let n = e.library.update(cx, |m, cx| m.run_tool(ToolId::Upscale, &[id], ProviderId::mock(), None, cx));
        assert_eq!(n, 0, "generating rows are not tool-eligible");
    }

    #[gpui::test]
    fn retry_restarts_a_failed_row(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = e.library.update(cx, |m, cx| {
            let id = m.lib.add_generating(MediaType::Image, Some(image_request("retry me").to_json()), Some("Mock".into()), Some("Mock".into()), None);
            m.lib.fail_generation_kind(&id, "boom", Some("server_error"));
            cx.notify();
            id
        });
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.get(&id).unwrap().status, Status::Failed));
        e.library.update(cx, |m, cx| m.retry(std::slice::from_ref(&id), cx));
        e.library.read_with(cx, |m, _| {
            let it = m.lib.get(&id).unwrap();
            assert_eq!(it.status, Status::Generating, "failed row flipped back to generating");
            assert!(it.error.is_none());
        });
    }

    // ----- relaunch recovery -----------------------------------------------------------------

    /// A row a previous run left generating, with `provider` and an optional job handle.
    fn in_flight_row(e: &crate::test_support::TestEnv, cx: &mut TestAppContext, provider: &str, job_id: Option<&str>, tool: Option<ToolId>) -> GenerationId {
        e.library.update(cx, |m, _| {
            let id = m.lib.add_generating(MediaType::Image, Some(image_request("x").to_json()), Some("mock".into()), Some(provider.into()), tool);
            m.lib.mark_running(&id, job_id.map(str::to_string), job_id.map(|_| "https://poll.example/1".to_string()));
            id
        })
    }

    #[gpui::test]
    fn relaunch_resumes_a_row_with_a_job_handle_for_the_time_it_has_left(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = in_flight_row(&e, cx, "Mock", Some("mock-image-1"), None);
        let (lib2, jobs) = crate::test_support::reopen_recording(&e, cx);
        let jobs = jobs.lock().unwrap();
        let [Job::Resume { id: job_for, job, provider, media_type, handle, remaining, is_upscaled }] = jobs.as_slice() else { panic!("one resume job, got {jobs:?}") };
        assert_eq!((job_for, provider, *media_type, *is_upscaled), (&id, &ProviderId::mock(), MediaType::Image, false));
        lib2.read_with(cx, |m, _| assert_eq!(Some(job), m.lib.get(&id).unwrap().active_job_id.as_ref(), "resumed as the attempt that was in flight"));
        assert_eq!(*handle, JobHandle { job_id: "mock-image-1".into(), poll_url: Some("https://poll.example/1".into()) });
        let image_deadline = majik_generation::engine::stale_timeout(MediaType::Image);
        assert!(*remaining <= image_deadline && *remaining > image_deadline - Duration::from_secs(20), "image deadline minus the age: {remaining:?}");
        lib2.read_with(cx, |m, _| {
            let it = m.lib.get(&id).unwrap();
            assert_eq!(it.status, Status::Generating, "looks like a live generation");
            assert_eq!(it.job_id.as_deref(), Some("mock-image-1"), "the handle survives");
        });
    }

    #[gpui::test]
    fn a_resumed_row_completes_in_place(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = in_flight_row(&e, cx, "Mock", Some("mock-image-1"), None);
        let (lib2, _jobs) = crate::test_support::reopen_recording(&e, cx);
        lib2.update(cx, |m, cx| m.apply(Event::Completed { id: id.clone(), job: m.attempt(&id.clone()), bytes: majik_core::images::solid_png(8, 8, [1, 2, 3]), is_upscaled: false }, cx));
        cx.run_until_parked();
        lib2.read_with(cx, |m, _| {
            let it = m.lib.get(&id).unwrap();
            assert_eq!(it.status, Status::Completed);
            assert!(it.job_id.is_none() && it.poll_url.is_none(), "the handle is spent");
            assert!(it.path.as_ref().is_some_and(|p| p.is_file()));
        });
    }

    #[gpui::test]
    fn recovery_leaves_a_row_the_engine_is_already_running_alone(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let (library, jobs) = crate::test_support::reopen_recording(&e, cx);
        // Generated in this process, accepted by the provider, still running when the keys finish
        // loading and recovery runs.
        let id = library.update(cx, |m, cx| {
            let id = m.generate(vec![image_request("live")], &[], None, cx).remove(0);
            m.lib.mark_running(&id, Some("mock-image-live".into()), None);
            m.recover_in_flight();
            id
        });
        let jobs = jobs.lock().unwrap();
        assert!(matches!(jobs.as_slice(), [Job::Generate { id: submitted, .. }] if submitted == &id), "no Resume on top of the live run: {jobs:?}");
        library.read_with(cx, |m, _| assert_eq!(m.lib.get(&id).unwrap().status, Status::Generating));
    }

    #[gpui::test]
    fn relaunch_fails_a_row_without_a_handle_as_interrupted(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = in_flight_row(&e, cx, "Mock", None, None);
        let (lib2, jobs) = crate::test_support::reopen_recording(&e, cx);
        assert!(jobs.lock().unwrap().is_empty(), "nothing to re-attach to");
        lib2.read_with(cx, |m, _| {
            let it = m.lib.get(&id).unwrap();
            assert_eq!(it.status, Status::Failed);
            assert_eq!(it.error_kind.as_deref(), Some("interrupted"));
            assert!(it.can_retry(), "Retry re-submits from the stored request");
        });
    }

    #[gpui::test]
    fn relaunch_fails_a_row_past_its_deadline_as_timed_out(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = in_flight_row(&e, cx, "Mock", Some("mock-image-1"), None);
        let past_deadline = majik_generation::engine::stale_timeout(MediaType::Image).as_millis() as u64 + 1_000;
        e.library.update(cx, |m, _| m.lib.set_created_at(&id, majik_core::now_ms() - past_deadline));
        let (lib2, jobs) = crate::test_support::reopen_recording(&e, cx);
        assert!(jobs.lock().unwrap().is_empty(), "a stale job isn't worth polling");
        lib2.read_with(cx, |m, _| {
            let it = m.lib.get(&id).unwrap();
            assert_eq!(it.status, Status::Failed);
            assert_eq!(it.error_kind.as_deref(), Some("timeout"));
            assert_eq!(it.error.as_deref(), Some("Generation timed out. Please try again."));
        });
    }

    /// A 30 s render is given a longer budget than the flat video default, and a relaunch has to
    /// use that same budget. Otherwise a clip still rendering at the provider is treated as timed
    /// out and the user loses a render they have already paid for.
    #[gpui::test]
    fn relaunch_judges_a_long_video_on_its_own_deadline(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let long = video_request(30);
        let flat = majik_generation::engine::stale_timeout(MediaType::Video);
        let own = majik_generation::engine::stale_timeout_for(&long.generation_type);
        assert!(own > flat, "a 30 s clip earns more than the flat video default");

        let id = e.library.update(cx, |m, _| {
            let id = m.lib.add_generating(MediaType::Video, Some(long.to_json()), Some("mock".into()), Some("Mock".into()), None);
            m.lib.mark_running(&id, Some("mock-video-1".into()), Some("https://poll.example/1".to_string()));
            id
        });
        // Older than the flat default, but still inside the budget this clip was given.
        let age = flat.as_millis() as u64 + 5_000;
        e.library.update(cx, |m, _| m.lib.set_created_at(&id, majik_core::now_ms() - age));

        let (lib2, jobs) = crate::test_support::reopen_recording(&e, cx);
        lib2.read_with(cx, |m, _| {
            let it = m.lib.get(&id).unwrap();
            assert_eq!(it.status, Status::Generating, "still rendering, not timed out");
            assert_eq!(it.error_kind, None);
        });
        let submitted = jobs.lock().unwrap().clone();
        match submitted.as_slice() {
            [Job::Resume { remaining, .. }] => {
                assert!(*remaining > Duration::ZERO, "resumed with time left");
                assert!(*remaining <= own - Duration::from_millis(age), "resumed for what its own deadline had left");
            }
            other => panic!("expected one Resume, got {other:?}"),
        }
    }

    #[gpui::test]
    fn relaunch_measures_a_retried_rows_age_from_its_attempt(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = in_flight_row(&e, cx, "Mock", Some("mock-image-1"), None);
        // The row is long past the image deadline; its second attempt was accepted just now.
        e.library.update(cx, |m, _| {
            m.lib.set_created_at(&id, majik_core::now_ms() - 3600 * 1000);
            m.lib.fail_generation(&id, "interrupted");
            m.lib.start_attempt(&id).unwrap();
            m.lib.mark_running(&id, Some("mock-image-2".into()), None);
        });
        let (lib2, jobs) = crate::test_support::reopen_recording(&e, cx);
        let jobs = jobs.lock().unwrap();
        let [Job::Resume { handle, remaining, .. }] = jobs.as_slice() else { panic!("the fresh attempt is resumed, got {jobs:?}") };
        assert_eq!(handle.job_id, "mock-image-2");
        assert!(*remaining > Duration::from_secs(400), "the time left is the attempt's: {remaining:?}");
        lib2.read_with(cx, |m, _| assert_eq!(m.lib.get(&id).unwrap().status, Status::Generating));
    }

    #[gpui::test]
    fn relaunch_cannot_resume_a_synchronous_provider(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = in_flight_row(&e, cx, "OpenRouter", Some("chatcmpl-1"), None);
        let (lib2, jobs) = crate::test_support::reopen_recording(&e, cx);
        assert!(jobs.lock().unwrap().is_empty());
        lib2.read_with(cx, |m, _| assert_eq!(m.lib.get(&id).unwrap().error_kind.as_deref(), Some("interrupted")));
    }

    #[gpui::test]
    fn relaunch_resumes_an_upscale_as_upscaled(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let _id = in_flight_row(&e, cx, "Replicate", Some("pred-1"), Some(ToolId::Upscale));
        let (_lib2, jobs) = crate::test_support::reopen_recording(&e, cx);
        let jobs = jobs.lock().unwrap();
        assert!(matches!(jobs.as_slice(), [Job::Resume { is_upscaled: true, provider, .. }] if *provider == ProviderId::replicate()), "{jobs:?}");
    }

    #[gpui::test]
    fn accepted_event_stores_the_handle_on_the_row(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let id = in_flight_row(&e, cx, "Mock", None, None);
        e.library.update(cx, |m, cx| m.apply(Event::Accepted { id: id.clone(), job: m.attempt(&id), external_id: Some("mock-image-9".into()), poll_url: None }, cx));
        e.library.read_with(cx, |m, _| {
            let it = m.lib.get(&id).unwrap();
            assert_eq!(it.status, Status::Generating);
            assert_eq!(it.job_id.as_deref(), Some("mock-image-9"));
        });
    }
}

#[cfg(test)]
mod album_tests {
    use super::*;
    use crate::test_support::env;
    use gpui::TestAppContext;
    use majik_core::{FeedFilter, MediaFilter};
    use majik_generation::{GenerationType, Request};
    use majik_providers::{catalog, AspectRatio, ImageGenerationSettings, ImageResolution};

    fn req() -> Request {
        Request::new(ProviderId::mock(), GenerationType::Image(ImageGenerationSettings { model: catalog::image::ALL[0].clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd }), "p", vec![])
    }

    #[gpui::test]
    fn generating_into_an_album_adds_membership(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let album = e.library.update(cx, |m, cx| m.create_album("A".into(), cx));
        let ids = e.library.update(cx, |m, cx| m.generate(vec![req(), req()], &[], Some(album.clone()), cx));
        e.library.read_with(cx, |m, _| {
            let feed = m.lib.feed(&FeedFilter::Album(album.clone()), MediaFilter::All);
            assert_eq!(feed.len(), 2);
            assert!(ids.iter().all(|id| feed.contains(id)));
        });
    }

    #[gpui::test]
    fn add_and_remove_from_album(cx: &mut TestAppContext) {
        let e = env(cx, 2, "Mock");
        let ids: Vec<_> = e.library.read_with(cx, |m, _| m.lib.generations().iter().map(|i| i.id.clone()).collect());
        let album = e.library.update(cx, |m, cx| m.create_album("Trip".into(), cx));
        e.library.update(cx, |m, cx| m.add_to_album(&album, &ids, cx));
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.feed(&FeedFilter::Album(album.clone()), MediaFilter::All).len(), 2));
        e.library.update(cx, |m, cx| m.remove_from_album(&album, &[ids[0].clone()], cx));
        e.library.read_with(cx, |m, _| {
            let feed = m.lib.feed(&FeedFilter::Album(album.clone()), MediaFilter::All);
            assert_eq!(feed.len(), 1, "removed from album only");
            assert!(m.lib.get(&ids[0]).is_some(), "still in the library");
        });
    }

    #[gpui::test]
    fn favorites_span_the_whole_library(cx: &mut TestAppContext) {
        let e = env(cx, 3, "Mock");
        let ids: Vec<_> = e.library.read_with(cx, |m, _| m.lib.generations().iter().map(|i| i.id.clone()).collect());
        e.library.update(cx, |m, cx| m.set_favorite(&[ids[0].clone(), ids[2].clone()], true, cx));
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.feed(&FeedFilter::Favorites, MediaFilter::All).len(), 2));
        // Unfavorite one.
        e.library.update(cx, |m, cx| m.set_favorite(&[ids[0].clone()], false, cx));
        e.library.read_with(cx, |m, _| assert_eq!(m.lib.feed(&FeedFilter::Favorites, MediaFilter::All).len(), 1));
    }

    #[gpui::test]
    fn tool_rows_appear_in_library_feed(cx: &mut TestAppContext) {
        use majik_core::model::ToolId;
        let e = env(cx, 2, "Mock");
        let ids: Vec<_> = e.library.read_with(cx, |m, _| m.lib.generations().iter().map(|i| i.id.clone()).collect());
        e.library.update(cx, |m, cx| m.run_tool(ToolId::Upscale, &ids, ProviderId::mock(), None, cx));
        e.library.read_with(cx, |m, _| {
            let feed = m.lib.feed(&FeedFilter::Library, MediaFilter::All);
            assert_eq!(feed.len(), 4, "the 2 originals and their 2 tool rows sit in the same feed");
            assert_eq!(feed.iter().filter(|id| m.lib.get(id).unwrap().tool == Some(ToolId::Upscale)).count(), 2);
        });
    }
}
