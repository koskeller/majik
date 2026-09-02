//! A folder of files (assets) plus a SQLite database of app state (`.majik/library.db`).
//!
//! The database is the source of truth. An [`Asset`] is a file the library holds: a generation's
//! output (`<uuid>.<ext>` in the root), an input it was given, or an import (`.majik/assets/`). A
//! [`Generation`] stores no file of its own but references its output asset and, through
//! [`GenerationInput`]s, the assets it consumed, so using an output as an input again shares the
//! row rather than copying bytes. Files that merely sit in the folder are not assets.
//!
//! On open every asset is checked against the folder: one whose file is gone is flagged
//! [`Asset::missing`] and its generation shown as [`Status::Missing`]. Such a row is never dropped —
//! favourites, albums and the request survive, and everything recovers when the file returns. A
//! generating or failed row whose output file exists is promoted to completed (a crash between
//! writing the file and the row). Deleting a generation soft-deletes it and leaves its assets alone; an asset can only be
//! trashed once no live generation references it.

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::db::{content_type_for_file, extension_for_content_type, Db, ASSETS_PREFIX};
use majik_storage::{BlobStore, LocalBlobStore};
use std::sync::Arc;
use crate::model::{Album, AlbumId, Asset, AssetId, Entry, EntryId, GenerationJob, JobId, JobStatus, JobTrace, GenerationId, GenerationInput, Generation, MediaType, Status, ToolId};
use crate::{now_ms, thumbnails, video};

pub const CACHE_DIR_NAME: &str = ".majik";
const DB_FILE: &str = "library.db";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FeedFilter {
    Library,
    Favorites,
    Album(AlbumId),
    /// Every asset (outputs, inputs and imports) rather than generations.
    Assets,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MediaFilter {
    #[default]
    All,
    Image,
    Video,
    Audio,
}

impl MediaFilter {
    pub const ALL: [MediaFilter; 4] = [MediaFilter::All, MediaFilter::Image, MediaFilter::Video, MediaFilter::Audio];

    pub fn label(self) -> &'static str {
        match self {
            MediaFilter::All => "All Items",
            MediaFilter::Image => "Photos",
            MediaFilter::Video => "Videos",
            MediaFilter::Audio => "Audio",
        }
    }

    pub fn matches(self, t: MediaType) -> bool {
        matches!(
            (self, t),
            (MediaFilter::All, _) | (MediaFilter::Image, MediaType::Image) | (MediaFilter::Video, MediaType::Video) | (MediaFilter::Audio, MediaType::Audio)
        )
    }
}

pub struct Library {
    root: PathBuf,
    cache_dir: PathBuf,
    /// Backing store for library-owned content (assets, thumbnails, trash). Local today; an
    /// S3-compatible backend can replace it without changing the rest of the library.
    blobs: Arc<dyn BlobStore>,
    db: Db,
    /// Live generations, newest first.
    generations: Vec<Generation>,
    albums: Vec<Album>,
    index: HashMap<GenerationId, usize>,
    /// Newest first.
    assets: Vec<Asset>,
    asset_index: HashMap<AssetId, usize>,
    /// Links of the live generations only.
    inputs: Vec<GenerationInput>,
}

impl Library {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root: PathBuf = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let cache_dir = root.join(CACHE_DIR_NAME);
        std::fs::create_dir_all(&cache_dir)?;
        let db = Db::open(&cache_dir.join(DB_FILE))?;
        let blobs: Arc<dyn BlobStore> = Arc::new(LocalBlobStore::new(&root));
        let mut lib = Self {
            root,
            cache_dir,
            blobs,
            db,
            generations: Vec::new(),
            albums: Vec::new(),
            index: HashMap::new(),
            assets: Vec::new(),
            asset_index: HashMap::new(),
            inputs: Vec::new(),
        };
        lib.reload()?;
        Ok(lib)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.cache_dir.join("assets")
    }

    /// The blob store backing library-owned content.
    pub fn blobs(&self) -> Arc<dyn BlobStore> {
        self.blobs.clone()
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn generations(&self) -> &[Generation] {
        &self.generations
    }

    pub fn get(&self, id: &GenerationId) -> Option<&Generation> {
        self.index.get(id).map(|&i| &self.generations[i])
    }

    pub fn get_mut(&mut self, id: &GenerationId) -> Option<&mut Generation> {
        match self.index.get(id) {
            Some(&i) => Some(&mut self.generations[i]),
            None => None,
        }
    }

    pub fn albums(&self) -> &[Album] {
        &self.albums
    }

    pub fn album(&self, id: &AlbumId) -> Option<&Album> {
        self.albums.iter().find(|a| &a.id == id)
    }

    /// Generations matching a sidebar filter and a media-type filter, newest first. The Assets
    /// filter lists no generations (see [`Self::entries`]).
    pub fn feed(&self, filter: &FeedFilter, media: MediaFilter) -> Vec<GenerationId> {
        self.generations_in(filter, media, false).map(|it| it.id.clone()).collect()
    }

    fn generations_in<'a>(&'a self, filter: &'a FeedFilter, media: MediaFilter, favorites_only: bool) -> impl Iterator<Item = &'a Generation> + 'a {
        let album_items: Option<&Vec<GenerationId>> = match filter {
            FeedFilter::Album(id) => self.album(id).map(|a| &a.items),
            _ => None,
        };
        self.generations
            .iter()
            .filter(move |it| media.matches(it.media_type))
            .filter(move |it| !favorites_only || it.is_favorite)
            .filter(move |it| match filter {
                FeedFilter::Library => true,
                FeedFilter::Favorites => it.is_favorite,
                FeedFilter::Album(_) => album_items.map(|v| v.contains(&it.id)).unwrap_or(false),
                FeedFilter::Assets => false,
            })
    }

    /// What the grid shows for a filter: generations, or for [`FeedFilter::Assets`] every asset
    /// (missing ones included), newest first. `favorites_only` is the grid's own toggle and keeps
    /// only favorited generations; assets carry no favorite, so the Assets feed ignores it.
    pub fn entries(&self, filter: &FeedFilter, media: MediaFilter, favorites_only: bool) -> Vec<EntryId> {
        match filter {
            FeedFilter::Assets => self.assets.iter().filter(|a| media.matches(a.kind)).map(|a| EntryId::Asset(a.id.clone())).collect(),
            _ => self.generations_in(filter, media, favorites_only).map(|it| EntryId::Generation(it.id.clone())).collect(),
        }
    }

    pub fn entry(&self, id: &EntryId) -> Option<Entry<'_>> {
        match id {
            EntryId::Generation(id) => self.get(id).map(Entry::Generation),
            EntryId::Asset(id) => self.asset(id).map(Entry::Asset),
        }
    }

    /// Rows still marked as generating (candidates for resume / stale cleanup).
    pub fn in_flight(&self) -> Vec<Generation> {
        self.generations.iter().filter(|i| i.status == Status::Generating).cloned().collect()
    }

    /// Kept for API compatibility: every mutation is written through immediately.
    pub fn save(&self) -> Result<()> {
        Ok(())
    }

    // ----- assets and links ------------------------------------------------------

    /// Every asset, newest first (missing ones included).
    pub fn assets(&self) -> &[Asset] {
        &self.assets
    }

    pub fn asset(&self, id: &AssetId) -> Option<&Asset> {
        self.asset_index.get(id).map(|&i| &self.assets[i])
    }

    fn asset_mut(&mut self, id: &AssetId) -> Option<&mut Asset> {
        match self.asset_index.get(id) {
            Some(&i) => Some(&mut self.assets[i]),
            None => None,
        }
    }

    /// The assets a generation was given, with their roles, ordered by role then position. A link
    /// whose asset row is gone is skipped.
    pub fn inputs(&self, id: &GenerationId) -> Vec<(GenerationInput, Asset)> {
        let mut inputs: Vec<(GenerationInput, Asset)> =
            self.inputs.iter().filter(|i| &i.generation_id == id).filter_map(|i| self.asset(&i.asset_id).map(|a| (i.clone(), a.clone()))).collect();
        inputs.sort_by(|a, b| (&a.0.role, a.0.position).cmp(&(&b.0.role, b.0.position)));
        inputs
    }

    /// Live generations that consumed the asset, newest first.
    pub fn generations_using(&self, id: &AssetId) -> Vec<GenerationId> {
        self.generations.iter().filter(|it| self.inputs.iter().any(|i| i.generation_id == it.id && &i.asset_id == id)).map(|it| it.id.clone()).collect()
    }

    /// The live generation whose output the asset is.
    pub fn generation_producing(&self, id: &AssetId) -> Option<GenerationId> {
        self.generations.iter().find(|it| it.output_asset_id.as_ref() == Some(id)).map(|it| it.id.clone())
    }

    /// Whether any live generation references the asset (as its output or one of its inputs). This
    /// alone decides whether it may be deleted.
    pub fn is_referenced(&self, id: &AssetId) -> bool {
        self.generation_producing(id).is_some() || self.inputs.iter().any(|i| &i.asset_id == id)
    }

}

impl Library {

    // ----- loading ---------------------------------------------------------------

    /// Re-read every row from the database and reconcile it with the folder (see the module docs).
    /// Newest first.
    pub fn reload(&mut self) -> Result<()> {
        let mut assets = self.db.load_assets(&self.root)?;
        for asset in &mut assets {
            let key = blob_key(&self.root, &asset.path);
            // One store lookup per asset tells both whether the file is there and how big it is.
            match key.as_deref().and_then(|key| self.blobs.len(key)) {
                Some(size) => {
                    asset.missing = false;
                    let mut dirty = asset.file_size != Some(size);
                    asset.file_size = Some(size);
                    if asset.width.is_none() || asset.height.is_none() {
                        dirty |= fill_asset_dimensions(asset);
                    }
                    if dirty {
                        self.db.set_asset_info(&asset.id, asset.width, asset.height, asset.file_size, asset.duration_secs)?;
                    }
                }
                None => {
                    tracing::warn!(target: "majik", "asset {}: file missing at {}", asset.id, asset.path.display());
                    asset.missing = true;
                }
            }
            if let Some(key) = asset.thumbnail.as_deref().and_then(thumbnails::thumb_key_for_path) {
                if !self.blobs.exists(&key) {
                    asset.thumbnail = None;
                }
            }
        }
        self.assets = assets;
        self.reindex_assets();
        self.inputs = self.db.load_inputs()?;

        let mut items = Vec::new();
        for mut item in self.db.load_generations()? {
            let mut dirty = false;
            let mut output = item.output_asset_id.as_ref().and_then(|id| self.asset(id)).cloned();
            if output.is_none() && item.status != Status::Completed {
                // A crash after the output asset was written but before the row flipped: the file
                // is named after the row, so its asset is recognisable.
                let stem = item.id.0.as_str();
                if let Some(orphan) = self.assets.iter().find(|a| !a.missing && a.path.file_stem().is_some_and(|s| s == stem) && self.generation_producing(&a.id).is_none()) {
                    item.output_asset_id = Some(orphan.id.clone());
                    output = Some(orphan.clone());
                    dirty = true;
                }
            }
            match (item.status, &output) {
                (Status::Completed | Status::Missing, Some(asset)) if !asset.missing => {
                    item.status = Status::Completed;
                    adopt_output(&mut item, asset);
                }
                (Status::Completed | Status::Missing, Some(asset)) => {
                    item.status = Status::Missing;
                    adopt_output(&mut item, asset);
                }
                (Status::Completed | Status::Missing, None) => {
                    tracing::warn!(target: "majik", "{}: no output asset", item.id);
                    item.status = Status::Missing;
                }
                (Status::Generating | Status::Failed, Some(asset)) if !asset.missing => {
                    // The file is there. Either the row never flipped (a crash mid-completion, so
                    // the attempt is still open and completes along with it), or a Missing row's
                    // file came back after a retry of it failed: the row mirrors the attempt that
                    // produced the file again, and the failed retry stays in the history unchanged.
                    // Only a file no attempt accounts for is credited to the active one.
                    let jobs = self.db.load_jobs(&item.id)?;
                    let active = item.active_job_id.as_ref().and_then(|id| jobs.iter().find(|j| &j.id == id));
                    let producer = jobs.iter().find(|j| j.status == JobStatus::Completed && j.output_asset_id.as_ref() == Some(&asset.id));
                    match (active, producer) {
                        (Some(job), Some(producer)) if job.status.is_terminal() => item.active_job_id = Some(producer.id.clone()),
                        (Some(job), _) => self.db.complete_job(&job.id, &asset.id, now_ms())?,
                        (None, _) => {}
                    }
                    item.status = Status::Completed;
                    item.error = None;
                    item.error_kind = None;
                    item.job_id = None;
                    item.poll_url = None;
                    adopt_output(&mut item, asset);
                    dirty = true;
                }
                (Status::Generating | Status::Failed, Some(_)) => {
                    // A retried Missing row keeps its output asset until it completes again; once the
                    // job has failed that asset points at nothing, so drop it like a fresh failure.
                    item.output_asset_id = None;
                    dirty = true;
                }
                (Status::Generating | Status::Failed, None) => {}
            }
            if dirty {
                self.db.upsert_generation(&item)?;
            }
            items.push(item);
        }
        self.generations = items;
        self.reindex();
        self.albums = self.db.load_albums()?;
        self.sweep_thumbnails();
        Ok(())
    }

    /// Drop thumbnails no asset points at any more (the asset was deleted or its file changed,
    /// which changes the thumbnail key). Thumbnails are a cache, so sweeping too much only costs a
    /// regeneration and a failure only costs disk space, so this is best-effort with a log.
    /// Remove every stored tier of a thumbnail. Best-effort with a log, like the sweep: a
    /// thumbnail is a cache, and a file left behind only costs disk space until the next sweep.
    fn delete_thumbnail_tiers(&self, thumb: &Path, what: &str) {
        for tier in thumbnails::TIERS {
            let Some(key) = thumbnails::sized_thumb_path(thumb, tier).as_deref().and_then(thumbnails::thumb_key_for_path) else { continue };
            if let Err(e) = self.blobs.delete(&key) {
                tracing::warn!(target: "majik", "removing thumbnail {what}: {e:#}");
            }
        }
    }

    fn sweep_thumbnails(&self) {
        // Every tier of a live asset's thumbnail, not just the standard one it records: the large
        // tier is a sibling file (`<hash>@800.jpg`), and sweeping it would delete on every launch
        // the work the feed did on the last one.
        let referenced: std::collections::HashSet<String> = self
            .assets
            .iter()
            .filter_map(|asset| asset.thumbnail.as_ref())
            .flat_map(|thumb| thumbnails::TIERS.iter().filter_map(move |tier| thumbnails::sized_thumb_path(thumb, *tier)))
            .filter_map(|thumb| thumb.file_name().map(|name| name.to_string_lossy().into_owned()))
            .collect();
        let keys = match self.blobs.list(thumbnails::THUMBS_PREFIX) {
            Ok(keys) => keys,
            Err(e) => {
                tracing::warn!(target: "majik", "listing thumbnails: {e:#}");
                return;
            }
        };
        for key in keys {
            let name = key.rsplit('/').next().unwrap_or(&key);
            if referenced.contains(name) {
                continue;
            }
            if let Err(e) = self.blobs.delete(&key) {
                tracing::warn!(target: "majik", "removing stale thumbnail {key}: {e:#}");
            }
        }
    }

    fn reindex(&mut self) {
        self.index = self.generations.iter().enumerate().map(|(i, it)| (it.id.clone(), i)).collect();
    }

    fn reindex_assets(&mut self) {
        self.asset_index = self.assets.iter().enumerate().map(|(i, a)| (a.id.clone(), i)).collect();
    }

    fn persist(&self, id: &GenerationId) -> Result<()> {
        match self.get(id) {
            Some(item) => self.db.upsert_generation(item),
            None => Ok(()),
        }
    }

    /// Copy an asset's file fields into every generation whose output it is.
    fn sync_items_from_asset(&mut self, id: &AssetId) {
        let Some(asset) = self.asset(id).cloned() else { return };
        for item in self.generations.iter_mut().filter(|it| it.output_asset_id.as_ref() == Some(id)) {
            adopt_output(item, &asset);
        }
    }

    // ----- mutations -------------------------------------------------------------

    /// Thumbnail of an item's output (thumbnails live on assets; this resolves the output).
    pub fn set_thumbnail(&mut self, id: &GenerationId, thumb: PathBuf) {
        let Some(asset_id) = self.get(id).and_then(|it| it.output_asset_id.clone()) else { return };
        self.set_asset_thumbnail(&asset_id, thumb);
    }

    pub fn set_asset_thumbnail(&mut self, id: &AssetId, thumb: PathBuf) {
        if let Some(asset) = self.asset_mut(id) {
            asset.thumbnail = Some(thumb.clone());
        }
        if let Err(e) = self.db.set_asset_thumbnail(id, Some(&thumb)) {
            tracing::warn!(target: "majik", "persisting thumbnail of {id}: {e:#}");
        }
        self.sync_items_from_asset(id);
    }

    pub fn set_favorite(&mut self, id: &GenerationId, favorite: bool) {
        if let Some(it) = self.get_mut(id) {
            it.is_favorite = favorite;
        }
        let _ = self.db.set_favorite(id, favorite);
    }

    pub fn create_album(&mut self, name: impl Into<String>) -> AlbumId {
        let album = Album { id: AlbumId::new(), name: name.into(), created_at_ms: now_ms(), items: Vec::new() };
        let _ = self.db.insert_album(&album);
        let id = album.id.clone();
        self.albums.push(album);
        id
    }

    pub fn rename_album(&mut self, id: &AlbumId, name: impl Into<String>) {
        let name = name.into();
        if let Some(a) = self.albums.iter_mut().find(|a| &a.id == id) {
            a.name = name.clone();
        }
        let _ = self.db.rename_album(id, &name);
    }

    pub fn delete_album(&mut self, id: &AlbumId) {
        self.albums.retain(|a| &a.id != id);
        let _ = self.db.delete_album(id);
    }

    pub fn add_to_album(&mut self, album: &AlbumId, ids: &[GenerationId]) {
        if let Some(a) = self.albums.iter_mut().find(|a| &a.id == album) {
            for id in ids {
                if !a.items.contains(id) {
                    a.items.push(id.clone());
                }
            }
        }
        let _ = self.db.add_to_album(album, ids, now_ms());
    }

    pub fn remove_from_album(&mut self, album: &AlbumId, ids: &[GenerationId]) {
        if let Some(a) = self.albums.iter_mut().find(|a| &a.id == album) {
            a.items.retain(|i| !ids.contains(i));
        }
        let _ = self.db.remove_from_album(album, ids);
    }

    /// Delete generations: they leave every feed and album, their assets stay (the output remains
    /// an asset of the library; see [`Self::delete_assets`]). An attempt still in flight is
    /// recorded as canceled with the row, since the engine's own outcome is never applied to a
    /// deleted row. Best-effort per item: a row that can't be written is kept, so in-memory state
    /// never disagrees with what was actually removed.
    pub fn delete_generations(&mut self, ids: &[GenerationId]) -> Result<()> {
        let mut deleted: Vec<GenerationId> = Vec::new();
        let mut first_err: Option<anyhow::Error> = None;
        for id in ids {
            if self.get(id).is_none() {
                continue;
            }
            let open = self.active_job(id).filter(|job| !job.status.is_terminal()).map(|job| job.id);
            let now = now_ms();
            let written = self.db.transaction(|db| {
                if let Some(job) = &open {
                    db.finish_job(job, JobStatus::Canceled, Some("Cancelled."), Some("cancelled"), now)?;
                }
                db.soft_delete_generation(id, now)
            });
            if let Err(e) = written {
                first_err.get_or_insert(e);
                continue;
            }
            for a in &mut self.albums {
                a.items.retain(|i| i != id);
            }
            deleted.push(id.clone());
        }
        self.generations.retain(|it| !deleted.contains(&it.id));
        self.inputs.retain(|i| !deleted.contains(&i.generation_id));
        self.reindex();
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Trash assets: the file goes to `.majik/trash/` (nothing is ever hard-deleted) and the row
    /// goes with it. Refused for an asset a live generation still references, since the generation
    /// would lose its output or an input it can be retried with. Best-effort per asset, first error
    /// returned.
    pub fn delete_assets(&mut self, ids: &[AssetId]) -> Result<()> {
        let mut deleted: Vec<AssetId> = Vec::new();
        let mut first_err: Option<anyhow::Error> = None;
        for id in ids {
            let Some(asset) = self.asset(id).cloned() else { continue };
            if self.is_referenced(id) {
                first_err.get_or_insert_with(|| anyhow!("{} is used by a generation", asset.file_name()));
                continue;
            }
            if !asset.missing {
                // Unique trash key so a later same-named delete can't overwrite this one.
                let key = format!(".majik/trash/{}-{}", now_ms(), asset.file_name());
                if let Err(e) = self.blobs.adopt(&key, &asset.path) {
                    first_err.get_or_insert_with(|| anyhow!("trashing {}: {e}", asset.path.display()));
                    continue;
                }
            }
            if let Err(e) = self.db.delete_asset(id) {
                first_err.get_or_insert(e);
                continue;
            }
            if let Some(thumb) = asset.thumbnail.as_deref() {
                self.delete_thumbnail_tiers(thumb, &format!("of {id}"));
            }
            deleted.push(id.clone());
        }
        self.assets.retain(|a| !deleted.contains(&a.id));
        self.reindex_assets();
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Insert a placeholder row in `Generating` state with its first attempt queued; returns its
    /// id (the attempt is [`Library::active_job`]).
    pub fn add_generating(
        &mut self,
        media_type: MediaType,
        request_json: Option<String>,
        model_name: Option<String>,
        provider: Option<String>,
        tool: Option<ToolId>,
    ) -> GenerationId {
        let id = GenerationId::new();
        let now = now_ms();
        let job = queued_job(&id, 1, now);
        let item = Generation {
            id: id.clone(),
            path: None,
            media_type,
            status: Status::Generating,
            created_at_ms: now,
            width: None,
            height: None,
            duration_secs: None,
            file_size: None,
            is_favorite: false,
            is_upscaled: false,
            thumbnail: None,
            output_asset_id: None,
            request_json,
            model_name,
            provider,
            error: None,
            error_kind: None,
            tool,
            job_id: None,
            poll_url: None,
            queued_at_ms: now,
            started_at_ms: None,
            active_job_id: Some(job.id.clone()),
        };
        // The row first (the job references it), then the attempt it points at.
        let written = self.db.transaction(|db| {
            let mut row = item.clone();
            row.active_job_id = None;
            db.upsert_generation(&row)?;
            db.insert_job(&job)?;
            db.set_active_job(&id, &job.id)
        });
        if let Err(e) = written {
            tracing::warn!(target: "majik", "persisting new row {id}: {e:#}");
        }
        self.generations.insert(0, item);
        self.reindex();
        id
    }

    // ----- attempts ---------------------------------------------------------------

    /// The attempt a row currently mirrors.
    pub fn active_job(&self, id: &GenerationId) -> Option<GenerationJob> {
        let job = self.get(id)?.active_job_id.as_ref()?;
        match self.db.job(job) {
            Ok(job) => job,
            Err(e) => {
                tracing::warn!(target: "majik", "reading the active attempt of {id}: {e:#}");
                None
            }
        }
    }

    /// Every attempt of a generation, first to last.
    pub fn jobs(&self, id: &GenerationId) -> Vec<GenerationJob> {
        self.db.load_jobs(id).unwrap_or_else(|e| {
            tracing::warn!(target: "majik", "reading the attempts of {id}: {e:#}");
            Vec::new()
        })
    }

    /// The HTTP exchanges of one attempt, in order.
    pub fn traces(&self, job: &JobId) -> Vec<JobTrace> {
        self.db.load_traces(job).unwrap_or_else(|e| {
            tracing::warn!(target: "majik", "reading the traces of attempt {job}: {e:#}");
            Vec::new()
        })
    }

    /// Append one exchange to an attempt's traces (bodies bounded) and fold it into the attempt's
    /// provider columns, as one write. Touches nothing the feed shows.
    pub fn record_trace(&mut self, job: &JobId, trace: JobTrace) -> Result<()> {
        let trace = trace.bounded();
        self.db.transaction(|db| db.record_trace(job, &trace)).map(|_| ())
    }

    /// Start the next attempt of a failed or missing row (a retry): a new queued job becomes the
    /// active one and the row flips back to generating, keeping its output asset so a regenerated
    /// file is written under the same id. Refused while the active attempt is still in flight; one
    /// attempt at a time is what keeps the engine's events unambiguous. Nothing changes in memory
    /// unless the attempt was written, so a row never spins for a job that doesn't exist.
    pub fn start_attempt(&mut self, id: &GenerationId) -> Result<JobId> {
        let item = self.get(id).ok_or_else(|| anyhow!("unknown item {id}"))?;
        if let Some(active) = self.active_job(id) {
            if !active.status.is_terminal() {
                return Err(anyhow!("{id}: attempt {} is still {:?}", active.attempt, active.status));
            }
        }
        let now = now_ms();
        let job = queued_job(&item.id, self.db.next_attempt(id)?, now);
        let mut row = item.clone();
        row.status = Status::Generating;
        row.error = None;
        row.error_kind = None;
        row.job_id = None;
        row.poll_url = None;
        row.queued_at_ms = now;
        row.started_at_ms = None;
        row.active_job_id = Some(job.id.clone());
        self.db.transaction(|db| {
            db.insert_job(&job)?;
            db.upsert_generation(&row)
        })?;
        if let Some(it) = self.get_mut(id) {
            *it = row;
        }
        Ok(job.id)
    }

    /// The provider accepted the row's attempt under a handle (kept for a resume after relaunch).
    pub fn mark_running(&mut self, id: &GenerationId, external_id: Option<String>, poll_url: Option<String>) {
        let now = now_ms();
        let Some(it) = self.get_mut(id) else { return };
        it.job_id = external_id.clone();
        it.poll_url = poll_url.clone();
        it.started_at_ms.get_or_insert(now);
        let Some(job) = it.active_job_id.clone() else { return };
        if let Err(e) = self.db.mark_job_running(&job, external_id.as_deref(), poll_url.as_deref(), now) {
            tracing::warn!(target: "majik", "persisting the handle of {id}: {e:#}");
        }
    }

    /// Record the assets a generation was given, in order; `role` keys are
    /// `majik_providers::AssetRole::raw`. Positions count within each role.
    pub fn attach_inputs(&mut self, id: &GenerationId, inputs: &[(AssetId, &str)]) -> Result<()> {
        let mut per_role: HashMap<&str, usize> = HashMap::new();
        for (asset_id, role) in inputs {
            let position = per_role.entry(role).or_default();
            let link = GenerationInput { generation_id: id.clone(), asset_id: asset_id.clone(), role: role.to_string(), position: *position };
            *position += 1;
            self.db.insert_input(&link)?;
            self.inputs.retain(|i| !(i.generation_id == link.generation_id && i.asset_id == link.asset_id && i.role == link.role));
            self.inputs.push(link);
        }
        Ok(())
    }

    /// Add a file to the library as an asset of its own (a composer drop, an import). Content
    /// addressed, so the same bytes again return the existing asset, and an asset whose file had
    /// gone missing gets it back. Returns the asset's id.
    pub fn import_asset(&mut self, content_type: &str, bytes: &[u8]) -> Result<AssetId> {
        let hash = content_hash(bytes);
        if let Some(id) = self.db.find_asset_by_hash(&hash)? {
            if let Some(existing) = self.asset(&id).cloned() {
                if existing.missing {
                    let key = blob_key(&self.root, &existing.path).ok_or_else(|| anyhow!("asset {id} has no blob key"))?;
                    self.blobs.put(&key, bytes)?;
                    if let Some(asset) = self.asset_mut(&id) {
                        asset.missing = false;
                        asset.file_size = Some(bytes.len() as u64);
                    }
                    self.sync_items_from_asset(&id);
                }
                return Ok(id);
            }
        }
        let kind = MediaType::from_content_type(content_type).ok_or_else(|| anyhow!("unsupported content type {content_type}"))?;
        let file_name = format!("{hash}.{}", extension_for_content_type(content_type));
        let key = format!("{ASSETS_PREFIX}/{file_name}");
        if self.blobs.len(&key) != Some(bytes.len() as u64) {
            self.blobs.put(&key, bytes)?;
        }
        let mut asset = Asset {
            id: AssetId::new(),
            content_hash: Some(hash),
            kind,
            content_type: content_type.to_string(),
            path: self.blobs.local_path(&key)?,
            width: None,
            height: None,
            file_size: Some(bytes.len() as u64),
            duration_secs: None,
            created_at_ms: now_ms(),
            thumbnail: None,
            missing: false,
        };
        fill_asset_dimensions(&mut asset);
        self.db.insert_asset(&asset, &key)?;
        let id = asset.id.clone();
        self.assets.insert(0, asset);
        self.reindex_assets();
        Ok(id)
    }

    /// Fill in dimensions / duration discovered later (e.g. audio probed in the background). They
    /// belong to the output asset.
    pub fn set_media_info(&mut self, id: &GenerationId, width: Option<u32>, height: Option<u32>, duration_secs: Option<f64>) {
        let Some(item) = self.get(id) else { return };
        let Some(asset_id) = item.output_asset_id.clone() else { return };
        if let Some(asset) = self.asset_mut(&asset_id) {
            if width.is_some() {
                asset.width = width;
            }
            if height.is_some() {
                asset.height = height;
            }
            if duration_secs.is_some() {
                asset.duration_secs = duration_secs;
            }
        }
        if let Err(e) = self.db.set_asset_info(&asset_id, width, height, None, duration_secs) {
            tracing::warn!(target: "majik", "persisting info of {asset_id}: {e:#}");
        }
        self.sync_items_from_asset(&asset_id);
    }

    /// Rewrite a row's creation time, and its active attempt's along with it. Only tests use it,
    /// to age a row past its stale deadline.
    pub fn set_created_at(&mut self, id: &GenerationId, created_at_ms: u64) {
        if let Some(it) = self.get_mut(id) {
            it.created_at_ms = created_at_ms;
            it.queued_at_ms = created_at_ms;
        }
        if let Err(e) = self.persist(id) {
            tracing::warn!(target: "majik", "persisting created_at of {id}: {e:#}");
        }
        if let Some(job) = self.get(id).and_then(|it| it.active_job_id.clone()) {
            if let Err(e) = self.db.set_job_created_at(&job, created_at_ms) {
                tracing::warn!(target: "majik", "persisting created_at of attempt {job}: {e:#}");
            }
        }
    }

    /// An asset's bytes, through the blob store (the only way library content is read).
    pub fn asset_bytes(&self, asset: &Asset) -> Result<Vec<u8>> {
        if asset.missing {
            return Err(anyhow!("{} has no file", asset.file_name()));
        }
        let key = blob_key(&self.root, &asset.path).ok_or_else(|| anyhow!("{} is outside the library", asset.path.display()))?;
        self.blobs.read(&key)
    }

    /// Write the produced bytes as `<id>.<ext>`, register them as the row's output asset and mark
    /// the row and its active attempt completed, in one transaction. A regenerated row (retry of a
    /// missing file) reuses its asset row. Memory changes only once the database has: on a failed
    /// write the row is still generating, with its attempt open to record the failure.
    pub fn complete_generation(&mut self, id: &GenerationId, bytes: &[u8], is_upscaled: bool) -> Result<PathBuf> {
        let item = self.get(id).ok_or_else(|| anyhow!("unknown item {id}"))?;
        let media_type = item.media_type;
        let file_name = format!("{}.{}", id, media_type.file_extension());
        let existing = item.output_asset_id.as_ref().and_then(|a| self.asset(a)).filter(|a| a.file_name() == file_name).cloned();
        self.blobs.put(&file_name, bytes)?;
        let path = self.blobs.local_path(&file_name)?;
        let hash = content_hash(bytes);
        let content_type = content_type_for_file(&file_name, media_type).to_string();
        let old_thumbnail = existing.as_ref().and_then(|a| a.thumbnail.clone());
        let is_new = existing.is_none();
        let mut asset = match existing {
            Some(mut asset) => {
                asset.content_hash = Some(hash);
                asset.content_type = content_type;
                asset.file_size = Some(bytes.len() as u64);
                asset.width = None;
                asset.height = None;
                asset.duration_secs = None;
                asset.thumbnail = None;
                asset.missing = false;
                asset
            }
            None => Asset {
                id: AssetId::new(),
                content_hash: Some(hash),
                kind: media_type,
                content_type,
                path: path.clone(),
                width: None,
                height: None,
                file_size: Some(bytes.len() as u64),
                duration_secs: None,
                created_at_ms: now_ms(),
                thumbnail: None,
                missing: false,
            },
        };
        // Only cheap image header reads here; video dims are probed off-thread by the app.
        if media_type == MediaType::Image {
            fill_asset_dimensions(&mut asset);
        }
        let asset_id = asset.id.clone();
        let mut row = item.clone();
        row.status = Status::Completed;
        row.is_upscaled = is_upscaled;
        row.error = None;
        row.error_kind = None;
        row.job_id = None;
        row.poll_url = None;
        row.output_asset_id = Some(asset_id.clone());
        adopt_output(&mut row, &asset);
        let now = now_ms();
        self.db.transaction(|db| {
            if is_new {
                db.insert_asset(&asset, &file_name)?;
            } else {
                db.update_asset_file(&asset)?;
            }
            db.upsert_generation(&row)?;
            match &row.active_job_id {
                Some(job) => db.complete_job(job, &asset_id, now),
                None => Ok(()),
            }
        })?;
        if is_new {
            self.assets.insert(0, asset);
            self.reindex_assets();
        } else if let Some(stored) = self.asset_mut(&asset_id) {
            *stored = asset;
        }
        if let Some(it) = self.get_mut(id) {
            *it = row;
        }
        if let Some(thumb) = old_thumbnail.as_deref() {
            self.delete_thumbnail_tiers(thumb, &format!("of {asset_id}"));
        }
        Ok(path)
    }

    pub fn fail_generation(&mut self, id: &GenerationId, message: impl Into<String>) {
        self.fail_generation_kind(id, message, None);
    }

    /// The attempt failed: the row shows the error, the attempt records it.
    pub fn fail_generation_kind(&mut self, id: &GenerationId, message: impl Into<String>, kind: Option<&str>) {
        self.end_attempt(id, JobStatus::Failed, message.into(), kind);
    }

    /// The user cancelled the attempt: shown as a failure ("Cancelled."), recorded as canceled.
    pub fn cancel_generation(&mut self, id: &GenerationId) {
        self.end_attempt(id, JobStatus::Canceled, "Cancelled.".into(), Some("cancelled"));
    }

    /// The row's attempt is over as `status`. An attempt already recorded as over (a retry refused
    /// before it could start, on a row whose last attempt failed, or completed for a Missing row)
    /// keeps what it recorded: the refusal becomes a new attempt that ended immediately, so the
    /// history never rewrites what a provider reported.
    fn end_attempt(&mut self, id: &GenerationId, status: JobStatus, message: String, kind: Option<&str>) {
        let now = now_ms();
        let refused = match self.active_job(id) {
            Some(active) if active.status.is_terminal() => match self.db.next_attempt(id) {
                Ok(attempt) => {
                    let mut job = queued_job(id, attempt, now);
                    job.status = status;
                    job.error = Some(message.clone());
                    job.error_kind = kind.map(str::to_string);
                    job.finished_at_ms = Some(now);
                    Some(job)
                }
                Err(e) => {
                    tracing::warn!(target: "majik", "numbering the refused attempt of {id}: {e:#}");
                    None
                }
            },
            _ => None,
        };
        let Some(it) = self.get_mut(id) else { return };
        it.status = Status::Failed;
        it.error = Some(message.clone());
        it.error_kind = kind.map(str::to_string);
        it.job_id = None;
        it.poll_url = None;
        if let Some(job) = &refused {
            it.active_job_id = Some(job.id.clone());
        }
        let row = it.clone();
        let written = self.db.transaction(|db| {
            match (&refused, &row.active_job_id) {
                (Some(job), _) => db.insert_job(job)?,
                (None, Some(job)) => db.finish_job(job, status, Some(&message), kind, now)?,
                (None, None) => {}
            }
            db.upsert_generation(&row)
        });
        if let Err(e) = written {
            tracing::warn!(target: "majik", "persisting the failure of {id}: {e:#}");
        }
    }
}

/// A fresh, not yet submitted attempt of `media`.
fn queued_job(media: &GenerationId, attempt: u32, now: u64) -> GenerationJob {
    GenerationJob {
        id: JobId::new(),
        generation_id: media.clone(),
        attempt,
        status: JobStatus::Queued,
        external_id: None,
        poll_url: None,
        output_asset_id: None,
        error: None,
        error_kind: None,
        provider_request_json: None,
        provider_create_response_json: None,
        provider_final_response_json: None,
        created_at_ms: now,
        started_at_ms: None,
        finished_at_ms: None,
    }
}

/// The content hash of a blob: what an asset row stores and what dedupes an import.
pub fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The blob key of a file under the root (forward slashes, whatever the platform).
fn blob_key(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = relative.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Some(parts.join("/"))
}

/// Copy an output asset's file fields onto its generation.
fn adopt_output(item: &mut Generation, asset: &Asset) {
    item.path = Some(asset.path.clone());
    item.width = asset.width;
    item.height = asset.height;
    item.duration_secs = asset.duration_secs;
    item.file_size = asset.file_size;
    item.thumbnail = asset.thumbnail.clone();
}

/// Header-only reads; returns whether anything was learned.
fn fill_asset_dimensions(asset: &mut Asset) -> bool {
    match asset.kind {
        MediaType::Image => match thumbnails::image_dimensions(&asset.path) {
            Some((w, h)) => {
                asset.width = Some(w);
                asset.height = Some(h);
                true
            }
            None => false,
        },
        MediaType::Video => match video::probe(&asset.path) {
            Ok(info) => {
                asset.width = info.width;
                asset.height = info.height;
                asset.duration_secs = info.duration_secs;
                true
            }
            Err(_) => false,
        },
        MediaType::Audio => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(seed: u8) -> Vec<u8> {
        crate::images::solid_png(4, 4, [seed, seed, seed])
    }

    /// A library with `n` completed generated images, plus a stray `notes.txt` and `stray.png`
    /// in the folder that must never show up.
    fn temp_library(n: u8) -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stray.png"), png(200)).unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"ignored").unwrap();
        let mut lib = Library::open(dir.path()).unwrap();
        for i in 0..n {
            let id = lib.add_generating(MediaType::Image, Some(format!(r#"{{"prompt":"p{i}"}}"#)), Some("m".into()), Some("Mock".into()), None);
            lib.complete_generation(&id, &png(i), false).unwrap();
        }
        (dir, lib)
    }

    /// Every tier of a live thumbnail survives a reopen, and all of them go when the asset does.
    /// The sweep used to keep only the standard `<hash>.<ext>` name, so the large tier the feed had
    /// just rendered was deleted on the next launch and had to be rendered again.
    #[test]
    fn reopening_keeps_every_tier_and_deleting_an_asset_takes_them_all() {
        let (dir, mut lib) = temp_library(1);
        let item = lib.generations()[0].clone();
        let path = item.path.clone().unwrap();
        let standard = thumbnails::ensure_thumbnail_for(&path, MediaType::Image, lib.blobs().as_ref()).unwrap();
        let large = thumbnails::ensure_thumbnail_sized(&path, MediaType::Image, thumbnails::THUMB_LARGE, lib.blobs().as_ref()).unwrap();
        let asset_id = item.output_asset_id.clone().unwrap();
        lib.set_asset_thumbnail(&asset_id, standard.clone());
        assert!(standard.exists() && large.exists());

        // Reopening runs the sweep.
        drop(lib);
        Library::open(dir.path()).unwrap();
        assert!(standard.exists(), "the standard tier survived");
        assert!(large.exists(), "and so did the large one");

        // A thumbnail nothing references still goes.
        let orphan = dir.path().join(thumbnails::THUMBS_PREFIX).join("deadbeef@800.jpg");
        std::fs::write(&orphan, png(1)).unwrap();
        let mut lib = Library::open(dir.path()).unwrap();
        assert!(!orphan.exists(), "an unreferenced tier is still swept");

        lib.delete_generations(std::slice::from_ref(&item.id)).unwrap();
        lib.delete_assets(&[asset_id]).unwrap();
        assert!(!standard.exists() && !large.exists(), "deleting the asset took every tier with it");
    }

    #[test]
    fn folder_files_are_not_library_items() {
        let (dir, lib) = temp_library(2);
        assert_eq!(lib.generations().len(), 2);
        assert!(lib.generations().iter().all(|i| i.file_name() != "stray.png"));
        let reopened = Library::open(dir.path()).unwrap();
        assert_eq!(reopened.generations().len(), 2, "still only generated rows after reopening");
        assert!(reopened.generations().iter().all(|i| i.status == Status::Completed && (i.width, i.height) == (Some(4), Some(4))));
    }

    #[test]
    fn favorites_and_albums_round_trip() {
        let (dir, mut lib) = temp_library(2);
        let id = lib.generations()[0].id.clone();
        lib.set_favorite(&id, true);
        let album = lib.create_album("Trip");
        lib.add_to_album(&album, std::slice::from_ref(&id));

        let lib2 = Library::open(dir.path()).unwrap();
        assert!(lib2.get(&id).unwrap().is_favorite);
        assert_eq!(lib2.feed(&FeedFilter::Favorites, MediaFilter::All), vec![id.clone()]);
        assert_eq!(lib2.feed(&FeedFilter::Album(album), MediaFilter::All), vec![id]);
    }

    #[test]
    fn entries_favorites_only_keeps_favorited_generations_and_every_asset() {
        let (_dir, mut lib) = temp_library(3);
        let favorite = lib.generations()[1].id.clone();
        lib.set_favorite(&favorite, true);
        let album = lib.create_album("Trip");
        lib.add_to_album(&album, &[lib.generations()[0].id.clone(), favorite.clone()]);

        assert_eq!(lib.entries(&FeedFilter::Library, MediaFilter::All, true), vec![EntryId::Generation(favorite.clone())]);
        assert_eq!(lib.entries(&FeedFilter::Album(album), MediaFilter::All, true), vec![EntryId::Generation(favorite)]);
        assert_eq!(lib.entries(&FeedFilter::Library, MediaFilter::Video, true), vec![], "combines with the media filter");
        assert_eq!(lib.entries(&FeedFilter::Library, MediaFilter::All, false).len(), 3);
        assert_eq!(lib.entries(&FeedFilter::Assets, MediaFilter::All, true).len(), 3, "assets have no favorite to filter on");
    }

    #[test]
    fn feed_library_includes_tool_rows() {
        let (_dir, mut lib) = temp_library(1);
        let id = lib.add_generating(MediaType::Image, None, Some("Mock Upscale".into()), Some("Mock".into()), Some(ToolId::Upscale));
        let feed = lib.feed(&FeedFilter::Library, MediaFilter::All);
        assert_eq!(feed.len(), 2, "tool output is a library item like any other");
        assert_eq!(feed[0], id, "newest first");
        assert_eq!(lib.feed(&FeedFilter::Favorites, MediaFilter::All).len(), 0);
    }

    #[test]
    fn generation_lifecycle_and_inputs() {
        let (dir, mut lib) = temp_library(2);
        let req = r#"{"provider":"Mock","kind":"image","model":"flux-2-pro","prompt":"hello"}"#.to_string();
        let id = lib.add_generating(MediaType::Image, Some(req), Some("FLUX.2 Pro".into()), Some("Mock".into()), None);
        lib.mark_running(&id, Some("job-1".into()), Some("https://poll".into()));
        let input = lib.import_asset("image/png", &crate::images::solid_png(2, 2, [0, 0, 0])).unwrap();
        assert!(lib.asset(&input).unwrap().path.starts_with(lib.assets_dir()));
        lib.attach_inputs(&id, &[(input.clone(), "reference_image")]).unwrap();
        assert_eq!(lib.generations()[0].status, Status::Generating);
        assert_eq!(lib.feed(&FeedFilter::Library, MediaFilter::All).len(), 3);
        assert!(lib.is_referenced(&input));

        // Relaunch mid-generation: the row, its job id and its inputs survive without a file.
        let lib_mid = Library::open(dir.path()).unwrap();
        let mid = lib_mid.get(&id).unwrap();
        assert_eq!(mid.status, Status::Generating);
        assert_eq!(mid.job_id.as_deref(), Some("job-1"));
        let inputs = lib_mid.inputs(&id);
        assert_eq!(inputs.len(), 1);
        assert_eq!((inputs[0].0.role.as_str(), &inputs[0].1.id), ("reference_image", &input));
        assert_eq!(lib_mid.generations_using(&input), vec![id.clone()]);
        drop(lib_mid);

        let png = crate::images::solid_png(6, 4, [9, 9, 9]);
        let path = lib.complete_generation(&id, &png, false).unwrap();
        assert!(path.exists());
        let item = lib.get(&id).unwrap();
        assert_eq!(item.width, Some(6));
        let output = item.output_asset_id.clone().expect("the output is an asset");
        assert_eq!(lib.asset(&output).unwrap().path, path);
        assert_eq!(lib.generation_producing(&output), Some(id.clone()));

        let lib2 = Library::open(dir.path()).unwrap();
        let it = lib2.get(&id).unwrap();
        assert_eq!(it.status, Status::Completed);
        assert_eq!(it.prompt().as_deref(), Some("hello"));
        assert!(it.can_recreate());
        assert!(it.job_id.is_none());
        assert_eq!(it.path.as_deref(), Some(path.as_path()));

        // Deleting the generation leaves its assets — and the file — alone …
        let mut lib3 = lib2;
        lib3.delete_generations(std::slice::from_ref(&id)).unwrap();
        assert!(lib3.get(&id).is_none());
        assert!(path.exists());
        assert!(lib3.asset(&output).is_some() && lib3.asset(&input).is_some());
        assert!(!lib3.is_referenced(&output) && !lib3.is_referenced(&input), "no live generation references them any more");
        // … so the assets can now be trashed explicitly.
        lib3.delete_assets(std::slice::from_ref(&output)).unwrap();
        assert!(lib3.asset(&output).is_none());
        assert!(!path.exists());
        assert!(dir.path().join(CACHE_DIR_NAME).join("trash").read_dir().unwrap().count() == 1, "trashed, not removed");
    }

    #[test]
    fn a_referenced_asset_cannot_be_deleted() {
        let (_dir, mut lib) = temp_library(1);
        let id = lib.generations()[0].id.clone();
        let output = lib.get(&id).unwrap().output_asset_id.clone().unwrap();
        let err = lib.delete_assets(std::slice::from_ref(&output)).unwrap_err();
        assert!(err.to_string().contains("used by a generation"), "{err}");
        assert!(lib.asset(&output).is_some() && lib.get(&id).unwrap().path.as_ref().unwrap().exists());
    }

    #[test]
    fn using_an_output_as_an_input_shares_the_asset() {
        let (dir, mut lib) = temp_library(1);
        let source = lib.generations()[0].id.clone();
        let output = lib.get(&source).unwrap().output_asset_id.clone().unwrap();
        let next = lib.add_generating(MediaType::Video, None, None, Some("Mock".into()), None);
        lib.attach_inputs(&next, &[(output.clone(), "first_frame")]).unwrap();
        assert_eq!(lib.assets().len(), 1, "no copy was made");
        assert_eq!(lib.generations_using(&output), vec![next.clone()]);
        assert_eq!(lib.generation_producing(&output), Some(source.clone()));
        // Deleting the source generation keeps the asset the other one needs.
        lib.delete_generations(std::slice::from_ref(&source)).unwrap();
        assert!(lib.is_referenced(&output));
        let lib2 = Library::open(dir.path()).unwrap();
        assert_eq!(lib2.inputs(&next)[0].1.id, output);
        assert!(lib2.generation_producing(&output).is_none());
    }

    #[test]
    fn import_dedupes_on_content_and_restores_a_missing_file() {
        let (_dir, mut lib) = temp_library(0);
        let bytes = png(5);
        let a = lib.import_asset("image/png", &bytes).unwrap();
        let b = lib.import_asset("image/png", &bytes).unwrap();
        assert_eq!(a, b, "same bytes, same asset");
        assert_eq!(lib.assets().len(), 1);
        let asset = lib.asset(&a).unwrap().clone();
        assert_eq!((asset.kind, asset.width, asset.file_size), (MediaType::Image, Some(4), Some(bytes.len() as u64)));
        assert!(lib.import_asset("text/plain", b"nope").is_err(), "not a media type");

        std::fs::remove_file(&asset.path).unwrap();
        lib.reload().unwrap();
        assert!(lib.asset(&a).unwrap().missing && lib.asset(&a).unwrap().file().is_none());
        assert_eq!(lib.import_asset("image/png", &bytes).unwrap(), a);
        assert!(!lib.asset(&a).unwrap().missing && asset.path.exists(), "importing the bytes again brings the file back");
    }

    #[test]
    fn removed_file_marks_row_missing_and_keeps_its_metadata() {
        let (dir, mut lib) = temp_library(2);
        let id = lib.generations()[0].id.clone();
        let path = lib.get(&id).unwrap().path.clone().unwrap();
        lib.set_favorite(&id, true);
        let album = lib.create_album("Trip");
        lib.add_to_album(&album, std::slice::from_ref(&id));
        drop(lib);

        std::fs::remove_file(&path).unwrap();
        let mut lib = Library::open(dir.path()).unwrap();
        assert_eq!(lib.generations().len(), 2, "the row is kept");
        let item = lib.get(&id).unwrap();
        assert_eq!(item.status, Status::Missing);
        assert_eq!(item.path.as_deref(), Some(path.as_path()), "still knows where the file belongs");
        assert!(item.is_favorite && item.can_retry());
        assert_eq!(lib.feed(&FeedFilter::Album(album.clone()), MediaFilter::All), vec![id.clone()]);

        // Mutating a missing row writes it back as completed with its file name intact …
        lib.set_favorite(&id, false);
        assert_eq!(lib.db().load_generations().unwrap().iter().find(|i| i.id == id).unwrap().status, Status::Completed);
        drop(lib);
        let lib = Library::open(dir.path()).unwrap();
        assert_eq!(lib.get(&id).unwrap().status, Status::Missing, "… but it is missing again on the next open");
        drop(lib);

        // … and it recovers by itself once the file is back.
        std::fs::write(&path, png(7)).unwrap();
        let lib = Library::open(dir.path()).unwrap();
        assert_eq!(lib.get(&id).unwrap().status, Status::Completed);
    }

    #[test]
    fn all_files_gone_keeps_every_row() {
        let (dir, lib) = temp_library(3);
        let paths: Vec<PathBuf> = lib.generations().iter().filter_map(|i| i.path.clone()).collect();
        drop(lib);
        for p in &paths {
            std::fs::remove_file(p).unwrap();
        }
        let lib = Library::open(dir.path()).unwrap();
        assert_eq!(lib.generations().len(), 3);
        assert!(lib.generations().iter().all(|i| i.status == Status::Missing));
    }

    #[test]
    fn reload_picks_up_a_regenerated_or_removed_file() {
        let (_dir, mut lib) = temp_library(1);
        let id = lib.generations()[0].id.clone();
        let path = lib.get(&id).unwrap().path.clone().unwrap();
        std::fs::remove_file(&path).unwrap();
        lib.reload().unwrap();
        assert_eq!(lib.get(&id).unwrap().status, Status::Missing);
        // Regenerating in place (retry) writes the file again under the same id.
        lib.start_attempt(&id).unwrap();
        lib.complete_generation(&id, &png(3), false).unwrap();
        assert_eq!(lib.get(&id).unwrap().status, Status::Completed);
        lib.reload().unwrap();
        assert_eq!(lib.get(&id).unwrap().status, Status::Completed);
    }

    #[test]
    fn a_retried_missing_row_that_fails_drops_its_stale_path() {
        let (_dir, mut lib) = temp_library(1);
        let id = lib.generations()[0].id.clone();
        assert!(lib.get(&id).unwrap().file().is_some(), "a completed row has a file to read");
        std::fs::remove_file(lib.get(&id).unwrap().path.clone().unwrap()).unwrap();
        lib.reload().unwrap();
        let missing = lib.get(&id).unwrap();
        assert!(missing.path.is_some() && missing.file().is_none(), "a missing row knows where its file should be but has none to read");
        // Retry in place, and the provider fails: the row must not keep pointing at nothing.
        lib.start_attempt(&id).unwrap();
        lib.fail_generation(&id, "provider down");
        lib.reload().unwrap();
        let failed = lib.get(&id).unwrap();
        assert_eq!(failed.status, Status::Failed);
        assert!(failed.path.is_none() && failed.file().is_none());
    }

    #[test]
    fn deleting_a_missing_row_removes_it() {
        let (_dir, mut lib) = temp_library(1);
        let id = lib.generations()[0].id.clone();
        std::fs::remove_file(lib.get(&id).unwrap().path.clone().unwrap()).unwrap();
        lib.reload().unwrap();
        lib.delete_generations(std::slice::from_ref(&id)).unwrap();
        assert!(lib.get(&id).is_none());
        assert_eq!(lib.db().generation_count().unwrap(), 0);
        assert!(lib.assets()[0].missing, "its missing output asset stays, unreferenced");
        assert!(!lib.is_referenced(&lib.assets()[0].id.clone()));
    }

    #[test]
    fn file_written_before_crash_promotes_interrupted_row() {
        let (dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, None, None, Some("Mock".into()), None);
        let file = format!("{id}.png");
        lib.fail_generation(&id, "interrupted");
        // Simulate the output asset being written without the row ever flipping.
        std::fs::write(dir.path().join(&file), png(1)).unwrap();
        let asset = Asset {
            id: AssetId::new(),
            content_hash: None,
            kind: MediaType::Image,
            content_type: "image/png".into(),
            path: dir.path().join(&file),
            width: None,
            height: None,
            file_size: None,
            duration_secs: None,
            created_at_ms: 1,
            thumbnail: None,
            missing: false,
        };
        lib.db().insert_asset(&asset, &file).unwrap();
        drop(lib);
        let lib = Library::open(dir.path()).unwrap();
        let item = lib.get(&id).unwrap();
        assert_eq!(item.status, Status::Completed);
        assert!(item.error.is_none() && item.width == Some(4));
    }

    fn attempts(lib: &Library, id: &GenerationId) -> Vec<(u32, JobStatus)> {
        lib.jobs(id).iter().map(|j| (j.attempt, j.status)).collect()
    }

    /// The row's status mirrors its active attempt's after every transition.
    fn assert_mirrored(lib: &Library, id: &GenerationId) {
        let item = lib.get(id).unwrap();
        let job = lib.active_job(id).expect("an active attempt");
        let expected = match job.status {
            JobStatus::Queued | JobStatus::Running => Status::Generating,
            JobStatus::Completed => Status::Completed,
            JobStatus::Failed | JobStatus::Canceled => Status::Failed,
        };
        assert_eq!(item.status, expected, "attempt {} is {:?}", job.attempt, job.status);
        assert_eq!(item.error, job.error);
        assert_eq!(item.error_kind, job.error_kind);
        assert_eq!(item.output_asset_id.is_some() && item.status == Status::Completed, job.output_asset_id.is_some() && job.status == JobStatus::Completed);
    }

    #[test]
    fn attempt_lifecycle_queued_running_completed() {
        let (dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, Some("{}".into()), Some("m".into()), Some("Mock".into()), None);
        let job = lib.active_job(&id).expect("attempt 1 queued with the row");
        assert_eq!((job.attempt, job.status, job.started_at_ms), (1, JobStatus::Queued, None));
        assert_eq!(lib.get(&id).unwrap().active_job_id, Some(job.id.clone()));
        assert_mirrored(&lib, &id);

        lib.mark_running(&id, Some("ext-1".into()), Some("https://poll".into()));
        let running = lib.active_job(&id).unwrap();
        assert_eq!((running.status, running.external_id.as_deref(), running.poll_url.as_deref()), (JobStatus::Running, Some("ext-1"), Some("https://poll")));
        assert!(running.started_at_ms.is_some());
        assert_eq!(lib.get(&id).unwrap().job_id.as_deref(), Some("ext-1"));
        assert_eq!(lib.get(&id).unwrap().started_at_ms, running.started_at_ms, "the row's clock is the attempt's start");
        assert_mirrored(&lib, &id);
        // What the row shows after a relaunch is what the attempt says.
        let reopened = Library::open(dir.path()).unwrap();
        assert_eq!((reopened.get(&id).unwrap().job_id.as_deref(), reopened.get(&id).unwrap().status), (Some("ext-1"), Status::Generating));
        assert_eq!(reopened.get(&id).unwrap().started_at_ms, running.started_at_ms);
        drop(reopened);

        lib.complete_generation(&id, &png(1), false).unwrap();
        let done = lib.active_job(&id).unwrap();
        assert_eq!(done.status, JobStatus::Completed);
        assert_eq!(done.output_asset_id, lib.get(&id).unwrap().output_asset_id);
        assert!(done.finished_at_ms.is_some() && done.error.is_none());
        assert_eq!(done.external_id.as_deref(), Some("ext-1"), "the attempt keeps its handle");
        assert!(lib.get(&id).unwrap().job_id.is_none(), "… the row stops offering it for resume");
        assert_mirrored(&lib, &id);
        assert_eq!(attempts(&lib, &id), [(1, JobStatus::Completed)]);
    }

    #[test]
    fn failed_attempt_records_error_and_kind_and_retry_is_attempt_two() {
        let (dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, Some("{}".into()), None, Some("Mock".into()), None);
        let first = lib.active_job(&id).unwrap().id;
        assert!(lib.start_attempt(&id).is_err(), "one attempt at a time");
        lib.fail_generation_kind(&id, "boom", Some("server_error"));
        let failed = lib.active_job(&id).unwrap();
        assert_eq!((failed.status, failed.error.as_deref(), failed.error_kind.as_deref()), (JobStatus::Failed, Some("boom"), Some("server_error")));
        assert!(failed.finished_at_ms.is_some());
        assert_mirrored(&lib, &id);
        let reopened = Library::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&id).unwrap().error.as_deref(), Some("boom"), "the error is read back from the attempt");
        drop(reopened);

        let second = lib.start_attempt(&id).unwrap();
        assert_ne!(second, first);
        assert_eq!(lib.get(&id).unwrap().active_job_id, Some(second.clone()));
        let item = lib.get(&id).unwrap();
        assert_eq!((item.status, item.error.as_deref()), (Status::Generating, None));
        assert_eq!(item.started_at_ms, None, "a retry's clock starts when the provider takes it");
        assert_eq!(attempts(&lib, &id), [(1, JobStatus::Failed), (2, JobStatus::Queued)]);
        assert_mirrored(&lib, &id);
        lib.complete_generation(&id, &png(2), false).unwrap();
        assert_eq!(attempts(&lib, &id), [(1, JobStatus::Failed), (2, JobStatus::Completed)]);
        assert_eq!(lib.jobs(&id)[0].error.as_deref(), Some("boom"), "history is kept");
    }

    #[test]
    fn a_retry_restarts_the_rows_clock() {
        let (dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, Some("{}".into()), None, Some("Mock".into()), None);
        lib.set_created_at(&id, 1_000);
        assert_eq!(lib.get(&id).unwrap().queued_at_ms, 1_000, "the first attempt's clock is the row's");
        lib.fail_generation(&id, "boom");
        lib.start_attempt(&id).unwrap();
        let item = lib.get(&id).unwrap();
        let attempt = lib.active_job(&id).unwrap();
        assert_eq!(item.created_at_ms, 1_000, "the row keeps its place in the feed");
        assert_eq!(item.queued_at_ms, attempt.created_at_ms, "… but its clock is the new attempt's");
        assert!(item.queued_at_ms > 1_000);
        let reopened = Library::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&id).unwrap().queued_at_ms, attempt.created_at_ms, "read back from the attempt");
    }

    #[test]
    fn cancel_marks_the_attempt_canceled_and_the_row_failed() {
        let (_dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Video, None, None, Some("Mock".into()), None);
        lib.cancel_generation(&id);
        let item = lib.get(&id).unwrap();
        assert_eq!((item.status, item.error.as_deref(), item.error_kind.as_deref()), (Status::Failed, Some("Cancelled."), Some("cancelled")));
        assert_eq!(lib.active_job(&id).unwrap().status, JobStatus::Canceled);
        assert_mirrored(&lib, &id);
        assert!(lib.start_attempt(&id).is_ok(), "a canceled attempt can be retried");
    }

    #[test]
    fn traces_are_kept_per_attempt_with_bounded_bodies() {
        let (_dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, None, None, Some("Mock".into()), None);
        let job = lib.active_job(&id).unwrap().id;
        let huge = "x".repeat(crate::model::TRACE_BODY_LIMIT * 3);
        let trace = JobTrace {
            at_ms: 1,
            label: crate::model::TraceLabel::Submit,
            method: "POST".into(),
            url: "https://provider.example/run".into(),
            status: Some(200),
            duration_ms: 9,
            request_body: Some(huge.clone()),
            response_body: Some(r#"{"request_id":"r1"}"#.into()),
            error: None,
        };
        lib.record_trace(&job, trace).unwrap();
        let traces = lib.traces(&job);
        assert_eq!(traces.len(), 1);
        assert!(traces[0].request_body.as_ref().unwrap().len() <= crate::model::TRACE_BODY_LIMIT, "bounded on the way in");
        let attempt = lib.active_job(&id).unwrap();
        assert_eq!(attempt.provider_create_response_json.as_deref(), Some(r#"{"request_id":"r1"}"#));
        assert!(attempt.provider_request_json.unwrap().ends_with("[__truncated__]"));
        assert!(lib.traces(&JobId::new()).is_empty(), "an unknown attempt has no trail");
    }

    #[test]
    fn promotion_after_a_crash_completes_the_active_attempt() {
        let (dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, None, None, Some("Mock".into()), None);
        // The file was written under the row's name, but the row (and the attempt) never flipped.
        std::fs::write(dir.path().join(format!("{id}.png")), png(4)).unwrap();
        let asset = crate::model::Asset {
            id: AssetId::new(),
            content_hash: None,
            kind: MediaType::Image,
            content_type: "image/png".into(),
            path: dir.path().join(format!("{id}.png")),
            width: None,
            height: None,
            file_size: None,
            duration_secs: None,
            created_at_ms: 1,
            thumbnail: None,
            missing: false,
        };
        lib.db().insert_asset(&asset, &format!("{id}.png")).unwrap();
        let reopened = Library::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&id).unwrap().status, Status::Completed);
        let attempt = reopened.active_job(&id).unwrap();
        assert_eq!((attempt.status, attempt.output_asset_id), (JobStatus::Completed, Some(asset.id)));
    }

    #[test]
    fn a_refused_retry_is_a_new_attempt_and_keeps_the_providers_verdict() {
        let (_dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, Some("{}".into()), None, Some("Mock".into()), None);
        lib.fail_generation_kind(&id, "Rate limited", Some("rate_limited"));
        // The app refuses the retry before it starts (its input is gone): attempt 1 is over already.
        lib.fail_generation(&id, "Can't retry: an input is no longer available.");
        assert_eq!(attempts(&lib, &id), [(1, JobStatus::Failed), (2, JobStatus::Failed)]);
        let jobs = lib.jobs(&id);
        assert_eq!((jobs[0].error.as_deref(), jobs[0].error_kind.as_deref()), (Some("Rate limited"), Some("rate_limited")), "the provider's verdict stands");
        assert!(jobs[1].error.as_deref().unwrap().contains("no longer available") && jobs[1].finished_at_ms.is_some());
        assert_mirrored(&lib, &id);
        // A Missing row's active attempt is its completed one: that one keeps its output too.
        let (_dir2, mut lib2) = temp_library(1);
        let done = lib2.generations()[0].id.clone();
        lib2.fail_generation(&done, "Can't retry: an input is no longer available.");
        let jobs = lib2.jobs(&done);
        assert_eq!((jobs[0].status, jobs[0].output_asset_id.is_some()), (JobStatus::Completed, true));
        assert_eq!(jobs[1].status, JobStatus::Failed);
        assert_mirrored(&lib2, &done);
    }

    #[test]
    fn a_restored_file_re_points_the_row_at_the_attempt_that_made_it() {
        let (dir, mut lib) = temp_library(1);
        let id = lib.generations()[0].id.clone();
        let path = lib.get(&id).unwrap().path.clone().unwrap();
        std::fs::remove_file(&path).unwrap();
        lib.reload().unwrap();
        assert_eq!(lib.get(&id).unwrap().status, Status::Missing);
        lib.start_attempt(&id).unwrap();
        lib.fail_generation_kind(&id, "provider down", Some("server_error"));
        // The file comes back (a backup, a re-import) before the next open.
        std::fs::write(&path, png(0)).unwrap();
        drop(lib);
        let lib = Library::open(dir.path()).unwrap();
        let item = lib.get(&id).unwrap();
        assert_eq!((item.status, item.error.as_deref()), (Status::Completed, None));
        assert!(item.file().is_some());
        let active = lib.active_job(&id).unwrap();
        assert_eq!((active.attempt, active.status), (1, JobStatus::Completed), "the row mirrors the attempt that produced its file");
        assert_eq!(attempts(&lib, &id), [(1, JobStatus::Completed), (2, JobStatus::Failed)]);
        assert_eq!(lib.jobs(&id)[1].error.as_deref(), Some("provider down"), "the failed retry is not rewritten as a success");
    }

    #[test]
    fn deleting_a_row_in_flight_cancels_its_attempt() {
        let (_dir, mut lib) = temp_library(0);
        let live = lib.add_generating(MediaType::Image, None, None, Some("Mock".into()), None);
        let failed = lib.add_generating(MediaType::Image, None, None, Some("Mock".into()), None);
        lib.fail_generation(&failed, "boom");
        lib.delete_generations(&[live.clone(), failed.clone()]).unwrap();
        let job = &lib.jobs(&live)[0];
        assert_eq!((job.status, job.error.as_deref()), (JobStatus::Canceled, Some("Cancelled.")));
        assert!(job.finished_at_ms.is_some(), "nothing stays in flight in the history");
        assert_eq!(lib.jobs(&failed)[0].status, JobStatus::Failed, "an attempt that was over is left as it was");
    }

    #[test]
    fn a_retry_that_cannot_be_written_leaves_the_row_as_it_was() {
        let (_dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, Some("{}".into()), None, Some("Mock".into()), None);
        lib.fail_generation_kind(&id, "boom", Some("server_error"));
        lib.db().set_read_only(true).unwrap();
        assert!(lib.start_attempt(&id).is_err());
        lib.db().set_read_only(false).unwrap();
        let item = lib.get(&id).unwrap();
        assert_eq!((item.status, item.error.as_deref()), (Status::Failed, Some("boom")), "no spinner for an attempt that doesn't exist");
        assert_eq!(attempts(&lib, &id), [(1, JobStatus::Failed)]);
        assert_mirrored(&lib, &id);
        assert!(lib.start_attempt(&id).is_ok(), "and the retry works once writes do");
    }

    #[test]
    fn a_completion_that_cannot_be_written_leaves_the_row_generating() {
        let (_dir, mut lib) = temp_library(0);
        let id = lib.add_generating(MediaType::Image, None, None, Some("Mock".into()), None);
        lib.db().set_read_only(true).unwrap();
        assert!(lib.complete_generation(&id, &png(1), false).is_err());
        lib.db().set_read_only(false).unwrap();
        let item = lib.get(&id).unwrap();
        assert_eq!((item.status, item.output_asset_id.is_none(), item.path.is_none()), (Status::Generating, true, true));
        assert!(lib.assets().is_empty(), "no asset the database doesn't have");
        assert_eq!(lib.active_job(&id).unwrap().status, JobStatus::Queued, "the attempt is still open for the failure to land on");
        lib.complete_generation(&id, &png(1), false).unwrap();
        assert_eq!((lib.get(&id).unwrap().status, lib.assets().len()), (Status::Completed, 1));
        assert_mirrored(&lib, &id);
    }

    #[test]
    fn asset_bytes_read_through_the_store_and_refuse_a_missing_file() {
        let (_dir, mut lib) = temp_library(1);
        let asset = lib.assets()[0].clone();
        assert_eq!(lib.asset_bytes(&asset).unwrap(), png(0));
        std::fs::remove_file(&asset.path).unwrap();
        lib.reload().unwrap();
        assert!(lib.asset_bytes(&lib.assets()[0].clone()).is_err());
    }

    #[test]
    fn delete_removes_the_thumbnail_and_reload_sweeps_stale_ones() {
        let (dir, mut lib) = temp_library(2);
        let (a, b) = (lib.generations()[0].id.clone(), lib.generations()[1].id.clone());
        let thumb_a = thumbnails::ensure_thumbnail(lib.get(&a).unwrap(), lib.blobs().as_ref()).unwrap();
        let thumb_b = thumbnails::ensure_thumbnail(lib.get(&b).unwrap(), lib.blobs().as_ref()).unwrap();
        lib.set_thumbnail(&a, thumb_a.clone());
        lib.set_thumbnail(&b, thumb_b.clone());
        assert_eq!(lib.get(&a).unwrap().thumbnail.as_deref(), Some(thumb_a.as_path()), "the item shows its output asset's thumbnail");
        let stale = dir.path().join(CACHE_DIR_NAME).join("thumbs").join("0123456789abcdef01234567.jpg");
        std::fs::write(&stale, b"orphan").unwrap();

        let asset_a = lib.get(&a).unwrap().output_asset_id.clone().unwrap();
        lib.delete_generations(std::slice::from_ref(&a)).unwrap();
        assert!(thumb_a.exists(), "the asset outlives its generation, thumbnail included");
        lib.delete_assets(std::slice::from_ref(&asset_a)).unwrap();
        assert!(!thumb_a.exists(), "deleted asset's thumbnail goes with it");
        assert!(thumb_b.exists() && stale.exists(), "nothing else touched by a delete");

        lib.reload().unwrap();
        assert!(!stale.exists(), "unreferenced thumbnail swept on reload");
        assert!(thumb_b.exists(), "referenced thumbnail kept");
        assert_eq!(lib.get(&b).unwrap().thumbnail.as_deref(), Some(thumb_b.as_path()));
    }

    #[test]
    fn library_folder_can_be_moved() {
        let (dir, mut lib) = temp_library(1);
        let id = lib.generations()[0].id.clone();
        let thumb = thumbnails::ensure_thumbnail(lib.get(&id).unwrap(), lib.blobs().as_ref()).unwrap();
        lib.set_thumbnail(&id, thumb);
        lib.set_favorite(&id, true);
        drop(lib);

        let moved = dir.path().with_file_name(format!("{}-moved", dir.path().file_name().unwrap().to_string_lossy()));
        std::fs::rename(dir.path(), &moved).unwrap();
        let lib = Library::open(&moved).unwrap();
        let item = lib.get(&id).unwrap();
        assert_eq!(item.status, Status::Completed);
        assert!(item.path.as_ref().unwrap().starts_with(&moved));
        let thumb = item.thumbnail.as_ref().expect("thumbnail follows the folder");
        assert!(thumb.starts_with(&moved) && thumb.exists());
        assert!(item.is_favorite);
        // Windows refuses to rename a folder while the SQLite file inside it is open.
        drop(lib);
        std::fs::rename(&moved, dir.path()).unwrap(); // let TempDir clean up
    }

    #[test]
    fn media_filter() {
        assert!(MediaFilter::All.matches(MediaType::Video));
        assert!(MediaFilter::Video.matches(MediaType::Video));
        assert!(!MediaFilter::Image.matches(MediaType::Video));
    }
}
